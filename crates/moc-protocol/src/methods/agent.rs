//! 子代理生命周期协议：创建、查询、消息控制与等待。

use crate::method::{ClientRequest, Method};
use crate::{FinishReason, Message};
use serde::{Deserialize, Serialize};

/// 子代理的运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentStatus {
    Queued,
    Running,
    Completed,
    Stopped,
    Failed,
    Idle,
}

/// 子代理信息；`id` 直接使用持久化子会话 ID。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInfo {
    pub id: String,
    pub parent_id: String,
    pub title: String,
    pub model: String,
    pub cwd: String,
    pub status: AgentStatus,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_message: Option<String>,
}

/// `agent/spawn`：创建并异步启动一个直接子代理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSpawn;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnParams {
    pub parent_session_id: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 仅允许 `inherit`、`default`、`explore`、`plan`；省略等同于 `inherit`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnResult {
    pub agent: AgentInfo,
}

/// `agent/list`：列出当前父会话的直接子代理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentList;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListParams {
    pub parent_session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListResult {
    pub agents: Vec<AgentInfo>,
}

/// `agent/send`：向非运行子代理启动下一次独立提示。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSend;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSendParams {
    pub parent_session_id: String,
    pub agent_id: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSendResult {
    pub accepted: bool,
    pub status: AgentStatus,
}

/// `agent/steer`：向运行中的子代理写入下一安全边界消息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSteer;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSteerParams {
    pub parent_session_id: String,
    pub agent_id: String,
    pub message: String,
}

/// `agent/stop`：请求取消运行中的子代理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentStop;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStopParams {
    pub parent_session_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStopResult {
    pub stopped: bool,
    pub status: AgentStatus,
}

/// `agent/wait`：等待子代理进入终态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentWait;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitParams {
    pub parent_session_id: String,
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitResult {
    pub agent: AgentInfo,
    pub completed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
}

macro_rules! client_method {
    ($ty:ident, $name:literal, $params:ty, $result:ty) => {
        impl Method for $ty {
            const NAME: &'static str = $name;
            type Params = $params;
            type Result = $result;
        }
        impl ClientRequest for $ty {}
    };
}

client_method!(
    AgentSpawn,
    "agent/spawn",
    AgentSpawnParams,
    AgentSpawnResult
);
client_method!(AgentList, "agent/list", AgentListParams, AgentListResult);
client_method!(AgentSend, "agent/send", AgentSendParams, AgentSendResult);
client_method!(
    AgentSteer,
    "agent/steer",
    AgentSteerParams,
    crate::SteerResult
);
client_method!(AgentStop, "agent/stop", AgentStopParams, AgentStopResult);
client_method!(AgentWait, "agent/wait", AgentWaitParams, AgentWaitResult);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn status_and_model_wire_shapes_are_stable() {
        assert_eq!(
            serde_json::to_value(AgentStatus::Running).unwrap(),
            json!("running")
        );
        let params = AgentSpawnParams {
            parent_session_id: "ses_parent".into(),
            task: "返回 CHILD_OK".into(),
            title: None,
            model: Some("explore".into()),
        };
        assert_eq!(serde_json::to_value(params).unwrap()["model"], "explore");
        assert!(
            serde_json::from_value::<AgentSpawnParams>(json!({
                "parent_session_id": "ses_parent",
                "task": "x",
                "unexpected": true
            }))
            .is_err()
        );
    }
    #[test]
    fn all_statuses_use_snake_case() {
        let values = [
            (AgentStatus::Queued, "queued"),
            (AgentStatus::Running, "running"),
            (AgentStatus::Completed, "completed"),
            (AgentStatus::Stopped, "stopped"),
            (AgentStatus::Failed, "failed"),
            (AgentStatus::Idle, "idle"),
        ];
        for (status, expected) in values {
            assert_eq!(
                serde_json::to_string(&status).unwrap(),
                format!("\"{expected}\"")
            );
        }
    }
}

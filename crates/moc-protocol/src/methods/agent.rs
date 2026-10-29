//! 子代理协议：创建、查询、消息投递、等待与结果交付。

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
    /// spawn 时选定的 agent preset；普通子代理为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
}

/// 一次代理间消息投递的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentMessageOutcome {
    /// 目标正在运行，消息进入其收件箱，将在下一个轮边界注入。
    Injected,
    /// 目标空闲，已用消息唤醒一个新回合。
    Woken,
    /// 投递失败（收件箱已满、目标正在收尾等），`error` 说明原因。
    Failed,
}

/// 一条代理间消息的来源元信息；随消息条目持久化。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageMeta {
    pub from_id: String,
    pub from_title: String,
    /// 落库条目只可能是 `Injected` 或 `Woken`。
    pub delivery: AgentMessageOutcome,
}

/// 单个目标的投递回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageReceipt {
    pub agent_id: String,
    pub title: String,
    pub outcome: AgentMessageOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// `agent/message`：向其它代理投递一条消息（运行中注入、空闲唤醒）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentMessage;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageParams {
    /// 调用方会话 ID。
    pub session_id: String,
    /// 代理 ID、`"parent"`（仅子代理可用）或 `"all"`（全部运行中的平级代理）。
    pub target: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageResult {
    /// 每个目标一条，按解析顺序。
    pub receipts: Vec<AgentMessageReceipt>,
}

/// 运行中 `wait_agent` 的进度行（`session/update` 的 `tool_progress` 使用）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolProgressRow {
    pub agent_id: String,
    pub title: String,
    pub status: AgentStatus,
    /// 自进入当前运行起的毫秒数；非运行状态为 0（渲染时不显示）。
    pub elapsed_ms: u64,
}

/// `agent/spawn`：创建并异步启动一个直接子代理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentSpawn;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnParams {
    /// 调用方会话 ID。
    pub session_id: String,
    pub task: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 仅允许 `inherit`、`default`、`explore`、`plan`；省略等同于 `inherit`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 已注册的 agent preset 名；省略为默认子代理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// 子代理 `yield` 交付物必须满足的 JSON Schema；省略则交付物为自由文本。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSpawnResult {
    pub agent: AgentInfo,
}

/// `agent/list`：列出调用方可见的平级代理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentList;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListParams {
    /// 调用方会话 ID。
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentListResult {
    pub agents: Vec<AgentInfo>,
}

/// `agent/stop`：请求取消运行中的子代理。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentStop;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentStopParams {
    /// 调用方会话 ID。
    pub session_id: String,
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

/// `agent/wait`：等待子代理进入终态或被消息唤醒。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AgentWait;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitParams {
    /// 调用方会话 ID。
    pub session_id: String,
    /// 省略或空 = 全部运行中的直接子代理。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_ids: Option<Vec<String>>,
    /// 无事件窗口；省略用默认 180 秒。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// 单个被等待代理的快照。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitEntry {
    pub id: String,
    pub title: String,
    pub status: AgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finish_reason: Option<FinishReason>,
    /// 带 `output_schema` spawn 的子代理交付的结构化结果；自由文本交付为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub structured_result: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentWaitResult {
    /// 本次等待覆盖的全部代理快照（按解析顺序）。
    pub agents: Vec<AgentWaitEntry>,
    /// 至少一个代理已进入终态。
    pub settled: bool,
    /// 有消息待投递（子代理消息或用户补充消息），将在下一轮输入中出现。
    pub messages_pending: bool,
    /// 窗口内无任何事件而返回。
    pub timed_out: bool,
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
client_method!(
    AgentMessage,
    "agent/message",
    AgentMessageParams,
    AgentMessageResult
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
            session_id: "ses_parent".into(),
            task: "返回 CHILD_OK".into(),
            title: None,
            model: Some("explore".into()),
            preset: None,
            output_schema: None,
        };
        assert_eq!(serde_json::to_value(params).unwrap()["model"], "explore");
        assert!(
            serde_json::from_value::<AgentSpawnParams>(json!({
                "session_id": "ses_parent",
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

    #[test]
    fn message_receipts_and_meta_use_snake_case_outcomes() {
        assert_eq!(
            serde_json::to_value(AgentMessageOutcome::Injected).unwrap(),
            json!("injected")
        );
        assert_eq!(
            serde_json::to_value(AgentMessageOutcome::Woken).unwrap(),
            json!("woken")
        );
        assert_eq!(
            serde_json::to_value(AgentMessageOutcome::Failed).unwrap(),
            json!("failed")
        );
        let meta = AgentMessageMeta {
            from_id: "ses_child".into(),
            from_title: "子代理 A".into(),
            delivery: AgentMessageOutcome::Injected,
        };
        assert_eq!(
            serde_json::to_value(&meta).unwrap(),
            json!({
                "from_id": "ses_child",
                "from_title": "子代理 A",
                "delivery": "injected"
            })
        );
        let receipt = AgentMessageReceipt {
            agent_id: "ses_child".into(),
            title: "子代理 A".into(),
            outcome: AgentMessageOutcome::Failed,
            error: Some("收件箱已满".into()),
        };
        assert_eq!(
            serde_json::to_value(&receipt).unwrap()["outcome"],
            json!("failed")
        );
        assert_eq!(
            serde_json::to_value(&receipt).unwrap()["error"],
            json!("收件箱已满")
        );
        let bare = AgentMessageReceipt {
            agent_id: "ses_child".into(),
            title: "子代理 A".into(),
            outcome: AgentMessageOutcome::Woken,
            error: None,
        };
        assert!(serde_json::to_value(&bare).unwrap().get("error").is_none());
        assert!(
            serde_json::from_value::<AgentMessageMeta>(json!({
                "from_id": "ses_child",
                "delivery": "injected",
                "unexpected": true
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<AgentMessageParams>(json!({
                "session_id": "ses_parent",
                "target": "all",
                "message": "hi",
                "unexpected": true
            }))
            .is_err()
        );
    }

    #[test]
    fn wait_result_omits_empty_fields() {
        let result = AgentWaitResult {
            agents: vec![AgentWaitEntry {
                id: "ses_child".into(),
                title: "子代理 A".into(),
                status: AgentStatus::Running,
                message: None,
                finish_reason: None,
                structured_result: None,
            }],
            settled: false,
            messages_pending: false,
            timed_out: true,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(
            value,
            json!({
                "agents": [{
                    "id": "ses_child",
                    "title": "子代理 A",
                    "status": "running"
                }],
                "settled": false,
                "messages_pending": false,
                "timed_out": true
            })
        );
        let params = AgentWaitParams {
            session_id: "ses_parent".into(),
            agent_ids: None,
            timeout_secs: None,
        };
        assert_eq!(
            serde_json::to_value(&params).unwrap(),
            json!({"session_id": "ses_parent"})
        );
        assert!(
            serde_json::from_value::<ToolProgressRow>(json!({
                "agent_id": "ses_child",
                "title": "子代理 A",
                "status": "running",
                "elapsed_ms": 1200,
                "unexpected": true
            }))
            .is_err()
        );
    }
}

//! 会话 shell 任务的投影与管理：前台命令同样登记为任务，前端据此计时、转后台和终止。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// 任务运行状态。`exited` 时退出码见 [`JobInfo::exit_code`]。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    Running,
    /// 已发出终止信号，等待进程组退出。
    Killing,
    Exited,
    Killed,
    TimedOut,
}

impl JobStatus {
    /// 是否已经结束（不会再变化）。
    pub fn is_terminal(self) -> bool {
        !matches!(self, Self::Running | Self::Killing)
    }
}

/// 任务生命周期：monitored 会自然结束并回报；persistent 是常驻服务，只能被终止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobLifecycle {
    Monitored,
    Persistent,
}

/// 任务从哪条路径进入（或尚未进入）后台。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobOrigin {
    /// 仍挂在发起它的 bash 工具调用上，模型在等结果。
    Foreground,
    /// 模型显式 `async=true`。
    Explicit,
    /// 超过 `tools.bash.foreground_after_secs` 自动转入后台。
    AutoBackground,
    /// 用户经 `job/background` 转入后台。
    UserBackground,
}

impl JobOrigin {
    pub fn is_background(self) -> bool {
        self != Self::Foreground
    }
}

/// 一个任务的快照。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobInfo {
    pub job_id: String,
    pub command: String,
    /// 模型给 bash 调用写的一句话说明。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// 发起它的 bash 工具调用；前端用它把任务挂到对应的工具卡片上。
    pub tool_call_id: String,
    pub status: JobStatus,
    /// 仅 `exited` 时可能有值；被信号杀死时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub lifecycle: JobLifecycle,
    pub origin: JobOrigin,
    /// monitored 后台任务超过 `tools.bash.stall_after_secs` 没有新输出。
    pub stalled: bool,
    /// 输出超过日志上限，中段被丢弃（保留开头与结尾）。
    pub output_elided: bool,
    pub started_at_ms: u64,
    /// 结束时的总耗时；运行中为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 完整日志的 `artifact://jobs/...` URI。
    pub log_uri: String,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobList;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobListParams {
    pub session_id: String,
    /// 为每个未结束的任务附带最后 N 行输出；省略时 `outputs` 为空。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_lines: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobListResult {
    /// 运行中与最近结束的任务，按 `job_id` 升序。
    pub jobs: Vec<JobInfo>,
    /// `tail_lines` 非空时各未结束任务的末尾输出（`job_id` → 文本），
    /// 读取失败的任务不出现。
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub outputs: std::collections::BTreeMap<String, String>,
}

/// `job/kill` / `job/background` 共用的定位参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobTarget {
    pub session_id: String,
    pub job_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobResult {
    pub job: JobInfo,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobKill;

/// 把前台任务转入后台：发起它的 bash 调用立即返回，任务结束后自动回报给模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobBackground;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobReadOutput;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobOutputParams {
    pub session_id: String,
    pub job_id: String,
    /// 返回最后几行；默认 200，上限 2000。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail_lines: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JobOutputResult {
    pub job: JobInfo,
    pub text: String,
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

client_method!(JobList, "job/list", JobListParams, JobListResult);
client_method!(JobKill, "job/kill", JobTarget, JobResult);
client_method!(JobBackground, "job/background", JobTarget, JobResult);
client_method!(
    JobReadOutput,
    "job/output",
    JobOutputParams,
    JobOutputResult
);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn job_info_uses_snake_case_and_omits_unset_fields() {
        let info = JobInfo {
            job_id: "job_1".into(),
            command: "cargo build".into(),
            description: None,
            tool_call_id: "call_1".into(),
            status: JobStatus::Running,
            exit_code: None,
            lifecycle: JobLifecycle::Monitored,
            origin: JobOrigin::AutoBackground,
            stalled: false,
            output_elided: false,
            started_at_ms: 1,
            duration_ms: None,
            log_uri: "artifact://jobs/job_1.log".into(),
        };
        assert_eq!(
            serde_json::to_value(&info).unwrap(),
            json!({
                "job_id": "job_1", "command": "cargo build", "tool_call_id": "call_1",
                "status": "running", "lifecycle": "monitored", "origin": "auto_background",
                "stalled": false, "output_elided": false, "started_at_ms": 1,
                "log_uri": "artifact://jobs/job_1.log"
            })
        );
    }
}

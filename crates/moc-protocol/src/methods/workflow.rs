//! workflow 方法（激活 / 审批 / run 查询与控制）与 DTO。
//!
//! 审批流：core 编译通过后发 [`SessionUpdate::Workflow`] `approval_requested`
//! 通知（载荷 [`WorkflowApproval`]），并阻塞等待前端 `session/workflow_approve`
//! 携带 `request_id` 的决策。写操作只允许持写租约的连接（server 侧统一检查）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::session::SessionInfo;
use crate::method::{ClientRequest, Method};

/// `session/workflow_activate`：会话激活 workflow 工具面。
///
/// 激活后 `create_workflow` 等工具进入 catalog，`<skills>` 出现内置
/// `workflow-authoring`。激活标志作为会话 entry 持久化，会话 resume 恢复。
/// `[workflow] enabled=false` 或连接无审批能力时返回明确错误。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowActivate;

impl Method for SessionWorkflowActivate {
    const NAME: &'static str = "session/workflow_activate";
    type Params = SessionWorkflowActivateParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionWorkflowActivate {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowActivateParams {
    pub session_id: String,
}

/// `session/workflow_approve`：对一次 workflow 审批请求的决策。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowApprove;

impl Method for SessionWorkflowApprove {
    const NAME: &'static str = "session/workflow_approve";
    type Params = SessionWorkflowApproveParams;
    type Result = WorkflowAck;
}

impl ClientRequest for SessionWorkflowApprove {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowApproveParams {
    pub session_id: String,
    /// 对应 `approval_requested` 载荷里的 request_id。
    pub request_id: String,
    pub decision: WorkflowDecision,
}

/// 审批决策：批准可携带每 role 模型与 ModelRef 重映射覆盖。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "action", deny_unknown_fields)]
pub enum WorkflowDecision {
    Approve {
        /// role_id → 模型 selector；未覆盖的 role 用审批窗默认。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        role_models: Option<std::collections::BTreeMap<String, String>>,
        /// `model()` 声明 ref → 模型 selector。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_ref_map: Option<std::collections::BTreeMap<String, String>>,
    },
    Reject {
        /// 回给模型的拒绝反馈。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
}

/// 简单确认结果（写操作的通用回执）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowAck {
    pub session_id: String,
}

/// `session/workflow_runs`：会话的 run 摘要列表。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowRuns;

impl Method for SessionWorkflowRuns {
    const NAME: &'static str = "session/workflow_runs";
    type Params = SessionWorkflowRunsParams;
    type Result = SessionWorkflowRunsResult;
}

impl ClientRequest for SessionWorkflowRuns {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowRunsParams {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowRunsResult {
    #[serde(default)]
    pub runs: Vec<WorkflowRunSummary>,
}

/// `session/workflow_run`：单个 run 的完整快照。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowRun;

impl Method for SessionWorkflowRun {
    const NAME: &'static str = "session/workflow_run";
    type Params = SessionWorkflowRunParams;
    type Result = WorkflowRunSnapshot;
}

impl ClientRequest for SessionWorkflowRun {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowRunParams {
    pub session_id: String,
    pub run_id: String,
}

/// `session/workflow_stop`：停止 run（终态 stopped(user)，可 resume）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowStop;

impl Method for SessionWorkflowStop {
    const NAME: &'static str = "session/workflow_stop";
    type Params = SessionWorkflowRunParams;
    type Result = WorkflowRunSummary;
}

impl ClientRequest for SessionWorkflowStop {}

/// `session/workflow_resume`：重启 stopped(errored 不可 resume) 的 run。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowResume;

impl Method for SessionWorkflowResume {
    const NAME: &'static str = "session/workflow_resume";
    type Params = SessionWorkflowRunParams;
    type Result = WorkflowRunSummary;
}

impl ClientRequest for SessionWorkflowResume {}

/// `session/workflow_set_role_model`：运行中改 role 模型（之后的 live ask 生效）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowSetRoleModel;

impl Method for SessionWorkflowSetRoleModel {
    const NAME: &'static str = "session/workflow_set_role_model";
    type Params = SessionWorkflowSetRoleModelParams;
    type Result = WorkflowAck;
}

impl ClientRequest for SessionWorkflowSetRoleModel {}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowSetRoleModelParams {
    pub session_id: String,
    pub run_id: String,
    pub role_id: String,
    /// 模型 selector；`inherit` 表示回到解析链默认。
    pub model: String,
}

/// `session/workflow_artifact`：读 run 的 artifact 内容（version 缺省最新）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionWorkflowArtifact;

impl Method for SessionWorkflowArtifact {
    const NAME: &'static str = "session/workflow_artifact";
    type Params = SessionWorkflowArtifactParams;
    type Result = WorkflowArtifactResult;
}

impl ClientRequest for SessionWorkflowArtifact {}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionWorkflowArtifactParams {
    pub session_id: String,
    pub run_id: String,
    pub artifact_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowArtifactResult {
    pub artifact_id: String,
    pub version: u32,
    /// file/markdown 为文本；chart/table/metrics/board 为结构化 JSON。
    pub content: Value,
}

// —— run 状态 DTO ——

/// run 状态（对齐 journal 终态机）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "state")]
pub enum WorkflowRunStatus {
    Pending,
    Running,
    Completed,
    Errored,
    Stopped {
        /// user | interrupted | provider。
        reason: WorkflowStopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStopReason {
    User,
    Interrupted,
    Provider,
}

/// run 摘要（列表项）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunSummary {
    pub run_id: String,
    pub status: WorkflowRunStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// phase 节点运行态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowPhaseState {
    pub id: String,
    pub name: String,
    /// pending | running | done | failed | cancelled。
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entered_at_ms: Option<u64>,
    /// 该 phase 下已完成的 ask/world 计数（运行中更新）。
    #[serde(default)]
    pub asks_done: u32,
    #[serde(default)]
    pub world_done: u32,
}

/// role（actor 实例）运行态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRoleState {
    pub role_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// pending | running | done | failed | cancelled。
    pub status: String,
    /// 解析后的实际模型 selector。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub asks_done: u32,
    #[serde(default)]
    pub asks_in_flight: u32,
    #[serde(default)]
    pub spent_tokens: u64,
    #[serde(default)]
    pub fan_out: bool,
}

/// run 完整快照（审批图与运行面板共用）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunSnapshot {
    #[serde(flatten)]
    pub summary: WorkflowRunSummary,
    /// 编译出的 plan（sites/phases/edges/roles/model_refs/commands/schemas）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Value>,
    #[serde(default)]
    pub phases: Vec<WorkflowPhaseState>,
    #[serde(default)]
    pub roles: Vec<WorkflowRoleState>,
    /// 已批准的 world.run 命令集。
    #[serde(default)]
    pub commands: Vec<String>,
    #[serde(default)]
    pub reports: Vec<Value>,
    /// 已发布 artifact 条目（id/member/version/spec）。
    #[serde(default)]
    pub artifacts: Vec<Value>,
    /// 打开的 hole（name/prompt/schema/site）。
    #[serde(default)]
    pub holes: Vec<Value>,
    #[serde(default)]
    pub usage: Value,
    /// 草稿脚本落盘路径。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub script_path: Option<String>,
}

/// 审批请求载荷（`approval_requested` 事件内容；§8）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowApproval {
    /// 审批请求 id：`session/workflow_approve` 用同一 id 回传决策。
    pub request_id: String,
    /// 已分配的 run id（批准后 run 以该 id 启动）。
    pub run_id: String,
    /// 脚本全文（前端可查看）。
    pub script: String,
    /// 编译产物 `WorkflowPlan`（sites/phases/edges/roles/model_refs/commands/schemas）。
    pub plan: Value,
    /// 默认选择：role_models（脚本字面量模型否则当前会话模型）+ model_ref_map。
    pub defaults: WorkflowApprovalDefaults,
    /// 脚本 `args` 值。
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub max_concurrency: usize,
    /// launch | amend | hole_fill。
    pub kind: WorkflowApprovalKind,
    /// `hole_fill` 时新增（未批准的）world.run 命令列表。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_commands: Option<Vec<String>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowApprovalKind {
    Launch,
    Amend,
    HoleFill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkflowApprovalDefaults {
    /// role_id → 默认模型。
    #[serde(default)]
    pub role_models: std::collections::BTreeMap<String, String>,
    /// `model()` 声明 ref → 默认模型。
    #[serde(default)]
    pub model_ref_map: std::collections::BTreeMap<String, String>,
}

/// `SessionUpdate::Workflow{run_id, event}` 的事件载荷。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case", tag = "kind", deny_unknown_fields)]
pub enum WorkflowEvent {
    /// 审批请求发出（前端弹审批窗）。
    ApprovalRequested {
        request: Box<WorkflowApproval>,
    },
    /// 审批已解决（批准后 run 启动 / 拒绝后工具返回反馈）。
    ApprovalResolved {
        request_id: String,
        approved: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        feedback: Option<String>,
    },
    /// run 状态变化（含终态）。
    RunStatus {
        status: WorkflowRunStatus,
    },
    PhaseEntered {
        phase: String,
    },
    /// role/ask/world 实例状态更新（phase 与 role 的运行态）。
    NodeUpdated {
        phases: Vec<WorkflowPhaseState>,
        roles: Vec<WorkflowRoleState>,
    },
    Log {
        level: String,
        message: String,
    },
    Report {
        item: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        artifact_id: Option<String>,
    },
    /// artifact 声明/发布。
    Artifact {
        artifact: Value,
    },
    HoleOpened {
        site_id: String,
        ordinal: u32,
        name: String,
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        schema: Option<Value>,
    },
    HoleFilled {
        name: String,
    },
    Usage {
        usage: Value,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names() {
        assert_eq!(SessionWorkflowActivate::NAME, "session/workflow_activate");
        assert_eq!(SessionWorkflowApprove::NAME, "session/workflow_approve");
        assert_eq!(SessionWorkflowRuns::NAME, "session/workflow_runs");
        assert_eq!(SessionWorkflowRun::NAME, "session/workflow_run");
        assert_eq!(SessionWorkflowStop::NAME, "session/workflow_stop");
        assert_eq!(SessionWorkflowResume::NAME, "session/workflow_resume");
        assert_eq!(
            SessionWorkflowSetRoleModel::NAME,
            "session/workflow_set_role_model"
        );
        assert_eq!(SessionWorkflowArtifact::NAME, "session/workflow_artifact");
    }

    #[test]
    fn decision_roundtrip() {
        let params = SessionWorkflowApproveParams {
            session_id: "s".into(),
            request_id: "r".into(),
            decision: WorkflowDecision::Approve {
                role_models: Some([("actor#1".into(), "gpt-4".into())].into_iter().collect()),
                model_ref_map: None,
            },
        };
        let v = serde_json::to_value(&params).unwrap();
        assert_eq!(v["decision"]["action"], "approve");
        let back: SessionWorkflowApproveParams = serde_json::from_value(v).unwrap();
        assert_eq!(back, params);
        let reject = WorkflowDecision::Reject {
            feedback: Some("no".into()),
        };
        let v = serde_json::to_value(reject).unwrap();
        assert_eq!(v["action"], "reject");
    }

    #[test]
    fn event_roundtrip() {
        let event = WorkflowEvent::HoleOpened {
            site_id: "hole#1".into(),
            ordinal: 0,
            name: "q".into(),
            prompt: "p".into(),
            schema: None,
        };
        let v = serde_json::to_value(&event).unwrap();
        assert_eq!(v["kind"], "hole_opened");
        let back: WorkflowEvent = serde_json::from_value(v).unwrap();
        assert_eq!(back, event);
    }
}

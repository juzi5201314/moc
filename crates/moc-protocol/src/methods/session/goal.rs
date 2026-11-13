use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// 一轮 goal 续跑的落库标记：写进该轮用户消息条目，回放时按它累计
/// [`GoalView::rounds_started`]。与 [`super::agent::AgentMessageMeta`] 同层，
/// 都是「这条用户消息不是人直接敲的」的来源标注。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoalRoundMeta {
    /// 本轮归属的目标 ID（CAS 身份的一半）。
    pub goal_id: String,
    /// 预约本轮时的目标 revision；目标被编辑后旧轮即失效。
    pub revision: u32,
    /// 本轮序号（从 1 起；无上限，只作展示与门槛判定）。
    pub round: u32,
}

/// 目标的生命周期相位。armed/disarmed 是进程内属性，不在此列。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GoalPhase {
    /// 进行中：armed 时每轮提示结束自动续跑。
    Active,
    /// 用户暂停：不续跑；`/goal resume` 恢复。
    Paused,
    /// 模型判定无法推进：不续跑；`/goal resume` 恢复。
    Blocked,
    /// 已达成：终态；新建目标前须 `/goal clear` 或直接再 `/goal <objective>` 覆盖。
    Complete,
}

/// 机器可路由 + 人类可读的 blocked 原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoalBlockReason {
    /// 稳定分类码（如 `model-reported`），lower-kebab-case。
    pub code: String,
    /// 给人与模型看的说明。
    pub message: String,
}

/// 当前目标投影：持久快照 + 回放计数 + 进程内 armed。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GoalView {
    /// 目标 ID（同一会话内不复用）。
    pub id: String,
    /// 乐观锁修订号：每次变更 +1；`update_goal` / RPC 写操作按它做 CAS。
    pub revision: u32,
    /// 目标原文。
    pub objective: String,
    pub phase: GoalPhase,
    /// 仅 `phase == blocked` 时存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<GoalBlockReason>,
    /// 已落库的 goal round 数；不设上限。
    pub rounds_started: u32,
    pub created_at_ms: u64,
    pub updated_at_ms: u64,
    /// 本进程是否允许自动续跑；不落库，会话重新加载后恒为 `false`。
    pub armed: bool,
}

/// `session/goal` 的动作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum GoalAction {
    /// 查看当前目标。
    Show,
    /// 创建目标；已有非 complete 目标时报错（先 `/goal edit` 或 `/goal clear`）。
    Create { objective: String },
    /// 替换目标原文；phase 与 armed 不变，revision +1 使进行中的旧轮失效。
    Edit { objective: String },
    /// 暂停自动续跑；若正在跑 goal round 则中止该轮。
    Pause,
    /// 恢复续跑：paused/blocked 回到 active；active 但 disarmed 的重新 armed。
    Resume,
    /// 清除目标（含 complete 目标）；变体名不用 `Clear` 是 TUI 守卫把该词留给浮层原语。
    #[serde(rename = "clear")]
    Unset,
}

/// `session/goal` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionGoalParams {
    /// 目标会话。
    pub session_id: String,
    pub action: GoalAction,
}

/// `session/goal` 的结果：变更后的当前目标（clear 或无目标时为 `None`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionGoalResult {
    #[serde(default)]
    pub goal: Option<GoalView>,
}

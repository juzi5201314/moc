//! 会话生命周期方法：`session/new` / `list` / `load` / `unload` / `rename` / `delete` / `unlock` / `set_mode` / `compact`。
//!
//! 会话状态、持久化与锁的实际持有全在 core；协议层只搬运 [`SessionInfo::lock`] 这类
//! 投影。锁语义只有一条：**某进程正在该会话里跑一次提示 agent 工作时上锁；其它 core
//! 可读不可写**。
//!
//! | 操作 | 被他人持锁时 |
//! |---|---|
//! | `session/list`、`session/load`、`config/get` | 始终允许 |
//! | `session/prompt`、`session/rename`、`session/delete`、`session/set_mode` | [`crate::RpcErrorCode::SessionLocked`] |
//! | `session/unlock{force:true}` | 无条件清除锁记录（强夺） |
//!
//! `force=false` 的 `session/unlock` 只释放**本进程**持有的锁；对他人持有的锁，
//! core 返回 [`crate::RpcErrorCode::SessionLocked`]，`force=true` 则无条件清除。

use super::{plan::PlanInfo, todo::TodoPhase};
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// `session/new`：新建会话。
///
/// 以计划模式创建时会立即应用 `models.roles.plan` 模型（该角色配置为 `none` 时沿用
/// 指定模型），退出计划模式再恢复创建时指定的模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionNew;

/// `session/list`：列出全部会话。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionList;

/// `session/load`：读取会话 transcript。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionLoad;

/// `session/load_page`：按 transcript 游标读取一段可展示历史。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionLoadPage;

/// `session/rewind`：把会话历史回退到指定消息。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRewind;

/// `session/rewind` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRewindParams {
    /// 目标会话。
    pub session_id: String,
    /// 目标 transcript 消息 ID；用户消息本身不保留，会恢复到输入框。
    pub entry_id: String,
}

/// `session/unload`：释放会话的常驻资源（不删除会话、不影响列表）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUnload;

/// `session/rename`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRename;

/// `session/delete`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDelete;

/// `session/unlock`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUnlock;

/// 会话工作模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Normal,
    /// 只读问答模式。
    Ask,
    Plan,
    Ptc,
    /// 模型自主维护实时上下文的实验模式。
    ClmExperimental,
}

/// 会话命令执行权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPermission {
    #[default]
    Full,
    Workspace,
}

impl SessionPermission {
    /// 返回供命令行与状态栏展示的稳定名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Workspace => "workspace",
        }
    }
}

/// `session/set_mode`：切换会话工作模式。
///
/// 进入计划模式会把会话模型切到 `models.roles.plan` 指定的模型（该角色配置为
/// `none` 时沿用当前模型），退出计划模式恢复进入前的模型；模型切换通过
/// [`crate::methods::SessionUpdate::ModelChanged`] 推送。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetMode;

/// `session/set_permission`：切换会话命令执行权限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetPermission;

/// `session/set_model`：按选择器切换会话模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetModel;

/// `session/compact`：手动压缩会话上下文。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCompact;

/// `session/context`：估算当前会话上下文，并按组成部分返回 token 明细。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionContext;
/// `session/context_content`：读取当前上下文里的真实正文，按分块返回。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionContextContent;
/// `session/usage`：读取当前会话累计的模型调用用量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage;

/// `session/usage` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageParams {
    /// 目标会话。
    pub session_id: String,
}

/// 一个模型在当前会话中的累计用量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelUsageSummary {
    /// 模型选择器。
    pub model: String,
    /// 模型请求次数。
    pub calls: u64,
    /// 输入 token 总数，包含缓存命中与缓存写入部分。
    pub input_tokens: u64,
    /// 输出 token 总数。
    pub output_tokens: u64,
    /// 缓存命中的输入 token；上游不提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// 缓存写入的输入 token；上游不提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// 思考 token；上游不提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// 每次模型请求从开始到结束的耗时之和，单位为毫秒。
    pub active_ms: u64,
}

/// 当前会话的模型用量与全部模型总和。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageResult {
    /// 按模型选择器排序的明细。
    pub models: Vec<ModelUsageSummary>,
    /// 全部模型的总和；`model` 固定为 `total`。
    pub total: ModelUsageSummary,
}

/// `session/context` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContextParams {
    /// 目标会话。
    pub session_id: String,
}

/// 一项上下文组成部分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageSection {
    /// 展示名称：英文短语，直接显示在上下文占用窗的分块列表里。
    pub name: String,
    /// 估算 token 数。
    pub tokens: u64,
}

/// 当前会话上下文估算结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageResult {
    /// 模型上下文窗口上限。
    pub context_window: u64,
    /// 所有组成部分合计。
    pub total_tokens: u64,
    /// 系统底座、项目指令、技能、工具定义、会话消息等明细；分块名是 `system`、
    /// `project`、`skills`、`tools`、`messages` 固定英文短语。
    pub sections: Vec<ContextUsageSection>,
}

/// `session/context_content` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionContextContentParams {
    /// 目标会话。
    pub session_id: String,
    /// 单个分块正文的字节上限；超出按字符边界截断，被丢弃的字节数记入
    /// [`ContextContentSection::truncated_bytes`]。`None` 表示不截断。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_section_bytes: Option<u32>,
}

/// 上下文正文的一个分块。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContentSection {
    /// 分块短名（1-3 个英文单词），直接显示在正文窗的标题行。
    pub name: String,
    /// 分块正文；截断时以 `…` 收尾。
    pub text: String,
    /// 该分块单独估算的 token；消息与工具数组的固定开销不摊到分块上，
    /// 合计口径见 [`ContextContentResult::total_tokens`]。
    pub tokens: u64,
    /// 被截断丢弃的字节数；`0` 表示正文完整。
    #[serde(default)]
    pub truncated_bytes: u64,
}

/// 当前上下文里的真实正文，顺序即送入模型的顺序。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextContentResult {
    /// 模型上下文窗口上限。
    pub context_window: u64,
    /// 与 `session/context` 同口径的合计估算 token。
    pub total_tokens: u64,
    /// 系统提示片段、工具 schema、注入消息与转写消息。
    pub sections: Vec<ContextContentSection>,
}

/// `session/new` 的参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionNewParams {
    /// 工作目录；`None` 由 core 取自己的当前目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 模型 ID；`None` 用 core 的默认模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 标题；`None` 由 core 生成（如取首条用户输入）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 新会话初始模式。
    #[serde(default)]
    pub mode: SessionMode,
}

/// 一条会话的元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    /// 会话 ID，由 core 生成（如 `ses_7f3a`）。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 当前模型 ID。
    pub model: String,
    /// 当前命令执行权限。
    #[serde(default)]
    pub permission: SessionPermission,
    /// 当前工作模式。
    #[serde(default)]
    pub mode: SessionMode,
    /// 父会话 ID；根会话为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 工作目录。
    pub cwd: String,
    /// 创建时间的 UNIX 毫秒时间戳。
    pub created_at_ms: u64,
    /// 最近一次更新的 UNIX 毫秒时间戳。
    pub updated_at_ms: u64,
    /// 锁投影；`None` 表示当前没有其它进程在该会话里跑提示。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<SessionLockInfo>,
}

/// 会话锁的投影。
///
/// 只用于**展示与强夺**：锁的判定与持有在 core，协议层不参与互斥。前端看到
/// `Some(_)` 时应当把该会话渲染为只读，并把 `unlock{force:true}` 提供给用户
/// 处理崩溃进程留下的陈旧锁。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLockInfo {
    /// 持锁进程 PID。
    pub pid: u32,
    /// 持锁前端形态（`tui` / `webui` / `desktop`）。
    pub frontend: String,
    /// 进程名；`None` 表示 core 未取到。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    /// 上锁时间的 UNIX 毫秒时间戳。
    pub acquired_at_ms: u64,
}

/// `session/list` 的参数：空对象。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {}

/// `session/list` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    /// 会话列表；core 按 `updated_at_ms` 降序排列。
    pub sessions: Vec<SessionInfo>,
}

/// `session/load` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadParams {
    /// 目标会话。
    pub session_id: String,
    /// 最多返回多少条 transcript 条目（从最新往前数）；`None` 表示全部。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
}

/// `session/load_page` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadPageParams {
    pub session_id: String,
    /// 只返回该条目之前的历史；省略时返回最新一页。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_entry_id: Option<String>,
    /// 返回条目数；core 会限制到安全范围。
    pub max_entries: u32,
    /// 单条消息展示投影的字节预算；不影响 transcript 原文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_message_bytes: Option<u32>,
}

/// `session/load_page` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadPageResult {
    pub session: SessionInfo,
    pub entries: Vec<TranscriptEntry>,
    pub has_more: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoPhase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<PlanInfo>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_calls: Vec<TranscriptChildCall>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub content_truncated: bool,
}

/// `session/load` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadResult {
    /// 会话元信息（含锁投影）。
    pub session: SessionInfo,
    /// transcript 条目；`truncated` 为真时只含最新的 `max_entries` 条。
    pub entries: Vec<TranscriptEntry>,
    /// 是否因 `max_entries` 截断。
    pub truncated: bool,
    /// 当前 todo 阶段；空值兼容旧 core。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoPhase>,
    /// 当前会话计划；空值兼容旧 core。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<PlanInfo>,
    /// PTC 子工具调用；不进入模型 transcript。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub child_calls: Vec<TranscriptChildCall>,
}

/// PTC 中由 JavaScript 发起的嵌套工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptChildCall {
    pub id: String,
    pub parent_tool_call_id: String,
    pub name: String,
    pub arguments: String,
    pub result: String,
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// 一条 transcript 条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptEntry {
    /// 条目 ID，由 core 生成。
    pub id: String,
    /// 记录时间的 UNIX 毫秒时间戳。
    pub at_ms: u64,
    /// 内容；复用 `moc-llm` 的消息类型。
    pub message: crate::Message,
    /// 该消息对应的工具执行状态；`None` 表示与工具无关。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolExecution>,
    /// 用户手动执行的命令；与 agent 工具调用区分，并控制模型可见性。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash: Option<super::bash::BashCommand>,
    /// 代理间消息的来源元信息；`None` 表示本地输入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<super::agent::AgentMessageMeta>,
}

impl TranscriptEntry {
    pub fn in_context(&self) -> bool {
        !self
            .bash
            .as_ref()
            .is_some_and(|bash| bash.exclude_from_context)
    }

    /// 模型视图：`!!` 不产生消息，`!` 带上命令来源，代理消息包上来源标签，
    /// 其余消息保持原样。
    pub fn context_message(&self) -> Option<crate::Message> {
        if !self.in_context() {
            return None;
        }
        if let Some(agent) = &self.agent {
            return Some(crate::Message::user_text(format!(
                "<agent-message from=\"{}\" id=\"{}\">\n{}\n</agent-message>",
                agent.from_title,
                agent.from_id,
                self.message.text()
            )));
        }
        Some(match &self.bash {
            Some(bash) => bash.context_message(&self.message),
            None => self.message.clone(),
        })
    }
}

/// 一次工具执行的终态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecution {
    /// 对应的 [`crate::ToolCall::id`]。
    pub tool_call_id: String,
    /// 终态。
    pub status: ToolStatus,
    /// 执行耗时毫秒；`None` 表示不计时（拒绝、取消等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// 工具执行终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// 正常结束。
    Completed,
    /// 执行失败（工具报错、超时等）。
    Error,
    /// 未获授权。
    Denied,
    /// 提示被取消。
    Cancelled,
}

/// `session/unload` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUnloadParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/rename` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameParams {
    /// 目标会话。
    pub session_id: String,
    /// 新标题。
    pub title: String,
}

/// `session/delete` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleteParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/unlock` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUnlockParams {
    /// 目标会话。
    pub session_id: String,
    /// `false` 只释放本进程持有的锁；`true` 无条件清除锁记录（强夺）。
    pub force: bool,
}

/// `session/set_mode` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSetModeParams {
    pub session_id: String,
    pub mode: SessionMode,
}

/// `session/set_permission` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSetPermissionParams {
    pub session_id: String,
    pub permission: SessionPermission,
}

/// `session/set_model` 的参数：按选择器切换会话模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSetModelParams {
    /// 目标会话。
    pub session_id: String,
    /// 模型选择器（`config/get` 返回的 `ModelInfo::id`，或 core 接受的别名）。
    pub selector: String,
    /// 目标模型容不下当前上下文时执行的动作；缺省 [`SessionModelOverflowAction::Switch`]。
    #[serde(default)]
    pub overflow: SessionModelOverflowAction,
}

/// 目标模型容不下当前上下文时 [`SessionSetModel`] 执行的动作。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionModelOverflowAction {
    /// 直接切换，交给后续请求的既有压缩逻辑。
    #[default]
    Switch,
    /// 切换后用目标模型压缩上下文。
    Compact,
    /// 不切换，回报缺口由前端询问用户。
    Ask,
}

/// 一次模型切换检查出的容量缺口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelSwitchOverflow {
    /// 当前上下文按目标模型口径的估算 token 数。
    pub tokens: u64,
    /// 目标模型的上下文窗口。
    pub context_window: u64,
    /// 目标模型的压缩触发线。
    pub threshold: u64,
    /// 压缩后的目标占用。
    pub target_tokens: u64,
}

/// `session/set_model` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSetModelResult {
    /// 当前会话信息；`overflow` 非空且动作为 `ask` 时仍是切换前的模型。
    pub session: SessionInfo,
    /// 本次检查出的容量缺口；`None` 表示目标模型装得下。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub overflow: Option<ModelSwitchOverflow>,
    /// 已切换但压缩失败的原因；`None` 表示没压缩或压缩成功。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction_error: Option<String>,
}

/// `session/compact` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// 一次上下文压缩的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactResult {
    pub method: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub summary_chars: u32,
}

impl Method for SessionNew {
    const NAME: &'static str = "session/new";
    type Params = SessionNewParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionNew {}

impl Method for SessionList {
    const NAME: &'static str = "session/list";
    type Params = SessionListParams;
    type Result = SessionListResult;
}

impl ClientRequest for SessionList {}

impl Method for SessionLoad {
    const NAME: &'static str = "session/load";
    type Params = SessionLoadParams;
    type Result = SessionLoadResult;
}

impl ClientRequest for SessionLoad {}
impl Method for SessionLoadPage {
    const NAME: &'static str = "session/load_page";
    type Params = SessionLoadPageParams;
    type Result = SessionLoadPageResult;
}

impl ClientRequest for SessionLoadPage {}
impl Method for SessionRewind {
    const NAME: &'static str = "session/rewind";
    type Params = SessionRewindParams;
    type Result = SessionLoadResult;
}

impl ClientRequest for SessionRewind {}

impl Method for SessionUnload {
    const NAME: &'static str = "session/unload";
    type Params = SessionUnloadParams;
    type Result = crate::methods::Empty;
}

impl ClientRequest for SessionUnload {}

impl Method for SessionRename {
    const NAME: &'static str = "session/rename";
    type Params = SessionRenameParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionRename {}

impl Method for SessionDelete {
    const NAME: &'static str = "session/delete";
    type Params = SessionDeleteParams;
    type Result = crate::methods::Empty;
}

impl ClientRequest for SessionDelete {}

impl Method for SessionUnlock {
    const NAME: &'static str = "session/unlock";
    type Params = SessionUnlockParams;
    type Result = crate::methods::Empty;
}

impl ClientRequest for SessionUnlock {}
impl Method for SessionUsage {
    const NAME: &'static str = "session/usage";
    type Params = SessionUsageParams;
    type Result = SessionUsageResult;
}

impl ClientRequest for SessionUsage {}

impl Method for SessionSetMode {
    const NAME: &'static str = "session/set_mode";
    type Params = SessionSetModeParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionSetMode {}

impl Method for SessionSetPermission {
    const NAME: &'static str = "session/set_permission";
    type Params = SessionSetPermissionParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionSetPermission {}

impl Method for SessionSetModel {
    const NAME: &'static str = "session/set_model";
    type Params = SessionSetModelParams;
    type Result = SessionSetModelResult;
}

impl ClientRequest for SessionSetModel {}

impl Method for SessionCompact {
    const NAME: &'static str = "session/compact";
    type Params = SessionCompactParams;
    type Result = CompactResult;
}

impl ClientRequest for SessionCompact {}

impl Method for SessionContext {
    const NAME: &'static str = "session/context";
    type Params = SessionContextParams;
    type Result = ContextUsageResult;
}

impl ClientRequest for SessionContext {}

impl Method for SessionContextContent {
    const NAME: &'static str = "session/context_content";
    type Params = SessionContextContentParams;
    type Result = ContextContentResult;
}

impl ClientRequest for SessionContextContent {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_new_accepts_partial_params() {
        let parsed: SessionNewParams =
            serde_json::from_value(serde_json::json!({})).expect("可反序列化");
        assert_eq!(parsed, SessionNewParams::default());
        let parsed: SessionNewParams =
            serde_json::from_value(serde_json::json!({ "cwd": "/tmp/ws" })).expect("可反序列化");
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp/ws"));
        assert!(parsed.model.is_none());
        // 未知字段必须暴露
        assert!(
            serde_json::from_value::<SessionNewParams>(serde_json::json!({ "cwdly": 1 })).is_err()
        );
        // 空结果序列化为 {}
        assert_eq!(
            serde_json::to_value(crate::methods::Empty {}).expect("可序列化"),
            serde_json::json!({})
        );
    }

    #[test]
    fn load_and_lock_shapes() {
        let params = SessionLoadParams {
            session_id: "ses_1".into(),
            max_entries: None,
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            serde_json::json!({ "session_id": "ses_1" })
        );
        let lock = SessionLockInfo {
            pid: 4242,
            frontend: "tui".into(),
            process_name: Some("moc-tui".into()),
            acquired_at_ms: 1_724_000_000_000,
        };
        assert_eq!(
            serde_json::to_value(&lock).expect("可序列化"),
            serde_json::json!({
                "pid": 4242,
                "frontend": "tui",
                "process_name": "moc-tui",
                "acquired_at_ms": 1_724_000_000_000u64
            })
        );
        assert_eq!(
            serde_json::to_value(ToolStatus::Denied).expect("可序列化"),
            serde_json::json!("denied")
        );
        let info = SessionInfo {
            id: "ses_1".into(),
            title: "t".into(),
            model: "m".into(),
            permission: SessionPermission::Full,
            parent_session_id: None,
            cwd: "/tmp".into(),
            created_at_ms: 1,
            mode: SessionMode::Plan,
            updated_at_ms: 2,
            lock: None,
        };
        assert_eq!(
            serde_json::to_value(&info).expect("可序列化"),
            serde_json::json!({
                "id": "ses_1", "title": "t", "model": "m", "permission": "full",
                "mode": "plan", "cwd": "/tmp", "created_at_ms": 1, "updated_at_ms": 2
            })
        );
        let old: SessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_1", "title": "t", "model": "m", "mode": "normal", "cwd": "/tmp",
            "created_at_ms": 1, "updated_at_ms": 2
        }))
        .expect("旧字段可回读");
        assert_eq!(old.permission, SessionPermission::Full);
        assert_eq!(
            serde_json::to_value(SessionPermission::Workspace).unwrap(),
            "workspace"
        );
    }
}

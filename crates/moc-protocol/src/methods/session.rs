//! 会话生命周期方法：`session/new` / `list` / `load` / `unload` / `rename` / `delete` / `unlock` / `set_engine` / `set_autonomy` / `compact`。
//!
//! 会话状态、持久化与锁的实际持有全在 core；协议层只搬运 [`SessionInfo::lock`] 这类
//! 投影。锁语义只有一条：**某进程正在该会话里跑一次提示 agent 工作时上锁；其它 core
//! 可读不可写**。
//!
//! | 操作 | 被他人持锁时 |
//! |---|---|
//! | `session/list`、`session/load`、`config/get` | 始终允许 |
//! | `session/prompt`、`session/rename`、`session/delete`、`session/set_engine`、`session/set_autonomy` | [`crate::RpcErrorCode::SessionLocked`] |
//! | `session/unlock{force:true}` | 无条件清除锁记录（强夺） |
//!
//! `force=false` 的 `session/unlock` 只释放**本进程**持有的锁；对他人持有的锁，
//! core 返回 [`crate::RpcErrorCode::SessionLocked`]，`force=true` 则无条件清除。

use super::{diff::ToolDiff, plan::PlanInfo, todo::TodoPhase};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// `session/new`：新建会话。
///
/// 模型省略时使用最近主动选择的模型；计划模式不影响模型选择。
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRewindParams {
    /// 目标会话。
    pub session_id: String,
    /// 目标 transcript 消息 ID；用户消息本身不保留，会恢复到输入框。
    pub entry_id: String,
    /// 返回的历史条数；`Some(0)` 用于随后按页重载的界面，省略保持完整返回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
    /// 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
    #[serde(default, skip_serializing_if = "is_false")]
    pub omit_auxiliary: bool,
}

/// `session/redo`：把最近一次回退截掉的历史原样接回。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRedo;

/// `session/redo` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRedoParams {
    /// 目标会话。
    pub session_id: String,
    /// 前滚目标条目 ID（含该条目）；只接受最近一次回退截掉的尾部里的条目。
    pub entry_id: String,
    /// 返回的历史条数；`Some(0)` 只取元信息，省略保持完整返回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
    /// 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
    #[serde(default, skip_serializing_if = "is_false")]
    pub omit_auxiliary: bool,
}

/// `session/reask`：把某个已收场的 `ask` 调用截回未回答状态并原样重放，
/// 让用户重新作答；答案落库为该调用的新结果。前端随后用 `session/retry`
/// 让模型带着新答案继续。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionReask;

/// `session/reask` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionReaskParams {
    /// 目标会话。
    pub session_id: String,
    /// 要重新作答的 `ask` 工具调用 ID。
    pub tool_call_id: String,
    /// 返回的历史条数；`Some(0)` 只取元信息，省略保持完整返回。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
    /// 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
    #[serde(default, skip_serializing_if = "is_false")]
    pub omit_auxiliary: bool,
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

/// 会话引擎：模型与工具的交互形态。
///
/// 引擎是会话级属性：`session/new` 或空白期（没有任何用户提示）的
/// `session/set_engine` 决定一次会话的引擎；会话跑过第一条用户提示后
/// [`SessionInfo::engine_locked`] 为真，引擎不能再改。
///
/// plan 与 ask 不是引擎：plan 是叠加在任意引擎上的持久模式
/// （[`SessionInfo::plan_active`]，`session/plan_mode` 进出），ask 是
/// [`super::prompt::PromptParams::mode`] 声明的单次提示只读模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionEngine {
    /// 旧 transcript 写作 `normal`；读取时按别名映射，写入一律用 `standard`。
    #[default]
    #[serde(alias = "normal")]
    Standard,
    /// 模型只调用 `execute`，全部工具经代码沙盒触达。
    Ptc,
    /// 模型自主维护实时上下文的实验引擎。
    ClmExperimental,
}

/// 会话自主程度：哪些确认由系统代答而不打扰用户。
///
/// 档位从低到高单向收紧人工介入；`session/set_autonomy` 随时可改，随会话持久化。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default, Serialize, Deserialize, JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum SessionAutonomy {
    /// 默认：所有确认都等用户本人。
    #[default]
    Default,
    /// `ask` 工具的问题由 core 代答：有推荐项选推荐项，没有推荐项交回模型自行决定。
    DontAsk,
    /// 在 [`Self::DontAsk`] 之上，计划提交（`exit_plan`）由前端自动批准「压缩并执行」。
    Unattended,
}

/// 会话命令执行权限。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SessionPermission {
    #[default]
    Full,
    Workspace,
    /// 工作区可写；工作区外的宿主路径不可读也不可写（运行时只读路径与会话 scratch 除外）。
    WorkspaceOnly,
}

impl SessionPermission {
    /// 返回供命令行与状态栏展示的稳定名称。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Workspace => "workspace",
            Self::WorkspaceOnly => "workspace-only",
        }
    }

    /// 模式严格性排序：越宽的数字越大。用于校验只能申请更宽的模式。
    pub const fn width(self) -> u8 {
        match self {
            Self::WorkspaceOnly => 0,
            Self::Workspace => 1,
            Self::Full => 2,
        }
    }

    /// 当前模式能否升级到 `target`；相同模式视为无需升级。
    pub const fn can_widen_to(self, target: Self) -> bool {
        target.width() >= self.width()
    }
}

/// `session/set_engine`：切换会话引擎。
///
/// 引擎只能在空白会话（还没跑过任何用户提示）期间修改；会话已有提示历史后
/// 请求会被拒绝——此时要换引擎只能新建会话。plan 模式的进出走
/// [`super::plan::SessionPlanMode`]，与本方法无关。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetEngine;

/// `session/set_autonomy`：切换会话自主程度。
///
/// 与引擎不同，自主程度随时可改、立即生效：它只决定之后的确认由谁应答，
/// 不改工具表、不影响前缀缓存。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetAutonomy;

/// `session/set_permission`：切换会话命令执行权限。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetPermission;

/// `session/set_model`：按选择器切换会话模型。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetModel;

/// `session/set_fast_mode`：开关会话的 Fast Mode。
///
/// 会话级偏好，随时可改、随会话持久化；只有模型配置了 `fast_mode = true` 时请求
/// 才带 `service_tier = "fast"`，换到不支持的模型时开关保留但不生效。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetFastMode;

/// `session/compact`：手动压缩会话上下文。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCompact;

/// `session/context`：估算当前会话上下文，并按组成部分返回 token 明细。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionContext;
/// `session/context_content`：读取当前上下文里的真实正文，按分块返回。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionContextContent;
/// `session/usage`：读取当前会话累计的模型调用用量
/// （会话自身 + 未被回退孤立的全部后代会话）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUsage;

/// `session/usage` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageParams {
    /// 目标会话。
    pub session_id: String,
}

/// 用量来源：会话自身、Fusion sidekick 子会话，或普通子代理（含其后代）。
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    #[default]
    Session,
    Sidekick,
    Subagent,
}

impl UsageSource {
    /// serde `skip_serializing_if` 用的判定：自身用量不写字段保持旧报文形状。
    pub fn is_session(&self) -> bool {
        matches!(self, UsageSource::Session)
    }
}

/// 一个模型在当前会话中的累计用量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelUsageSummary {
    /// 模型选择器。
    pub model: String,
    /// 这行用量属于会话自身还是后代会话；自身用量序列化时省略。
    #[serde(default, skip_serializing_if = "UsageSource::is_session")]
    pub source: UsageSource,
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
    /// 首 token 等待（TTFT）总时长，单位为毫秒；只统计上报了首 token 的请求。
    #[serde(default)]
    pub ttft_ms: u64,
    /// 上报了首 token 的请求数；平均 TTFT 用它做分母，失败请求与非流式请求不计入。
    #[serde(default)]
    pub ttft_calls: u64,
    /// 首 token 之后的生成总时长，单位为毫秒；只统计上报了首 token 的请求。
    #[serde(default)]
    pub gen_ms: u64,
}

/// 当前会话的模型用量与全部模型总和
/// （会话自身 + 未被回退孤立的全部后代会话）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageResult {
    /// 按来源与模型选择器排序的明细（会话自身、sidekick、普通子代理）。
    pub models: Vec<ModelUsageSummary>,
    /// 全部模型的总和；`model` 固定为 `total`。
    pub total: ModelUsageSummary,
    /// 工具执行耗时之和，单位为毫秒；手动 bash 与嵌套子调用不计入。
    #[serde(default)]
    pub tool_ms: u64,
}

/// `session/tools`：返回当前会话下一轮请求的工具定义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionToolsParams {
    pub session_id: String,
}

pub struct SessionTools;
impl Method for SessionTools {
    const NAME: &'static str = "session/tools";
    type Params = SessionToolsParams;
    type Result = Vec<moc_llm::ToolDef>;
    crate::method::json_codec!();
}
impl ClientRequest for SessionTools {}

/// `session/context` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextParams {
    /// 目标会话。
    pub session_id: String,
}

/// 一项上下文组成部分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageSection {
    /// 展示名称，直接显示在上下文占用窗的分块列表里。
    pub name: String,
    /// 估算 token 数。
    pub tokens: u64,
}

/// 当前会话上下文估算结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageResult {
    /// 模型上下文窗口上限。
    pub context_window: u64,
    /// 所有组成部分合计。
    pub total_tokens: u64,
    /// 按来源分类：`system`、`AGENTS.md (p+g)`、`skills`、`tools`、`messages`；
    /// 插件贡献提示词时额外显示 `plugins`。运行时提醒与工具结果归入 `messages`。
    pub sections: Vec<ContextUsageSection>,
}

/// 打开当前上下文；指定 view_id 时只读取已冻结视图的后续目录页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextContentParams {
    pub session_id: String,
    #[serde(default)]
    pub view_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// 冻结上下文中的分类或来源条目；目录与隐藏项不携带正文。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextContentSection {
    pub id: String,
    pub name: String,
    /// `category`、`markdown` 或 `json`；不暴露编码协议的字段树。
    pub kind: String,
    pub source: Option<String>,
    pub tokens: u64,
    pub children: u64,
    /// 隐藏原因；有值时不可读取正文或子节点，仍计入上下文用量。
    pub hidden: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextContentResult {
    pub view_id: String,
    pub context_window: u64,
    pub total_tokens: u64,
    pub model: String,
    pub sections: Vec<ContextContentSection>,
    pub next_cursor: Option<u64>,
    /// 当前状态无法编码成模型请求时的原因。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextReadParams {
    pub session_id: String,
    pub view_id: String,
    pub node_id: String,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextReadResult {
    pub view_id: String,
    pub node_id: String,
    pub children: Vec<ContextContentSection>,
    pub text: Option<String>,
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextCloseParams {
    pub session_id: String,
    pub view_id: String,
}

pub struct SessionContextRead;
impl Method for SessionContextRead {
    const NAME: &'static str = "session/context_read";
    type Params = SessionContextReadParams;
    type Result = ContextReadResult;
    crate::method::json_codec!();
}
impl ClientRequest for SessionContextRead {}

pub struct SessionContextClose;
impl Method for SessionContextClose {
    const NAME: &'static str = "session/context_close";
    type Params = SessionContextCloseParams;
    type Result = super::Empty;
    crate::method::json_codec!();
}
impl ClientRequest for SessionContextClose {}

/// `session/new` 的参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// 新会话初始引擎；创建后即为会话引擎，跑过首条提示后锁定。
    #[serde(default)]
    pub engine: SessionEngine,
    /// 新会话直接进入 plan 模式。
    #[serde(default)]
    pub plan_active: bool,
    /// 新会话初始 Fast Mode 开关（通常沿用上一个会话）。
    #[serde(default)]
    pub fast_mode: bool,
}

/// 一条会话的元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// 当前会话引擎。
    #[serde(default)]
    pub engine: SessionEngine,
    /// 当前自主程度。
    #[serde(default)]
    pub autonomy: SessionAutonomy,
    /// plan 模式是否激活；激活时本次各次提示按计划模式运行。
    #[serde(default)]
    pub plan_active: bool,
    /// Fast Mode 开关；当前模型支持时请求带 `service_tier = "fast"`。
    #[serde(default)]
    pub fast_mode: bool,
    /// 引擎是否已锁定：会话跑过第一条用户提示后为真，此后 `session/set_engine` 会被拒绝。
    #[serde(default)]
    pub engine_locked: bool,
    /// 父会话 ID：只有 `spawn_agent` 出来的子代理会话有，其余会话为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// 消息回退分叉的来源会话 ID。**纯用户侧出身信息**：分叉在模型侧与普通会话
    /// 完全一致，模型永远不会看到它。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    /// spawn 时选定的 agent preset 名；普通子代理与普通会话为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_preset: Option<String>,
    /// 有效委托深度：顶层会话为 0，直接子代理为 1。旧 header 未记录深度的
    /// 子代理按 1 处理。
    #[serde(default)]
    pub delegation_depth: u32,
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

impl SessionInfo {
    /// 是否为子代理会话：唯一判据是父会话存在。回退分叉不是子代理。
    ///
    /// 提示词身份、工具可见性与代理名册都必须走这一个判据，避免出现
    /// 「系统提示说我是子代理、工具表却不给 yield」这类自相矛盾。
    pub fn is_subagent(&self) -> bool {
        self.parent_session_id.is_some()
    }
}

/// 会话锁的投影。
///
/// 只用于**展示**：锁的判定与持有在 core，协议层不参与互斥。前端看到
/// `Some(_)` 时应当把该会话渲染为只读，并提示持有者（进程名与 pid）。
///
/// 内核文件锁没有安全的跨进程抢占方式，core 不会因为 `unlock` 而清除别人的锁：
/// 要接管会话，请让持有者 `session/unload` 或退出进程。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {}

/// `session/list` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    /// 会话列表；core 按 `updated_at_ms` 降序排列。
    pub sessions: Vec<SessionInfo>,
}

/// `session/search`：跨会话正文子串搜索（消息 `Text` 块，大小写不敏感）。
///
/// 标题与工作目录的匹配由前端在 `session/list` 结果上本地完成，这里只补
/// 本地做不了的正文搜索。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSearch;

/// `session/search` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchParams {
    /// 搜索词；对消息 `Text` 块做大小写不敏感的子串匹配。
    pub query: String,
    /// 最多返回多少个会话；默认 20，上限 100。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u32>,
    /// 只搜某个工作目录下的会话；省略时搜全部。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// 会话内的一条命中。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchMatch {
    /// 命中条目的 transcript ID，可用于历史定位。
    pub entry_id: String,
    /// 命中消息的角色（`user`/`assistant` 等线上角色名）。
    pub role: String,
    /// 命中点前后的摘录（空白已折叠）。
    pub snippet: String,
    /// 条目时间戳（毫秒）。
    pub at_ms: u64,
}

/// 一个会话的命中：取它最近的几条匹配。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchHit {
    /// 命中的会话（与 `session/list` 相同的投影）。
    pub session: SessionInfo,
    /// 会话内命中，按条目时间从新到旧，最多 3 条。
    pub matches: Vec<SessionSearchMatch>,
}

/// `session/search` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSearchResult {
    /// 命中的会话，按 `session/list` 的 `updated_at_ms` 降序。
    pub matches: Vec<SessionSearchHit>,
    /// 命中数到达上限或扫描预算耗尽、更早的会话未扫描时为真。
    #[serde(default)]
    pub truncated: bool,
}

/// `session/load` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadParams {
    /// 目标会话。
    pub session_id: String,
    /// 最多返回多少条 transcript 条目（从最新往前数）；`None` 表示全部。
    /// `Some(0)` 只取会话元信息、todo 和计划，不带消息及子工具结果。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
    /// 省略 todo、计划、子工具和目标正文；界面随后通过 `session/history` 获取完整状态。
    #[serde(default, skip_serializing_if = "is_false")]
    pub omit_auxiliary: bool,
    /// 只读打开：不取跨进程写租约，写操作会被 core 拒绝。
    ///
    /// 另一个 core 进程持有写租约时，只有只读打开能成功。
    #[serde(default, skip_serializing_if = "is_false")]
    pub read_only: bool,
}

fn is_false(value: &bool) -> bool {
    !*value
}

/// 前滚候选列表的独立分页；正文保持完整，超预算的单条候选独占一页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RedoPageParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_entry_id: Option<String>,
    pub max_entries: u32,
}

/// 计划版本按创建顺序分页；游标是当前计划列表的排他上界，追加版本不移动旧下标。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlanPageParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<u32>,
    /// 0 表示本次不读取计划；非零值受 core 的条数和序列化字节预算限制。
    pub max_entries: u32,
}

/// `session/load_page` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
    /// 保留完整正文、图片和关联子工具结果，忽略单条投影预算；页预算按父调用组计量。
    /// 单个超大条目独占一页；需要跨帧读取时使用 `session/history`。
    #[serde(default, skip_serializing_if = "is_false")]
    pub full_messages: bool,
    /// 显式指定时前滚候选受分页预算限制；省略保持旧返回形状。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_page: Option<RedoPageParams>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan_page: Option<PlanPageParams>,
}

/// `session/load_page` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// 最近一次回退截掉、仍可前滚接回的 transcript 条目；栈顶以外的更早回退不提供。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redo_entries: Vec<TranscriptEntry>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub redo_has_more: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redo_tip_entry_id: Option<String>,
    /// 还有更早计划时，下一次请求使用的排他上界。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plans_before: Option<u32>,
    /// 当前会话目标；无目标或旧 core 为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalView>,
}

/// `session/load` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// 当前会话目标；无目标或旧 core 为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalView>,
}

/// PTC 中由 JavaScript 发起的嵌套工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// edit/write 的结构化 diff；旧会话日志没有此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ToolDiff>,
}

/// 一条运行时提醒里的一条目。
///
/// 只记摘要不记正文：正文就是消息本身，摘要只用来判断「这个条目是否已经在历史里」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReminderItem {
    /// 稳定条目键，例如 `sandbox`、`mode`、`mcp-index`。
    pub key: String,
    /// 该条目正文的摘要；正文变了才重新注入。
    pub digest: String,
}

/// 挂在一条 `Custom` 消息上的提醒标记。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReminderStamp {
    /// 本条消息合并了哪些条目。
    pub items: Vec<ReminderItem>,
}

/// 一条 transcript 条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
    /// goal 续跑轮的来源元信息；`None` 表示非续跑消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_round: Option<GoalRoundMeta>,
    /// 运行时提醒的来源标记；`None` 表示普通对话消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<ReminderStamp>,
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolExecution {
    /// 对应的 [`crate::ToolCall::id`]。
    pub tool_call_id: String,
    /// 终态。
    pub status: ToolStatus,
    /// 执行耗时毫秒；`None` 表示不计时（拒绝、取消等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 工具产出的结构化 diff；随 transcript 持久化，历史回放据此显示改动。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ToolDiff>,
}

/// 工具执行终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUnloadParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/rename` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameParams {
    /// 目标会话。
    pub session_id: String,
    /// 新标题。
    pub title: String,
}

/// `session/delete` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleteParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/unlock` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUnlockParams {
    /// 目标会话。
    pub session_id: String,
    /// 兼容字段：旧前端曾用它强夺他人锁。新语义下 core 只释放本进程持有的租约，
    /// 传 `true` 也不会跨进程抢占；别人持有时返回 `-32015`。
    pub force: bool,
}

/// `session/set_engine` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSetEngineParams {
    pub session_id: String,
    pub engine: SessionEngine,
}

/// `session/set_autonomy` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSetAutonomyParams {
    pub session_id: String,
    pub autonomy: SessionAutonomy,
}

/// `session/set_permission` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSetPermissionParams {
    pub session_id: String,
    pub permission: SessionPermission,
    /// 清空本会话已积累的沙盒路径/命令授权；模式变化时 core 也会自动清空。
    #[serde(default)]
    pub clear_grants: bool,
}

/// `session/set_fast_mode` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionSetFastModeParams {
    pub session_id: String,
    pub enabled: bool,
}

/// `session/set_model` 的参数：按选择器切换会话模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// 一次上下文压缩的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CompactResult {
    pub method: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// 本次是否由模型重新生成了摘要；`false` 表示只裁剪了历史（`shake`）。
    pub full: bool,
    pub summary_chars: u32,
}

impl Method for SessionNew {
    const NAME: &'static str = "session/new";
    type Params = SessionNewParams;
    type Result = SessionInfo;
    crate::method::json_codec!();
}

impl ClientRequest for SessionNew {}

impl Method for SessionList {
    const NAME: &'static str = "session/list";
    type Params = SessionListParams;
    type Result = SessionListResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionList {}

impl Method for SessionSearch {
    const NAME: &'static str = "session/search";
    type Params = SessionSearchParams;
    type Result = SessionSearchResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionSearch {}

impl Method for SessionLoad {
    const NAME: &'static str = "session/load";
    type Params = SessionLoadParams;
    type Result = SessionLoadResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionLoad {}
impl Method for SessionLoadPage {
    const NAME: &'static str = "session/load_page";
    type Params = SessionLoadPageParams;
    type Result = SessionLoadPageResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionLoadPage {}
impl Method for SessionRewind {
    const NAME: &'static str = "session/rewind";
    type Params = SessionRewindParams;
    type Result = SessionLoadResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionRewind {}

impl Method for SessionRedo {
    const NAME: &'static str = "session/redo";
    type Params = SessionRedoParams;
    type Result = SessionLoadResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionRedo {}

impl Method for SessionReask {
    const NAME: &'static str = "session/reask";
    type Params = SessionReaskParams;
    type Result = SessionLoadResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionReask {}

impl Method for SessionUnload {
    const NAME: &'static str = "session/unload";
    type Params = SessionUnloadParams;
    type Result = crate::methods::Empty;
    crate::method::json_codec!();
}

impl ClientRequest for SessionUnload {}

impl Method for SessionRename {
    const NAME: &'static str = "session/rename";
    type Params = SessionRenameParams;
    type Result = SessionInfo;
    crate::method::json_codec!();
}

impl ClientRequest for SessionRename {}

impl Method for SessionDelete {
    const NAME: &'static str = "session/delete";
    type Params = SessionDeleteParams;
    type Result = crate::methods::Empty;
    crate::method::json_codec!();
}

impl ClientRequest for SessionDelete {}

impl Method for SessionUnlock {
    const NAME: &'static str = "session/unlock";
    type Params = SessionUnlockParams;
    type Result = crate::methods::Empty;
    crate::method::json_codec!();
}

impl ClientRequest for SessionUnlock {}
impl Method for SessionUsage {
    const NAME: &'static str = "session/usage";
    type Params = SessionUsageParams;
    type Result = SessionUsageResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionUsage {}

impl Method for SessionSetEngine {
    const NAME: &'static str = "session/set_engine";
    type Params = SessionSetEngineParams;
    type Result = SessionInfo;
    crate::method::json_codec!();
}

impl ClientRequest for SessionSetEngine {}

impl Method for SessionSetAutonomy {
    const NAME: &'static str = "session/set_autonomy";
    type Params = SessionSetAutonomyParams;
    type Result = SessionInfo;
    crate::method::json_codec!();
}

impl ClientRequest for SessionSetAutonomy {}

impl Method for SessionSetPermission {
    const NAME: &'static str = "session/set_permission";
    type Params = SessionSetPermissionParams;
    type Result = SessionInfo;
    crate::method::json_codec!();
}

impl ClientRequest for SessionSetPermission {}

impl Method for SessionSetFastMode {
    const NAME: &'static str = "session/set_fast_mode";
    type Params = SessionSetFastModeParams;
    type Result = SessionInfo;
    crate::method::json_codec!();
}

impl ClientRequest for SessionSetFastMode {}

impl Method for SessionSetModel {
    const NAME: &'static str = "session/set_model";
    type Params = SessionSetModelParams;
    type Result = SessionSetModelResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionSetModel {}

impl Method for SessionCompact {
    const NAME: &'static str = "session/compact";
    type Params = SessionCompactParams;
    type Result = CompactResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionCompact {}

impl Method for SessionContext {
    const NAME: &'static str = "session/context";
    type Params = SessionContextParams;
    type Result = ContextUsageResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionContext {}

impl Method for SessionContextContent {
    const NAME: &'static str = "session/context_content";
    type Params = SessionContextContentParams;
    type Result = ContextContentResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionContextContent {}

/// `session/goal`：查看或变更会话目标（持久目标 + 自动续轮）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionGoal;

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

impl Method for SessionGoal {
    const NAME: &'static str = "session/goal";
    type Params = SessionGoalParams;
    type Result = SessionGoalResult;
    crate::method::json_codec!();
}

impl ClientRequest for SessionGoal {}

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
            omit_auxiliary: false,
            read_only: false,
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            serde_json::json!({ "session_id": "ses_1" })
        );
        assert_eq!(
            serde_json::from_value::<SessionLoadParams>(
                serde_json::json!({ "session_id": "ses_1" })
            )
            .unwrap(),
            params,
        );
        let light = SessionLoadParams {
            omit_auxiliary: true,
            max_entries: Some(0),
            ..params.clone()
        };
        assert_eq!(
            serde_json::to_value(light).unwrap(),
            serde_json::json!({
                "session_id": "ses_1", "max_entries": 0, "omit_auxiliary": true
            })
        );
        let old_page: SessionLoadPageParams = serde_json::from_value(serde_json::json!({
            "session_id": "ses_1", "max_entries": 64
        }))
        .unwrap();
        assert!(old_page.plan_page.is_none());
        assert!(!old_page.full_messages);
        assert_eq!(
            serde_json::to_value(old_page).unwrap(),
            serde_json::json!({
                "session_id": "ses_1", "max_entries": 64
            })
        );
        let readonly = SessionLoadParams {
            session_id: "ses_1".into(),
            max_entries: None,
            omit_auxiliary: false,
            read_only: true,
        };
        assert_eq!(
            serde_json::to_value(&readonly).expect("可序列化"),
            serde_json::json!({ "session_id": "ses_1", "read_only": true })
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
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/tmp".into(),
            created_at_ms: 1,
            engine: SessionEngine::Ptc,
            autonomy: SessionAutonomy::Unattended,
            plan_active: true,
            fast_mode: true,
            engine_locked: true,
            updated_at_ms: 2,
            lock: None,
        };
        assert_eq!(
            serde_json::to_value(&info).expect("可序列化"),
            serde_json::json!({
                "id": "ses_1", "title": "t", "model": "m", "permission": "full",
                "engine": "ptc", "autonomy": "unattended",
                "plan_active": true, "fast_mode": true, "engine_locked": true,
                "delegation_depth": 0,
                "cwd": "/tmp", "created_at_ms": 1, "updated_at_ms": 2
            })
        );
        // 旧报文的 `mode: "normal"` 读作 `engine: "standard"`；`autonomy` 缺省为 `default`。
        let old: SessionInfo = serde_json::from_value(serde_json::json!({
            "id": "ses_1", "title": "t", "model": "m", "engine": "normal", "cwd": "/tmp",
            "created_at_ms": 1, "updated_at_ms": 2
        }))
        .expect("旧取值可回读");
        assert_eq!(old.engine, SessionEngine::Standard);
        assert_eq!(old.autonomy, SessionAutonomy::Default);
        assert!(!old.fast_mode);
        assert_eq!(old.permission, SessionPermission::Full);
        assert_eq!(
            serde_json::to_value(SessionPermission::Workspace).unwrap(),
            "workspace"
        );
        assert_eq!(
            serde_json::to_value(SessionPermission::WorkspaceOnly).unwrap(),
            "workspace_only"
        );
        assert_eq!(SessionPermission::WorkspaceOnly.as_str(), "workspace-only");
        assert!(SessionPermission::Workspace.can_widen_to(SessionPermission::Full));
        assert!(!SessionPermission::Full.can_widen_to(SessionPermission::Workspace));
        assert!(SessionPermission::Workspace.can_widen_to(SessionPermission::Workspace));
        assert!(
            !serde_json::from_value::<SessionSetPermissionParams>(serde_json::json!({
                "session_id": "ses_1", "permission": "workspace_only"
            }))
            .expect("clear_grants 可缺省")
            .clear_grants
        );
    }

    /// `session/goal` 的线协议形状：action 按 tag 平铺、相位小写蛇形、
    /// `clear` 子命令映射到 `Unset` 变体（Rust 侧避 `Clear` 关键字用途）。
    #[test]
    fn goal_params_and_view_shapes() {
        use serde_json::json;

        for (json, expected) in [
            (json!({ "action": "show" }), GoalAction::Show),
            (
                json!({ "action": "create", "objective": "做到 x" }),
                GoalAction::Create {
                    objective: "做到 x".into(),
                },
            ),
            (
                json!({ "action": "edit", "objective": "改成 y" }),
                GoalAction::Edit {
                    objective: "改成 y".into(),
                },
            ),
            (json!({ "action": "pause" }), GoalAction::Pause),
            (json!({ "action": "resume" }), GoalAction::Resume),
            (json!({ "action": "clear" }), GoalAction::Unset),
        ] {
            let parsed: GoalAction = serde_json::from_value(json.clone()).expect("动作可反序列化");
            assert_eq!(parsed, expected, "{json}");
            assert_eq!(serde_json::to_value(&parsed).expect("可序列化"), json);
        }
        assert!(serde_json::from_value::<GoalAction>(json!({ "action": "nope" })).is_err());

        let params = SessionGoalParams {
            session_id: "ses_1".into(),
            action: GoalAction::Pause,
        };
        // `action` 字段内嵌 tagged 动作对象：`{action:{action:"pause"}}`。
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({ "session_id": "ses_1", "action": { "action": "pause" } })
        );
        assert!(
            serde_json::from_value::<SessionGoalParams>(json!({
                "session_id": "ses_1",
                "action": "show",
                "extra": 1
            }))
            .is_err()
        );

        for (phase, name) in [
            (GoalPhase::Active, "active"),
            (GoalPhase::Paused, "paused"),
            (GoalPhase::Blocked, "blocked"),
            (GoalPhase::Complete, "complete"),
        ] {
            assert_eq!(serde_json::to_value(phase).unwrap(), json!(name));
        }

        let view = GoalView {
            id: "goal_1".into(),
            revision: 2,
            objective: "目标".into(),
            phase: GoalPhase::Blocked,
            blocked_reason: Some(GoalBlockReason {
                code: "model-reported".into(),
                message: "做不到".into(),
            }),
            rounds_started: 4,
            created_at_ms: 1,
            updated_at_ms: 2,
            armed: false,
        };
        assert_eq!(
            serde_json::to_value(&view).expect("可序列化"),
            json!({
                "id": "goal_1", "revision": 2, "objective": "目标",
                "phase": "blocked",
                "blocked_reason": { "code": "model-reported", "message": "做不到" },
                "rounds_started": 4, "created_at_ms": 1, "updated_at_ms": 2,
                "armed": false
            })
        );
        let meta = GoalRoundMeta {
            goal_id: "goal_1".into(),
            revision: 2,
            round: 4,
        };
        assert_eq!(
            serde_json::to_value(&meta).expect("可序列化"),
            json!({ "goal_id": "goal_1", "revision": 2, "round": 4 })
        );
    }
}

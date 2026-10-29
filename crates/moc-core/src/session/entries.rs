//! JSONL transcript 的 entry 结构，以及它到协议类型的转换。

use moc_llm::{FinishReason, Message};
use moc_protocol::methods::{
    PlanAnnotation, PlanStatus, SessionInfo, SessionLockInfo, SessionMode, SessionPermission,
    TodoPhase, ToolExecution, TranscriptEntry,
};
use serde::{Deserialize, Serialize};

use crate::ids;

/// transcript 里持久化的模式值。
///
/// 与 [`SessionMode`] 方言不是一回事：旧版本曾把 `ask` / `plan` 当成会话模式
/// 直接写进 transcript。方言与契约拆分后新代码只会写出三个方言值，但读取旧
/// transcript 时必须能接受全部五种取值并按旧语义映射回方言与 plan 叠加态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PersistedMode {
    Normal,
    /// 旧记录的持久 ask 模式；回放为 Normal 方言。
    Ask,
    /// 旧记录的 plan 模式；回放为 Normal 方言 + plan 叠加态。
    Plan,
    Ptc,
    ClmExperimental,
}

impl PersistedMode {
    /// 持久化值对应的会话方言。
    pub fn dialect(self) -> SessionMode {
        match self {
            Self::Normal | Self::Ask | Self::Plan => SessionMode::Normal,
            Self::Ptc => SessionMode::Ptc,
            Self::ClmExperimental => SessionMode::ClmExperimental,
        }
    }

    /// 旧模式的语义是否等价于「进入 plan」。
    pub fn legacy_enters_plan(self) -> bool {
        matches!(self, Self::Plan)
    }
}

impl From<SessionMode> for PersistedMode {
    fn from(mode: SessionMode) -> Self {
        match mode {
            SessionMode::Normal => Self::Normal,
            SessionMode::Ptc => Self::Ptc,
            SessionMode::ClmExperimental => Self::ClmExperimental,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum PtcStateChange {
    Put {
        key: String,
        value: serde_json::Value,
    },
    Delete {
        key: String,
    },
}

/// 一个会话 JSONL 文件的一行记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Entry {
    /// 文件首行的稳定会话元信息。
    Header {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        version: u32,
        session_id: String,
        cwd: String,
        model: String,
        mode: PersistedMode,
        #[serde(default)]
        permission: SessionPermission,
        /// 只有 `spawn_agent` 创建的子代理才有父会话，其它会话为 `null`。模型侧据此
        /// 决定是否使用子代理约定和工具，所以不能借这个字段表达其它关系。
        parent_session_id: Option<String>,
        /// 消息回退分叉的来源会话；只给用户看，模型侧完全不可见。旧记录没有这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        forked_from: Option<String>,
        /// spawn 时的委托深度；旧子代理 header 没有它，读取按 1 处理。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        delegation_depth: Option<u32>,
        /// spawn 时解析的 agent preset 快照；普通子代理为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<AgentSessionMeta>,
        title: String,
        created_at_ms: u64,
    },
    /// 对话消息及可选工具/结束信息。
    Message {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        message: Message,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tool: Option<ToolExecution>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        finish_reason: Option<FinishReason>,
        /// 手动命令的来源以及是否进入上下文；普通对话没有这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bash: Option<moc_protocol::BashCommand>,
        /// 代理间消息的来源信息；普通输入没有这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent: Option<moc_protocol::methods::AgentMessageMeta>,
        /// 运行时提醒标记；`None` 表示普通对话消息。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reminder: Option<moc_protocol::ReminderStamp>,
    },
    /// 一次上下文压缩。
    Compaction {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        method: String,
        summary: String,
        first_kept_id: Option<String>,
        tokens_before: u64,
        tokens_after: u64,
        instructions: Option<String>,
        native: Option<Box<serde_json::Value>>,
        /// 生成该 checkpoint 的模型；旧记录没有这个字段。原生数据只对这个模型有效。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Grok Build 整体替换历史时附带的运行时状态提醒；其它压缩方法为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reminder: Option<String>,
    },
    /// 模式方言变化（`session/set_mode`；只会出现在空白会话期）。
    ModeChange {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        mode: PersistedMode,
        /// 写入时的 plan 叠加态权威投影；旧记录没有这个字段，回放按旧模式值映射。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        plan_active: Option<bool>,
    },
    /// plan 叠加态变化（`session/plan_mode`）。
    PlanMode {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        active: bool,
    },
    /// 命令执行权限变化。
    PermissionChange {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        permission: SessionPermission,
    },
    /// 模型变化。
    ModelChange {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        model: String,
    },
    /// 标题变化。
    TitleChange {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        title: String,
    },
    /// Todo 清单快照。
    Todos {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        phases: Vec<TodoPhase>,
    },
    /// 计划状态变化。
    Plan {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        plan_id: String,
        title: String,
        path: String,
        markdown: String,
        status: PlanStatus,
        /// 审阅版本号；起草中的计划为 0。
        #[serde(default)]
        version: u32,
        /// 本次修订基于的上一版；旧记录没有这个字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_plan_id: Option<String>,
    },
    /// 用户对某个计划版本提交的一批批注。
    PlanFeedback {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        plan_id: String,
        annotations: Vec<PlanAnnotation>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
    },
    /// PTC 子工具结果；独立于模型 transcript。
    ChildTool {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        call: moc_protocol::methods::session::TranscriptChildCall,
    },
    PtcState {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        parent_tool_call_id: String,
        change: PtcStateChange,
    },
    /// 一次模型请求的累计用量、激活耗时与首 token 等待。
    Usage {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        model: String,
        usage: moc_llm::Usage,
        active_ms: u64,
        /// 首 token 等待（TTFT）；非流式请求与失败请求为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        ttft_ms: Option<u64>,
    },
    /// CLM 模式当前实时上下文的版本快照；不进入普通 transcript。
    ClmContext {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        content: String,
    },
    /// `/clear` 后模型上下文重建边界。
    ResetBoundary {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
    },
}

/// 非 header entry 的载荷；公共 ID 和 parent 链由 store 分配。
#[derive(Debug, Clone, PartialEq)]
pub enum EntryPayload {
    Message {
        message: Message,
        tool: Option<ToolExecution>,
        finish_reason: Option<FinishReason>,
        /// 代理间消息的来源信息；普通输入为 `None`。
        agent: Option<moc_protocol::methods::AgentMessageMeta>,
    },
    /// 运行时提醒：一段按条目差分后写入历史的 `Role::Custom` 消息。
    ///
    /// 与 `Message` 分开，是因为提醒不是对话内容：它要带条目摘要供重放时判断
    /// 「哪些条目已经在历史里」，而且不能参与用户消息的计数。
    Reminder {
        message: Message,
        /// 本条消息合并了哪些条目；只记摘要，正文在 `message` 里。
        items: Vec<moc_protocol::ReminderItem>,
    },
    Bash {
        command: moc_protocol::BashCommand,
        output: String,
        tool: ToolExecution,
    },
    Compaction {
        method: String,
        summary: String,
        first_kept_id: Option<String>,
        tokens_before: u64,
        tokens_after: u64,
        instructions: Option<String>,
        native: Option<Box<serde_json::Value>>,
        /// 生成该 checkpoint 的模型；沿用上一份 checkpoint 时保留原来的来源。
        model: Option<String>,
        /// Grok Build 压缩时快照的运行时状态提醒；其它方法为 `None`。
        reminder: Option<String>,
    },
    /// 方言变化；`plan_active` 是写入时的 plan 状态投影，供回放直接采用。
    ModeChange {
        mode: SessionMode,
        plan_active: bool,
    },
    /// plan 叠加态变化。
    PlanMode {
        active: bool,
    },
    PermissionChange {
        permission: SessionPermission,
    },
    ModelChange {
        model: String,
    },
    TitleChange {
        title: String,
    },
    Todos {
        phases: Vec<TodoPhase>,
    },
    Plan {
        plan_id: String,
        title: String,
        path: String,
        markdown: String,
        status: PlanStatus,
        version: u32,
        previous_plan_id: Option<String>,
    },
    /// 用户提交的审阅批注；只更新计划投影，不进入模型 transcript。
    PlanFeedback {
        plan_id: String,
        annotations: Vec<PlanAnnotation>,
        note: Option<String>,
    },
    ChildTool {
        call: moc_protocol::methods::session::TranscriptChildCall,
    },
    PtcState {
        parent_tool_call_id: String,
        change: PtcStateChange,
    },
    Usage {
        model: String,
        usage: moc_llm::Usage,
        active_ms: u64,
        ttft_ms: Option<u64>,
    },
    ClmContext {
        content: String,
    },
    ResetBoundary,
}

impl EntryPayload {
    /// 由计划投影构造一条计划状态条目。
    ///
    /// 所有写计划状态的地方都走这里，`Plan` 条目新增字段时不必逐个调用点补。
    /// 批注不进入条目：它只由 [`EntryPayload::PlanFeedback`] 写入，回放时沿用
    /// 已有投影。
    pub fn plan(plan: &moc_protocol::methods::PlanInfo, status: PlanStatus) -> Self {
        Self::Plan {
            plan_id: plan.plan_id.clone(),
            title: plan.title.clone(),
            path: plan.path.clone(),
            markdown: plan.markdown.clone(),
            status,
            version: plan.version,
            previous_plan_id: plan.previous_plan_id.clone(),
        }
    }
}

/// 会话的来源。两个字段互斥：
///
/// - `parent_session_id`：`spawn_agent` 创建的子代理。模型侧据此获得 `<subagent>` 约定、
///   `yield` 和代理通信能力，所以只有真正的 spawn 才能写入。
/// - `forked_from`：消息回退分叉出来的独立会话。分叉在模型侧和普通会话完全一致，
///   这个字段只给用户标出来源，永远不参与提示词、工具或代理列表的判断。
/// spawn 时解析并快照进 header 的委托契约。
///
/// 四个字段彼此独立：普通子代理可以只带 `output_schema`（结构化 yield），
/// preset 子代理则同时携带提示片段和工具面策略。快照在创建时冻结，插件
/// 卸载或 preset 更新不影响已存在的子代理会话。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentSessionMeta {
    /// 选用的 preset 名；普通子代理为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preset: Option<String>,
    /// preset 注入 `<subagent>` 块的系统提示片段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    /// preset 的工具面策略（白名单或拒绝列表）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_policy: Option<crate::agents::ToolPolicy>,
    /// `yield` 交付物必须满足的 JSON Schema；省略为自由文本交付。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_schema: Option<serde_json::Value>,
}

impl AgentSessionMeta {
    /// 契约是否为空：四个字段全空时 `SessionOrigin.agent` 不写入 header。
    pub fn is_empty(&self) -> bool {
        self.preset.is_none()
            && self.prompt.is_none()
            && self.tool_policy.is_none()
            && self.output_schema.is_none()
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionOrigin {
    pub parent_session_id: Option<String>,
    pub forked_from: Option<String>,
    /// spawn 时的委托深度；非子代理为 `None`。
    pub delegation_depth: Option<u32>,
    /// spawn 的 preset 快照；普通子代理与普通会话为 `None`。
    pub agent: Option<AgentSessionMeta>,
}

impl SessionOrigin {
    /// 顶层会话：既不是子代理，也不是分叉。
    pub fn root() -> Self {
        Self::default()
    }

    /// 子代理：由 `parent_session_id` 指定的会话在第 `delegation_depth` 层 spawn。
    pub fn spawned(
        parent_session_id: impl Into<String>,
        delegation_depth: u32,
        agent: Option<AgentSessionMeta>,
    ) -> Self {
        Self {
            parent_session_id: Some(parent_session_id.into()),
            forked_from: None,
            delegation_depth: Some(delegation_depth),
            agent,
        }
    }

    /// 回退分叉：从 `source` 会话分叉出的独立会话。
    pub fn forked(source: impl Into<String>) -> Self {
        Self {
            parent_session_id: None,
            forked_from: Some(source.into()),
            delegation_depth: None,
            agent: None,
        }
    }
}

impl Entry {
    /// 创建 JSONL 文件的第一行。
    #[expect(
        clippy::too_many_arguments,
        reason = "header 的字段就是持久化格式本身，逐字段对应比包装结构更直白"
    )]
    pub fn header(
        session_id: String,
        cwd: String,
        model: String,
        mode: SessionMode,
        permission: SessionPermission,
        origin: SessionOrigin,
        title: String,
        created_at_ms: u64,
    ) -> Self {
        debug_assert!(
            !(origin.parent_session_id.is_some() && origin.forked_from.is_some()),
            "会话来源的两个字段互斥：子代理和回退分叉不能同时存在"
        );
        Self::Header {
            id: ids::entry_id(),
            parent_id: None,
            at_ms: created_at_ms,
            version: 1,
            session_id,
            cwd,
            model,
            mode: mode.into(),
            permission,
            parent_session_id: origin.parent_session_id,
            forked_from: origin.forked_from,
            delegation_depth: origin.delegation_depth,
            agent: origin.agent,
            title,
            created_at_ms,
        }
    }

    /// 从载荷构造并连接到上一条 entry。
    pub fn from_payload(payload: EntryPayload, parent_id: String, at_ms: u64) -> Self {
        let id = ids::entry_id();
        let parent_id = Some(parent_id);
        match payload {
            EntryPayload::Message {
                message,
                tool,
                finish_reason,
                agent,
            } => Self::Message {
                id,
                parent_id,
                at_ms,
                message,
                tool,
                finish_reason,
                bash: None,
                agent,
                reminder: None,
            },
            EntryPayload::Reminder { message, items } => Self::Message {
                id,
                parent_id,
                at_ms,
                message,
                tool: None,
                finish_reason: None,
                bash: None,
                agent: None,
                reminder: Some(moc_protocol::ReminderStamp { items }),
            },
            EntryPayload::Bash {
                command,
                output,
                tool,
            } => Self::Message {
                id,
                parent_id,
                at_ms,
                message: Message::user_text(output),
                tool: Some(tool),
                finish_reason: None,
                bash: Some(command),
                agent: None,
                reminder: None,
            },
            EntryPayload::Compaction {
                method,
                summary,
                first_kept_id,
                tokens_before,
                tokens_after,
                instructions,
                native,
                model,
                reminder,
            } => Self::Compaction {
                id,
                parent_id,
                at_ms,
                method,
                summary,
                first_kept_id,
                tokens_before,
                tokens_after,
                instructions,
                native,
                model,
                reminder,
            },
            EntryPayload::ModeChange { mode, plan_active } => Self::ModeChange {
                id,
                parent_id,
                at_ms,
                mode: mode.into(),
                plan_active: Some(plan_active),
            },
            EntryPayload::PlanMode { active } => Self::PlanMode {
                id,
                parent_id,
                at_ms,
                active,
            },
            EntryPayload::PermissionChange { permission } => Self::PermissionChange {
                id,
                parent_id,
                at_ms,
                permission,
            },
            EntryPayload::ModelChange { model } => Self::ModelChange {
                id,
                parent_id,
                at_ms,
                model,
            },
            EntryPayload::TitleChange { title } => Self::TitleChange {
                id,
                parent_id,
                at_ms,
                title,
            },
            EntryPayload::Todos { phases } => Self::Todos {
                id,
                parent_id,
                at_ms,
                phases,
            },
            EntryPayload::Plan {
                plan_id,
                title,
                path,
                markdown,
                status,
                version,
                previous_plan_id,
            } => Self::Plan {
                id,
                parent_id,
                at_ms,
                plan_id,
                title,
                path,
                markdown,
                status,
                version,
                previous_plan_id,
            },
            EntryPayload::PlanFeedback {
                plan_id,
                annotations,
                note,
            } => Self::PlanFeedback {
                id,
                parent_id,
                at_ms,
                plan_id,
                annotations,
                note,
            },
            EntryPayload::ChildTool { call } => Self::ChildTool {
                id,
                parent_id,
                at_ms,
                call,
            },
            EntryPayload::PtcState {
                parent_tool_call_id,
                change,
            } => Self::PtcState {
                id,
                parent_id,
                at_ms,
                parent_tool_call_id,
                change,
            },
            EntryPayload::Usage {
                model,
                usage,
                active_ms,
                ttft_ms,
            } => Self::Usage {
                id,
                parent_id,
                at_ms,
                model,
                usage,
                active_ms,
                ttft_ms,
            },
            EntryPayload::ClmContext { content } => Self::ClmContext {
                id,
                parent_id,
                at_ms,
                content,
            },
            EntryPayload::ResetBoundary => Self::ResetBoundary {
                id,
                parent_id,
                at_ms,
            },
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Header { id, .. }
            | Self::Message { id, .. }
            | Self::Compaction { id, .. }
            | Self::ModeChange { id, .. }
            | Self::PlanMode { id, .. }
            | Self::PermissionChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::TitleChange { id, .. }
            | Self::Todos { id, .. }
            | Self::Plan { id, .. }
            | Self::PlanFeedback { id, .. }
            | Self::ChildTool { id, .. }
            | Self::PtcState { id, .. }
            | Self::Usage { id, .. }
            | Self::ClmContext { id, .. }
            | Self::ResetBoundary { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Header { parent_id, .. }
            | Self::Message { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::ModeChange { parent_id, .. }
            | Self::PlanMode { parent_id, .. }
            | Self::PermissionChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::TitleChange { parent_id, .. }
            | Self::Todos { parent_id, .. }
            | Self::Plan { parent_id, .. }
            | Self::PlanFeedback { parent_id, .. }
            | Self::ChildTool { parent_id, .. }
            | Self::PtcState { parent_id, .. }
            | Self::Usage { parent_id, .. }
            | Self::ClmContext { parent_id, .. }
            | Self::ResetBoundary { parent_id, .. } => parent_id.as_deref(),
        }
    }
    /// 把非 header entry 挂到新的 transcript 父节点上。
    pub fn set_parent_id(&mut self, parent_id: String) {
        match self {
            Self::Header { .. } => {}
            Self::Message {
                parent_id: value, ..
            }
            | Self::Compaction {
                parent_id: value, ..
            }
            | Self::ModeChange {
                parent_id: value, ..
            }
            | Self::PlanMode {
                parent_id: value, ..
            }
            | Self::PermissionChange {
                parent_id: value, ..
            }
            | Self::ModelChange {
                parent_id: value, ..
            }
            | Self::TitleChange {
                parent_id: value, ..
            }
            | Self::Todos {
                parent_id: value, ..
            }
            | Self::Plan {
                parent_id: value, ..
            }
            | Self::PlanFeedback {
                parent_id: value, ..
            }
            | Self::ChildTool {
                parent_id: value, ..
            }
            | Self::PtcState {
                parent_id: value, ..
            }
            | Self::Usage {
                parent_id: value, ..
            }
            | Self::ClmContext {
                parent_id: value, ..
            }
            | Self::ResetBoundary {
                parent_id: value, ..
            } => *value = Some(parent_id),
        }
    }

    pub fn at_ms(&self) -> u64 {
        match self {
            Self::Header { at_ms, .. }
            | Self::Message { at_ms, .. }
            | Self::Compaction { at_ms, .. }
            | Self::ModeChange { at_ms, .. }
            | Self::PlanMode { at_ms, .. }
            | Self::PermissionChange { at_ms, .. }
            | Self::ModelChange { at_ms, .. }
            | Self::TitleChange { at_ms, .. }
            | Self::Todos { at_ms, .. }
            | Self::Plan { at_ms, .. }
            | Self::PlanFeedback { at_ms, .. }
            | Self::ChildTool { at_ms, .. }
            | Self::PtcState { at_ms, .. }
            | Self::Usage { at_ms, .. }
            | Self::ClmContext { at_ms, .. }
            | Self::ResetBoundary { at_ms, .. } => *at_ms,
        }
    }

    pub fn header_fields(
        &self,
    ) -> Option<(
        &str,
        &str,
        &str,
        SessionMode,
        SessionPermission,
        &str,
        u64,
        u64,
    )> {
        match self {
            Self::Header {
                session_id,
                cwd,
                model,
                mode,
                permission,
                title,
                created_at_ms,
                at_ms,
                ..
            } => Some((
                session_id,
                cwd,
                model,
                mode.dialect(),
                *permission,
                title,
                *created_at_ms,
                *at_ms,
            )),
            _ => None,
        }
    }

    pub fn transcript_entry(&self) -> Option<TranscriptEntry> {
        match self {
            Self::Message {
                id,
                at_ms,
                message,
                tool,
                bash,
                agent,
                reminder,
                ..
            } => Some(TranscriptEntry {
                id: id.clone(),
                at_ms: *at_ms,
                message: message.clone(),
                tool: tool.clone(),
                bash: bash.clone(),
                agent: agent.clone(),
                reminder: reminder.clone(),
            }),
            _ => None,
        }
    }

    pub fn session_info(&self, lock: Option<SessionLockInfo>) -> Option<SessionInfo> {
        let Self::Header {
            session_id,
            cwd,
            model,
            mode,
            permission,
            parent_session_id,
            forked_from,
            delegation_depth,
            agent,
            title,
            created_at_ms,
            at_ms,
            ..
        } = self
        else {
            return None;
        };
        Some(SessionInfo {
            id: session_id.clone(),
            title: title.clone(),
            model: model.clone(),
            permission: *permission,
            mode: mode.dialect(),
            // 旧 header 的持久模式值映射到叠加态；新 header 只会是方言值。
            plan_active: mode.legacy_enters_plan(),
            // header 投影时还没有任何用户提示，方言必然未锁定。
            mode_locked: false,
            parent_session_id: parent_session_id.clone(),
            forked_from: forked_from.clone(),
            agent_preset: agent.as_ref().and_then(|meta| meta.preset.clone()),
            // 旧 header 未记录深度的子代理按 1 处理（旧行为：子代理不能再 spawn）。
            delegation_depth: delegation_depth
                .unwrap_or_else(|| u32::from(parent_session_id.is_some())),
            cwd: cwd.clone(),
            created_at_ms: *created_at_ms,
            updated_at_ms: *at_ms,
            lock,
        })
    }
}

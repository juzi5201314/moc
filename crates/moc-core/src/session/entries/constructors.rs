use super::Entry;
use super::EntryPayload;
use super::HeaderFields;
use super::SessionOrigin;
use crate::ids;
use moc_llm::Message;
use moc_protocol::methods::SessionAutonomy;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionLockInfo;
use moc_protocol::methods::SessionPermission;
use moc_protocol::methods::TranscriptEntry;

impl Entry {
    // 固定 JSON 编码的实例化位置，避免日志追加与重写各自展开大型 Entry 的 Serde 链。
    #[inline(never)]
    pub(crate) fn to_json_vec(&self) -> serde_json::Result<Vec<u8>> {
        serde_json::to_vec(self)
    }

    /// 创建 JSONL 文件的第一行。
    #[expect(
        clippy::too_many_arguments,
        reason = "header 的字段就是持久化格式本身，逐字段对应比包装结构更直白"
    )]
    pub fn header(
        session_id: String,
        cwd: String,
        model: String,
        engine: SessionEngine,
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
            engine: engine.into(),
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
                goal,
            } => Self::Message {
                id,
                parent_id,
                at_ms,
                message,
                tool,
                finish_reason,
                bash: None,
                agent,
                agent_result: None,
                goal,
                reminder: None,
            },
            EntryPayload::AgentResult { meta, report } => Self::Message {
                id,
                parent_id,
                at_ms,
                message: Message::custom(report),
                tool: None,
                finish_reason: None,
                bash: None,
                agent: None,
                agent_result: Some(meta),
                goal: None,
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
                agent_result: None,
                goal: None,
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
                agent_result: None,
                goal: None,
                reminder: None,
            },
            EntryPayload::Compaction {
                method,
                summary,
                first_kept_id,
                tokens_before,
                tokens_after,
                instructions,
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
                // 旧记录里可能有 remote 压缩的 `native` 载荷；新写入一律为 None。
                native: None,
                model,
                reminder,
            },
            EntryPayload::EngineChange {
                engine,
                plan_active,
            } => Self::EngineChange {
                id,
                parent_id,
                at_ms,
                engine: engine.into(),
                plan_active: Some(plan_active),
            },
            EntryPayload::PlanMode { active } => Self::PlanMode {
                id,
                parent_id,
                at_ms,
                active,
            },
            EntryPayload::WorkflowActivated { active } => Self::WorkflowActivated {
                id,
                parent_id,
                at_ms,
                active,
            },
            EntryPayload::AutonomyChange { autonomy } => Self::AutonomyChange {
                id,
                parent_id,
                at_ms,
                autonomy,
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
            EntryPayload::FastModeChange { enabled } => Self::FastModeChange {
                id,
                parent_id,
                at_ms,
                enabled,
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
            EntryPayload::GoalChange { goal } => Self::GoalChange {
                id,
                parent_id,
                at_ms,
                goal,
            },
        }
    }
    pub fn id(&self) -> &str {
        match self {
            Self::Header { id, .. }
            | Self::Message { id, .. }
            | Self::Compaction { id, .. }
            | Self::EngineChange { id, .. }
            | Self::AutonomyChange { id, .. }
            | Self::PlanMode { id, .. }
            | Self::WorkflowActivated { id, .. }
            | Self::PermissionChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::FastModeChange { id, .. }
            | Self::TitleChange { id, .. }
            | Self::Todos { id, .. }
            | Self::Plan { id, .. }
            | Self::PlanFeedback { id, .. }
            | Self::ChildTool { id, .. }
            | Self::PtcState { id, .. }
            | Self::Usage { id, .. }
            | Self::ClmContext { id, .. }
            | Self::ResetBoundary { id, .. }
            | Self::GoalChange { id, .. } => id,
        }
    }

    pub fn parent_id(&self) -> Option<&str> {
        match self {
            Self::Header { parent_id, .. }
            | Self::Message { parent_id, .. }
            | Self::Compaction { parent_id, .. }
            | Self::EngineChange { parent_id, .. }
            | Self::AutonomyChange { parent_id, .. }
            | Self::PlanMode { parent_id, .. }
            | Self::WorkflowActivated { parent_id, .. }
            | Self::PermissionChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::FastModeChange { parent_id, .. }
            | Self::TitleChange { parent_id, .. }
            | Self::Todos { parent_id, .. }
            | Self::Plan { parent_id, .. }
            | Self::PlanFeedback { parent_id, .. }
            | Self::ChildTool { parent_id, .. }
            | Self::PtcState { parent_id, .. }
            | Self::Usage { parent_id, .. }
            | Self::ClmContext { parent_id, .. }
            | Self::ResetBoundary { parent_id, .. }
            | Self::GoalChange { parent_id, .. } => parent_id.as_deref(),
        }
    }
    pub fn at_ms(&self) -> u64 {
        match self {
            Self::Header { at_ms, .. }
            | Self::Message { at_ms, .. }
            | Self::Compaction { at_ms, .. }
            | Self::EngineChange { at_ms, .. }
            | Self::AutonomyChange { at_ms, .. }
            | Self::PlanMode { at_ms, .. }
            | Self::WorkflowActivated { at_ms, .. }
            | Self::PermissionChange { at_ms, .. }
            | Self::ModelChange { at_ms, .. }
            | Self::FastModeChange { at_ms, .. }
            | Self::TitleChange { at_ms, .. }
            | Self::Todos { at_ms, .. }
            | Self::Plan { at_ms, .. }
            | Self::PlanFeedback { at_ms, .. }
            | Self::ChildTool { at_ms, .. }
            | Self::PtcState { at_ms, .. }
            | Self::Usage { at_ms, .. }
            | Self::ClmContext { at_ms, .. }
            | Self::ResetBoundary { at_ms, .. }
            | Self::GoalChange { at_ms, .. } => *at_ms,
        }
    }

    pub fn header_fields(&self) -> Option<HeaderFields<'_>> {
        match self {
            Self::Header {
                session_id,
                cwd,
                model,
                engine,
                permission,
                title,
                created_at_ms,
                at_ms,
                ..
            } => Some((
                session_id,
                cwd,
                model,
                engine.engine(),
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
                agent_result,
                goal,
                reminder,
                ..
            } => Some(TranscriptEntry {
                id: id.clone(),
                at_ms: *at_ms,
                message: message.clone(),
                tool: tool.clone(),
                bash: bash.clone(),
                agent: agent.clone(),
                agent_result: agent_result.clone(),
                goal_round: goal.clone(),
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
            engine,
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
            engine: engine.engine(),
            autonomy: SessionAutonomy::default(),
            // 旧 header 的持久模式值映射到 plan 模式；新 header 只会是引擎值。
            plan_active: engine.legacy_enters_plan(),
            // header 投影时还没有任何用户提示，引擎必然未锁定。
            engine_locked: false,
            fast_mode: false,
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

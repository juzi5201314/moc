//! JSONL transcript entry schema 与协议投影。

use moc_llm::{FinishReason, Message};
use moc_protocol::methods::{
    PlanStatus, SessionInfo, SessionLockInfo, SessionMode, SessionPermission, TodoPhase,
    ToolExecution, TranscriptEntry,
};
use serde::{Deserialize, Serialize};

use crate::ids;

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
        mode: SessionMode,
        #[serde(default)]
        permission: SessionPermission,
        parent_session_id: Option<String>,
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
        /// 手动命令的来源及上下文可见性；普通对话没有此字段。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bash: Option<moc_protocol::BashCommand>,
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
    },
    /// 工作模式变化。
    ModeChange {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        mode: SessionMode,
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
    /// 一次模型请求的累计用量与激活耗时。
    Usage {
        id: String,
        parent_id: Option<String>,
        at_ms: u64,
        model: String,
        usage: moc_llm::Usage,
        active_ms: u64,
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

/// 非 header entry 的载荷；公共 ID 与 parent 链由 store 分配。
#[derive(Debug, Clone, PartialEq)]
pub enum EntryPayload {
    Message {
        message: Message,
        tool: Option<ToolExecution>,
        finish_reason: Option<FinishReason>,
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
    },
    ModeChange {
        mode: SessionMode,
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
    },
    ClmContext {
        content: String,
    },
    ResetBoundary,
}

impl Entry {
    /// 创建 session JSONL 第一行。
    #[allow(clippy::too_many_arguments)]
    pub fn header(
        session_id: String,
        cwd: String,
        model: String,
        mode: SessionMode,
        permission: SessionPermission,
        parent_session_id: Option<String>,
        title: String,
        created_at_ms: u64,
    ) -> Self {
        Self::Header {
            id: ids::entry_id(),
            parent_id: None,
            at_ms: created_at_ms,
            version: 1,
            session_id,
            cwd,
            model,
            mode,
            permission,
            parent_session_id,
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
            } => Self::Message {
                id,
                parent_id,
                at_ms,
                message,
                tool,
                finish_reason,
                bash: None,
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
            },
            EntryPayload::Compaction {
                method,
                summary,
                first_kept_id,
                tokens_before,
                tokens_after,
                instructions,
                native,
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
            },
            EntryPayload::ModeChange { mode } => Self::ModeChange {
                id,
                parent_id,
                at_ms,
                mode,
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
            } => Self::Plan {
                id,
                parent_id,
                at_ms,
                plan_id,
                title,
                path,
                markdown,
                status,
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
            } => Self::Usage {
                id,
                parent_id,
                at_ms,
                model,
                usage,
                active_ms,
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
            | Self::PermissionChange { id, .. }
            | Self::ModelChange { id, .. }
            | Self::TitleChange { id, .. }
            | Self::Todos { id, .. }
            | Self::Plan { id, .. }
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
            | Self::PermissionChange { parent_id, .. }
            | Self::ModelChange { parent_id, .. }
            | Self::TitleChange { parent_id, .. }
            | Self::Todos { parent_id, .. }
            | Self::Plan { parent_id, .. }
            | Self::ChildTool { parent_id, .. }
            | Self::PtcState { parent_id, .. }
            | Self::Usage { parent_id, .. }
            | Self::ClmContext { parent_id, .. }
            | Self::ResetBoundary { parent_id, .. } => parent_id.as_deref(),
        }
    }
    /// 把非头部 entry 接到新的 transcript 父节点上。
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
            | Self::PermissionChange { at_ms, .. }
            | Self::ModelChange { at_ms, .. }
            | Self::TitleChange { at_ms, .. }
            | Self::Todos { at_ms, .. }
            | Self::Plan { at_ms, .. }
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
                *mode,
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
                ..
            } => Some(TranscriptEntry {
                id: id.clone(),
                at_ms: *at_ms,
                message: message.clone(),
                tool: tool.clone(),
                bash: bash.clone(),
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
            mode: *mode,
            parent_session_id: parent_session_id.clone(),
            cwd: cwd.clone(),
            created_at_ms: *created_at_ms,
            updated_at_ms: *at_ms,
            lock,
        })
    }
}

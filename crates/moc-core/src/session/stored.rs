//! 内存中的日志顺序只保存共享展示记录与少量日志专属字段。
//! 对外持久化仍使用 Entry；消息元数据不在两份大数组中重复存储。
use super::entries::Entry;
use moc_llm::FinishReason;
use moc_protocol::TranscriptEntry;
use std::sync::Arc;

#[derive(Debug, Clone)]
pub enum StoredEntry {
    Message {
        parent_id: Option<String>,
        finish_reason: Option<FinishReason>,
        entry: Arc<TranscriptEntry>,
    },
    Other(Box<Entry>),
}

impl StoredEntry {
    pub(super) fn new(log: Entry, projected: Option<Arc<TranscriptEntry>>) -> Self {
        match log {
            Entry::Message {
                parent_id,
                finish_reason,
                ..
            } => Self::Message {
                parent_id,
                finish_reason,
                entry: projected.expect("消息已有共享投影"),
            },
            other => Self::Other(Box::new(other)),
        }
    }

    pub fn id(&self) -> &str {
        match self {
            Self::Message { entry, .. } => &entry.id,
            Self::Other(entry) => entry.id(),
        }
    }

    pub fn message_entry(&self) -> Option<&TranscriptEntry> {
        match self {
            Self::Message { entry, .. } => Some(entry),
            Self::Other(_) => None,
        }
    }

    pub fn to_entry(&self) -> Entry {
        match self {
            Self::Message {
                parent_id,
                finish_reason,
                entry,
            } => Entry::Message {
                id: entry.id.clone(),
                parent_id: parent_id.clone(),
                at_ms: entry.at_ms,
                message: entry.message.clone(),
                tool: entry.tool.clone(),
                finish_reason: finish_reason.clone(),
                bash: entry.bash.clone(),
                agent: entry.agent.clone(),
                goal: entry.goal_round.clone(),
                reminder: entry.reminder.clone(),
            },
            Self::Other(entry) => (**entry).clone(),
        }
    }
}

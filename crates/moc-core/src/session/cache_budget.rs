use super::entries::Entry;
use super::state::SessionState;
use super::stored::StoredEntry;
use moc_llm::Message;

/// 冷加载后的体积权重：非消息日志可能有计划/PTC/子工具的大正文，投影还会持有副本。
/// 两倍序列化大小可高估旧版本，目的在于驱逐顺序；它不是分配器或 RSS 的精确统计。
pub(super) fn state_cache_bytes(state: &SessionState) -> usize {
    state.entries.iter().fold(0usize, |total, entry| {
        let bytes = match entry {
            StoredEntry::Message {
                entry, parent_id, ..
            } => message_storage_bytes(&entry.message).saturating_add(serialized_bytes(&(
                &entry.id,
                parent_id,
                &entry.tool,
                &entry.bash,
                &entry.agent,
                &entry.goal_round,
                &entry.reminder,
            ))),
            StoredEntry::Other(entry) => {
                let bytes = serialized_bytes(entry).saturating_mul(2);
                match entry.as_ref() {
                    Entry::Compaction { native, .. } => {
                        bytes.saturating_add(serialized_bytes(native))
                    }
                    _ => bytes,
                }
            }
        };
        total.saturating_add(bytes).saturating_add(1024)
    })
}

pub(super) fn message_storage_bytes(message: &Message) -> usize {
    message
        .content
        .archive_index_bytes()
        .unwrap_or_else(|| serialized_bytes(&message.content))
}

pub(super) fn serialized_bytes(value: &impl serde::Serialize) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).expect("会话记录可序列化且计数写者不会失败");
    counter.0
}

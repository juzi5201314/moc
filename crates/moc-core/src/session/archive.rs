//! 把旧正文冻结到私有只读映射；是否仍属模型上下文与是否驻留堆相互独立。
use std::collections::{HashMap, HashSet};
use std::io::{BufWriter, Write};
use std::ops::Range;
use std::sync::Arc;

use bytes::Bytes;
use moc_llm::MessageContent;

use super::state::SessionState;
use crate::CoreError;

const MIN_ARCHIVE_BYTES: usize = 64 * 1024;
/// 保留近期显示窗口；更旧正文仍可完整读取，不改变上下文选择。
pub(super) const HOT_MESSAGES: usize = 128;
pub(super) const FREEZE_BYTES: usize = 4 * 1024 * 1024;
const HOT_BYTES: usize = 2 * 1024 * 1024;
const REPLAY_BATCH: usize = 1024;

pub(super) fn cold_end(state: &SessionState) -> usize {
    let floor = state
        .context_start
        .max(state.transcript.len().saturating_sub(HOT_MESSAGES));
    let mut bytes = 0usize;
    let mut end = state.transcript.len();
    while end > floor {
        bytes = bytes.saturating_add(super::message_storage_bytes(
            &state.transcript[end - 1].message,
        ));
        if bytes > HOT_BYTES {
            break;
        }
        end -= 1;
    }
    end
}

pub(super) struct Batch {
    changes: Vec<(usize, MessageContent)>,
}

impl Batch {
    pub(super) fn apply(self, state: &mut SessionState) {
        for (index, content) in self.changes {
            state.replace_content(index, content);
        }
        state.unfrozen_bytes = 0;
    }
}

struct FrozenMap(Arc<memmap2::Mmap>);
impl AsRef<[u8]> for FrozenMap {
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

struct Counted<W> {
    writer: W,
    bytes: usize,
}
impl<W: Write> Write for Counted<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.writer.write(bytes)?;
        self.bytes = self
            .bytes
            .checked_add(written)
            .ok_or_else(|| std::io::Error::other("冻结正文超过可寻址范围"))?;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.writer.flush()
    }
}

/// 调用者在后台线程执行；只有整批文件写完、映射与类型校验全部成功后才替换原文。
pub(super) fn prepare(state: &SessionState, range: Range<usize>) -> Result<Batch, CoreError> {
    debug_assert!(range.start <= range.end && range.end <= state.transcript.len());
    let mut changes = Vec::new();
    let mut unique = HashSet::new();
    let mut resident = Vec::new();
    let mut resident_bytes = 0usize;
    for index in range.clone() {
        let message = &state.transcript[index].message;
        let content = &message.content;
        if content.is_archived() {
            if content.archive_cache_loaded() {
                changes.push((index, content.cold_clone()));
            }
        } else if !content.is_empty() && unique.insert(content.storage_id()) {
            resident_bytes = resident_bytes.saturating_add(super::message_storage_bytes(message));
            resident.push(content.clone());
        }
    }
    if resident_bytes < MIN_ARCHIVE_BYTES {
        return Ok(Batch { changes });
    }

    // 匿名临时文件只由本函数持有，没有可供其他写者打开的会话路径。
    let mut file = tempfile::tempfile()?;
    let mut ranges = Vec::with_capacity(resident.len());
    {
        let mut writer = Counted {
            writer: BufWriter::new(&mut file),
            bytes: 0,
        };
        for content in &resident {
            let start = writer.bytes;
            serde_json::to_writer(&mut writer, content)
                .map_err(|error| CoreError::Session(format!("冻结历史正文失败：{error}")))?;
            ranges.push((content.storage_id(), start..writer.bytes));
        }
        writer.flush()?;
    }
    // SAFETY: 文件为私有匿名快照，写者已结束；句柄不发布且紧接着关闭。
    // 整个映射生命周期没有覆盖、截断或其他可变映射，原日志重写不影响此快照。
    let mapping = Arc::new(unsafe { memmap2::MmapOptions::new().map(&file)? });
    drop(file);
    let bytes = Bytes::from_owner(FrozenMap(Arc::clone(&mapping)));
    let mut archived = HashMap::with_capacity(ranges.len());
    for (id, range) in ranges {
        let content = MessageContent::from_archive(bytes.slice(range))
            .map_err(|error| CoreError::Session(format!("校验冻结正文失败：{error}")))?;
        archived.insert(id, content);
    }
    for index in range {
        let content = &state.transcript[index].message.content;
        if let Some(archived) = archived.get(&content.storage_id()) {
            changes.push((index, archived.clone()));
        }
    }
    #[cfg(unix)]
    {
        // SAFETY: 校验已结束，没有存活的字节切片借用，Batch 尚未发布给任何读取者。
        // 这是只读文件映射而非匿名/COW 映射；丢弃驻留页后只会从同一不可变文件重新读回。
        if let Err(error) = unsafe { mapping.unchecked_advise(memmap2::UncheckedAdvice::DontNeed) }
        {
            tracing::debug!(%error, "操作系统未接受归档页回收建议");
        }
    }
    Ok(Batch { changes })
}

pub(super) fn freeze(state: &mut SessionState, range: Range<usize>) -> bool {
    match prepare(state, range) {
        Ok(batch) => {
            batch.apply(state);
            true
        }
        // 这是可重建的存储优化，不是持久日志写入；失败必须保留原文和已提交操作的结果。
        Err(error) => {
            tracing::warn!(%error, "历史正文冻结失败，保留内存中的完整原文");
            false
        }
    }
}

pub(super) struct Replayer {
    state: Option<SessionState>,
    frontier: usize,
    archive_enabled: bool,
}

impl Default for Replayer {
    fn default() -> Self {
        Self {
            state: None,
            frontier: 0,
            archive_enabled: true,
        }
    }
}

impl Replayer {
    pub(super) fn push(&mut self, entry: super::entries::Entry) -> Result<(), CoreError> {
        let boundary = matches!(
            &entry,
            super::entries::Entry::Compaction { .. } | super::entries::Entry::ResetBoundary { .. }
        );
        if let Some(state) = &mut self.state {
            state.replay_entry(entry)?;
            if self.archive_enabled && (boundary || state.unfrozen_bytes >= FREEZE_BYTES || state.transcript.len().saturating_sub(self.frontier) >= REPLAY_BATCH + HOT_MESSAGES) {
                let end = cold_end(state);
                self.frontier = self.frontier.min(end);
                self.archive_enabled = freeze(state, self.frontier..end);
                while self.frontier < end {
                    let content = &state.transcript[self.frontier].message.content;
                    if !content.is_archived() && !content.is_empty() {
                        break;
                    }
                    self.frontier += 1;
                }
            }
        } else {
            self.state = Some(SessionState::new(entry)?);
        }
        Ok(())
    }

    pub(super) fn finish(self) -> Result<SessionState, CoreError> {
        let mut state = self
            .state
            .ok_or_else(|| CoreError::Session("transcript 缺少 header".into()))?;
        if self.archive_enabled {
            let end = cold_end(&state);
            freeze(&mut state, self.frontier.min(end)..end);
        }
        Ok(state)
    }
}

#[cfg(test)]
mod tests {
    use super::super::entries::{Entry, EntryPayload, SessionOrigin};
    use super::*;
    use moc_llm::Message;
    use moc_protocol::{SessionEngine, SessionPermission};

    #[test]
    fn uncompressed_history_stays_complete_shared_and_cold() {
        let header = Entry::header(
            "ses_cold".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "测试".into(),
            1,
        );
        let mut parent = header.id().to_owned();
        let mut expected = vec![header.clone()];
        let mut replay = Replayer::default();
        replay.push(header).unwrap();
        for index in 0..1200 {
            let message = Message::user_text(format!("{index}:{}", "正文".repeat(128)));
            let entry = Entry::from_payload(
                EntryPayload::Message {
                    message,
                    tool: None,
                    finish_reason: None,
                    agent: None,
                    goal: None,
                },
                parent,
                index + 2,
            );
            parent = entry.id().to_owned();
            expected.push(entry.clone());
            replay.push(entry).unwrap();
        }
        let mut state = replay.finish().unwrap();
        assert_eq!(state.context_start, 0, "冻结不能变更语义上下文");
        assert!(state.transcript[0].message.content.is_archived());
        assert!(
            !state
                .transcript
                .last()
                .unwrap()
                .message
                .content
                .is_archived()
        );
        let super::super::stored::StoredEntry::Message { entry, .. } = &state.entries[1] else {
            panic!("消息记录")
        };
        assert!(Arc::ptr_eq(entry, &state.transcript[0]));
        assert_eq!(
            serde_json::to_value(state.log_entries()).unwrap(),
            serde_json::to_value(&expected).unwrap()
        );
        assert!(!state.transcript[0].message.content.archive_cache_loaded());
        let snapshot = state.clone();
        state.replace_content(0, Message::user_text("新正文").content);
        assert!(snapshot.transcript[0].message.text().starts_with("0:"));
        assert_eq!(
            state.log_entries()[1]
                .transcript_entry()
                .unwrap()
                .message
                .text(),
            "新正文"
        );
        assert!(
            std::mem::size_of::<super::super::stored::StoredEntry>() < std::mem::size_of::<Entry>()
        );
    }
}

pub(super) fn replay(entries: Vec<super::entries::Entry>) -> Result<SessionState, CoreError> {
    let mut replay = Replayer::default();
    for entry in entries {
        replay.push(entry)?;
    }
    replay.finish()
}

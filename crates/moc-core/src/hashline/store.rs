//! 每个会话的文件快照、已服务锚点与 undo 历史。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

/// 文件元数据及原始内容组成的快照纪元；任一变化都会使 served 记录失效。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotId {
    pub device: u64,
    pub inode: u64,
    pub modified_secs: i64,
    pub modified_nanos: i64,
    pub changed_secs: i64,
    pub changed_nanos: i64,
    pub size: u64,
    pub content_hash: u128,
}

impl SnapshotId {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                modified_secs: metadata.mtime(),
                modified_nanos: metadata.mtime_nsec(),
                changed_secs: metadata.ctime(),
                changed_nanos: metadata.ctime_nsec(),
                size: metadata.size(),
                content_hash: 0,
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            // 卷序列号与文件索引标识文件；写入时间检测常规内容修改。
            // 稳定版 MetadataExt 不提供 ChangeTime，创建时间补充文件代际信息。
            let modified = metadata.last_write_time();
            let created = metadata.creation_time();
            Self {
                device: u64::from(metadata.volume_serial_number().unwrap_or_default()),
                inode: metadata.file_index().unwrap_or(created),
                modified_secs: i64::try_from(modified / 10_000_000).unwrap_or(i64::MAX),
                modified_nanos: i64::try_from((modified % 10_000_000) * 100).unwrap_or(i64::MAX),
                changed_secs: i64::try_from(created / 10_000_000).unwrap_or(i64::MAX),
                changed_nanos: i64::try_from((created % 10_000_000) * 100).unwrap_or(i64::MAX),
                size: metadata.file_size(),
                content_hash: 0,
            }
        }
        #[cfg(all(not(unix), not(windows)))]
        {
            let modified = metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok());
            let (modified_secs, modified_nanos) = modified
                .map(|duration| {
                    (
                        i64::try_from(duration.as_secs()).unwrap_or(i64::MAX),
                        i64::from(duration.subsec_nanos()),
                    )
                })
                .unwrap_or((0, 0));
            Self {
                device: 0,
                inode: 0,
                modified_secs,
                modified_nanos,
                changed_secs: modified_secs,
                changed_nanos: modified_nanos,
                size: metadata.len(),
                content_hash: 0,
            }
        }
    }
    pub(crate) fn from_metadata_and_bytes(metadata: &std::fs::Metadata, bytes: &[u8]) -> Self {
        let mut snapshot = Self::from_metadata(metadata);
        snapshot.content_hash = xxhash_rust::xxh3::xxh3_128(bytes);
        snapshot
    }
}

/// 文件当下版本及可撤销编辑的原始字节。
#[derive(Debug, Clone)]
pub struct FileSnapshot {
    pub bytes: Vec<u8>,
    pub lines: Vec<String>,
    pub canons: Vec<u32>,
    pub epoch: SnapshotId,
    pub bom: bool,
    pub line_ending: LineEnding,
}

/// 原始文件的行尾格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineEnding {
    Lf,
    CrLf,
    Cr,
}

impl LineEnding {
    pub fn bytes(self) -> &'static [u8] {
        match self {
            Self::Lf => b"\n",
            Self::CrLf => b"\r\n",
            Self::Cr => b"\r",
        }
    }
}

/// 只有成功 `read` 发给模型的行才可授权编辑。
#[derive(Debug, Clone, Default)]
pub struct ServedFile {
    /// 行号 → 是否曾展示给模型（0 基下标）。
    pub served: Vec<bool>,
    pub epoch: Option<SnapshotId>,
}

impl ServedFile {
    pub fn new() -> Self {
        Self::default()
    }

    /// 覆盖式登记本次读取实际展示给模型的多个行窗口。
    pub fn serve(
        &mut self,
        epoch: SnapshotId,
        line_count: usize,
        windows: &[std::ops::Range<usize>],
    ) {
        if self.epoch != Some(epoch) || self.served.len() != line_count {
            self.served = vec![false; line_count];
            self.epoch = Some(epoch);
        }
        for window in windows {
            let start = window.start.min(line_count);
            let end = window.end.min(line_count);
            if start < end {
                self.served[start..end].fill(true);
            }
        }
    }

    /// 外部改写后使本纪元所有 served 许可失效。
    pub fn invalidate(&mut self) {
        self.epoch = None;
        self.served.fill(false);
    }

    /// 将已服务窗口按编辑产生的原行位移重定位到新文件。
    pub fn rebase(&mut self, moves: &[(usize, isize)], line_count: usize) -> usize {
        let mut rebased = vec![false; line_count];
        let mut dropped = 0;
        for (position, was_served) in self.served.iter().copied().enumerate() {
            if !was_served {
                continue;
            }
            let delta = moves
                .iter()
                .filter(|(start, _)| *start <= position)
                .next_back()
                .map_or(0, |(_, delta)| *delta);
            if delta == isize::MIN {
                dropped += 1;
                continue;
            }
            let Some(target) = position.checked_add_signed(delta) else {
                dropped += 1;
                continue;
            };
            if target < line_count {
                rebased[target] = true;
            } else {
                dropped += 1;
            }
        }
        self.served = rebased;
        dropped
    }
}

/// 文件锚点快照与 undo 栈；按路径保存字节，并统一维护撤销次序。
pub struct AnchorStore {
    files: RwLock<HashMap<PathBuf, ServedFile>>,
    snapshots: RwLock<HashMap<PathBuf, FileSnapshot>>,
    undo: RwLock<UndoHistory>,
    noop: Mutex<Option<(String, usize)>>,
}

#[derive(Default)]
struct UndoHistory {
    by_path: HashMap<PathBuf, Vec<UndoRecord>>,
    order: Vec<PathBuf>,
    /// 全部记录的字节总和；按字节淘汰时使用。
    bytes: usize,
}

/// 单路径最多保留的撤销记录数。
const MAX_UNDO_RECORDS_PER_PATH: usize = 8;
/// 全部撤销记录的字节上限；超过该值的单条记录不保留。
const MAX_UNDO_BYTES: usize = 64 * 1024 * 1024;

/// 撤销栈的淘汰规则：先保证单路径深度，再按全局最旧优先压到字节上限。
///
/// 每条记录保存整份旧内容，没有上限时长时间会话会随写入累积到数 GB。
impl UndoHistory {
    /// 淘汰某路径最旧的记录及其在全局次序中的位置。
    fn evict_front(&mut self, path: &Path) {
        let Some(stack) = self.by_path.get_mut(path) else {
            return;
        };
        let removed = stack.remove(0);
        self.bytes = self.bytes.saturating_sub(removed.bytes.len());
        let empty = stack.is_empty();
        if empty {
            self.by_path.remove(path);
        }
        if let Some(index) = self.order.iter().position(|entry| entry.as_path() == path) {
            self.order.remove(index);
        }
    }

    /// 淘汰全店最旧的记录；没有记录时返回 false。
    fn evict_oldest(&mut self) -> bool {
        let Some(path) = self.order.first().cloned() else {
            return false;
        };
        self.evict_front(&path);
        true
    }
}

#[derive(Clone)]
pub(crate) struct UndoRecord {
    pub bytes: Arc<[u8]>,
    pub expected: SnapshotId,
}

impl AnchorStore {
    pub fn new() -> Self {
        Self {
            files: RwLock::new(HashMap::new()),
            snapshots: RwLock::new(HashMap::new()),
            undo: RwLock::new(UndoHistory::default()),
            noop: Mutex::new(None),
        }
    }

    pub fn served(&self, path: &Path) -> Option<ServedFile> {
        self.files.read().get(path).cloned()
    }

    pub fn record_served(
        &self,
        path: PathBuf,
        windows: &[std::ops::Range<usize>],
        snapshot: FileSnapshot,
    ) {
        self.files.write().entry(path.clone()).or_default().serve(
            snapshot.epoch,
            snapshot.lines.len(),
            windows,
        );
        self.snapshots.write().insert(path, snapshot);
    }

    pub fn commit_edit(&self, path: PathBuf, served: ServedFile, snapshot: FileSnapshot) {
        self.files.write().insert(path.clone(), served);
        self.snapshots.write().insert(path, snapshot);
    }

    pub fn observe_edit(&self, payload: String, changed: bool) -> usize {
        let mut previous = self.noop.lock();
        if changed {
            *previous = None;
            return 0;
        }
        let count = previous
            .as_ref()
            .filter(|(last, _)| *last == payload)
            .map_or(1, |(_, count)| count + 1);
        *previous = Some((payload, count));
        count
    }

    pub fn invalidate(&self, path: &Path) {
        if let Some(served) = self.files.write().get_mut(path) {
            served.invalidate();
        }
        self.snapshots.write().remove(path);
    }

    /// 压入一条撤销记录；超过字节上限的单条记录不保留并返回 `false`。
    pub fn push_undo(&self, path: PathBuf, bytes: Vec<u8>, expected: SnapshotId) -> bool {
        self.push_undo_with_budget(path, bytes, expected, MAX_UNDO_BYTES)
    }

    /// 带显式预算的压入：`budget` 是整栈保留的字节上限，淘汰顺序为全局最旧优先。
    fn push_undo_with_budget(
        &self,
        path: PathBuf,
        bytes: Vec<u8>,
        expected: SnapshotId,
        budget: usize,
    ) -> bool {
        if bytes.len() > budget {
            return false;
        }
        let added = bytes.len();
        let mut undo = self.undo.write();
        undo.by_path
            .entry(path.clone())
            .or_default()
            .push(UndoRecord {
                bytes: bytes.into(),
                expected,
            });
        undo.order.push(path.clone());
        undo.bytes = undo.bytes.saturating_add(added);
        while undo
            .by_path
            .get(&path)
            .is_some_and(|stack| stack.len() > MAX_UNDO_RECORDS_PER_PATH)
        {
            undo.evict_front(&path);
        }
        while undo.bytes > budget && undo.evict_oldest() {}
        true
    }

    pub(crate) fn peek_undo(&self, path: &Path) -> Option<UndoRecord> {
        self.undo.read().by_path.get(path)?.last().cloned()
    }

    pub(crate) fn commit_undo(&self, path: &Path, restored: SnapshotId) {
        let mut undo = self.undo.write();
        let popped = if let Some(stack) = undo.by_path.get_mut(path) {
            let popped = stack.pop();
            if let Some(previous) = stack.last_mut() {
                // 原子替换会改变 inode；下一次撤销必须认可刚恢复的版本。
                previous.expected = restored;
            }
            popped
        } else {
            return;
        };
        undo.bytes = undo
            .bytes
            .saturating_sub(popped.map_or(0, |record| record.bytes.len()));
        if let Some(index) = undo.order.iter().rposition(|entry| entry.as_path() == path) {
            undo.order.remove(index);
        }
        if undo.by_path.get(path).is_some_and(Vec::is_empty) {
            undo.by_path.remove(path);
        }
    }

    pub fn latest_undo_path(&self) -> Option<PathBuf> {
        self.undo.read().order.last().cloned()
    }

    /// 测试用：撤销栈当前保留的总字节数。
    #[cfg(test)]
    fn undo_bytes(&self) -> usize {
        self.undo.read().bytes
    }

    /// 测试用：某路径当前保留的撤销记录数。
    #[cfg(test)]
    fn undo_record_count(&self, path: &Path) -> usize {
        self.undo.read().by_path.get(path).map_or(0, Vec::len)
    }
}

impl Default for AnchorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(size: u64) -> SnapshotId {
        SnapshotId {
            device: 0,
            inode: 0,
            modified_secs: 0,
            modified_nanos: 0,
            changed_secs: 0,
            changed_nanos: 0,
            size,
            content_hash: 0,
        }
    }

    #[test]
    fn content_hash_detects_same_metadata_and_size_changes() {
        let temporary = tempfile::NamedTempFile::new().expect("临时文件");
        std::fs::write(temporary.path(), b"before").expect("写测试文件");
        let metadata = std::fs::metadata(temporary.path()).expect("元数据");
        let before = SnapshotId::from_metadata_and_bytes(&metadata, b"before");
        let after = SnapshotId::from_metadata_and_bytes(&metadata, b"change");
        assert_ne!(before.content_hash, after.content_hash);
    }

    #[test]
    fn partial_reads_keep_previous_windows() {
        let mut served = ServedFile::new();
        let epoch = id(3);
        served.serve(epoch, 4, &[0..1]);
        served.serve(epoch, 4, &[2..3]);
        assert_eq!(served.served, vec![true, false, true, false]);
    }

    #[test]
    fn rebase_drops_rows_past_new_file_end() {
        let mut served = ServedFile {
            served: vec![true, true, true],
            epoch: Some(id(3)),
        };
        assert_eq!(served.rebase(&[(1, -2)], 1), 1);
        assert_eq!(served.served, vec![true]);
    }

    #[test]
    fn undo_depth_is_bounded_per_path() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        let store = AnchorStore::new();
        for index in 0..MAX_UNDO_RECORDS_PER_PATH + 3 {
            assert!(store.push_undo(path.clone(), vec![7u8; 16], id(index as u64)));
        }
        assert_eq!(store.undo_record_count(&path), MAX_UNDO_RECORDS_PER_PATH);
    }

    #[test]
    fn undo_bytes_are_bounded_and_oversized_records_are_rejected() {
        let dir = tempfile::tempdir().expect("临时目录");
        let store = AnchorStore::new();
        let path = dir.path().join("file.txt");
        let budget = 300;
        assert!(store.push_undo_with_budget(path.clone(), vec![1u8; 150], id(1), budget));
        assert!(store.push_undo_with_budget(path.clone(), vec![2u8; 150], id(1), budget));
        assert_eq!(store.undo_bytes(), budget);
        assert!(!store.push_undo_with_budget(path, vec![0u8; budget + 1], id(1), budget));
    }

    #[test]
    fn invalidating_snapshot_revokes_all_served_rows() {
        let path = std::path::PathBuf::from("src.rs");
        let store = AnchorStore::new();
        let snapshot = FileSnapshot {
            bytes: b"fn main() {}\n".to_vec(),
            lines: vec!["fn main() {}".into()],
            canons: vec![0],
            epoch: id(1),
            bom: false,
            line_ending: LineEnding::Lf,
        };
        store.record_served(path.clone(), &[0..1], snapshot);
        store.invalidate(&path);
        assert!(
            store
                .served(&path)
                .expect("served record")
                .served
                .iter()
                .all(|shown| !shown)
        );
    }
}

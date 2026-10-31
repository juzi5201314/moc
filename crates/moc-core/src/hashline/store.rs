//! 每个会话的文件快照、tag 版本链与 undo 历史。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::hashline::{content_hash, tag_of};

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
            // 稳定版 MetadataExt 不提供卷序列号、文件索引和 ChangeTime；创建时间
            // 代替文件代际标识，变化检测由时间戳、大小和内容哈希承担。
            let modified = metadata.last_write_time();
            let created = metadata.creation_time();
            Self {
                device: 0,
                inode: created,
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

/// 版本链上限：本会话连续编辑超过该次数后，最旧的 tag 不再可用。
const MAX_VERSIONS: usize = 16;

/// 版本链中的一个文件版本。
#[derive(Debug, Clone)]
struct Version {
    hash: u64,
    tag: u16,
    len: usize,
    /// 本版本到下一版本的位移表（按起始位置升序）；当前版本为空。
    moves: Vec<(usize, isize)>,
}

/// 旧 tag 无法解析到版本链上的原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TagError {
    Unknown,
    /// 16 位 tag 碰撞：多个内容不同的版本共用同一 tag。
    Ambiguous,
}

/// 一个文件在本会话中的可编辑状态：当前快照纪元、已展示行与编辑版本链。
///
/// 只有成功 `read` 或 diff 展示给模型的行才可编辑；本会话自身的编辑把旧
/// 版本保留在链上，旧 tag 的行号可沿位移表映射到当前坐标。
#[derive(Debug, Clone, Default)]
pub struct ServedFile {
    pub epoch: Option<SnapshotId>,
    /// 当前版本已展示行的位图。按 u64 字打包，n 行只占 n/8 字节，大文件也不按行分配。
    /// 不变量：`seen.len() == len.div_ceil(64)`。
    seen: Vec<u64>,
    len: usize,
    /// 本会话自身编辑形成的版本链，最旧在前，`last()` 是当前版本；长度 ≤ `MAX_VERSIONS`。
    versions: Vec<Version>,
}

impl ServedFile {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记本次展示给模型的行窗口并返回当前 tag。
    ///
    /// epoch 变化（外部修改、首次读取）时重置为只有当前版本的链。
    pub fn serve(
        &mut self,
        epoch: SnapshotId,
        lines: &[String],
        windows: &[std::ops::Range<usize>],
    ) -> u16 {
        if self.epoch != Some(epoch) || self.versions.is_empty() {
            let hash = content_hash(lines);
            self.versions = vec![Version {
                hash,
                tag: tag_of(hash),
                len: lines.len(),
                moves: Vec::new(),
            }];
            self.len = lines.len();
            self.seen = vec![0; self.len.div_ceil(64)];
            self.epoch = Some(epoch);
        }
        debug_assert_eq!(self.len, lines.len(), "同 epoch 的行数必须与当前版本一致");
        for window in windows {
            for position in window.start.min(self.len)..window.end.min(self.len) {
                self.mark_seen(position);
            }
        }
        debug_assert_eq!(self.seen.len(), self.len.div_ceil(64));
        self.versions.last().expect("版本链非空").tag
    }

    fn mark_seen(&mut self, position: usize) {
        self.seen[position / 64] |= 1 << (position % 64);
    }

    pub fn is_seen(&self, position: usize) -> bool {
        position < self.len && self.seen[position / 64] & (1 << (position % 64)) != 0
    }

    pub fn current_tag(&self) -> Option<u16> {
        self.versions.last().map(|version| version.tag)
    }

    /// 版本链中某个版本的行数。
    pub fn version_len(&self, version: usize) -> usize {
        self.versions[version].len
    }

    /// 返回本会话一次编辑之后的状态，不修改当前状态。
    ///
    /// 已展示位图按位移表搬到新坐标（被删除的行丢弃），版本链压入新版本；
    /// `epoch` 原样保留，由提交阶段覆盖。
    pub fn advanced(&self, moves: &[(usize, isize)], lines: &[String]) -> Self {
        debug_assert!(
            moves.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "位移表必须按起始位置升序"
        );
        let len = lines.len();
        let mut seen = vec![0u64; len.div_ceil(64)];
        for (word_index, &word) in self.seen.iter().enumerate() {
            let mut bits = word;
            while bits != 0 {
                let bit = usize::try_from(bits.trailing_zeros()).expect("bit index < 64");
                let position = word_index * 64 + bit;
                bits &= bits - 1;
                if let Some(target) = shifted_position(moves, position)
                    && target < len
                {
                    seen[target / 64] |= 1 << (target % 64);
                }
            }
        }
        let mut versions = self.versions.clone();
        if let Some(last) = versions.last_mut() {
            last.moves = moves.to_vec();
        }
        let hash = content_hash(lines);
        versions.push(Version {
            hash,
            tag: tag_of(hash),
            len,
            moves: Vec::new(),
        });
        if versions.len() > MAX_VERSIONS {
            versions.remove(0);
        }
        debug_assert!(versions.len() <= MAX_VERSIONS);
        Self {
            epoch: self.epoch,
            seen,
            len,
            versions,
        }
    }

    /// 把 tag 解析为版本链下标；内容相同的多个版本取最新的一个。
    pub fn resolve(&self, tag: u16) -> Result<usize, TagError> {
        let mut found: Option<(usize, u64)> = None;
        for (index, version) in self.versions.iter().enumerate().rev() {
            if version.tag != tag {
                continue;
            }
            match found {
                None => found = Some((index, version.hash)),
                Some((_, hash)) if hash != version.hash => return Err(TagError::Ambiguous),
                Some(_) => {}
            }
        }
        found.ok_or(TagError::Unknown).map(|(index, _)| index)
    }

    /// 把某个版本中的 0 基位置映射到当前版本；途中被删除或改写时返回 `None`。
    pub fn map_line(&self, version: usize, position: usize) -> Option<usize> {
        self.versions[version..self.versions.len() - 1]
            .iter()
            .try_fold(position, |position, step| {
                shifted_position(&step.moves, position)
            })
    }

    /// 外部改写后使本纪元所有 served 许可与版本链失效。
    pub fn invalidate(&mut self) {
        self.epoch = None;
        self.seen.clear();
        self.versions.clear();
        self.len = 0;
    }
}

/// 按位移表求原位置的新位置；落在删除区间时返回 `None`。
/// 调用方保证 `moves` 按起始位置升序。
fn shifted_position(moves: &[(usize, isize)], position: usize) -> Option<usize> {
    let index = moves.partition_point(|(start, _)| *start <= position);
    if index == 0 {
        return Some(position);
    }
    let (_, delta) = moves[index - 1];
    if delta == isize::MIN {
        return None;
    }
    position.checked_add_signed(delta)
}

/// 文件锚点快照与 undo 栈；按路径保存字节，并统一维护撤销次序。
pub struct AnchorStore {
    files: RwLock<HashMap<PathBuf, ServedFile>>,
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
            undo: RwLock::new(UndoHistory::default()),
            noop: Mutex::new(None),
        }
    }

    pub fn served(&self, path: &Path) -> Option<ServedFile> {
        self.files.read().get(path).cloned()
    }

    /// 登记一次读取展示的行窗口，返回当前 tag。
    pub fn record_served(
        &self,
        path: PathBuf,
        snapshot: &FileSnapshot,
        windows: &[std::ops::Range<usize>],
    ) -> u16 {
        self.files
            .write()
            .entry(path)
            .or_default()
            .serve(snapshot.epoch, &snapshot.lines, windows)
    }

    pub fn commit_edit(&self, path: PathBuf, served: ServedFile) {
        self.files.write().insert(path, served);
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

    fn rows(values: &[&str]) -> Vec<String> {
        values.iter().map(|line| (*line).to_owned()).collect()
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
    fn older_tag_maps_untouched_rows_and_drops_rewritten_ones() {
        let mut served = ServedFile::new();
        let before = rows(&["one", "two", "three"]);
        let old_tag = served.serve(id(3), &before, &[0..3]);
        // 删除第 2 行（位置 1），并在开头插入一行：one → 1，three → 3。
        let after = rows(&["top", "one", "three"]);
        let next = served.advanced(&[(0, 0), (0, 1), (1, isize::MIN), (2, 0)], &after);
        let version = next.resolve(old_tag).expect("旧 tag 仍在版本链上");
        assert_eq!(next.map_line(version, 0), Some(1));
        assert_eq!(next.map_line(version, 1), None);
        assert_eq!(next.map_line(version, 2), Some(2));
        assert!(next.is_seen(1) && next.is_seen(2) && !next.is_seen(0));
        assert_ne!(next.current_tag(), Some(old_tag));
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
            epoch: id(1),
            bom: false,
            line_ending: LineEnding::Lf,
        };
        store.record_served(path.clone(), &snapshot, &[0..1]);
        store.invalidate(&path);
        let served = store.served(&path).expect("served record");
        assert_eq!(served.current_tag(), None);
        assert!(!served.is_seen(0));
    }
}

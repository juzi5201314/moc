//! 每个会话的文件快照、已服务锚点与 undo 历史。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::hashline::canon_hash;

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
    pub hashes: Vec<u32>,
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
#[derive(Debug, Clone)]
pub struct ServedFile {
    /// 行号 → 锚点；`None` 表示未 served 或孤儿愈合后退役。
    pub hashes: Vec<Option<u32>>,
    /// 行号 → canon 摘要；值 0 表示空行或未 served。
    pub canons: Vec<u32>,
    pub epoch: Option<SnapshotId>,
    /// 当前文件修改后不可再分配的锚点及其死亡位置。
    pub retired: Vec<(u32, usize)>,
    pub reported: Vec<u32>,
    pub cards: Vec<usize>,
}

impl ServedFile {
    pub fn new() -> Self {
        Self {
            hashes: Vec::new(),
            canons: Vec::new(),
            epoch: None,
            retired: Vec::new(),
            reported: Vec::new(),
            cards: Vec::new(),
        }
    }

    /// 将一次 read 实际展示给模型的 0 基行范围登记为可编辑。
    pub fn record_served(
        &mut self,
        rows: &[(usize, u32)],
        lines: &[String],
        snapshot: &FileSnapshot,
    ) {
        let complete = rows.len() == lines.len()
            && rows
                .iter()
                .enumerate()
                .all(|(index, (position, _))| index == *position);
        if complete || self.epoch != Some(snapshot.epoch) || self.hashes.len() != lines.len() {
            self.hashes.resize(lines.len(), None);
            self.canons.resize(lines.len(), 0);
            self.hashes.fill(None);
            self.canons.fill(0);
            self.retired.clear();
            self.reported.clear();
            self.cards.resize(lines.len(), 0);
            self.epoch = Some(snapshot.epoch);
        }
        // 锚点固定在 62³ 内；临时反向表把孤儿愈合从逐行扫描降为线性。
        let mut positions = vec![usize::MAX; super::ANCHOR_SPACE as usize];
        for (position, hash) in self.hashes.iter().enumerate() {
            if let Some(hash) = hash {
                debug_assert!(*hash < super::ANCHOR_SPACE);
                positions[*hash as usize] = position;
            }
        }
        for &(position, hash) in rows {
            if position >= self.hashes.len() {
                continue;
            }
            debug_assert!(hash < super::ANCHOR_SPACE);
            let previous = positions[hash as usize];
            if previous != usize::MAX && previous != position {
                self.hashes[previous] = None;
                self.canons[previous] = 0;
            }
            positions[hash as usize] = position;
            self.hashes[position] = Some(hash);
            self.canons[position] = canon_hash(&lines[position]);
            self.cards[position] = position;
        }
    }

    /// 外部改写后使本纪元所有 served 许可失效。
    pub fn invalidate(&mut self) {
        self.epoch = None;
        self.hashes.fill(None);
        self.canons.fill(0);
        self.cards.fill(0);
    }
}

impl Default for ServedFile {
    fn default() -> Self {
        Self::new()
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

    pub fn record_served(&self, path: PathBuf, rows: &[(usize, u32)], snapshot: FileSnapshot) {
        self.files
            .write()
            .entry(path.clone())
            .or_default()
            .record_served(rows, &snapshot.lines, &snapshot);
        self.snapshots.write().insert(path, snapshot);
    }

    pub fn snapshot(&self, path: &Path) -> Option<FileSnapshot> {
        self.snapshots.read().get(path).cloned()
    }

    pub fn reuse_hashes(&self, path: &Path, snapshot: &mut FileSnapshot) {
        if let Some(previous) = self.snapshots.read().get(path)
            && previous.epoch == snapshot.epoch
        {
            snapshot.hashes.clone_from(&previous.hashes);
        }
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

    pub fn push_undo(&self, path: PathBuf, bytes: Vec<u8>, expected: SnapshotId) {
        let mut undo = self.undo.write();
        undo.by_path
            .entry(path.clone())
            .or_default()
            .push(UndoRecord {
                bytes: bytes.into(),
                expected,
            });
        undo.order.push(path);
    }

    pub(crate) fn peek_undo(&self, path: &Path) -> Option<UndoRecord> {
        self.undo.read().by_path.get(path)?.last().cloned()
    }

    pub(crate) fn commit_undo(&self, path: &Path, restored: SnapshotId) {
        let mut undo = self.undo.write();
        let empty = if let Some(stack) = undo.by_path.get_mut(path) {
            stack.pop();
            if let Some(previous) = stack.last_mut() {
                // 原子替换会改变 inode；下一次撤销必须认可刚恢复的版本。
                previous.expected = restored;
            }
            stack.is_empty()
        } else {
            return;
        };
        if let Some(index) = undo.order.iter().rposition(|entry| entry.as_path() == path) {
            undo.order.remove(index);
        }
        if empty {
            undo.by_path.remove(path);
        }
    }

    pub fn latest_undo_path(&self) -> Option<PathBuf> {
        self.undo.read().order.last().cloned()
    }

    pub fn clear_undo(&self, path: &Path) {
        let mut undo = self.undo.write();
        undo.by_path.remove(path);
        undo.order.retain(|entry| entry.as_path() != path);
    }
}

impl Default for AnchorStore {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use crate::hashline::assign_hashes;

    use super::*;

    fn snapshot(text: &str) -> FileSnapshot {
        let temporary = tempfile::NamedTempFile::new().expect("临时文件");
        fs::write(temporary.path(), text).expect("写测试文件");
        let metadata = fs::metadata(temporary.path()).expect("元数据");
        let bytes = text.as_bytes().to_vec();
        let bom = bytes.starts_with(&[0xef, 0xbb, 0xbf]);
        let content = if bom { &bytes[3..] } else { &bytes[..] };
        let line_ending = if content.windows(2).any(|window| window == b"\r\n") {
            LineEnding::CrLf
        } else if content.contains(&b'\r') {
            LineEnding::Cr
        } else {
            LineEnding::Lf
        };
        let text = String::from_utf8_lossy(content)
            .replace("\r\n", "\n")
            .replace('\r', "\n");
        let lines: Vec<String> = text.split('\n').map(str::to_owned).collect();
        let hashes = assign_hashes(&lines, &[]).expect("锚点分配");
        let canons = lines.iter().map(|line| canon_hash(line)).collect();
        let epoch = SnapshotId::from_metadata_and_bytes(&metadata, &bytes);
        FileSnapshot {
            bytes,
            lines,
            hashes,
            canons,
            epoch,
            bom,
            line_ending,
        }
    }

    #[test]
    fn content_hash_detects_same_metadata_and_size_changes() {
        let temporary = tempfile::NamedTempFile::new().expect("临时文件");
        fs::write(temporary.path(), b"before").expect("写测试文件");
        let metadata = fs::metadata(temporary.path()).expect("元数据");
        let before = SnapshotId::from_metadata_and_bytes(&metadata, b"before");
        let after = SnapshotId::from_metadata_and_bytes(&metadata, b"change");

        assert_ne!(before.content_hash, after.content_hash);
    }

    #[test]
    fn only_read_rows_are_served_and_duplicate_moves_heal_old_positions() {
        let snapshot = snapshot("first\nsecond\nthird\n");
        let mut served = ServedFile::new();
        served.record_served(
            &[(0, snapshot.hashes[0]), (2, snapshot.hashes[2])],
            &snapshot.lines,
            &snapshot,
        );
        assert_eq!(
            served.hashes,
            vec![
                Some(snapshot.hashes[0]),
                None,
                Some(snapshot.hashes[2]),
                None
            ]
        );
        assert_eq!(served.canons[1], 0);
    }

    #[test]
    fn invalidating_snapshot_revokes_all_served_rows() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("src.rs");
        let snapshot = snapshot("fn main() {}\n");
        let store = AnchorStore::new();
        store.record_served(path.clone(), &[(0, snapshot.hashes[0])], snapshot);
        store.invalidate(&path);
        assert!(store.snapshot(&path).is_none());
        assert!(
            store
                .served(&path)
                .unwrap()
                .hashes
                .iter()
                .all(Option::is_none)
        );
    }
}

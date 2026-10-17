//! 每个会话的文件快照、稳定锚点令牌与 undo 历史。

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::hashline::{AnchorToken, ID_TOTAL, id_from_index, id_index, id_of, token_key};

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

/// 单路径最多保留的已退役令牌数；超过后允许复用最旧的令牌。
const MAX_RETIRED_TOKENS: usize = 65_536;

/// 只有成功 `read` 或 diff 展示给模型的行才持有令牌。
///
/// `entries[position]` 为 `Some(token)` 表示该行已展示；令牌行号是它最近一次
/// 展示时的位置，随行移动而保留，使旧锚点在位移后仍能精确命中原行。
#[derive(Debug, Clone, Default)]
pub struct ServedFile {
    pub entries: Vec<Option<AnchorToken>>,
    pub epoch: Option<SnapshotId>,
    /// 已失效令牌；新令牌优先避开它们，避免旧锚点误命中后来的行。
    retired: BTreeSet<u64>,
}

impl ServedFile {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_served(&self, position: usize) -> bool {
        self.entries.get(position).is_some_and(Option::is_some)
    }

    pub fn token(&self, position: usize) -> Option<AnchorToken> {
        self.entries.get(position).copied().flatten()
    }

    /// 覆盖式登记本次读取实际展示给模型的多个行窗口。
    ///
    /// 同 epoch 下已有令牌保持不变，只把令牌行号更新到当前位置；新行分配
    /// 令牌。返回窗口内实际登记的行与令牌，供调用方按当前行号渲染。
    pub fn serve(
        &mut self,
        epoch: SnapshotId,
        lines: &[String],
        windows: &[std::ops::Range<usize>],
    ) -> Vec<(usize, AnchorToken)> {
        if self.epoch != Some(epoch) || self.entries.len() != lines.len() {
            self.entries = vec![None; lines.len()];
            self.retired.clear();
            self.epoch = Some(epoch);
        }
        self.refresh(lines, windows, true)
    }

    /// 只刷新已服务行的令牌行号，不新增服务范围。
    ///
    /// 用于编辑失败后的“当前锚点”回灌：只更新真正展示的窗口，避免把未读行
    /// 误标成可编辑。
    pub fn refresh_existing(
        &mut self,
        lines: &[String],
        windows: &[std::ops::Range<usize>],
    ) -> Vec<(usize, AnchorToken)> {
        self.refresh(lines, windows, false)
    }

    fn refresh(
        &mut self,
        lines: &[String],
        windows: &[std::ops::Range<usize>],
        extend: bool,
    ) -> Vec<(usize, AnchorToken)> {
        debug_assert_eq!(self.entries.len(), lines.len());
        let mut used: BTreeSet<u64> = self
            .entries
            .iter()
            .flatten()
            .map(|token| token_key(*token))
            .collect();
        used.extend(self.retired.iter().copied());
        let mut served = Vec::new();
        for window in windows {
            let start = window.start.min(lines.len());
            let end = window.end.min(lines.len());
            for position in start..end {
                let previous = self.entries[position];
                if !extend && previous.is_none() {
                    continue;
                }
                let line = position + 1;
                let preferred =
                    previous.map_or_else(|| id_of(line, &lines[position]), |token| token.id);
                if let Some(old) = previous {
                    used.remove(&token_key(old));
                }
                let mut index = id_index(preferred);
                let mut probes = 0u16;
                while used.contains(&token_key_at(line, index)) && probes < ID_TOTAL {
                    index = (index + 1) % ID_TOTAL;
                    probes += 1;
                }
                let token = AnchorToken {
                    line,
                    id: id_from_index(u32::from(index)),
                };
                if let Some(old) = previous
                    && old != token
                {
                    self.retire(old);
                    used.insert(token_key(old));
                }
                used.insert(token_key(token));
                self.entries[position] = Some(token);
                served.push((position, token));
            }
        }
        served
    }

    /// 将已服务行按编辑产生的原行位移重定位到新文件。
    ///
    /// 位移表必须按起始位置升序；删除区间用 `isize::MIN` 标记，对应行丢弃。
    /// 返回被丢弃的行数。
    pub fn rebase(&mut self, moves: &[(usize, isize)], line_count: usize) -> usize {
        debug_assert!(
            moves.windows(2).all(|pair| pair[0].0 <= pair[1].0),
            "rebase 位移表必须按起始位置升序"
        );
        let mut rebased = vec![None; line_count];
        let mut dropped = 0;
        for position in 0..self.entries.len() {
            let Some(token) = self.entries[position] else {
                continue;
            };
            match shifted_position(moves, position) {
                Some(target) if target < line_count => {
                    debug_assert!(
                        rebased[target].is_none(),
                        "合法位移表不应把两行映射到同一位置"
                    );
                    rebased[target] = Some(token);
                }
                _ => {
                    self.retire(token);
                    dropped += 1;
                }
            }
        }
        self.entries = rebased;
        dropped
    }

    /// 返回按位移表重定位后的副本，不修改当前状态。
    pub fn rebased(&self, moves: &[(usize, isize)], line_count: usize) -> Self {
        let mut next = self.clone();
        next.rebase(moves, line_count);
        next
    }

    /// 外部改写后使本纪元所有 served 许可失效。
    pub fn invalidate(&mut self) {
        self.epoch = None;
        self.entries.fill(None);
        self.retired.clear();
    }

    fn retire(&mut self, token: AnchorToken) {
        self.retired.insert(token_key(token));
        if self.retired.len() > MAX_RETIRED_TOKENS {
            // 极端长会话下只保留近期退役记录；令牌空间按行号隔离，
            // 复用只可能在同一行号上发生，旧锚点误命中的概率极低。
            self.retired.clear();
        }
    }
}

/// 位移表里「该位置已删除」的标记。
fn token_key_at(line: usize, index: u16) -> u64 {
    u64::try_from(line).expect("line number must fit in u64") * u64::from(ID_TOTAL)
        + u64::from(index)
}

/// 省略 `moves` 为空时的分支；调用方已保证升序。
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

    /// 登记一次读取展示的行窗口，返回窗口内当前令牌。
    pub fn record_served(
        &self,
        path: PathBuf,
        snapshot: &FileSnapshot,
        windows: &[std::ops::Range<usize>],
    ) -> Vec<(usize, AnchorToken)> {
        self.files
            .write()
            .entry(path)
            .or_default()
            .serve(snapshot.epoch, &snapshot.lines, windows)
    }

    /// 编辑失败后的当前锚点回灌：优先返回离失败锚点最近的已服务行。
    ///
    /// 只刷新选中的行令牌；`max_lines` 限制输出规模。epoch 或行数不匹配时
    /// 返回 None，由调用方提示重读。
    pub fn refresh_nearest(
        &self,
        path: &Path,
        snapshot: &FileSnapshot,
        hints: &[usize],
        max_lines: usize,
    ) -> Option<Vec<(usize, AnchorToken)>> {
        let mut files = self.files.write();
        let served = files.get_mut(path)?;
        if served.epoch != Some(snapshot.epoch) || served.entries.len() != snapshot.lines.len() {
            return None;
        }
        let mut positions: Vec<usize> = served
            .entries
            .iter()
            .enumerate()
            .filter_map(|(position, entry)| entry.is_some().then_some(position))
            .collect();
        if !hints.is_empty() {
            positions.sort_by_key(|position| {
                hints
                    .iter()
                    .map(|hint| hint.abs_diff(position + 1))
                    .min()
                    .unwrap_or(0)
            });
            positions.truncate(max_lines.max(1));
            positions.sort_unstable();
        } else {
            positions.truncate(max_lines.max(1));
        }
        let windows = contiguous_windows(&positions);
        Some(served.refresh_existing(&snapshot.lines, &windows))
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

/// 将有序位置合并成连续窗口。
fn contiguous_windows(positions: &[usize]) -> Vec<std::ops::Range<usize>> {
    let mut windows = Vec::new();
    let mut iter = positions.iter().copied();
    let Some(mut start) = iter.next() else {
        return windows;
    };
    let mut end = start + 1;
    for position in iter {
        if position == end {
            end += 1;
        } else {
            windows.push(start..end);
            start = position;
            end = position + 1;
        }
    }
    windows.push(start..end);
    windows
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
    fn partial_reads_keep_previous_windows() {
        let mut served = ServedFile::new();
        let epoch = id(3);
        let lines = rows(&["one", "two", "three", "four"]);
        served.serve(epoch, &lines, &[0..1]);
        served.serve(epoch, &lines, &[2..3]);
        assert!(served.is_served(0));
        assert!(!served.is_served(1));
        assert!(served.is_served(2));
        assert!(!served.is_served(3));
    }

    #[test]
    fn duplicate_rows_get_distinct_tokens_and_keep_them_across_shifts() {
        let mut served = ServedFile::new();
        let epoch = id(3);
        let lines = rows(&["end", "end", "tail"]);
        served.serve(epoch, &lines, &[0..3]);
        let first = served.token(0).expect("第一行令牌");
        let second = served.token(1).expect("第二行令牌");
        let third = served.token(2).expect("第三行令牌");
        assert_eq!(first.id, second.id, "同内容首选 id 相同");
        assert_ne!(first, second, "行号使完整令牌唯一");

        // 在第一行后插入一行：后两行下移，但令牌随行保留。
        served.rebase(&[(1, 0), (1, 1), (3, 1)], 4);
        assert_eq!(served.token(0), Some(first));
        assert_eq!(served.token(2), Some(second));
        assert_eq!(served.token(3), Some(third));
        assert!(served.token(1).is_none());
    }

    #[test]
    fn refresh_updates_rendered_rows_without_serving_new_ones() {
        let mut served = ServedFile::new();
        let epoch = id(3);
        let lines = rows(&["one", "two", "three"]);
        served.serve(epoch, &lines, &[0..2]);
        // 文件顶部插入一行后重新展示：令牌行号更新，id 保持不变。
        let shifted = rows(&["new", "one", "two", "three"]);
        served.rebase(&[(0, 0), (0, 1), (3, 1)], 4);
        let moved = served.token(1).expect("移动后的第一行");
        let refreshed = served.refresh_existing(&shifted, &[0..1]);
        assert!(refreshed.is_empty(), "未服务的新行不应被刷新");
        let refreshed = served.refresh_existing(&shifted, &[1..2]);
        assert_eq!(refreshed.len(), 1);
        assert_eq!(refreshed[0].0, 1);
        assert_eq!(refreshed[0].1.id, moved.id);
        assert_eq!(refreshed[0].1.line, 2);
        assert!(!served.is_served(0), "新插入的首行未被服务");
        assert!(served.is_served(2), "原有第二行仍在服务表中");
    }

    #[test]
    fn rebase_drops_rows_past_new_file_end() {
        let mut served = ServedFile::new();
        let epoch = id(3);
        served.serve(epoch, &rows(&["one", "two", "three"]), &[0..3]);
        let token = served.token(0).expect("令牌");
        // 删除第 2、3 行（1、2 号位置），只保留第 1 行。
        assert_eq!(
            served.rebase(&[(1, isize::MIN), (2, isize::MIN), (3, -2)], 1),
            2
        );
        assert_eq!(served.entries.len(), 1);
        assert_eq!(served.token(0), Some(token));
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
    fn refresh_nearest_prefers_rows_near_the_failed_anchor() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("file.txt");
        std::fs::write(&path, "one\ntwo\nthree\nfour\nfive\n").expect("写文件");
        let snapshot = crate::hashline::served::read_snapshot(&path, 4096).expect("读取快照");
        let store = AnchorStore::new();
        store.record_served(path.clone(), &snapshot, &[0..snapshot.lines.len()]);
        let tokens = store
            .refresh_nearest(&path, &snapshot, &[4], 2)
            .expect("刷新成功");
        let positions: Vec<usize> = tokens.iter().map(|(position, _)| *position).collect();
        assert_eq!(positions, vec![2, 3], "优先返回第 3、4 行");
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
        assert!(served.entries.iter().all(Option::is_none));
    }
}

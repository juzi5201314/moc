//! 结构化文件 diff：`edit` / `write` 工具在执行时由 core 产出，随
//! [`super::update::SessionUpdate::ToolFinished`] 与
//! [`super::permission::PermissionRequestParams`] 透出，前端零猜测渲染。
//!
//! 表示是**行平铺 + hunk 分组**：每个 [`DiffHunk`] 是一段连续变更（含上下文行），
//! hunk 之间是未变化的省略区；每行带 old/new 行号（`equal` 两者都有，`delete`
//! 只有 old，`insert` 只有 new），前端可以直接画行号 gutter，不必自己重放对齐。
//!
//! # 兼容
//!
//! 两个字段都以 `Option` + `default` + `skip_serializing_if` 模式出现：
//! `None` 时线上不出现该键，旧前端照常解析。

use serde::{Deserialize, Serialize};

/// 一次文件变更的结构化 diff。
///
/// 通常只含一个文件；`edit` 同时携带 `move` 时是两个（源 + 目标）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolDiff {
    /// 涉及的文件，按变更顺序排列。
    pub files: Vec<ToolFileDiff>,
    /// 变更超出上限被截断时为 `true`；此时 hunks 只覆盖前缀。
    pub truncated: bool,
}

/// 单个文件的 diff。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFileDiff {
    /// 目标文件路径（绝对路径）。
    pub path: String,
    /// 连续变更段（含上下文行），按文件顺序排列。
    pub hunks: Vec<DiffHunk>,
}

/// 一段连续变更：变更行 + 两侧上下文行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffHunk {
    /// 上下文前的旧文件起始行（1-based；hunk 无 delete/equal 行时等于插入点）。
    pub old_start: u32,
    /// 上下文前的新文件起始行（1-based；hunk 无 insert/equal 行时等于删除点）。
    pub new_start: u32,
    /// 该 hunk 的行序列。
    pub lines: Vec<DiffLine>,
}

/// diff 中的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiffLine {
    /// 行类别。
    pub kind: DiffLineKind,
    /// 旧文件中的行号（1-based）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub old_no: Option<u32>,
    /// 新文件中的行号（1-based）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_no: Option<u32>,
    /// 行内容（不含行尾换行）。
    pub text: String,
}

/// [`DiffLine`] 的类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffLineKind {
    /// 未变化行（上下文）。
    Equal,
    /// 旧文件中被删除的行。
    Delete,
    /// 新文件中新增的行。
    Insert,
}

/// 差异区域行数上限：剥离共同前后缀后，任一侧的差异区域超过该值时
/// 只对齐前缀并置 `truncated`。
///
/// 前后缀裁剪让这个上限约束**真正不同的内容**，而不是文件总行数：大文件里
/// 的小改动只对改动附近做 O(k·k) 对齐（k ≤ 该值 + 两侧上下文），不会因为
/// 文件有几千行就被误判为截断。
pub const DIFF_MAX_LINES: usize = 2000;

/// hunk 上下文行数：每个方向 3 行。
pub const DIFF_CONTEXT_LINES: usize = 3;

/// 两个变更区间相距 ≤ 该值（上下文行数 ×2）时合并为一个 hunk。
const DIFF_MERGE_GAP_LINES: usize = DIFF_CONTEXT_LINES * 2;

/// 由「旧文件行」与「新文件行」构造单文件 [`ToolDiff`]。
///
/// 这是 core 侧 `edit` / `write` 共用的唯一入口：输入是两份完整行序列
/// （不含行尾换行），输出是带上下文与行号的 hunk 序列；总行数超过
/// [`DIFF_MAX_LINES`] 时只对前缀对齐并置 `truncated`。
pub fn build_tool_diff(
    path: impl Into<String>,
    old_lines: &[&str],
    new_lines: &[&str],
) -> ToolDiff {
    let (hunks, truncated) = build_hunks(old_lines, new_lines);
    ToolDiff {
        files: vec![ToolFileDiff {
            path: path.into(),
            hunks,
        }],
        truncated,
    }
}

/// 组装多文件 [`ToolDiff`]（`edit` + `move` 双文件场景）。
pub fn build_tool_diff_files(files: Vec<ToolFileDiff>, truncated: bool) -> ToolDiff {
    ToolDiff { files, truncated }
}

/// 对两份行序列做 LCS 对齐，产出带行号的 hunk 序列与整体截断标志。
///
/// 先剥离共同前缀/后缀，只对差异窗口做对齐；因此文件很大但改动很小时，
/// 工作量和输出都与改动规模相关，而不是与文件行数相关。差异区域任一侧
/// 超过 [`DIFF_MAX_LINES`] 时只对开头部分做对齐并置 `truncated`（只有这种
/// 情况才可能有改动没进 hunks）。
pub fn build_hunks(old_lines: &[&str], new_lines: &[&str]) -> (Vec<DiffHunk>, bool) {
    let max_prefix = old_lines.len().min(new_lines.len());
    let prefix = old_lines[..max_prefix]
        .iter()
        .zip(&new_lines[..max_prefix])
        .take_while(|(old, new)| old == new)
        .count();
    let max_suffix = max_prefix - prefix;
    let suffix = old_lines[old_lines.len() - max_suffix..]
        .iter()
        .rev()
        .zip(new_lines[new_lines.len() - max_suffix..].iter().rev())
        .take_while(|(old, new)| old == new)
        .count();
    let old_diff_len = old_lines.len() - prefix - suffix;
    let new_diff_len = new_lines.len() - prefix - suffix;
    let truncated = old_diff_len > DIFF_MAX_LINES || new_diff_len > DIFF_MAX_LINES;

    // 差异窗口两侧各带 3 行上下文；截断时不再包含尾部上下文。
    let context_before = prefix.min(DIFF_CONTEXT_LINES);
    let context_after = suffix.min(DIFF_CONTEXT_LINES);
    let old_start = prefix - context_before;
    let new_start = old_start;
    let old_len = if truncated {
        context_before + old_diff_len.min(DIFF_MAX_LINES)
    } else {
        context_before + old_diff_len + context_after
    };
    let new_len = if truncated {
        context_before + new_diff_len.min(DIFF_MAX_LINES)
    } else {
        context_before + new_diff_len + context_after
    };
    let old_window = &old_lines[old_start..old_start + old_len];
    let new_window = &new_lines[new_start..new_start + new_len];

    let table = lcs_table(old_window, new_window);
    // 标准回溯从表尾走向表头；先试 Insert 再试 Delete，reverse 之后恰好得到
    // 「同区域内 delete 行在前、insert 行在后」的展示顺序
    let mut raw: Vec<(DiffLineKind, usize, usize)> = Vec::new();
    let (mut oi, mut ni) = (old_window.len(), new_window.len());
    while oi > 0 || ni > 0 {
        if oi > 0 && ni > 0 && old_window[oi - 1] == new_window[ni - 1] {
            raw.push((DiffLineKind::Equal, oi - 1, ni - 1));
            oi -= 1;
            ni -= 1;
        } else if ni > 0 && (oi == 0 || table[oi][ni - 1] >= table[oi - 1][ni]) {
            raw.push((DiffLineKind::Insert, oi, ni - 1));
            ni -= 1;
        } else {
            raw.push((DiffLineKind::Delete, oi - 1, ni));
            oi -= 1;
        }
    }
    raw.reverse();

    let lines: Vec<DiffLine> = raw
        .into_iter()
        .map(|(kind, old_index, new_index)| {
            let text = match kind {
                DiffLineKind::Delete => old_window[old_index],
                DiffLineKind::Insert | DiffLineKind::Equal => new_window[new_index],
            };
            DiffLine {
                kind,
                old_no: (kind != DiffLineKind::Insert).then(|| (old_start + old_index + 1) as u32),
                new_no: (kind != DiffLineKind::Delete).then(|| (new_start + new_index + 1) as u32),
                text: text.to_owned(),
            }
        })
        .collect();

    let hunks = group_hunks(lines);
    (hunks, truncated)
}

/// 一段已知的行替换：旧文件 `[start, end)` 行由 `replacement` 替换。
///
/// `start`/`end` 是 0-based 半开区间；`end == start` 表示纯插入。
#[derive(Debug, Clone, Copy)]
pub struct LineChange<'a> {
    pub start: usize,
    pub end: usize,
    pub replacement: &'a [String],
}

/// 由已知的行替换构造完整 hunk（不跑 LCS）。
///
/// `changes` 必须按 `start` 升序且互不重叠，`end` 不得越过 `old_lines`。
/// 上下文行数与 hunk 合并规则与 [`build_hunks`] 相同；这里不设行数上限，
/// 因为 edit 的改动范围由工具调用本身限定，前端另有渲染安全上限。
///
/// 替换块内的文本即使与旧行相同也按删除/插入显示：它描述的是调用方实际
/// 执行的编辑范围，而不是重新推断出的最小差异。
pub fn build_hunks_from_changes(old_lines: &[&str], changes: &[LineChange<'_>]) -> Vec<DiffHunk> {
    debug_assert!(
        changes.windows(2).all(|pair| pair[0].end <= pair[1].start),
        "changes 必须按 start 升序且互不重叠"
    );
    debug_assert!(
        changes
            .iter()
            .all(|change| change.start <= change.end && change.end <= old_lines.len()),
        "changes 不能越出旧文件范围"
    );

    // 前后完全一致的空操作不进入 diff；过滤后行位移仍然正确。
    let kept: Vec<&LineChange<'_>> = changes
        .iter()
        .filter(|change| !is_noop(old_lines, change))
        .collect();
    if kept.is_empty() {
        return Vec::new();
    }

    let mut delta_before = Vec::with_capacity(kept.len());
    let mut delta = 0isize;
    for change in &kept {
        delta_before.push(delta);
        delta += replacement_delta(change);
    }

    // 每个变更扩展上下文；窗口为半开区间，相邻窗口直接合并（对应
    // `DIFF_MERGE_GAP_LINES` 的合并规则）。
    let mut windows: Vec<(usize, usize, usize, usize)> = Vec::new();
    for (index, change) in kept.iter().enumerate() {
        let start = change.start.saturating_sub(DIFF_CONTEXT_LINES);
        let end = change
            .end
            .saturating_add(DIFF_CONTEXT_LINES)
            .min(old_lines.len());
        match windows.last_mut() {
            Some((_, last, _, last_end)) if start <= *last_end => {
                *last = index;
                *last_end = end;
            }
            _ => windows.push((index, index, start, end)),
        }
    }

    let mut hunks = Vec::with_capacity(windows.len());
    for (first, last, window_start, window_end) in windows {
        let mut lines = Vec::new();
        let mut cursor = window_start;
        let mut delta = delta_before[first];
        for change in &kept[first..=last] {
            for index in cursor..change.start {
                lines.push(equal_line(old_lines, index, delta));
            }
            for (offset, text) in old_lines[change.start..change.end].iter().enumerate() {
                lines.push(DiffLine {
                    kind: DiffLineKind::Delete,
                    old_no: Some((change.start + offset + 1) as u32),
                    new_no: None,
                    text: (*text).to_owned(),
                });
            }
            for (offset, text) in change.replacement.iter().enumerate() {
                let new_no = change.start as isize + delta + offset as isize + 1;
                lines.push(DiffLine {
                    kind: DiffLineKind::Insert,
                    old_no: None,
                    new_no: Some(new_no as u32),
                    text: text.clone(),
                });
            }
            cursor = change.end;
            delta += replacement_delta(change);
        }
        for index in cursor..window_end {
            lines.push(equal_line(old_lines, index, delta));
        }

        let old_start = lines
            .iter()
            .find_map(|line| line.old_no)
            .unwrap_or_else(|| {
                // 纯插入 hunk 没有任何旧行号：用 1-based 插入位置，
                // 与 hunk 间隔的计算口径保持一致。
                (kept[first].start + 1) as u32
            });
        let new_start = lines.iter().find_map(|line| line.new_no).unwrap_or(1);
        hunks.push(DiffHunk {
            old_start,
            new_start,
            lines,
        });
    }
    hunks
}

/// 旧行与替换行完全一致时为 true（空操作，不进入 diff）。
fn is_noop(old_lines: &[&str], change: &LineChange<'_>) -> bool {
    old_lines[change.start..change.end]
        .iter()
        .copied()
        .eq(change.replacement.iter().map(String::as_str))
}

/// 该替换带来的新行数变化。
fn replacement_delta(change: &LineChange<'_>) -> isize {
    change.replacement.len() as isize - (change.end - change.start) as isize
}

fn equal_line(old_lines: &[&str], index: usize, delta: isize) -> DiffLine {
    DiffLine {
        kind: DiffLineKind::Equal,
        old_no: Some((index + 1) as u32),
        new_no: Some((index as isize + delta + 1) as u32),
        text: old_lines[index].to_owned(),
    }
}

/// LCS 长度表（`(old+1) × (new+1)`）。
fn lcs_table(old_lines: &[&str], new_lines: &[&str]) -> Vec<Vec<u32>> {
    let mut table = vec![vec![0u32; new_lines.len() + 1]; old_lines.len() + 1];
    for (oi, old) in old_lines.iter().enumerate() {
        for (ni, new) in new_lines.iter().enumerate() {
            table[oi + 1][ni + 1] = if old == new {
                table[oi][ni] + 1
            } else {
                table[oi][ni + 1].max(table[oi + 1][ni])
            };
        }
    }
    table
}

/// 把平铺的 diff 行按「连续变更 + 上下文」分组为 hunk。
///
/// 相邻变更区间之间隔 ≤ [`DIFF_MERGE_GAP_LINES`] 行 Equal 时合并；每个变更
/// 区间只保留两侧 [`DIFF_CONTEXT_LINES`] 行上下文。行号已由调用方填好，
/// 这里只做切分与上下文裁剪。
fn group_hunks(lines: Vec<DiffLine>) -> Vec<DiffHunk> {
    // 变更行的下标区间（含首尾）
    let mut change_ranges: Vec<(usize, usize)> = Vec::new();
    for (index, line) in lines.iter().enumerate() {
        if line.kind != DiffLineKind::Equal {
            match change_ranges.last_mut() {
                Some((_, end)) if *end + 1 == index => *end = index,
                _ => change_ranges.push((index, index)),
            }
        }
    }
    if change_ranges.is_empty() {
        return Vec::new();
    }
    // 合并近邻区间
    let mut merged: Vec<(usize, usize)> = Vec::with_capacity(change_ranges.len());
    for &(start, end) in &change_ranges {
        match merged.last_mut() {
            Some((_, prev_end)) if start <= *prev_end + DIFF_MERGE_GAP_LINES + 1 => {
                *prev_end = end;
            }
            _ => merged.push((start, end)),
        }
    }
    // 每个区间扩展上下文并裁剪
    let mut hunks = Vec::with_capacity(merged.len());
    for &(start, end) in &merged {
        let hunk_start = start.saturating_sub(DIFF_CONTEXT_LINES);
        let hunk_end = (end + DIFF_CONTEXT_LINES).min(lines.len() - 1);
        let slice = &lines[hunk_start..=hunk_end];
        // old_start / new_start 取 hunk 内第一个可用行号；纯插入 hunk 没有任何
        // old_no，此时 old_start 用第一行的 new_no 起始（等于插入点前一行 + 1）
        let old_start = slice.iter().find_map(|line| line.old_no).unwrap_or(1);
        let new_start = slice.iter().find_map(|line| line.new_no).unwrap_or(1);
        hunks.push(DiffHunk {
            old_start,
            new_start,
            lines: slice.to_vec(),
        });
    }
    hunks
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_shape_round_trip() {
        let diff = ToolDiff {
            files: vec![ToolFileDiff {
                path: "/tmp/a.rs".into(),
                hunks: vec![DiffHunk {
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Equal,
                            old_no: Some(1),
                            new_no: Some(1),
                            text: "fn main() {".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Delete,
                            old_no: Some(2),
                            new_no: None,
                            text: "    println!(\"a\");".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Insert,
                            old_no: None,
                            new_no: Some(2),
                            text: "    println!(\"b\");".into(),
                        },
                    ],
                }],
            }],
            truncated: false,
        };
        let value = serde_json::to_value(&diff).expect("可序列化");
        assert_eq!(
            value,
            json!({
                "files": [{
                    "path": "/tmp/a.rs",
                    "hunks": [{
                        "old_start": 1,
                        "new_start": 1,
                        "lines": [
                            { "kind": "equal", "old_no": 1, "new_no": 1, "text": "fn main() {" },
                            { "kind": "delete", "old_no": 2, "text": "    println!(\"a\");" },
                            { "kind": "insert", "new_no": 2, "text": "    println!(\"b\");" }
                        ]
                    }]
                }],
                "truncated": false
            })
        );
        let round_trip: ToolDiff = serde_json::from_value(value).expect("可反序列化");
        assert_eq!(round_trip, diff);
    }

    fn lines(text: &str) -> Vec<&str> {
        text.lines().collect()
    }

    #[test]
    fn detects_single_line_change_with_context() {
        let old = lines("a\nb\nc\nd\ne");
        let new = lines("a\nb\nC\nd\ne");
        let diff = build_tool_diff("/tmp/a.txt", &old, &new);
        assert!(!diff.truncated);
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].hunks.len(), 1);
        let hunk = &diff.files[0].hunks[0];
        // 变更行 + 两侧各 3 行上下文 = a b -c +C d e
        assert_eq!(hunk.lines.len(), 6);
        assert_eq!(hunk.lines[0].kind, DiffLineKind::Equal);
        assert_eq!(hunk.lines[2].kind, DiffLineKind::Delete);
        assert_eq!(hunk.lines[2].old_no, Some(3));
        assert_eq!(hunk.lines[3].kind, DiffLineKind::Insert);
        assert_eq!(hunk.lines[3].new_no, Some(3));
        assert_eq!(hunk.old_start, 1);
        assert_eq!(hunk.new_start, 1);
    }

    #[test]
    fn pure_insertion_reports_no_old_lines() {
        let old = lines("a\nb");
        let new = lines("a\nb\nc\nd");
        let diff = build_tool_diff("/tmp/a.txt", &old, &new);
        let hunk = &diff.files[0].hunks[0];
        assert!(
            hunk.lines
                .iter()
                .all(|line| line.kind != DiffLineKind::Delete)
        );
        assert_eq!(hunk.lines[2].text, "c");
        assert_eq!(hunk.lines[2].new_no, Some(3));
    }

    #[test]
    fn identical_input_produces_no_hunks() {
        let old = lines("a\nb");
        let diff = build_tool_diff("/tmp/a.txt", &old, &old);
        assert!(diff.files[0].hunks.is_empty());
    }

    #[test]
    fn rewrite_is_truncated_to_cap() {
        let old: Vec<String> = (0..DIFF_MAX_LINES + 5)
            .map(|i| format!("old {i}"))
            .collect();
        let new: Vec<String> = (0..DIFF_MAX_LINES + 5)
            .map(|i| format!("new {i}"))
            .collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
        let diff = build_tool_diff("/tmp/big.txt", &old_refs, &new_refs);
        assert!(diff.truncated);
        assert!(!diff.files[0].hunks.is_empty());
    }

    /// 文件很大但改动很小时不能被判成截断：改动位于旧 2000 行上限之后也一样。
    #[test]
    fn small_change_in_large_file_is_not_truncated() {
        let changed_at = DIFF_MAX_LINES + 50;
        let old: Vec<String> = (0..DIFF_MAX_LINES + 100)
            .map(|index| format!("line {index}"))
            .collect();
        let mut new = old.clone();
        new[changed_at] = "changed".into();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
        let diff = build_tool_diff("/tmp/big.txt", &old_refs, &new_refs);
        assert!(!diff.truncated, "文件大不代表改动大");
        assert_eq!(diff.files[0].hunks.len(), 1);
        let hunk = &diff.files[0].hunks[0];
        assert!(
            hunk.lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Delete
                    && line.text == format!("line {changed_at}"))
        );
        assert!(
            hunk.lines
                .iter()
                .any(|line| line.kind == DiffLineKind::Insert && line.text == "changed")
        );
        assert_eq!(hunk.old_start, (changed_at - DIFF_CONTEXT_LINES + 1) as u32);
    }

    /// 超大但完全相同的输入不应被误标为截断，也不应产生 hunk。
    #[test]
    fn identical_large_inputs_are_not_truncated() {
        let old: Vec<String> = (0..DIFF_MAX_LINES + 100)
            .map(|index| format!("line {index}"))
            .collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let diff = build_tool_diff("/tmp/big.txt", &old_refs, &old_refs);
        assert!(!diff.truncated);
        assert!(diff.files[0].hunks.is_empty());
    }

    /// 已知变更直接构造完整 hunk：相距很远的改动各自保留，行号正确。
    #[test]
    fn known_distant_changes_keep_both_hunks() {
        let old: Vec<String> = (0..5000).map(|index| format!("line {index}")).collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let first = vec!["first".to_owned()];
        let second = vec!["second".to_owned()];
        let changes = [
            LineChange {
                start: 10,
                end: 11,
                replacement: &first,
            },
            LineChange {
                start: 4000,
                end: 4001,
                replacement: &second,
            },
        ];
        let hunks = build_hunks_from_changes(&old_refs, &changes);
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 8, "上下文从第 8 行开始");
        assert_eq!(hunks[1].old_start, 3998, "上下文从第 3998 行开始");
        let new_no_of = |hunk: &DiffHunk, text: &str| {
            hunk.lines
                .iter()
                .find(|line| line.text == text)
                .and_then(|line| line.new_no)
        };
        assert_eq!(new_no_of(&hunks[0], "first"), Some(11));
        assert_eq!(new_no_of(&hunks[1], "second"), Some(4001));
    }

    /// 近邻变更合并为一个 hunk；no-op 变更不标成改动。
    #[test]
    fn known_changes_merge_context_and_skip_noops() {
        let old: Vec<String> = (0..30).map(|index| format!("line {index}")).collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let noop = vec!["line 5".to_owned()];
        let first = vec!["first".to_owned()];
        let second = vec!["second".to_owned()];
        let changes = [
            LineChange {
                start: 5,
                end: 6,
                replacement: &noop,
            },
            LineChange {
                start: 10,
                end: 11,
                replacement: &first,
            },
            LineChange {
                start: 17,
                end: 18,
                replacement: &second,
            },
        ];
        let hunks = build_hunks_from_changes(&old_refs, &changes);
        assert_eq!(hunks.len(), 1, "相隔 6 行未变时合并为一个 hunk");
        let lines = &hunks[0].lines;
        assert!(
            !lines
                .iter()
                .any(|line| line.text == "line 5" && line.kind != DiffLineKind::Equal),
            "no-op 不应标成改动"
        );
        assert!(lines.iter().any(|line| line.text == "first"));
        assert!(lines.iter().any(|line| line.text == "second"));
    }

    /// 纯插入与纯删除的新行号按累积位移计算。
    #[test]
    fn known_insertions_and_deletions_number_lines() {
        let old: Vec<String> = (0..10).map(|index| format!("line {index}")).collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let inserted = vec!["new a".to_owned(), "new b".to_owned()];
        let hunks = build_hunks_from_changes(
            &old_refs,
            &[LineChange {
                start: 5,
                end: 5,
                replacement: &inserted,
            }],
        );
        assert_eq!(hunks[0].old_start, 3, "上下文从第 3 行开始");
        let inserts: Vec<(u32, &str)> = hunks[0]
            .lines
            .iter()
            .filter(|line| line.kind == DiffLineKind::Insert)
            .map(|line| (line.new_no.expect("插入行有 new_no"), line.text.as_str()))
            .collect();
        assert_eq!(inserts, [(6, "new a"), (7, "new b")]);

        let hunks = build_hunks_from_changes(
            &old_refs,
            &[LineChange {
                start: 5,
                end: 7,
                replacement: &[],
            }],
        );
        let deletes: Vec<(u32, &str)> = hunks[0]
            .lines
            .iter()
            .filter(|line| line.kind == DiffLineKind::Delete)
            .map(|line| (line.old_no.expect("删除行有 old_no"), line.text.as_str()))
            .collect();
        assert_eq!(deletes, [(6, "line 5"), (7, "line 6")]);
        let trailing = hunks[0]
            .lines
            .iter()
            .find(|line| line.kind == DiffLineKind::Equal && line.old_no == Some(8))
            .expect("第 8 行是尾部上下文");
        assert_eq!(trailing.new_no, Some(6), "删除两行后新行号前移 2");
    }
}

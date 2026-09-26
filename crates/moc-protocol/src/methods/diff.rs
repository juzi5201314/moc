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

/// diff 行数上限：hunk 总行数（含上下文）超过即截断。
///
/// 前端按可视密度折算，2000 行足够铺满多屏；超出部分对模型回灌内容（工具
/// `content`）没有影响，只影响展示。
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
/// 行数任一侧超过 [`DIFF_MAX_LINES`] 时只对前缀做对齐并置整体截断：
/// O(n·m) 只发生在受上界保护的范围内，超大文件不会拖垮工具执行；
/// 分组裁剪后 wire 上的行总量仍有界（变更行 ≤ 2 × cap + 上下文）。
pub fn build_hunks(old_lines: &[&str], new_lines: &[&str]) -> (Vec<DiffHunk>, bool) {
    let cap = DIFF_MAX_LINES;
    let truncated = old_lines.len() > cap || new_lines.len() > cap;
    let limit_old = old_lines.len().min(cap);
    let limit_new = new_lines.len().min(cap);

    let table = lcs_table(&old_lines[..limit_old], &new_lines[..limit_new]);
    // 标准回溯从表尾走向表头；先试 Insert 再试 Delete，reverse 之后恰好得到
    // 「同区域内 delete 行在前、insert 行在后」的展示顺序
    let mut raw: Vec<(DiffLineKind, usize, usize)> = Vec::new();
    let (mut oi, mut ni) = (limit_old, limit_new);
    while oi > 0 || ni > 0 {
        if oi > 0 && ni > 0 && old_lines[oi - 1] == new_lines[ni - 1] {
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
                DiffLineKind::Delete => old_lines[old_index],
                DiffLineKind::Insert | DiffLineKind::Equal => new_lines[new_index],
            };
            DiffLine {
                kind,
                old_no: (kind != DiffLineKind::Insert).then(|| (old_index as u32) + 1),
                new_no: (kind != DiffLineKind::Delete).then(|| (new_index as u32) + 1),
                text: text.to_owned(),
            }
        })
        .collect();

    let hunks = group_hunks(lines);
    (hunks, truncated)
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
                        DiffLine { kind: DiffLineKind::Equal, old_no: Some(1), new_no: Some(1), text: "fn main() {".into() },
                        DiffLine { kind: DiffLineKind::Delete, old_no: Some(2), new_no: None, text: "    println!(\"a\");".into() },
                        DiffLine { kind: DiffLineKind::Insert, old_no: None, new_no: Some(2), text: "    println!(\"b\");".into() },
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
        assert!(hunk.lines.iter().all(|line| line.kind != DiffLineKind::Delete));
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
        let old: Vec<String> = (0..DIFF_MAX_LINES + 5).map(|i| format!("old {i}")).collect();
        let new: Vec<String> = (0..DIFF_MAX_LINES + 5).map(|i| format!("new {i}")).collect();
        let old_refs: Vec<&str> = old.iter().map(String::as_str).collect();
        let new_refs: Vec<&str> = new.iter().map(String::as_str).collect();
        let diff = build_tool_diff("/tmp/big.txt", &old_refs, &new_refs);
        assert!(diff.truncated);
        assert!(!diff.files[0].hunks.is_empty());
    }
}

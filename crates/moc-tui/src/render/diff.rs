//! 结构化 diff 的终端渲染：行号 gutter + 删除/添加底色 + 语法高亮。
//!
//! 视觉规格参照 omp 的 Edit 盒：单列行号（delete 用旧行号、insert 用新行号、
//! equal 用新行号），变更行铺 `diff_delete_bg` / `diff_insert_bg`，内容按文件
//! 扩展名做语法高亮；hunk 之间用 `⋯ N 行未变` 分隔，整份 diff 超出构造
//! 上限时在末尾补一行提示（只补一次，不按文件重复）。

use moc_protocol::methods::{DiffLineKind, ToolDiff, ToolFileDiff};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::highlight;
use crate::theme::Theme;

/// diff 内部渲染行数上限：超出的行不渲染（协议侧同样有截断，双保险）。
pub const MAX_RENDERED_ROWS: usize = 4000;

/// diff 统计：删除/添加行数。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DiffStats {
    pub insertions: usize,
    pub deletions: usize,
}

/// 统计一个 diff 的变更规模（工具盒标题行用）。
pub fn stats(diff: &ToolDiff) -> DiffStats {
    let mut stats = DiffStats::default();
    for file in &diff.files {
        for hunk in &file.hunks {
            for line in &hunk.lines {
                match line.kind {
                    DiffLineKind::Insert => stats.insertions += 1,
                    DiffLineKind::Delete => stats.deletions += 1,
                    DiffLineKind::Equal => {}
                }
            }
        }
    }
    stats
}

/// 渲染 diff 全部文件；`limit` 是内容行上限（`usize::MAX` = 不限，
/// 内部仍受 [`MAX_RENDERED_ROWS`] 约束），达到上限且还有内容时补一行省略提示。
/// `diff.truncated` 的提示描述整份 diff，只追加一次。
pub fn render_diff(
    diff: &ToolDiff,
    width: usize,
    theme: &Theme,
    limit: usize,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for file in &diff.files {
        render_file(file, width, theme, &mut lines);
        if lines.len() >= limit {
            break;
        }
    }
    // 截断提示描述的是整份 diff，不能按文件重复打印。
    if diff.truncated {
        lines.push(Line::from(Span::styled(
            "⋯ 其余改动超出 diff 上限，未显示".to_owned(),
            Style::new().fg(theme.accent_warning),
        )));
    }
    let rendered = rendered_row_count(diff, limit);
    lines.truncate(rendered);
    let omitted = row_count(diff).saturating_sub(rendered);
    if omitted > 0 {
        lines.push(Line::from(Span::styled(
            format!("⋯ 还有 {omitted} 行"),
            Style::new().fg(theme.gray_dim),
        )));
    }
    lines
}

/// [`render_diff`] 实际落在行数上限内的内容行数（不含省略提示行）。
pub fn rendered_row_count(diff: &ToolDiff, limit: usize) -> usize {
    row_count(diff).min(limit).min(MAX_RENDERED_ROWS)
}

/// diff 的展示行数（与 [`render_diff`] 同规则，但不做高亮）。
///
/// 懒渲染用它估算工具块高度；hunk 之间的「⋯ N 行未变」与整份 diff 的
/// 截断提示各占一行。行数与宽度无关：diff 行不做换行，超长行交给终端自身折行。
pub fn row_count(diff: &ToolDiff) -> usize {
    let mut rows = 0usize;
    for file in &diff.files {
        if rows > MAX_RENDERED_ROWS {
            // 渲染到上限就收束，后续文件不再进渲染器
            break;
        }
        rows += file
            .hunks
            .iter()
            .map(|hunk| hunk.lines.len())
            .sum::<usize>();
        rows += file.hunks.len().saturating_sub(1);
    }
    rows + usize::from(diff.truncated)
}

fn render_file(file: &ToolFileDiff, width: usize, theme: &Theme, out: &mut Vec<Line<'static>>) {
    let syntax_token = syntax_token_of(&file.path);
    // 整文件共用一个高亮会话：块注释/多行字符串的解析状态跨行延续
    let mut highlighter = highlight::LineHighlighter::new(&syntax_token);
    let mut source_line = String::new();
    // gutter 宽度 = 全文件最大行号位数 + 1 位修饰符（- / + / 空格）
    let gutter_width = file
        .hunks
        .iter()
        .flat_map(|hunk| hunk.lines.iter())
        .filter_map(|line| line.old_no.or(line.new_no))
        .max()
        .map_or(1, |max| max.to_string().len())
        .max(1);
    let content_width = width.max(gutter_width + 4).saturating_sub(gutter_width + 4);
    // 上一个 hunk 在旧文件里覆盖到的最后一行：hunk 间隔只数真正被省略的
    // 未变行，不能拿两个 hunk 的起点相减（那样会把前一个 hunk 的尾部上下文
    // 也算进去）。
    let mut previous_last_old: Option<u32> = None;

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        if hunk_index > 0 {
            let first_old = hunk
                .lines
                .iter()
                .find_map(|line| line.old_no)
                .unwrap_or(hunk.old_start);
            let previous = previous_last_old.unwrap_or(0);
            let gap = first_old.saturating_sub(previous).saturating_sub(1);
            out.push(Line::from(Span::styled(
                format!("{}⋯ {} 行未变", " ".repeat(gutter_width + 1), gap),
                Style::new().fg(theme.gray_dim),
            )));
        }
        let hunk_lines = &hunk.lines;
        let mut intra_ranges: Vec<Option<std::ops::Range<usize>>> = vec![None; hunk_lines.len()];
        for i in 0..hunk_lines.len().saturating_sub(1) {
            if hunk_lines[i].kind == DiffLineKind::Delete
                && hunk_lines[i + 1].kind == DiffLineKind::Insert
                && let Some((del_range, ins_range)) =
                    find_intra_line_diff(&hunk_lines[i].text, &hunk_lines[i + 1].text)
                {
                    intra_ranges[i] = Some(del_range);
                    intra_ranges[i + 1] = Some(ins_range);
                }
        }

        for (line_idx, line) in hunk.lines.iter().enumerate() {
            if out.len() > MAX_RENDERED_ROWS {
                return;
            }
            let (marker, number, number_color, bg, fallback_fg) = match line.kind {
                DiffLineKind::Delete => (
                    '-',
                    line.old_no,
                    theme.diff_delete_fg,
                    theme.diff_delete_bg,
                    theme.diff_delete_fg,
                ),
                DiffLineKind::Insert => (
                    '+',
                    line.new_no,
                    theme.diff_insert_fg,
                    theme.diff_insert_bg,
                    theme.diff_insert_fg,
                ),
                DiffLineKind::Equal => (
                    ' ',
                    line.new_no,
                    theme.diff_gutter_fg,
                    theme.bg_base,
                    theme.text_secondary,
                ),
            };
            let gutter = match number {
                Some(number) => format!("{marker}{number:>gutter_width$}│"),
                None => format!("{marker}{}│", " ".repeat(gutter_width)),
            };

            let mut spans: Vec<Span<'static>> = vec![
                Span::styled(gutter + " ", Style::new().fg(number_color).bg(bg)),
                Span::styled("  ".to_owned(), Style::new().bg(bg)),
            ];
            let changed_range = intra_ranges.get(line_idx).and_then(Option::as_ref);
            source_line.clear();
            source_line.push_str(&line.text);
            source_line.push('\n');
            let spans_of_line = highlighter.line(&source_line);
            let mut byte_offset = 0;
            for span in &spans_of_line {
                let span_len = span.text.len();
                let span_range = byte_offset..byte_offset + span_len;
                byte_offset += span_len;
                let is_changed = changed_range
                    .is_some_and(|r| span_range.start < r.end && span_range.end > r.start);
                let mut style = Style::new()
                    .fg(span.fg.map(highlight::rgb_to_color).unwrap_or(fallback_fg))
                    .bg(bg);
                if is_changed {
                    style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
                }
                spans.push(Span::styled(span.text.clone(), style));
            }
            let content_width_used: usize = spans_of_line
                .iter()
                .map(|span| super::wrap::str_width(&span.text))
                .sum();
            if content_width_used < content_width && line.kind != DiffLineKind::Equal {
                spans.push(Span::styled(
                    " ".repeat(content_width - content_width_used),
                    Style::new().bg(bg),
                ));
            }
            out.push(Line::from(spans));
        }
        previous_last_old = Some(
            hunk.lines
                .iter()
                .rev()
                .find_map(|line| line.old_no)
                .unwrap_or_else(|| hunk.old_start.saturating_sub(1)),
        );
    }
}

fn find_intra_line_diff(
    del: &str,
    ins: &str,
) -> Option<(std::ops::Range<usize>, std::ops::Range<usize>)> {
    if del == ins || del.is_empty() || ins.is_empty() {
        return None;
    }
    let del_b = del.as_bytes();
    let ins_b = ins.as_bytes();
    let mut prefix = 0;
    while prefix < del_b.len() && prefix < ins_b.len() && del_b[prefix] == ins_b[prefix] {
        prefix += 1;
    }
    while prefix > 0 && !del.is_char_boundary(prefix) {
        prefix -= 1;
    }

    let mut suffix_del = del_b.len();
    let mut suffix_ins = ins_b.len();
    while suffix_del > prefix
        && suffix_ins > prefix
        && del_b[suffix_del - 1] == ins_b[suffix_ins - 1]
    {
        suffix_del -= 1;
        suffix_ins -= 1;
    }
    while suffix_del < del.len() && !del.is_char_boundary(suffix_del) {
        suffix_del += 1;
    }
    while suffix_ins < ins.len() && !ins.is_char_boundary(suffix_ins) {
        suffix_ins += 1;
    }

    if prefix <= suffix_del && prefix <= suffix_ins && (prefix < suffix_del || prefix < suffix_ins)
    {
        Some((prefix..suffix_del, prefix..suffix_ins))
    } else {
        None
    }
}

/// 路径 → syntect 语言标记（按扩展名；不认识时按纯文本）。
fn syntax_token_of(path: &str) -> String {
    std::path::Path::new(path)
        .extension()
        .and_then(|ext| ext.to_str())
        .unwrap_or("txt")
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorLevel;
    use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind};

    /// 逐行渲染共用高亮会话：hunk 分隔、行号 gutter 与行内差异标记都保持原样。
    #[test]
    fn renders_hunk_lines_with_shared_highlighter() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let diff = ToolDiff {
            files: vec![ToolFileDiff {
                path: "src/lib.rs".into(),
                hunks: vec![
                    DiffHunk {
                        old_start: 1,
                        new_start: 1,
                        lines: vec![
                            DiffLine {
                                kind: DiffLineKind::Delete,
                                old_no: Some(1),
                                new_no: None,
                                text: "let value = 1;".into(),
                            },
                            DiffLine {
                                kind: DiffLineKind::Insert,
                                old_no: None,
                                new_no: Some(1),
                                text: "let value = 2;".into(),
                            },
                        ],
                    },
                    DiffHunk {
                        old_start: 10,
                        new_start: 10,
                        lines: vec![DiffLine {
                            kind: DiffLineKind::Equal,
                            old_no: Some(10),
                            new_no: Some(10),
                            text: "}".into(),
                        }],
                    },
                ],
            }],
            truncated: false,
        };

        let lines = render_diff(&diff, 80, &theme, usize::MAX);
        assert_eq!(lines.len(), 4, "两行改动 + 一条 hunk 分隔 + 一行上下文");
        let text_of = |line: &Line<'static>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert!(text_of(&lines[0]).contains("let value = 1;"));
        assert!(text_of(&lines[1]).contains("let value = 2;"));
        assert!(
            text_of(&lines[2]).contains("行未变"),
            "hunk 之间给出未变行数"
        );
        for (line, changed) in [(&lines[0], '1'), (&lines[1], '2')] {
            assert!(
                line.spans.iter().any(|span| span.content.contains(changed)
                    && span.style.add_modifier.contains(Modifier::UNDERLINED)),
                "行内差异片段应带下划线标记"
            );
        }
    }

    /// hunk 间隔只数真正被省略的未变行，不把前一个 hunk 的尾部上下文算进去。
    #[test]
    fn hunk_gap_counts_only_omitted_unchanged_lines() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let equal = |number: u32| DiffLine {
            kind: DiffLineKind::Equal,
            old_no: Some(number),
            new_no: Some(number),
            text: format!("line {number}"),
        };
        let delete = |number: u32| DiffLine {
            kind: DiffLineKind::Delete,
            old_no: Some(number),
            new_no: None,
            text: format!("old {number}"),
        };
        let insert = |number: u32| DiffLine {
            kind: DiffLineKind::Insert,
            old_no: None,
            new_no: Some(number),
            text: format!("new {number}"),
        };
        let hunk = |old_start: u32, lines: Vec<DiffLine>| DiffHunk {
            old_start,
            new_start: old_start,
            lines,
        };
        let diff = ToolDiff {
            files: vec![ToolFileDiff {
                path: "src/lib.rs".into(),
                hunks: vec![
                    hunk(
                        2,
                        vec![
                            equal(2),
                            equal(3),
                            equal(4),
                            delete(5),
                            insert(5),
                            equal(6),
                            equal(7),
                            equal(8),
                        ],
                    ),
                    hunk(
                        17,
                        vec![
                            equal(17),
                            equal(18),
                            equal(19),
                            delete(20),
                            insert(20),
                            equal(21),
                            equal(22),
                            equal(23),
                        ],
                    ),
                ],
            }],
            truncated: false,
        };
        let lines = render_diff(&diff, 80, &theme, usize::MAX);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("⋯ 8 行未变"), "只应省略 8 行：{text}");
        assert!(
            !text.contains("⋯ 14 行未变"),
            "不能把前一个 hunk 自身的行也算进去"
        );
    }

    /// 截断提示属于整份 diff：多文件也只出现一次，行数估算与渲染一致。
    #[test]
    fn truncation_notice_prints_once_for_all_files() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let file = |path: &str| ToolFileDiff {
            path: path.into(),
            hunks: vec![DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Insert,
                    old_no: None,
                    new_no: Some(1),
                    text: "x".into(),
                }],
            }],
        };
        let diff = ToolDiff {
            files: vec![file("a.rs"), file("b.rs")],
            truncated: true,
        };
        let lines = render_diff(&diff, 80, &theme, usize::MAX);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert_eq!(
            text.matches("其余改动超出").count(),
            1,
            "截断提示只出现一次：{text}"
        );
        assert_eq!(row_count(&diff), lines.len(), "行数估算与渲染同源");

        let limited = render_diff(&diff, 80, &theme, 1);
        let limited_text = limited
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            limited_text.contains("还有 2 行"),
            "被行数上限挤掉的行仍要报数：{limited_text}"
        );
    }
}

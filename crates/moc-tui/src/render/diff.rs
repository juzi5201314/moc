//! 结构化 diff 的终端渲染：行号 gutter + 删除/添加底色 + 语法高亮。
//!
//! 视觉规格参照 omp 的 Edit 盒：单列行号（delete 用旧行号、insert 用新行号、
//! equal 用新行号），变更行铺 `diff_delete_bg` / `diff_insert_bg`，内容按文件
//! 扩展名做语法高亮；hunk 之间用 `⋯ N 行未变` 分隔。

use moc_protocol::methods::{DiffLineKind, ToolDiff, ToolFileDiff};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::highlight;
use crate::theme::Theme;

/// diff 内部渲染行数上限：超出的行不渲染（协议侧同样有截断，双保险）。
const MAX_RENDERED_ROWS: usize = 4000;

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

/// 渲染 diff 全部文件。
pub fn render_diff(diff: &ToolDiff, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for file in &diff.files {
        render_file(file, diff.truncated, width, theme, &mut lines);
    }
    lines
}

fn render_file(
    file: &ToolFileDiff,
    truncated: bool,
    width: usize,
    theme: &Theme,
    out: &mut Vec<Line<'static>>,
) {
    let syntax_token = syntax_token_of(&file.path);
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

    for (hunk_index, hunk) in file.hunks.iter().enumerate() {
        if hunk_index > 0 {
            let gap = hunk
                .old_start
                .saturating_sub(file.hunks[hunk_index - 1].old_start)
                .saturating_sub(1);
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
            {
                if let Some((del_range, ins_range)) =
                    find_intra_line_diff(&hunk_lines[i].text, &hunk_lines[i + 1].text)
                {
                    intra_ranges[i] = Some(del_range);
                    intra_ranges[i + 1] = Some(ins_range);
                }
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
            let highlighted = highlight::highlight(&syntax_token, &line.text);
            if let Some(spans_of_line) = highlighted.lines.first() {
                let mut byte_offset = 0;
                for span in spans_of_line {
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
            }
            let content_width_used: usize = highlighted.lines.first().map_or(0, |spans| {
                spans
                    .iter()
                    .map(|span| super::wrap::str_width(&span.text))
                    .sum()
            });
            if content_width_used < content_width && line.kind != DiffLineKind::Equal {
                spans.push(Span::styled(
                    " ".repeat(content_width - content_width_used),
                    Style::new().bg(bg),
                ));
            }
            out.push(Line::from(spans));
        }
    }

    if truncated {
        out.push(Line::from(Span::styled(
            format!("{}⋯ 改动过多，已省略部分内容", " ".repeat(gutter_width + 1)),
            Style::new().fg(theme.accent_warning),
        )));
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

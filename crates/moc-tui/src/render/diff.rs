//! 结构化 diff 的终端渲染：行号 gutter + 删除/添加底色 + 语法高亮。
//!
//! 视觉规格参照 omp 的 Edit 盒：单列行号（delete 用旧行号、insert 用新行号、
//! equal 用新行号），变更行铺 `diff_delete_bg` / `diff_insert_bg`，内容按文件
//! 扩展名做语法高亮；hunk 之间用 `⋯ N 行未变` 分隔。

use moc_protocol::methods::{DiffLineKind, ToolDiff, ToolFileDiff};
use ratatui::style::Style;
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
        for line in &hunk.lines {
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
            // 内容逐行独立高亮——diff 行在文件里不连续，共享解析状态会着色错误
            let highlighted = highlight::highlight(&syntax_token, &line.text);
            if let Some(spans_of_line) = highlighted.lines.first() {
                for span in spans_of_line {
                    spans.push(Span::styled(
                        span.text.clone(),
                        Style::new()
                            .fg(span.fg.map(highlight::rgb_to_color).unwrap_or(fallback_fg))
                            .bg(bg),
                    ));
                }
            }
            // 行内容宽度不足时用背景色补齐，保证变更行的底色贯穿整行
            let content_width_used: usize = highlighted
                .lines
                .first()
                .map(|spans| {
                    spans
                        .iter()
                        .map(|span| super::wrap::str_width(&span.text))
                        .sum()
                })
                .unwrap_or(0);
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
            format!("{}⋯ diff 已截断（超出上限）", " ".repeat(gutter_width + 1)),
            Style::new().fg(theme.accent_warning),
        )));
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

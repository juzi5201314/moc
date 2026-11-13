use super::CONTEXT_GAP;
use super::CONTEXT_INDENT;
use super::CONTEXT_NAME_COL;
use crate::render::modal::CONTEXT_PERCENT_COL;
use crate::render::modal::CONTEXT_VALUE_COL;
use crate::render::wrap::str_width;
use crate::render::wrap::truncate;
use crate::theme::Theme;
use moc_protocol::methods::ContextUsageResult;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// 占比：保留一位小数；非零但不足 0.1% 显示 `<0.1%`，不缩成 0.0%。
pub(crate) fn share_percent(part: u64, total: u64) -> String {
    let percent = part as f64 * 100.0 / total.max(1) as f64;
    if percent > 0.0 && percent < 0.05 {
        "<0.1%".to_owned()
    } else {
        format!("{percent:.1}%")
    }
}

/// 上下文占用明细：分块列表 + 按窗口刻度的分块色条与边界标注。
///
/// 列表每块以 `▌` 色标起头，颜色与色条上对应那段一致；色条按上下文窗口定标，
/// 因此空会话就是一整条暗色轨道，已用部分永远落在左侧。边界标注落在各分块
/// 右界，只在不重叠处显示，并优先保住「已用总量」与窗口上限两条。
pub(in super::super) fn context_lines(
    usage: &ContextUsageResult,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let total = usage.total_tokens;
    let window = usage.context_window;
    let mut lines = Vec::with_capacity(usage.sections.len() + 6);
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
        Span::styled(
            crate::render::format_tokens(total),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {}", crate::render::format_tokens(window)),
            Style::new().fg(theme.gray),
        ),
        Span::styled(
            format!(" · {}", share_percent(total, window)),
            Style::new().fg(theme.gray),
        ),
    ]));
    lines.push(Line::from(String::new()));
    for (index, section) in usage.sections.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
            Span::styled("▌ ", Style::new().fg(theme.chart(index))),
            Span::styled(
                format_section_name(&section.name, CONTEXT_NAME_COL),
                Style::new().fg(theme.text_secondary),
            ),
            Span::styled(
                format!(
                    "{:>CONTEXT_VALUE_COL$}",
                    crate::render::format_tokens(section.tokens)
                ),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled(" ".repeat(CONTEXT_GAP), Style::new()),
            Span::styled(
                format!(
                    "{:>CONTEXT_PERCENT_COL$}",
                    share_percent(section.tokens, total)
                ),
                Style::new().fg(theme.gray),
            ),
        ]));
    }
    lines.push(Line::from(String::new()));
    // 分块右界：累积 token 按 `max(已用, 窗口)` 折算成格位；累积单调所以格位不回头
    let cells = width.saturating_sub(CONTEXT_INDENT).max(1);
    let scale = window.max(total).max(1);
    let cells_u64 = u64::try_from(cells).unwrap_or(u64::MAX);
    let mut marks = Vec::with_capacity(usage.sections.len());
    let mut cumulative = 0u64;
    for section in &usage.sections {
        cumulative = cumulative.saturating_add(section.tokens);
        let cell = cumulative
            .saturating_mul(cells_u64)
            .saturating_add(scale / 2)
            / scale;
        marks.push((
            cumulative,
            usize::try_from(cell).unwrap_or(cells).min(cells),
        ));
    }
    lines.push(context_bar_line(&marks, cells, theme));
    lines.push(context_scale_line(&marks, total, window, cells, theme));
    lines
}

/// 分块色条：各分块按格位铺 `━`（颜色与列表色标一致），窗口剩余部分铺暗色 `─`。
pub(super) fn context_bar_line(
    marks: &[(u64, usize)],
    cells: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut spans = Vec::with_capacity(marks.len() + 2);
    spans.push(Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()));
    let mut drawn = 0usize;
    for (index, &(_, boundary)) in marks.iter().enumerate() {
        if boundary > drawn {
            spans.push(Span::styled(
                "━".repeat(boundary - drawn),
                Style::new().fg(theme.chart(index)),
            ));
            drawn = boundary;
        }
    }
    if drawn < cells {
        spans.push(Span::styled(
            "─".repeat(cells - drawn),
            Style::new().fg(theme.border),
        ));
    }
    Line::from(spans)
}

/// 分块边界标注：写出各分块右界的累积用量，窗口上限贴住色条末端。
pub(super) fn context_scale_line(
    marks: &[(u64, usize)],
    total: u64,
    window: u64,
    cells: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut taken = vec![false; cells];
    let mut placed: Vec<(usize, String, Color)> = Vec::with_capacity(marks.len() + 1);
    if total > 0
        && let Some(&(_, boundary)) = marks.last()
    {
        push_label(
            &mut taken,
            &mut placed,
            boundary,
            crate::render::format_tokens(total),
            theme.chart(marks.len() - 1),
        );
    }
    let cap = crate::render::format_tokens(window);
    push_label(&mut taken, &mut placed, cells, cap, theme.gray_dim);
    for (index, &(cumulative, boundary)) in marks.iter().enumerate() {
        let text = crate::render::format_tokens(cumulative);
        if placed.iter().any(|(_, existing, _)| *existing == text) {
            continue;
        }
        push_label(&mut taken, &mut placed, boundary, text, theme.chart(index));
    }
    placed.sort_unstable_by_key(|(start, ..)| *start);
    let mut spans = Vec::with_capacity(placed.len() * 2 + 2);
    spans.push(Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()));
    let mut cursor = 0usize;
    for (start, text, color) in placed {
        let width = str_width(&text);
        if start > cursor {
            spans.push(Span::styled(" ".repeat(start - cursor), Style::new()));
        }
        cursor = start + width;
        spans.push(Span::styled(text, Style::new().fg(color)));
    }
    if cursor < cells {
        spans.push(Span::styled(" ".repeat(cells - cursor), Style::new()));
    }
    Line::from(spans)
}

/// 分块显示名格式化：按终端显示宽度对齐；超过列宽时截断加省略号，不足时补空格。
pub(super) fn format_section_name(name: &str, target_width: usize) -> String {
    let current_width = str_width(name);
    if current_width <= target_width {
        format!("{name}{}", " ".repeat(target_width - current_width))
    } else {
        truncate(name, target_width).into_owned()
    }
}

/// 落一个边界标注：以分块右界为锚点右对齐（使数值落位在该分块色条区间的末端）；
/// 若数值为 0k 则以起点左对齐。空间不足或与已落位的标注相碰时直接放弃。
pub(super) fn push_label(
    taken: &mut [bool],
    placed: &mut Vec<(usize, String, Color)>,
    target_boundary: usize,
    text: String,
    color: Color,
) {
    let width = str_width(&text);
    let cells = taken.len();
    if width == 0 || width > cells {
        return;
    }
    let position = if target_boundary == 0 {
        0
    } else {
        target_boundary.min(cells).saturating_sub(width)
    };
    let end = (position + width).min(cells);
    let position = end.saturating_sub(width);
    let clear = !taken[position..end].iter().any(|cell| *cell)
        && (position == 0 || !taken[position - 1])
        && (end == cells || !taken[end]);
    if clear {
        taken[position..end].fill(true);
        placed.push((position, text, color));
    }
}

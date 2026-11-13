use super::super::wrap::line_width;
use super::super::wrap::str_width;
use super::super::wrap::wrap_spans;
use super::super::wrap::wrap_text;
use super::Run;
use super::TABLE_COLUMN_FLOOR;
use super::TABLE_COLUMN_GAP;
use super::TABLE_COLUMN_TOKEN_MAX;
use super::TableAcc;
use crate::theme::Theme;
use moc_core::chart::RawTable;
use pulldown_cmark::Alignment;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// 表格单元格的纯文本：与 [`raw_table`] 用同一份去装饰结果。
pub(super) fn table_cell_text(cells: &[Vec<Run>], index: usize) -> String {
    cells
        .get(index)
        .map(|cell| cell.iter().map(|run| run.text.as_str()).collect())
        .unwrap_or_default()
}

/// 每列的自然宽度、不可断词宽度与是否含宽字符。
pub(super) fn table_metrics(
    table: &TableAcc,
    columns: usize,
) -> (Vec<usize>, Vec<usize>, Vec<bool>) {
    let mut natural = vec![1usize; columns];
    let mut longest_word = vec![1usize; columns];
    let mut has_wide = vec![false; columns];
    for cells in std::iter::once(&table.header).chain(table.rows.iter()) {
        for (index, cell) in cells.iter().enumerate().take(columns) {
            let text: String = cell.iter().map(|run| run.text.as_str()).collect();
            natural[index] = natural[index].max(str_width(&text));
            let word = text.split_whitespace().map(str_width).max().unwrap_or(1);
            longest_word[index] = longest_word[index].max(word);
            has_wide[index] |= text
                .chars()
                .any(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1);
        }
    }
    (natural, longest_word, has_wide)
}

/// 计算可见列宽：放得下就按内容自然宽度（不拉伸，窄表不撑满整行）；超宽时先
/// 按可读下限收缩、再退到硬下限，单元格在列内折行。`None` 表示连两格一列都放不下。
pub(super) fn table_widths(
    available: usize,
    natural: &[usize],
    longest_word: &[usize],
    has_wide: &[bool],
) -> Option<(Vec<usize>, usize)> {
    let columns = natural.len();
    if columns == 0 {
        return None;
    }
    let two_cell_min = columns.saturating_mul(2);
    let gap = if available >= two_cell_min + 2 * (columns - 1) {
        TABLE_COLUMN_GAP
    } else if available >= two_cell_min + (columns - 1) {
        1
    } else {
        return None;
    };
    let usable = available.saturating_sub(gap.saturating_mul(columns - 1));
    let mut widths = natural.to_vec();
    if widths.iter().sum::<usize>() > usable {
        let mut floors: Vec<usize> = (0..columns)
            .map(|index| {
                let token_floor =
                    longest_word[index].clamp(TABLE_COLUMN_FLOOR, TABLE_COLUMN_TOKEN_MAX);
                let mut floor = natural[index].min(token_floor);
                if has_wide[index] {
                    floor = floor.max(2);
                }
                floor.clamp(1, widths[index])
            })
            .collect();
        shrink_widths(&mut widths, &floors, usable);
        if widths.iter().sum::<usize>() > usable {
            for (index, floor) in floors.iter_mut().enumerate() {
                *floor = (if has_wide[index] { 2 } else { 1 }).min(widths[index]);
            }
            shrink_widths(&mut widths, &floors, usable);
        }
    }
    Some((widths, gap))
}

/// 把总宽压到 `target`：每次从“超下限最多”的列减一格，保持列宽均衡。
pub(super) fn shrink_widths(widths: &mut [usize], floors: &[usize], target: usize) {
    let mut total: usize = widths.iter().sum();
    while total > target {
        let mut best: Option<(usize, usize)> = None;
        for (index, width) in widths.iter().enumerate() {
            let slack = width.saturating_sub(floors[index]);
            if slack > 0 && best.is_none_or(|(_, current)| slack > current) {
                best = Some((index, slack));
            }
        }
        let Some((index, _)) = best else {
            break;
        };
        widths[index] -= 1;
        total -= 1;
    }
}

/// 单元格折行：与正文同一套断点规则（[`wrap_spans`]），保留内联样式，按列对齐补齐到列宽。
pub(super) fn wrap_cell_runs(
    runs: &[Run],
    width: usize,
    align: Alignment,
    base_style: Style,
) -> Vec<Vec<Span<'static>>> {
    let spans: Vec<Span<'static>> = runs
        .iter()
        .map(|run| Span::styled(run.text.clone(), base_style.patch(run.style)))
        .collect();
    wrap_spans(&spans, width)
        .into_iter()
        .map(|line| {
            let free = width.saturating_sub(line_width(&line));
            let (left, right) = match align {
                Alignment::Right => (free, 0),
                Alignment::Center => (free / 2, free - free / 2),
                Alignment::Left | Alignment::None => (0, free),
            };
            let mut row = Vec::with_capacity(line.spans.len() + 2);
            if left > 0 {
                row.push(Span::styled(" ".repeat(left), base_style));
            }
            row.extend(line.spans);
            if right > 0 {
                row.push(Span::styled(" ".repeat(right), base_style));
            }
            row
        })
        .collect()
}

/// 单行表格：每个单元格按列宽折行，保留单元格内联样式，行高取该行最多的行数。
pub(super) fn table_row_lines(
    cells: &[Vec<Run>],
    widths: &[usize],
    alignments: &[Alignment],
    gap: usize,
    theme: &Theme,
    header: bool,
) -> Vec<Line<'static>> {
    let base_style = if header {
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text_primary)
    };
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let runs = cells.get(index).map_or(&[][..], Vec::as_slice);
            let align = alignments.get(index).copied().unwrap_or(Alignment::None);
            wrap_cell_runs(runs, (*width).max(1), align, base_style)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let gap_span = Span::styled(" ".repeat(gap), base_style);
    let mut lines = Vec::with_capacity(height);
    for line_index in 0..height {
        let mut spans = Vec::with_capacity(widths.len().saturating_mul(2));
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                spans.push(gap_span.clone());
            }
            if let Some(cell_line) = wrapped[index].get(line_index) {
                spans.extend(cell_line.iter().cloned());
            } else {
                spans.push(Span::styled(" ".repeat(*width), base_style));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// 表头与行之间、行与行之间的细线。
pub(super) fn table_rule(width: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width),
        Style::new().fg(theme.border_subtle),
    ))
}

/// 极窄终端：逐字段列出，保证不丢内容。
pub(super) fn stacked_table_lines(
    table: &TableAcc,
    available: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let available = available.max(1);
    let style = Style::new().fg(theme.text_primary);
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let header: Vec<String> = (0..columns)
        .map(|index| table_cell_text(&table.header, index).trim().to_owned())
        .collect();
    let mut lines = Vec::new();
    if table.rows.is_empty() {
        for line in wrap_text(&header.join("  "), available) {
            lines.push(Line::from(Span::styled(line, style)));
        }
        return lines;
    }
    for (row_index, row) in table.rows.iter().enumerate() {
        for (index, label) in header.iter().enumerate() {
            let value = table_cell_text(row, index);
            let text = match (label.is_empty(), value.is_empty()) {
                (true, _) => value,
                (false, true) => label.clone(),
                (false, false) => format!("{label}: {value}"),
            };
            for line in wrap_text(&text, available) {
                lines.push(Line::from(Span::styled(line, style)));
            }
        }
        if row_index + 1 < table.rows.len() {
            lines.push(table_rule(available, theme));
        }
    }
    lines
}

/// 累积表格 → 纯文本快照。
///
/// 走的是同一份去装饰结果（`**x**`、`` `x` ``、链接文字都已还原成可见文本），
/// 所以「点击入口时做的分析」与「屏幕上看到的表格」永远一致；行内单元格数量
/// 不足时补空串，列数以表头与最宽行为准。
pub(super) fn raw_table(table: &TableAcc) -> RawTable {
    let column_count = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let cell = |runs: &[Run]| -> String {
        runs.iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let row = |cells: &[Vec<Run>]| -> Vec<String> {
        (0..column_count)
            .map(|index| cells.get(index).map_or_else(String::new, |runs| cell(runs)))
            .collect()
    };
    RawTable {
        header: row(&table.header),
        rows: table.rows.iter().map(|cells| row(cells)).collect(),
    }
}

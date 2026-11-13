use super::super::wrap::clip_line;
use super::super::wrap::wrap_text;
use super::BodyEdge;
use super::TextRows;
use ratatui::text::Line;
use ratatui::text::Span;

/// 一行的纯文本（不含样式）。
pub(super) fn plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// 正文行右移 `indent` 列，并截到 `width` 列（超宽以 `…` 收尾）：块正文落位的唯一出口。
///
/// 空行保持为空，块尾留白因此不会变成一串空格。
pub(super) fn hang(line: Line<'static>, indent: usize, width: usize) -> Line<'static> {
    let mut line = clip_line(line, width);
    if indent > 0 && line.width() > 0 {
        line.spans.insert(0, Span::raw(" ".repeat(indent)));
    }
    line
}

/// 结果文本的折行（每源行按宽度折行，`limit` 为展示行上限）。
///
/// [`BodyEdge::Head`] 保留前段、[`BodyEdge::Tail`] 保留后段（bash 的输出
/// 只看最新几行）；渲染与懒渲染估算共用同一条截断规则，两条路径必须
/// 得出同一个行数。
pub(super) fn clamp_rows(result: &str, width: usize, limit: usize, edge: BodyEdge) -> TextRows {
    let width = width.max(8);
    let lines: Vec<&str> = result.lines().collect();
    match edge {
        BodyEdge::Head => {
            let mut rows: Vec<String> = Vec::new();
            let mut shown = 0usize;
            let mut dropped = 0usize;
            for line in &lines {
                if shown >= limit {
                    dropped += 1;
                    continue;
                }
                for row in wrap_text(line, width) {
                    if shown >= limit {
                        break;
                    }
                    shown += 1;
                    rows.push(row);
                }
            }
            TextRows {
                rows,
                shown,
                dropped,
            }
        }
        BodyEdge::Tail => {
            // 所有源行按宽度折行后取末尾若干**显示行**（bash／grep 只看最新输出）。
            let mut all: Vec<String> = Vec::with_capacity(lines.len());
            let mut widths: Vec<usize> = Vec::with_capacity(lines.len());
            for line in &lines {
                let rows = wrap_text(line, width);
                widths.push(rows.len());
                all.extend(rows);
            }
            let show_from = all.len().saturating_sub(limit);
            let mut dropped = 0usize;
            let mut before = 0usize;
            for rows in widths {
                if before >= show_from {
                    break;
                }
                before += rows;
                dropped += 1;
            }
            let shown = all.len().saturating_sub(show_from);
            debug_assert!(shown <= limit, "tail 切片不得超过展示预算");
            TextRows {
                rows: all.into_iter().skip(show_from).collect(),
                shown,
                dropped,
            }
        }
    }
}

/// 文本按源行数截断（不折行）：展示的文本与被略去的源行数。
pub(super) fn head_lines(text: &str, limit: usize) -> (String, usize) {
    let mut shown = String::new();
    let mut dropped = 0usize;
    for (index, line) in text.split('\n').enumerate() {
        if index < limit {
            if index > 0 {
                shown.push('\n');
            }
            shown.push_str(line);
        } else {
            dropped += 1;
        }
    }
    (shown, dropped)
}

use super::super::wrap::str_width;
use super::super::wrap::truncate;
use super::super::wrap::wrap_text;
use super::Grid;
use super::Ink;
use super::SeqEvent;
use super::SeqRow;
use super::Sequence;

pub(super) fn layout_sequence(sequence: &Sequence, width: usize) -> Option<Grid> {
    let count = sequence.actors.len();
    if count == 0 || count > 12 {
        return None;
    }
    let gap = 2;
    let mut col_w = sequence
        .actors
        .iter()
        .map(|label| str_width(label).clamp(3, 24))
        .max()
        .unwrap_or(4);
    while count * col_w + (count - 1) * gap > width && col_w > 3 {
        col_w -= 1;
    }
    let total = count * col_w + (count - 1) * gap;
    if total > width {
        return None;
    }
    let origin = (width - total) / 2;
    let center = |index: usize| origin + index * (col_w + gap) + col_w / 2;
    let labels: Vec<String> = sequence
        .actors
        .iter()
        .map(|label| truncate(label, col_w).into_owned())
        .collect();
    let mut rows: Vec<SeqRow> = vec![SeqRow::Header, SeqRow::Life];
    for event in &sequence.events {
        match event {
            SeqEvent::Message {
                from,
                to,
                text,
                dashed,
            } if from == to => {
                rows.push(SeqRow::SelfTop { actor: *from });
                rows.push(SeqRow::SelfMid {
                    actor: *from,
                    text: text.clone(),
                });
                rows.push(SeqRow::SelfBot { actor: *from });
            }
            SeqEvent::Message {
                from,
                to,
                text,
                dashed,
            } => rows.push(SeqRow::Message {
                from: *from,
                to: *to,
                text: text.clone(),
                dashed: *dashed,
            }),
            SeqEvent::Note(text) => {
                for line in wrap_text(&format!("note: {text}"), width) {
                    rows.push(SeqRow::Text(line));
                }
            }
            SeqEvent::Open(title) => rows.push(SeqRow::Text(format!("┌ {title}"))),
            SeqEvent::Close => rows.push(SeqRow::Text("└".to_string())),
        }
    }
    if rows.len() > 400 {
        return None;
    }
    let mut grid = Grid::new(width, rows.len().max(1));
    for (y, row) in rows.iter().enumerate() {
        match row {
            SeqRow::Header => {
                for (index, label) in labels.iter().enumerate() {
                    let x =
                        origin + index * (col_w + gap) + col_w.saturating_sub(str_width(label)) / 2;
                    grid.put_str(x, y, label, Ink::Text);
                }
            }
            SeqRow::Life => {
                for index in 0..count {
                    grid.put_str(center(index), y, "│", Ink::Border);
                }
            }
            SeqRow::Message {
                from,
                to,
                text,
                dashed,
            } => {
                draw_message(
                    &mut grid,
                    y,
                    center(*from),
                    center(*to),
                    text,
                    *dashed,
                    width,
                );
            }
            SeqRow::SelfTop { actor } => {
                let x = center(*actor);
                grid.put_str(x, y, "│─┐", Ink::Border);
            }
            SeqRow::SelfMid { actor, text } => {
                let x = center(*actor);
                grid.put_str(x, y, "│ │", Ink::Border);
                let label_x = (x + 4).min(width.saturating_sub(1));
                grid.put_str(label_x, y, text, Ink::Text);
            }
            SeqRow::SelfBot { actor } => {
                let x = center(*actor);
                grid.put_str(x, y, "│◀┘", Ink::Arrow);
            }
            SeqRow::Text(text) => grid.put_str(0, y, text, Ink::Text),
        }
    }
    Some(grid)
}

pub(super) fn draw_message(
    grid: &mut Grid,
    y: usize,
    from: usize,
    to: usize,
    text: &str,
    dashed: bool,
    width: usize,
) {
    let (left, right, forward) = if from <= to {
        (from, to, true)
    } else {
        (to, from, false)
    };
    let line = if dashed { "┄" } else { "─" };
    if left == right {
        grid.put_str(left, y, "│", Ink::Border);
        return;
    }
    grid.put_str(left, y, "│", Ink::Border);
    let mut x = left + 1;
    while x < right {
        grid.put_str(x, y, line, Ink::Border);
        x += str_width(line);
    }
    let arrow = if forward { "▶" } else { "◀" };
    if forward {
        grid.put_str(right, y, arrow, Ink::Arrow);
    } else {
        grid.put_str(left, y, arrow, Ink::Arrow);
        grid.put_str(right, y, "│", Ink::Border);
    }
    if !text.is_empty() {
        let label_x = (right + 2).min(width.saturating_sub(1));
        grid.put_str(label_x, y, text, Ink::Text);
    }
}

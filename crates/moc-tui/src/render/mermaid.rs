use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::theme::Theme;

const MAX_NODES: usize = 40;
const MAX_EDGES: usize = 80;
const ROW_GAP: usize = 2;
const NODE_GAP: usize = 2;
const COL_GAP: usize = 3;

const N: u8 = 0b0001;
const E: u8 = 0b0010;
const S: u8 = 0b0100;
const W: u8 = 0b1000;
const ARROW_N: u8 = 0b0001_0000;
const ARROW_E: u8 = 0b0010_0000;
const ARROW_S: u8 = 0b0100_0000;
const ARROW_W: u8 = 0b1000_0000;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Dir {
    Td,
    Bt,
    Lr,
    Rl,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Shape {
    Box,
    Round,
    Diamond,
    Dot,
}

struct Graph {
    dir: Dir,
    nodes: Vec<GNode>,
    edges: Vec<GEdge>,
}

struct GNode {
    label: String,
    shape: Shape,
}

struct GEdge {
    from: usize,
    to: usize,
    label: String,
    /// `x`/`o`/`>` 箭头头或 `<` 反箭头算有向；裸的 `---`/`--`/`-.-` 只画连线。
    directed: bool,
}

enum Diagram {
    Graph(Graph),
    Sequence(Sequence),
}

struct Sequence {
    actors: Vec<String>,
    events: Vec<SeqEvent>,
}

enum SeqEvent {
    Message {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
    },
    Note(String),
    Open(String),
    Close,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ink {
    None,
    Border,
    Text,
    Arrow,
}

struct Cell {
    ch: char,
    ink: Ink,
    skip: bool,
}

struct Grid {
    w: usize,
    h: usize,
    cells: Vec<Cell>,
    mask: Vec<u8>,
}

#[derive(Clone)]
struct Place {
    x: usize,
    y: usize,
    w: usize,
    h: usize,
    lines: Vec<String>,
}

/// 画出支持的图。宽度是正文列数。空图、超限或认不出的图种返回 `None`。
pub fn render(source: &str, width: usize, theme: &Theme) -> Option<Vec<Line<'static>>> {
    let width = width.max(8);
    let grid = match parse(source)? {
        Diagram::Graph(graph) => layout_graph(&graph, width)?,
        Diagram::Sequence(sequence) => layout_sequence(&sequence, width)?,
    };
    let lines = grid.into_lines(theme);
    if lines.is_empty() { None } else { Some(lines) }
}

mod parse;
use parse::parse;
mod graph;
use graph::layout_graph;
mod sequence;
use sequence::layout_sequence;

enum Header {
    Flow(Dir),
    State(Dir),
    Sequence,
}

enum SeqRow {
    Header,
    Life,
    Message {
        from: usize,
        to: usize,
        text: String,
        dashed: bool,
    },
    SelfTop {
        actor: usize,
    },
    SelfMid {
        actor: usize,
        text: String,
    },
    SelfBot {
        actor: usize,
    },
    Text(String),
}

impl Grid {
    fn new(w: usize, h: usize) -> Self {
        let len = w.saturating_mul(h);
        Self {
            w,
            h,
            cells: (0..len)
                .map(|_| Cell {
                    ch: ' ',
                    ink: Ink::None,
                    skip: false,
                })
                .collect(),
            mask: vec![0; len],
        }
    }

    fn put_str(&mut self, x: usize, y: usize, text: &str, ink: Ink) {
        if y >= self.h {
            return;
        }
        let mut col = x;
        for ch in text.chars() {
            let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if width == 0 || col + width > self.w {
                break;
            }
            self.cells[y * self.w + col] = Cell {
                ch,
                ink,
                skip: false,
            };
            for step in 1..width {
                self.cells[y * self.w + col + step] = Cell {
                    ch: ' ',
                    ink,
                    skip: true,
                };
            }
            col += width;
        }
    }

    fn set_ink(&mut self, x: usize, y: usize, ink: Ink) {
        if x < self.w && y < self.h {
            self.cells[y * self.w + x].ink = ink;
        }
    }

    fn vline(&mut self, x: usize, y0: usize, y1: usize) {
        let (start, end) = if y0 <= y1 { (y0, y1) } else { (y1, y0) };
        for y in start..=end {
            let mut bit = 0;
            if y > start {
                bit |= N;
            }
            if y < end {
                bit |= S;
            }
            if start == end {
                bit |= N | S;
            }
            self.mask_at(x, y, bit);
        }
    }

    fn hline(&mut self, x0: usize, x1: usize, y: usize) {
        let (start, end) = if x0 <= x1 { (x0, x1) } else { (x1, x0) };
        for x in start..=end {
            let mut bit = 0;
            if x > start {
                bit |= W;
            }
            if x < end {
                bit |= E;
            }
            if start == end {
                bit |= E | W;
            }
            self.mask_at(x, y, bit);
        }
    }

    fn arrow(&mut self, x: usize, y: usize, bit: u8) {
        self.mask_at(x, y, bit);
    }

    fn mask_at(&mut self, x: usize, y: usize, bit: u8) {
        if x < self.w && y < self.h {
            self.mask[y * self.w + x] |= bit;
        }
    }

    fn into_lines(self, theme: &Theme) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        for y in 0..self.h {
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut buf = String::new();
            let mut ink = Ink::None;
            let flush = |spans: &mut Vec<Span<'static>>, buf: &mut String, ink: Ink| {
                if buf.is_empty() {
                    return;
                }
                spans.push(Span::styled(std::mem::take(buf), ink_style(ink, theme)));
            };
            for x in 0..self.w {
                if self.cells[y * self.w + x].skip {
                    continue;
                }
                let (ch, cell_ink) = self.glyph(x, y);
                if cell_ink != ink && !buf.is_empty() {
                    flush(&mut spans, &mut buf, ink);
                }
                ink = cell_ink;
                buf.push(ch);
            }
            while buf.ends_with(' ') {
                buf.pop();
            }
            flush(&mut spans, &mut buf, ink);
            if spans.is_empty() {
                lines.push(Line::from(""));
            } else {
                lines.push(Line::from(spans));
            }
        }
        while lines.last().is_some_and(|line| {
            line.spans
                .iter()
                .all(|span| span.content.chars().all(|ch| ch == ' '))
        }) {
            lines.pop();
        }
        lines
    }

    fn glyph(&self, x: usize, y: usize) -> (char, Ink) {
        let cell = &self.cells[y * self.w + x];
        if cell.ch != ' ' {
            return (cell.ch, cell.ink);
        }
        let mask = self.mask[y * self.w + x];
        if mask & ARROW_S != 0 {
            return ('▼', Ink::Arrow);
        }
        if mask & ARROW_N != 0 {
            return ('▲', Ink::Arrow);
        }
        if mask & ARROW_E != 0 {
            return ('▶', Ink::Arrow);
        }
        if mask & ARROW_W != 0 {
            return ('◀', Ink::Arrow);
        }
        let ch = match mask & 0b1111 {
            0 => ' ',
            bits if bits == N || bits == S || bits == (N | S) => '│',
            bits if bits == E || bits == W || bits == (E | W) => '─',
            bits if bits == (S | E) => '┌',
            bits if bits == (S | W) => '┐',
            bits if bits == (N | E) => '└',
            bits if bits == (N | W) => '┘',
            bits if bits == (N | S | E) => '├',
            bits if bits == (N | S | W) => '┤',
            bits if bits == (E | W | S) => '┬',
            bits if bits == (E | W | N) => '┴',
            bits if bits == (N | E | W | S) => '┼',
            _ => '·',
        };
        (ch, if ch == ' ' { Ink::None } else { Ink::Border })
    }
}

fn ink_style(ink: Ink, theme: &Theme) -> Style {
    match ink {
        Ink::Border => Style::new().fg(theme.border),
        Ink::Text => Style::new().fg(theme.text_primary),
        Ink::Arrow => Style::new().fg(theme.gray),
        Ink::None => Style::new(),
    }
}

#[cfg(test)]
mod tests;

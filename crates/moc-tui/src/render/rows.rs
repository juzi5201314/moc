//! 大段排版按页压缩；只解码视口附近的页，保留准确行号与全部样式。
use ratatui::{
    layout::Alignment,
    style::Style,
    text::{Line, Span},
};
use serde::{Deserialize, Serialize};
use std::ops::Range;

const PAGE_ROWS: usize = 128;

#[derive(Serialize, Deserialize)]
struct StoredLine {
    style: Style,
    alignment: Option<Alignment>,
    spans: Vec<(String, Style)>,
}

/// 小正文直接用 Vec；大正文仅保留一个未封口页和压缩页。
/// 页下标由行号除以固定页长得到，不需要哈希查找。
pub struct RowBuffer {
    paged: bool,
    pages: Vec<Box<[u8]>>,
    tail: Vec<Line<'static>>,
    count: usize,
    inserts: Vec<(usize, Line<'static>)>,
}

impl RowBuffer {
    pub fn new(paged: bool) -> Self {
        Self {
            paged,
            pages: Vec::new(),
            tail: Vec::new(),
            count: 0,
            inserts: Vec::new(),
        }
    }
    pub fn len(&self) -> usize {
        self.count + self.inserts.len()
    }
    pub fn last(&self) -> Option<&Line<'static>> {
        self.tail.last()
    }
    pub fn push(&mut self, line: Line<'static>) {
        debug_assert!(self.inserts.is_empty(), "入口插入只发生在正文落定后");
        if self.paged && self.tail.len() == PAGE_ROWS {
            self.seal();
        }
        self.tail.push(line);
        self.count += 1;
    }
    pub fn extend(&mut self, lines: impl IntoIterator<Item = Line<'static>>) {
        for line in lines {
            self.push(line);
        }
    }
    fn seal(&mut self) {
        let rows: Vec<_> = std::mem::take(&mut self.tail)
            .into_iter()
            .map(|line| StoredLine {
                style: line.style,
                alignment: line.alignment,
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| (span.content.into_owned(), span.style))
                    .collect(),
            })
            .collect();
        let bytes = serde_json::to_vec(&rows).expect("排版行可序列化");
        self.pages.push(
            zstd::bulk::compress(&bytes, 1)
                .expect("排版页可压缩")
                .into_boxed_slice(),
        );
    }
    pub fn insert(&mut self, row: usize, line: Line<'static>) {
        debug_assert!(row <= self.len());
        debug_assert!(
            self.inserts
                .last()
                .is_none_or(|(previous, _)| *previous < row)
        );
        self.inserts.push((row, line));
    }
    fn page(&self, index: usize) -> Vec<Line<'static>> {
        if index == self.pages.len() {
            return self.tail.clone();
        }
        let bytes = zstd::stream::decode_all(self.pages[index].as_ref()).expect("排版页可解压");
        let rows: Vec<StoredLine> = serde_json::from_slice(&bytes).expect("内部排版页格式有效");
        rows.into_iter()
            .map(|row| Line {
                spans: row
                    .spans
                    .into_iter()
                    .map(|(text, style)| Span::styled(text, style))
                    .collect(),
                style: row.style,
                alignment: row.alignment,
            })
            .collect()
    }
    pub fn window(&self, range: Range<usize>) -> Vec<Line<'static>> {
        let mut out = Vec::with_capacity(range.len().min(self.len()));
        let mut insert = self.inserts.partition_point(|(row, _)| *row < range.start);
        let mut page_index = usize::MAX;
        let mut page = Vec::new();
        for row in range.start..range.end.min(self.len()) {
            if self.inserts.get(insert).is_some_and(|(at, _)| *at == row) {
                out.push(self.inserts[insert].1.clone());
                insert += 1;
                continue;
            }
            let original = row - insert;
            if !self.paged {
                out.push(self.tail[original].clone());
            } else {
                let index = original / PAGE_ROWS;
                if index != page_index {
                    page = self.page(index);
                    page_index = index;
                }
                out.push(page[original % PAGE_ROWS].clone());
            }
        }
        out
    }
    pub fn into_vec(mut self) -> Vec<Line<'static>> {
        if !self.paged && self.inserts.is_empty() {
            return self.tail;
        }
        if !self.paged {
            for (row, line) in self.inserts {
                self.tail.insert(row, line);
            }
            return self.tail;
        }
        self.window(0..self.len())
    }
    pub fn stored_bytes(&self) -> usize {
        self.pages.iter().map(|page| page.len()).sum::<usize>()
            + (self.tail.len() + self.inserts.len()) * std::mem::size_of::<Line<'static>>()
            + self
                .tail
                .iter()
                .chain(self.inserts.iter().map(|(_, line)| line))
                .map(|line| {
                    line.spans.capacity() * std::mem::size_of::<Span<'static>>()
                        + line
                            .spans
                            .iter()
                            .map(|span| span.content.len())
                            .sum::<usize>()
                })
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn pages_and_insertions_preserve_every_line_and_style() {
        let mut paged = RowBuffer::new(true);
        let mut expected = Vec::new();
        for index in 0..1000 {
            let line = Line::from(vec![Span::styled(
                format!("行 {index} 中英 mixed"),
                Style::new().fg(ratatui::style::Color::Rgb(1, 2, 3)),
            )]);
            paged.push(line.clone());
            expected.push(line);
        }
        for row in [0, 127, 128, 257, 1004] {
            let line = Line::from("插入入口");
            paged.insert(row, line.clone());
            expected.insert(row, line);
        }
        for start in (0..expected.len()).step_by(31) {
            let end = (start + 97).min(expected.len());
            assert_eq!(paged.window(start..end), expected[start..end]);
        }
        assert!(paged.stored_bytes() < 100_000);
        assert_eq!(paged.into_vec(), expected);
    }
}

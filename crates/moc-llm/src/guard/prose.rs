//! 有界中英文段落指纹；代码与结构化内容不进入相似度窗口。

use std::collections::VecDeque;
use xxhash_rust::xxh3::xxh3_64;

use super::{SEGMENT_CHARS, SEGMENT_WINDOW};
#[derive(Default)]
pub(super) struct Prose {
    line: String,
    segment: String,
    segment_chars: usize,
    pub(super) fenced: bool,
    window: VecDeque<Segment>,
    count: usize,
    stale_run: usize,
    detect_stale: bool,
}

struct Segment {
    words: Vec<u64>,
    shingles: Vec<u64>,
    anchors: Vec<u64>,
}

impl Prose {
    pub(super) fn new(detect_stale: bool) -> Self {
        Self {
            detect_stale,
            ..Default::default()
        }
    }

    pub(super) fn push(&mut self, ch: char) -> Option<String> {
        if ch == '\n' {
            return self.finish_line();
        }
        self.line.push(ch);
        if self.line.len() >= SEGMENT_CHARS * 4 {
            return self.finish_line();
        }
        None
    }

    fn finish_line(&mut self) -> Option<String> {
        let line = self.line.trim();
        if line.starts_with("```") || line.starts_with("~~~") {
            self.fenced = !self.fenced;
        } else if line.is_empty() {
            self.line.clear();
            return self.consume();
        } else if !self.fenced && !code_line(&self.line) {
            self.segment.push_str(line);
            self.segment.push(' ');
            self.segment_chars += line.chars().count();
        } else {
            self.segment.clear();
            self.segment_chars = 0;
            self.window.clear();
            self.stale_run = 0;
        }
        self.line.clear();
        if self.segment_chars >= SEGMENT_CHARS {
            return self.consume();
        }
        None
    }

    pub(super) fn flush(&mut self) -> Option<String> {
        self.finish_line().or_else(|| self.consume())
    }

    fn consume(&mut self) -> Option<String> {
        if self.segment_chars < 120 {
            self.segment.clear();
            self.segment_chars = 0;
            return None;
        }
        let current = fingerprint(&self.segment);
        self.segment.clear();
        self.segment_chars = 0;
        if current.words.len() < 12 {
            return None;
        }
        let new_anchor = current.anchors.iter().any(|anchor| {
            self.window
                .iter()
                .all(|old| old.anchors.binary_search(anchor).is_err())
        });
        let cluster = 1 + self
            .window
            .iter()
            .filter(|old| similar(&current.shingles, &old.shingles))
            .count();
        let unseen = current
            .words
            .iter()
            .filter(|word| {
                self.window
                    .iter()
                    .all(|old| old.words.binary_search(word).is_err())
            })
            .count();
        let stale = !new_anchor && unseen * 100 <= current.words.len() * 8;
        self.stale_run = if stale { self.stale_run + 1 } else { 0 };
        if self.window.len() == SEGMENT_WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(current);
        debug_assert!(self.window.len() <= SEGMENT_WINDOW);
        self.count += 1;
        if self.count >= 10 && !new_anchor && cluster >= 6 {
            return Some(format!(
                "近期段落中出现 {cluster} 段高度相似且没有新引用的内容"
            ));
        }
        if self.detect_stale && self.stale_run >= 12 {
            return Some("连续 12 段复用已有词汇，未增加具体信息".into());
        }
        None
    }
}

fn code_line(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('#')
        || trimmed.starts_with("**")
        || trimmed.starts_with('|')
        || line.starts_with("    ")
        || line.starts_with('\t')
        || trimmed.contains(['=', '{', '}', '[', ']', '<', '>'])
        || trimmed.ends_with([';', ':'])
}

fn cjk(ch: char) -> bool {
    matches!(ch as u32, 0x3400..=0x9fff | 0xf900..=0xfaff | 0x20000..=0x3134f | 0x3040..=0x30ff | 0xac00..=0xd7af)
}

fn fingerprint(text: &str) -> Segment {
    let lower = text.to_lowercase();
    let mut tokens = Vec::new();
    let mut word_start = None;
    for (offset, ch) in lower.char_indices() {
        if !ch.is_alphanumeric() || cjk(ch) {
            if let Some(start) = word_start.take() {
                add_word(&lower[start..offset], &mut tokens);
            }
            if cjk(ch) {
                tokens.push(xxh3_64(&lower.as_bytes()[offset..offset + ch.len_utf8()]));
            }
        } else {
            word_start.get_or_insert(offset);
        }
    }
    if let Some(start) = word_start {
        add_word(&lower[start..], &mut tokens);
    }
    let mut shingles = Vec::with_capacity(tokens.len());
    for words in tokens.windows(3) {
        let mut bytes = [0; 24];
        for (i, word) in words.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        shingles.push(xxh3_64(&bytes));
    }
    let mut anchors: Vec<_> = text
        .split_whitespace()
        .filter(|word| {
            word.contains(['`', '/', '_'])
                || (word.contains('.') && word.chars().any(char::is_alphabetic))
                || word.chars().skip(1).any(char::is_uppercase)
        })
        .map(|word| {
            xxh3_64(
                word.trim_matches(|ch: char| !ch.is_alphanumeric() && !"/_.".contains(ch))
                    .to_lowercase()
                    .as_bytes(),
            )
        })
        .collect();
    tokens.sort_unstable();
    tokens.dedup();
    shingles.sort_unstable();
    shingles.dedup();
    anchors.sort_unstable();
    anchors.dedup();
    Segment {
        words: tokens,
        shingles,
        anchors,
    }
}

fn add_word(word: &str, tokens: &mut Vec<u64>) {
    if word.chars().any(char::is_alphabetic) {
        tokens.push(xxh3_64(word.as_bytes()));
    }
}

fn similar(left: &[u64], right: &[u64]) -> bool {
    let (mut i, mut j, mut intersection) = (0, 0, 0);
    while i < left.len() && j < right.len() {
        match left[i].cmp(&right[j]) {
            std::cmp::Ordering::Less => i += 1,
            std::cmp::Ordering::Greater => j += 1,
            std::cmp::Ordering::Equal => {
                intersection += 1;
                i += 1;
                j += 1;
            }
        }
    }
    let union = left.len() + right.len() - intersection;
    union > 0 && intersection * 100 >= union * 92
}

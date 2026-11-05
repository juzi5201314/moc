//! 显示宽度感知的排版原语：折行与截断的唯一实现。
//!
//! 终端行宽按「显示列数」计算（CJK 宽字符占 2 列），`unicode-width` 提供测量。
//! 渲染层遇到「放不下」只有两种合法处理，全部走这里：
//!
//! - **折行**（正文）：[`wrap_text`] / [`wrap_spans`]。断点优先级：空格与 CJK
//!   字间 > `/ - _ . , : ; ) ] }` 之后 > 逐字硬切；断点前后遵守 CJK 避头尾
//!   （`，。、）」` 等不落行首，`（「【` 等不落行尾）。续行行首空格丢弃、行尾空格裁掉。
//! - **截断**（单行标题、摘要、列表项）：[`truncate`] / [`truncate_head`] /
//!   [`clip_line`]，超宽一律以 [`ELLIPSIS`] 收尾；要固定右缘就用 [`fit`] / [`fit_line`]
//!   补齐到整宽。
//!
//! 渲染层不得把超宽内容交给 ratatui 静默裁掉：那样既没有省略号，也会让
//! `Paragraph::wrap` 二次折行、命中几何错位。

use std::borrow::Cow;

use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

/// 行内截断的省略号（占 1 列）。
pub const ELLIPSIS: char = '…';

/// 行首禁则：这些标点不得出现在续行行首（避头）。
fn no_line_start(ch: char) -> bool {
    matches!(
        ch,
        '，' | '。'
            | '、'
            | '；'
            | '：'
            | '！'
            | '？'
            | '）'
            | '】'
            | '」'
            | '』'
            | '》'
            | '〉'
            | '”'
            | '’'
            | '…'
            | '·'
            | '～'
            | ','
            | '.'
            | ';'
            | ':'
            | '!'
            | '?'
            | ')'
            | ']'
            | '}'
            | '%'
    )
}

/// 行尾禁则：这些开括号/引号不得留在行尾（避尾）。
fn no_line_end(ch: char) -> bool {
    matches!(
        ch,
        '（' | '【' | '「' | '『' | '《' | '〈' | '“' | '‘' | '(' | '[' | '{'
    )
}

/// 次级断点：长路径、标识符、URL 只能在这些字符之后断开。
fn soft_break_after(ch: char) -> bool {
    matches!(
        ch,
        '/' | '\\' | '-' | '_' | '.' | ',' | ':' | ';' | ')' | ']' | '}' | '&' | '|' | '='
    )
}

fn char_width(ch: char) -> usize {
    ch.width().unwrap_or(0)
}

/// `chars[i]` 之前能否断行（不含空格断点，空格由调用方处理）。
fn can_break_before(chars: &[char], i: usize) -> Option<bool> {
    let (prev, next) = (chars[i - 1], chars[i]);
    if prev == ' ' || next == ' ' || no_line_start(next) || no_line_end(prev) {
        return None;
    }
    if char_width(prev) > 1 || char_width(next) > 1 {
        return Some(true); // CJK 字间：一级断点
    }
    soft_break_after(prev).then_some(false) // 标点之后：次级断点
}

/// 把一段（不含换行符）按宽度切成行区间 `(start, end)`，区间已裁掉首尾空格。
///
/// 贪心：每行尽量装满，溢出时回退到最近的一级断点，没有再用次级断点，
/// 都没有才逐字硬切。硬切也不让避头字符落到行首（连同前一个字一起换行）。
fn break_rows(chars: &[char], width: usize) -> Vec<(usize, usize)> {
    let mut rows = Vec::new();
    // 段首缩进属于正文，保留；只有续行才丢行首空格
    let mut start = 0usize;
    let mut used = 0usize;
    // 断点：(本行结束位置, 下一行开始位置)
    let mut primary: Option<(usize, usize)> = None;
    let mut secondary: Option<(usize, usize)> = None;
    let mut i = 0usize;
    let mut last_content = None;
    while i < chars.len() {
        let ch = chars[i];
        let w = char_width(ch);
        if i > 0 && chars[i - 1] != ' ' { last_content = Some(i - 1); }
        // i 单调前进；最后一个非空格的位置足以判断本行是否已有内容。
        if i > start && last_content.is_some_and(|index| index >= start) {
            if ch == ' ' && chars[i - 1] != ' ' {
                // 空格串：本行在空格前结束，下一行从空格串之后开始
                let mut resume = i;
                while resume < chars.len() && chars[resume] == ' ' {
                    resume += 1;
                }
                let blocked = no_line_end(chars[i - 1])
                    || chars.get(resume).copied().is_some_and(no_line_start);
                if !blocked {
                    primary = Some((i, resume));
                }
            } else if let Some(is_primary) = can_break_before(chars, i) {
                let slot = if is_primary {
                    &mut primary
                } else {
                    &mut secondary
                };
                *slot = Some((i, i));
            }
        }
        if used + w <= width || ch == ' ' {
            // 空格永不触发溢出：它要么成为断点被吞掉，要么在行尾被裁掉
            used += w;
            i += 1;
            continue;
        }
        let (end, resume) = match primary.or(secondary) {
            Some(point) => point,
            None => hard_cut(chars, start, i),
        };
        rows.push(trim_row(chars, start, end));
        start = resume;
        while start < chars.len() && chars[start] == ' ' {
            start += 1;
        }
        used = chars[start..i.max(start)]
            .iter()
            .map(|c| char_width(*c))
            .sum();
        primary = None;
        secondary = None;
        i = i.max(start);
    }
    rows.push(trim_row(chars, start, chars.len()));
    rows
}

/// 没有任何断点时在 `i` 之前硬切；避头字符带上前一个字一起换行。
fn hard_cut(chars: &[char], start: usize, i: usize) -> (usize, usize) {
    if i == start {
        // 单个字比整行还宽：独占一行，保证前进
        return (i + 1, i + 1);
    }
    if (no_line_start(chars[i]) || no_line_end(chars[i - 1])) && i - 1 > start {
        return (i - 1, i - 1);
    }
    (i, i)
}

/// 裁掉区间首尾空格（首部只裁续行；段首缩进在 `start == 0` 时保留）。
fn trim_row(chars: &[char], start: usize, end: usize) -> (usize, usize) {
    let mut end = end.max(start);
    while end > start && chars[end - 1] == ' ' {
        end -= 1;
    }
    (start, end)
}

/// 按换行符切段：末尾换行不多造一行，与 `str::lines` 口径一致。
fn segments(text: &str) -> impl Iterator<Item = &str> {
    let trimmed = text.strip_suffix('\n').unwrap_or(text);
    trimmed.split('\n')
}

/// 把文本按显示宽度折成多行；换行符是硬断行，空行占一行。
///
/// `width == 0` 视为不限宽（每段原样一行）。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for segment in segments(text) {
        let chars: Vec<char> = segment.chars().collect();
        if width == 0 {
            lines.push(segment.to_owned());
            continue;
        }
        for (start, end) in break_rows(&chars, width) {
            lines.push(chars[start..end].iter().collect());
        }
    }
    lines
}

/// 带样式的折行：断点规则与 [`wrap_text`] 相同，样式逐字跟随原 span。
///
/// span 内的换行符同样是硬断行。返回的每行都不超过 `width` 列（单字比
/// `width` 还宽时除外）；调用方负责在每行前拼前缀（列表悬挂、引用竖线）。
pub fn wrap_spans(spans: &[Span<'_>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    wrap_spans_into(spans, width, |line| out.push(line));
    out
}

pub(super) fn wrap_spans_into(
    spans: &[Span<'_>],
    width: usize,
    mut emit: impl FnMut(Line<'static>),
) {
    let mut chars: Vec<char> = Vec::new();
    // 下标是段内字符位置，严格递增；样式只在变化时记录一次。
    let mut styles: Vec<(usize, Style)> = Vec::new();
    let mut flush = |chars: &mut Vec<char>, styles: &mut Vec<(usize, Style)>| {
        let rows = if width == 0 {
            vec![(0, chars.len())]
        } else {
            break_rows(chars, width)
        };
        for (start, end) in rows {
            emit(styled_row(chars, styles, start, end));
        }
        chars.clear();
        styles.clear();
    };
    let last = spans.len().saturating_sub(1);
    for (index, span) in spans.iter().enumerate() {
        let content = span.content.as_ref();
        let content = if index == last {
            content.strip_suffix('\n').unwrap_or(content)
        } else {
            content
        };
        for ch in content.chars() {
            if ch == '\n' {
                flush(&mut chars, &mut styles);
            } else {
                if styles.last().is_none_or(|(_, style)| *style != span.style) {
                    styles.push((chars.len(), span.style));
                }
                chars.push(ch);
            }
        }
    }
    flush(&mut chars, &mut styles);
}

/// 一行字符按样式合并回 span。
fn styled_row(chars: &[char], styles: &[(usize, Style)], start: usize, end: usize) -> Line<'static> {
    let mut spans = Vec::new();
    if start == end { return Line::from(spans); }
    debug_assert_eq!(styles.first().map(|(offset, _)| *offset), Some(0));
    let mut index = styles.partition_point(|(offset, _)| *offset <= start) - 1;
    let mut cursor = start;
    while cursor < end {
        let next = styles.get(index + 1).map_or(end, |(offset, _)| end.min(*offset));
        spans.push(Span::styled(chars[cursor..next].iter().collect::<String>(), styles[index].1));
        cursor = next;
        index += 1;
    }
    Line::from(spans)
}

/// 字符串显示宽度。
pub fn str_width(text: &str) -> usize {
    text.chars().map(char_width).sum()
}

/// 一行的显示宽度（各 span 之和）。
pub fn line_width(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| str_width(span.content.as_ref()))
        .sum()
}

/// 截断到 `width` 列，超宽时保留开头并以 [`ELLIPSIS`] 收尾。
///
/// 结果宽度 ≤ `width`；宽字符不切开，省略号前的空格裁掉（`foo …` → `foo…`）。
/// 放得下就原样返回，不分配。
pub fn truncate<'a>(text: impl Into<Cow<'a, str>>, width: usize) -> Cow<'a, str> {
    let text = text.into();
    if str_width(&text) <= width {
        return text;
    }
    if width == 0 {
        return Cow::Borrowed("");
    }
    let budget = width - 1;
    let mut used = 0usize;
    let mut end = 0usize;
    for (index, ch) in text.char_indices() {
        let w = char_width(ch);
        if used + w > budget {
            break;
        }
        used += w;
        end = index + ch.len_utf8();
    }
    let mut out = String::with_capacity(end + ELLIPSIS.len_utf8());
    out.push_str(text[..end].trim_end());
    out.push(ELLIPSIS);
    Cow::Owned(out)
}

/// 截断到 `width` 列，超宽时保留结尾、以 [`ELLIPSIS`] 开头。
///
/// 用于输入框、搜索串：光标在结尾，越靠后的字符越是正在输入的内容。
pub fn truncate_head<'a>(text: impl Into<Cow<'a, str>>, width: usize) -> Cow<'a, str> {
    let text = text.into();
    if str_width(&text) <= width {
        return text;
    }
    if width == 0 {
        return Cow::Borrowed("");
    }
    let budget = width - 1;
    let mut used = 0usize;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let w = char_width(ch);
        if used + w > budget {
            break;
        }
        used += w;
        start = index;
    }
    let mut out = String::with_capacity(text.len() - start + ELLIPSIS.len_utf8());
    out.push(ELLIPSIS);
    out.push_str(text[start..].trim_start());
    Cow::Owned(out)
}

/// 截断或右补空格到**恰好** `width` 列：列表项、并排栏、高亮带右缘固定。
pub fn fit(text: &str, width: usize) -> String {
    let shown = truncate(text, width);
    let pad = width - str_width(&shown);
    let mut out = String::with_capacity(shown.len() + pad);
    out.push_str(&shown);
    out.extend(std::iter::repeat_n(' ', pad));
    out
}

/// 截断一行到 `width` 列，超宽时以 [`ELLIPSIS`] 收尾（沿用被截处的样式）。
pub fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line_width(&line) <= width {
        return line;
    }
    let style = line.style;
    if width == 0 {
        return Line::default().style(style);
    }
    let budget = width - 1;
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
    let mut used = 0usize;
    let mut tail_style = Style::new();
    for span in line.spans {
        let mut kept = String::new();
        let mut whole = true;
        for ch in span.content.chars() {
            let w = char_width(ch);
            if used + w > budget {
                whole = false;
                break;
            }
            kept.push(ch);
            used += w;
        }
        tail_style = span.style;
        if !kept.is_empty() {
            spans.push(Span::styled(kept, span.style));
        }
        if !whole {
            break;
        }
    }
    if let Some(last) = spans.last_mut() {
        let trimmed = last.content.trim_end().to_owned();
        last.content = trimmed.into();
    }
    spans.push(Span::styled(ELLIPSIS.to_string(), tail_style));
    Line::from(spans).style(style)
}

/// 截断或右补空格到**恰好** `width` 列（行版 [`fit`]）。
pub fn fit_line(line: Line<'static>, width: usize) -> Line<'static> {
    let line = clip_line(line, width);
    let used = line_width(&line);
    if used == width {
        return line;
    }
    let style = line.style;
    let mut spans = line.spans;
    spans.push(Span::raw(" ".repeat(width - used)));
    Line::from(spans).style(style)
}

/// 从第 `start_chars` 个字符起，按显示宽度最多取 `max_width` 列。
///
/// 不切开宽字符：边界上的宽字符整体跳过，结果可能比 `max_width` 少一列。
pub(crate) fn display_window(text: &str, start_chars: usize, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut width = 0usize;
    for ch in text.chars().skip(start_chars) {
        let ch_width = char_width(ch);
        if width + ch_width > max_width {
            break;
        }
        width += ch_width;
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use ratatui::style::{Color, Style};
    use ratatui::text::{Line, Span};

    use super::{
        clip_line, display_window, fit, fit_line, line_width, str_width, truncate, truncate_head,
        wrap_spans, wrap_text,
    };

    #[test]
    fn display_window_is_display_width_aware() {
        assert_eq!(display_window("abcdef", 0, 3), "abc");
        assert_eq!(display_window("abcdef", 3, 3), "def");
        assert_eq!(display_window("中文abc", 0, 3), "中");
        assert_eq!(display_window("中文abc", 1, 3), "文a");
        assert_eq!(display_window("中文abc", 2, 3), "abc");
        assert_eq!(display_window("中文", 0, 1), "", "放不下的宽字符整体跳过");
        assert_eq!(display_window("abc", 0, 0), "");
        assert_eq!(display_window("abc", 9, 3), "");
        for start in 0..6 {
            let window = display_window("中文ab🙂cd", start, 5);
            assert!(str_width(&window) <= 5);
        }
    }

    /// 断点处的空格被吞掉：续行既不以空格开头，纯空格也不成行。
    #[test]
    fn break_spaces_never_lead_or_trail_rows() {
        assert_eq!(wrap_text("a    b", 3), vec!["a", "b"]);
        assert_eq!(wrap_text("aa    bb", 4), vec!["aa", "bb"]);
        assert_eq!(wrap_text("   ", 5), vec![""], "全空格仍保留一行占位");
        assert_eq!(wrap_text("", 5), vec![""]);
        assert_eq!(wrap_text("  ab cd", 5), vec!["  ab", "cd"], "段首缩进保留");
    }

    /// 英文按单词断开，不在单词中间硬切。
    #[test]
    fn words_break_at_spaces_not_mid_word() {
        assert_eq!(
            wrap_text("the quick brown fox", 10),
            vec!["the quick", "brown fox"]
        );
        assert_eq!(wrap_text("skipping rows", 10), vec!["skipping", "rows"]);
    }

    /// 无空格的长标识符/路径优先在 `/ _ -` 之后断，没有才硬切。
    #[test]
    fn long_tokens_prefer_punctuation_breaks_then_hard_cut() {
        assert_eq!(
            wrap_text("crates/moc-tui/src/render", 15),
            vec!["crates/moc-tui/", "src/render"]
        );
        assert_eq!(
            wrap_text("init_prefers_list", 10),
            vec!["init_", "prefers_", "list"]
        );
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    /// 一级断点（空格）优先于次级断点（标点），即使后者能装得更满。
    #[test]
    fn spaces_win_over_punctuation_breaks() {
        assert_eq!(wrap_text("see a/b/c/d", 9), vec!["see", "a/b/c/d"]);
    }

    /// CJK 字间可断，但避头标点不落行首、开括号不落行尾。
    #[test]
    fn cjk_breaks_respect_kinsoku() {
        assert_eq!(wrap_text("中文中文", 4), vec!["中文", "中文"]);
        let rows = wrap_text("一二三，四五", 6);
        assert!(
            rows.iter().all(|row| !row.starts_with('，')),
            "逗号不得落在行首：{rows:?}"
        );
        assert!(rows.iter().all(|row| str_width(row) <= 6));
        let rows = wrap_text("一二（三四", 6);
        assert!(
            rows.iter().all(|row| !row.ends_with('（')),
            "开括号不得留在行尾：{rows:?}"
        );
    }

    #[test]
    fn rows_never_exceed_width() {
        let text =
            "中文 mixed_with/paths-and，标点。还有（括号）以及 verylongidentifierwithoutbreaks";
        for width in 1..30 {
            for row in wrap_text(text, width) {
                assert!(str_width(&row) <= width.max(2), "宽 {width} 溢出：{row:?}");
            }
        }
    }

    /// 换行符是硬断行：每段独立折行，空行占一行，末尾换行不多造一行。
    #[test]
    fn newlines_break_rows_without_collapsing() {
        assert_eq!(wrap_text("第一行\n第二行", 20), vec!["第一行", "第二行"]);
        assert_eq!(wrap_text("a\n\nb", 20), vec!["a", "", "b"]);
        assert_eq!(
            wrap_text("a\n", 20),
            vec!["a"],
            "末尾换行与 str::lines 同口径"
        );
        assert_eq!(wrap_text("\n", 20), vec![""]);
        assert_eq!(
            wrap_text("abcdef\nghijkl", 4),
            vec!["abcd", "ef", "ghij", "kl"]
        );
        assert_eq!(wrap_text("  ab\ncd", 4), vec!["  ab", "cd"]);
    }

    /// 带样式折行：断点与纯文本一致，样式逐字跟随。
    #[test]
    fn wrap_spans_keeps_styles_across_breaks() {
        let red = Style::new().fg(Color::Red);
        let blue = Style::new().fg(Color::Blue);
        let spans = [
            Span::styled("hello ", red),
            Span::styled("world wide", blue),
        ];
        let rows = wrap_spans(&spans, 11);
        let text: Vec<String> = rows.iter().map(|line| line.to_string()).collect();
        assert_eq!(text, vec!["hello world", "wide"]);
        assert_eq!(rows[0].spans[0].style, red);
        assert_eq!(rows[0].spans[1].style, blue);
        assert_eq!(rows[1].spans[0].style, blue);
        assert_eq!(
            wrap_spans(&[Span::raw("a\nb")], 10).len(),
            2,
            "span 内换行是硬断行"
        );
    }

    #[test]
    fn truncate_ends_with_ellipsis_within_width() {
        assert_eq!(truncate("abc", 3), "abc");
        assert_eq!(truncate("abcdef", 4), "abc…");
        assert_eq!(truncate("中文字", 4), "中…", "宽字符不切开");
        assert_eq!(truncate("ab cdef", 4), "ab…", "省略号前不留空格");
        assert_eq!(truncate("abc", 1), "…");
        assert_eq!(truncate("abc", 0), "");
        assert_eq!(truncate_head("abcdef", 4), "…def");
        assert_eq!(truncate_head("中文字", 4), "…字");
    }

    /// `fit` / `fit_line` 的结果恰好占满宽度：右缘固定，不随宽字符跳动。
    #[test]
    fn fit_pins_right_edge() {
        for text in ["中文中文中文", "abcdefg", "a中b", ""] {
            for width in 0..9 {
                assert_eq!(str_width(&fit(text, width)), width, "{text:?} @ {width}");
                let line = fit_line(Line::from(text.to_owned()), width);
                assert_eq!(line_width(&line), width, "{text:?} @ {width}");
            }
        }
    }

    #[test]
    fn clip_line_keeps_span_styles_and_marks_cut() {
        let red = Style::new().fg(Color::Red);
        let line = Line::from(vec![Span::raw("ab"), Span::styled("cdef", red)]);
        let clipped = clip_line(line, 4);
        assert_eq!(clipped.to_string(), "abc…");
        assert_eq!(clipped.spans.last().map(|span| span.style), Some(red));
    }
}

//! 显示宽度感知的换行。
//!
//! 终端行宽按「显示列数」计算（CJK 宽字符占 2 列），`unicode-width`
//! 提供测量；换行在空格断点优先，超长单词硬切。

use unicode_width::UnicodeWidthChar;

/// 把文本按显示宽度折成多行；换行符是硬断行，不参与折行。
///
/// 空格断点优先：断行时行尾空格被丢弃、续行行首空格被吞掉（与终端
/// `Wrap` 语义一致）；单个 token 超过宽度时按字符硬切。硬断行按
/// `str::lines` 口径处理：末尾换行不多造一行，换行之间的空行占一行。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut segments: Vec<&str> = text.split('\n').collect();
    if segments.len() > 1 && segments.last().is_some_and(|segment| segment.is_empty()) {
        segments.pop();
    }
    for segment in segments {
        wrap_segment(segment, width, &mut lines);
    }
    lines
}

/// 折一段（不含换行符）并追加到 `lines`；空段保留成空行，硬断行由调用方切段。
fn wrap_segment(text: &str, width: usize, lines: &mut Vec<String>) {
    let start = lines.len();
    let mut current = String::new();
    let mut current_width = 0usize;

    for token in tokenize(text) {
        // 断行后的行首空格不保留（段首缩进属于正文，保留）
        if token.chars().all(|ch| ch == ' ') && current.is_empty() && lines.len() > start {
            continue;
        }
        let token_width = str_width(&token);
        if current_width + token_width <= width {
            current.push_str(&token);
            current_width += token_width;
            continue;
        }
        // token 放不下：先结算当前行（行尾空格不保留）
        if !current.is_empty() {
            let finished = std::mem::take(&mut current);
            push_row(lines, start, &finished);
            current_width = 0;
        }
        if token_width <= width {
            if token.chars().all(|ch| ch == ' ') {
                continue; // 断行位置的空格已被消耗
            }
            current.push_str(&token);
            current_width = token_width;
        } else {
            // 硬切超长 token：按字符宽度逐个装箱
            for ch in token.chars() {
                let ch_width = ch.width().unwrap_or(0);
                if current_width + ch_width > width {
                    let finished = std::mem::take(&mut current);
                    push_row(lines, start, &finished);
                    current_width = 0;
                }
                current.push(ch);
                current_width += ch_width;
            }
        }
    }
    if current.trim_end().is_empty() && lines.len() == start {
        lines.push(String::new()); // 空段占一行，行数才与估高一致
    } else {
        push_row(lines, start, &current);
    }
}

/// 结算一行：丢弃行尾空格；纯空格行只有「整段还没有任何行」时才入列。
///
/// 断行位置的空格已被消耗，把它当成一行只会凭空多出一行（调用方按行数估高、
/// 排版），因此不再入列。
fn push_row(lines: &mut Vec<String>, start: usize, row: &str) {
    let row = row.trim_end();
    if !row.is_empty() || (start == 0 && lines.is_empty()) {
        lines.push(row.to_owned());
    }
}

/// 切 token：保留空格信息（空格与相邻 token 分离，便于回溯断点）。
fn tokenize(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut in_space = false;
    for ch in text.chars() {
        let is_space = ch == ' ';
        if is_space != in_space && !token.is_empty() {
            tokens.push(std::mem::take(&mut token));
        }
        in_space = is_space;
        token.push(ch);
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

/// 字符串显示宽度。
pub fn str_width(text: &str) -> usize {
    text.chars().map(|ch| ch.width().unwrap_or(0)).sum()
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
        let ch_width = ch.width().unwrap_or(0);
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
    use super::*;

    #[test]
    fn wraps_at_word_boundaries() {
        assert_eq!(wrap_text("hello world", 7), vec!["hello", "world"]);
    }

    #[test]
    fn hard_splits_long_words() {
        assert_eq!(wrap_text("abcdefgh", 3), vec!["abc", "def", "gh"]);
    }

    #[test]
    fn wide_chars_count_double() {
        // 4 个全角字符宽 8：宽度 5 时第一个字符放不下第二行
        assert_eq!(wrap_text("中文中文", 5), vec!["中文", "中文"]);
        assert_eq!(str_width("中文"), 4);
    }

    #[test]
    fn zero_width_returns_single_line() {
        assert_eq!(wrap_text("abc", 0), vec!["abc"]);
    }

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

    /// 纯空格行不入列：断行位置的空格已被消耗，空行只会放大高度。
    #[test]
    fn pure_space_rows_are_dropped() {
        assert_eq!(wrap_text("a    b", 3), vec!["a", " b"]);
        assert_eq!(wrap_text("aa    bb", 4), vec!["aa", "bb"]);
        assert_eq!(wrap_text("   ", 5), vec![""], "全空格仍保留一行占位");
        assert_eq!(wrap_text("", 5), vec![""]);
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
        // 每段各自折行，断点不跨段
        assert_eq!(
            wrap_text("abcdef\nghijkl", 4),
            vec!["abcd", "ef", "ghij", "kl"]
        );
        // 段首缩进属于正文，断行后的续行空格照旧吞掉
        assert_eq!(wrap_text("  ab\ncd", 4), vec!["  ab", "cd"]);
    }
}

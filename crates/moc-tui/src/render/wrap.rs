//! 显示宽度感知的换行。
//!
//! 终端行宽按「显示列数」计算（CJK 宽字符占 2 列），`unicode-width`
//! 提供测量；换行在空格断点优先，超长单词硬切。

use unicode_width::UnicodeWidthChar;

/// 把一行文本按显示宽度折成多行。
///
/// 空格断点优先：断行时行尾空格被丢弃、续行行首空格被吞掉（与终端
/// `Wrap` 语义一致）；单个 token 超过宽度时按字符硬切。
pub fn wrap_text(text: &str, width: usize) -> Vec<String> {
    if width == 0 {
        return vec![text.to_owned()];
    }
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for token in tokenize(text) {
        // 断行后的行首空格不保留（与终端 wrap 语义一致）
        if token.chars().all(|ch| ch == ' ') && current.is_empty() && !lines.is_empty() {
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
            lines.push(finished.trim_end().to_owned());
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
                    lines.push(finished.trim_end().to_owned());
                    current_width = 0;
                }
                current.push(ch);
                current_width += ch_width;
            }
        }
    }
    lines.push(current.trim_end().to_owned());
    lines
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
}

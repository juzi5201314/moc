//! 语法高亮：syntect + two-face 语法集（与 grok-build 同款组合）。
//!
//! 语法集与高亮主题在进程内只构建一次（编译期打包的 binary dump，
//! 首次访问 ~几十毫秒，之后零成本）；颜色输出统一走主题量化管线，
//! 非 truecolor 终端与整个 UI 保持一致的降级观感。

use std::sync::OnceLock;

use syntect::easy::HighlightLines;
use syntect::highlighting::{Style, Theme};
use syntect::parsing::SyntaxSet;

use crate::theme::detect_color_level;

static SYNTAX_SET: OnceLock<SyntaxSet> = OnceLock::new();
static HIGHLIGHT_THEME: OnceLock<Theme> = OnceLock::new();

fn syntax_set() -> &'static SyntaxSet {
    SYNTAX_SET.get_or_init(two_face::syntax::extra_newlines)
}

fn highlight_theme() -> &'static Theme {
    HIGHLIGHT_THEME.get_or_init(|| {
        // OneHalfDark 与主题的 TokyoNight 系强调色最协调
        two_face::theme::extra()
            .get(two_face::theme::EmbeddedThemeName::OneHalfDark)
            .clone()
    })
}

/// 一段高亮后的代码：每行若干 (前景 RGB, 文本) 片段。
pub struct HighlightedCode {
    pub lines: Vec<Vec<Span>>,
}

/// 高亮片段；颜色为 syntect 前景色（可选——无着色片段用 `None`）。
pub struct Span {
    pub fg: Option<(u8, u8, u8)>,
    pub text: String,
}

/// 逐行喂入的高亮会话：解析状态跨行延续（块注释、多行字符串、续行）。
///
/// 需要按行高亮又要求状态连续的场景（diff 逐行渲染）复用一个会话，不要每行
/// 重建 [`HighlightLines`]：那样每行都从头解析，续行会按普通代码着色。
pub struct LineHighlighter {
    highlighter: HighlightLines<'static>,
}

impl LineHighlighter {
    /// 按语言标记建立会话；未识别的标记按纯文本。
    pub fn new(info: &str) -> Self {
        let syntax = syntax_set()
            .find_syntax_by_token(info.trim())
            .unwrap_or_else(|| syntax_set().find_syntax_plain_text());
        Self {
            highlighter: HighlightLines::new(syntax, highlight_theme()),
        }
    }

    /// 高亮一行；`line` 保留行尾换行符，syntect 需要它推进解析状态。
    pub fn line(&mut self, line: &str) -> Vec<Span> {
        match self.highlighter.highlight_line(line, syntax_set()) {
            Ok(regions) => regions
                .into_iter()
                .filter_map(|(style, text)| {
                    let text = text.trim_end_matches(['\n', '\r']);
                    (!text.is_empty()).then(|| Span {
                        fg: fg_rgb(style),
                        text: text.to_owned(),
                    })
                })
                .collect(),
            Err(error) => {
                // 降级：语法高亮失败不该吞掉这一行内容，按纯文本输出
                tracing::warn!(%error, "语法高亮失败，按纯文本输出该行");
                vec![Span {
                    fg: None,
                    text: line.trim_end_matches(['\n', '\r']).to_owned(),
                }]
            }
        }
    }
}

/// 对代码块做整块高亮（行级别，保留跨行解析状态）。
///
/// `info` 是 markdown fence 的语言标记（```` ```rust ```` 中的 `rust`）；
/// 未识别时按纯文本渲染。整块输入保证 syntect 的 `ParseState` 连续。单行高亮失败
/// 时该行退化为纯文本并 `warn`，不静默丢内容。
pub fn highlight(info: &str, code: &str) -> HighlightedCode {
    let mut highlighter = LineHighlighter::new(info);
    HighlightedCode {
        lines: syntect::util::LinesWithEndings::from(code)
            .map(|line| highlighter.line(line))
            .collect(),
    }
}

/// syntect 前景色；主题未指定时回退终端默认。
fn fg_rgb(style: Style) -> Option<(u8, u8, u8)> {
    let fg = style.foreground;
    // 主题对"未着色"片段常用纯黑/纯白占位，保持终端默认更协调
    if fg.a == 0 {
        return None;
    }
    Some((fg.r, fg.g, fg.b))
}

/// 终端色彩能力缓存（供运行时把高亮 RGB 量化为终端色）。
pub fn color_level() -> crate::theme::ColorLevel {
    static LEVEL: OnceLock<crate::theme::ColorLevel> = OnceLock::new();
    *LEVEL.get_or_init(detect_color_level)
}

/// 供渲染层把任意运行时 RGB 转为 ratatui 颜色（与主题同一量化管线）。
pub fn rgb_to_color(rgb: (u8, u8, u8)) -> ratatui::style::Color {
    crate::theme::rgb_to_color(rgb, color_level())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 逐行会话保留跨行解析状态：块注释的续行沿用注释色。
    ///
    /// 断言用原始前景色（不经终端量化），与运行环境的色彩能力无关。第二段钉住
    /// 「每行重建会话」的后果——diff 逐行渲染的旧实现就是这样丢掉状态的。
    #[test]
    fn shared_session_keeps_block_comment_state() {
        let mut session = LineHighlighter::new("rust");
        let first = session.line("/* 注释首行\n");
        let continuation = session.line("续行内容 */\n");
        let comment_fg = first
            .first()
            .and_then(|span| span.fg)
            .expect("注释首行应有前景色");
        assert_eq!(
            continuation.first().and_then(|span| span.fg),
            Some(comment_fg),
            "同一会话内续行应沿用注释色"
        );

        let mut fresh = LineHighlighter::new("rust");
        assert_ne!(
            fresh.line("续行内容 */\n").first().and_then(|span| span.fg),
            Some(comment_fg),
            "无上下文时该行不应识别为注释"
        );
    }
}

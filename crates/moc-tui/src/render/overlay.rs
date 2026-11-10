//! 浮层的唯一绘制入口：不透明底、卡片规格、滚动条。
//!
//! 所有压在正文之上的东西（模态卡片、补全/引擎菜单、队列、BTW、回退视图、
//! 上下文正文、Todo 卡片、设置页弹窗）都必须先 [`paint_opaque`]，不得直接
//! `render_widget(Clear, …)`。卡片类浮层再走 [`Card`]，版式规格只在这里定义一次。

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use super::wrap::{line_width, truncate};
use crate::theme::Theme;

/// 卡片左右内边距（列）：边框与正文之间。
pub const CARD_PAD_X: u16 = 1;
/// 卡片与终端左右边缘的最小留白（列）。
pub const CARD_MARGIN_X: u16 = 2;
/// 卡片正文的上下留白（行）：标题下、底边框上各 1 行。
pub const CARD_PAD_Y: u16 = 1;
/// 卡片默认最大外宽。
pub const CARD_MAX_WIDTH: u16 = 96;
/// 带预览的宽卡片最大外宽（提问选项预览、计划审批）。
pub const CARD_WIDE_MAX_WIDTH: u16 = 120;
/// 卡片最小外宽。
pub const CARD_MIN_WIDTH: u16 = 24;

/// 把 `area` 变成不透明底：清空内容、铺底色，并修复被浮层切开的宽字符。
///
/// 宽字符跨在浮层边界上时，ratatui 的 diff 会把它的尾格当成「被引导格盖住」
/// 而跳过：引导格在浮层左侧一列、尾格正好是浮层左边框时，边框格永远发不到
/// 终端，左框线就此消失。因此左邻是宽字符时把它换成空格（保留样式）。
/// 右边界无需处理：浮层内的引导格被重置后，ratatui 写入时已把尾格清成空格。
pub fn paint_opaque(buffer: &mut Buffer, area: Rect, style: Style) {
    let area = area.intersection(buffer.area);
    if area.is_empty() {
        return;
    }
    for y in area.top()..area.bottom() {
        if area.x > buffer.area.x {
            split_wide(buffer, area.x - 1, y);
        }
        for x in area.left()..area.right() {
            let cell = &mut buffer[(x, y)];
            cell.reset();
            cell.set_style(style);
        }
    }
}

/// `(x, y)` 是宽字符时换成同样式的空格。
fn split_wide(buffer: &mut Buffer, x: u16, y: u16) {
    let cell = &mut buffer[(x, y)];
    if super::wrap::str_width(cell.symbol()) > 1 {
        cell.set_symbol(" ");
    }
}

/// 非卡片浮层（补全/引擎菜单、队列、BTW、上下文正文）的边框：与 [`Card`]
/// 同一圆角、`border_active`、底色与左右内边距，标题由调用方按需追加。
pub fn panel(theme: &Theme) -> Block<'static> {
    Block::bordered()
        .border_set(ratatui::symbols::border::ROUNDED)
        .border_style(Style::new().fg(theme.border_active))
        .style(Style::new().bg(theme.bg_base).fg(theme.text_primary))
        .padding(Padding::horizontal(CARD_PAD_X))
}

/// 浮层标题 span：` {title} ` 加粗，超宽截断到 `width` 列。
pub fn title_span(title: &str, width: usize, theme: &Theme) -> Span<'static> {
    Span::styled(
        truncate(format!(" {title} "), width).into_owned(),
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )
}

/// 卡片/浮层底部的操作提示行：排版好的一行字与其中可点击的键位（列区间相对行首）。
#[derive(Default)]
pub struct Footer {
    pub line: Line<'static>,
    pub keys: Vec<super::labels::HintKey>,
}

impl Footer {
    /// 提示行画在 `(x, y)` 起时各键位的屏幕命中区。
    pub fn hits(&self, x: u16, y: u16) -> impl Iterator<Item = super::labels::KeyHit> + '_ {
        self.keys.iter().map(move |key| key.at(x, y))
    }
}

/// [`super::labels::hints`] 排版、`gray_dim` 着色的提示行；单键项可点击。
pub fn footer(items: &[(&str, &str)], width: usize, theme: &Theme) -> Footer {
    let (text, keys) = super::labels::hint_layout(items, width);
    Footer {
        line: Line::from(Span::styled(text, Style::new().fg(theme.gray_dim))),
        keys,
    }
}

/// 列表项：`❯ ` / 两格缩进 + 正文，补满 `width` 列，选中整行反白成同一条色带。
pub fn list_row(text: &str, selected: bool, width: usize, theme: &Theme) -> Line<'static> {
    let marker = if selected { "❯ " } else { "  " };
    let style = if selected {
        Style::new()
            .fg(theme.accent_user)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text_secondary)
    };
    Line::from(Span::styled(
        super::wrap::fit(&format!("{marker}{text}"), width),
        style,
    ))
}

/// 卡片外宽：两侧至少留 [`CARD_MARGIN_X`]，再按 `max` 封顶。
pub fn card_width(area: Rect, max: u16) -> u16 {
    area.width
        .saturating_sub(CARD_MARGIN_X * 2)
        .min(max)
        .max(CARD_MIN_WIDTH.min(area.width))
}

/// 外宽对应的正文宽度（扣边框与左右内边距）。
pub fn content_width(card_width: u16) -> usize {
    usize::from(card_width.saturating_sub(2 + CARD_PAD_X * 2))
}

/// 卡片固定开销行数：上下边框 + 上下留白 + footer 及其上方空行。
fn chrome_rows(has_footer: bool) -> u16 {
    2 + CARD_PAD_Y * 2 + if has_footer { 2 } else { 0 }
}

/// `area` 内卡片正文最多能放几行；调用方据此截窗/分页，卡片自身不截正文。
pub fn max_body_rows(area: Rect, has_footer: bool) -> usize {
    usize::from(area.height.saturating_sub(chrome_rows(has_footer)))
}

/// 一张卡片：标题写在顶边框，正文逐行预排好（不再折行），footer 固定在底部。
///
/// 规格：圆角 `border_active` 边框；标题 ` {title} ` 加粗、超宽截断；左右内边距
/// [`CARD_PAD_X`]；标题下与底边框上各留 [`CARD_PAD_Y`] 行；footer 与正文之间空 1 行。
pub struct Card {
    pub title: String,
    pub body: Vec<Line<'static>>,
    pub footer: Option<Footer>,
}

/// 卡片落位：鼠标命中按 `body` 的行号换算（第 i 行正文在 `body.y + i`），
/// `keys` 是 footer 上可点击键位的屏幕命中区。
#[derive(Debug, Clone, Default)]
pub struct CardLayout {
    pub area: Rect,
    pub body: Rect,
    pub keys: Vec<super::labels::KeyHit>,
}

impl Card {
    /// 外高：正文行数 + 固定开销，不超过 `area`。
    pub fn height(&self, area: Rect) -> u16 {
        let body = u16::try_from(self.body.len().max(1)).unwrap_or(u16::MAX);
        body.saturating_add(chrome_rows(self.footer.is_some()))
            .min(area.height)
    }

    /// 在 `area` 中居中绘制，外宽 `width`。
    ///
    /// 正文必须已按 [`content_width`] 折好或截好，且不超过 [`max_body_rows`]：
    /// 卡片不做二次折行，行号即屏幕行，命中几何才可靠。
    pub fn render(self, frame: &mut Frame, area: Rect, width: u16, theme: &Theme) -> CardLayout {
        let width = width.min(area.width);
        let height = self.height(area);
        let rect = Rect {
            x: area.x + (area.width - width) / 2,
            y: area.y + (area.height - height) / 2,
            width,
            height,
        };
        self.render_at(frame, rect, theme)
    }

    /// 在给定矩形绘制（位置由调用方决定，如贴着输入框的批注编辑器）。
    pub fn render_at(self, frame: &mut Frame, rect: Rect, theme: &Theme) -> CardLayout {
        let inner_width = content_width(rect.width);
        debug_assert!(
            self.body
                .iter()
                .chain(self.footer.as_ref().map(|footer| &footer.line))
                .all(|line| line_width(line) <= inner_width),
            "卡片正文必须先按 content_width 折行/截断（{inner_width} 列）"
        );
        debug_assert!(
            self.body.len()
                <= usize::from(
                    rect.height
                        .saturating_sub(chrome_rows(self.footer.is_some()))
                )
                .max(1),
            "卡片正文超过可见行数：调用方必须先按 max_body_rows 截窗"
        );
        let base = Style::new().bg(theme.bg_base).fg(theme.text_primary);
        paint_opaque(frame.buffer_mut(), rect, base);
        let title = truncate(
            format!(" {} ", self.title),
            usize::from(rect.width.saturating_sub(4)),
        );
        frame.render_widget(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active))
                .style(base)
                .title(Span::styled(
                    title.into_owned(),
                    Style::new()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                )),
            rect,
        );
        let x = rect.x + 1 + CARD_PAD_X;
        let inner_w = u16::try_from(inner_width).unwrap_or(u16::MAX);
        let top = rect.y + 1 + CARD_PAD_Y;
        let bottom = rect.bottom().saturating_sub(1 + CARD_PAD_Y);
        let footer_rows = if self.footer.is_some() { 2 } else { 0 };
        let body_bottom = bottom.saturating_sub(footer_rows).max(top);
        let body = Rect::new(x, top, inner_w, body_bottom - top);
        frame.render_widget(Paragraph::new(self.body), body);
        let mut keys = Vec::new();
        if let Some(footer) = self.footer
            && bottom > top
        {
            keys.extend(footer.hits(x, bottom - 1));
            frame.render_widget(
                Paragraph::new(footer.line),
                Rect::new(x, bottom - 1, inner_w, 1),
            );
        }
        CardLayout {
            area: rect,
            body,
            keys,
        }
    }
}

/// 竖向滚动条：一列 `▐`，滑块 `scrollbar_fg`、轨道 `scrollbar_bg`。
///
/// 主转写区、上下文面板、设置页共用同一字形与配色；`thumb_top`/`thumb` 由
/// 各自的滚动几何算好再传入。
pub fn paint_scrollbar(
    buffer: &mut Buffer,
    track: Rect,
    thumb_top: u16,
    thumb: u16,
    theme: &Theme,
) {
    for row in 0..track.height {
        let in_thumb = thumb > 0 && row >= thumb_top && row < thumb_top.saturating_add(thumb);
        let color = if in_thumb {
            theme.scrollbar_fg
        } else {
            theme.scrollbar_bg
        };
        buffer.set_string(track.x, track.y + row, "▐", Style::new().fg(color));
    }
}

#[cfg(test)]
mod tests {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::buffer::Buffer;
    use ratatui::layout::Rect;
    use ratatui::style::Style;
    use ratatui::text::Line;

    use super::{Card, paint_opaque};
    use crate::theme::Theme;

    /// 左邻是宽字符时，浮层左边框必须真的发到终端（经过 diff），不能被尾格吞掉。
    #[test]
    fn left_border_survives_wide_char_neighbor() {
        let theme = Theme::global();
        let mut terminal = Terminal::new(TestBackend::new(20, 6)).expect("测试终端");
        let background = |frame: &mut ratatui::Frame| {
            for y in 0..6 {
                frame
                    .buffer_mut()
                    .set_string(0, y, "中".repeat(10), Style::new());
            }
        };
        terminal.draw(background).expect("首帧");
        // 「中」在偶数列：x=5 是尾格，浮层左边框正好落在这里
        let card_area = Rect::new(5, 0, 12, 6);
        terminal
            .draw(|frame| {
                background(frame);
                let card = Card {
                    title: "t".into(),
                    body: vec![Line::from("x")],
                    footer: None,
                };
                card.render_at(frame, card_area, theme);
            })
            .expect("浮层帧");
        let screen = terminal.backend().buffer();
        for y in 0..6 {
            let symbol = screen[(5, y)].symbol();
            assert!(
                matches!(symbol, "│" | "╭" | "╰"),
                "第 {y} 行左边框缺失：{symbol:?}"
            );
        }
    }

    #[test]
    fn paint_opaque_splits_wide_left_neighbor() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 6, 1));
        buffer.set_string(0, 0, "中中中", Style::new());
        paint_opaque(&mut buffer, Rect::new(1, 0, 2, 1), Style::new());
        assert_eq!(buffer[(0, 0)].symbol(), " ", "左邻宽字符被切开");
        assert_eq!(buffer[(4, 0)].symbol(), "中", "浮层外完整宽字符不动");
    }
}

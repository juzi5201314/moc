//! 上下文正文窗：分块着色标题、折叠正文与可拖拽滚动条。
//!
//! 分块表由 core 给（`session/context_content`），这里只负责排版：每块一行标题
//! （色标 + 折叠标记 + 短名 + token），正文按列宽预折行；超过
//! [`CONTEXT_COLLAPSED_LINES`] 的块默认折叠成「还有 N 行」提示，点击标题或正文
//! 任意一行都能展开。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::modal::{
    CONTEXT_COLLAPSED_LINES, ContextContentState, ContextLayout, ContextScrollbar, Modal,
};
use crate::render::labels::KeyHit;
use crate::render::overlay;
use crate::render::wrap::{str_width, wrap_text};
use crate::theme::Theme;

/// 正文窗最大宽度（列）：再宽也只是一行更长的正文，读起来更费眼。
const CONTEXT_MAX_WIDTH: u16 = 120;
/// 正文窗最大高度（行）。
const CONTEXT_MAX_HEIGHT: u16 = 40;
/// 正文列相对色标的缩进（格）。
const CONTEXT_INDENT: usize = 2;
/// 标题行里 token 列与名字之间的最少间隔（格）。
const CONTEXT_TITLE_GAP: usize = 2;

/// 正文窗尺寸：约占终端四分之三宽、五分之四高，并按上限与边界收口。
fn modal_area(area: Rect) -> Rect {
    let width = (area.width / 4 * 3)
        .clamp(24, CONTEXT_MAX_WIDTH)
        .min(area.width.saturating_sub(2));
    let height = (area.height / 5 * 4)
        .clamp(10, CONTEXT_MAX_HEIGHT)
        .min(area.height.saturating_sub(2));
    Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    }
}

/// 画正文窗；布局与命中几何都写回模态状态，鼠标事件下一帧据此换算。
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    let area = modal_area(area);
    // 点到正文窗外即关闭（与 Esc 同义）。
    app.modal_card_area = area;
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
        return;
    };
    // 卡片内区（边框与左右内边距之内）：总览行、空行、正文、空行、提示条。
    let panel = overlay::panel(theme).title(overlay::title_span(
        "上下文内容",
        usize::from(area.width.saturating_sub(4)),
        theme,
    ));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);
    let body = Rect {
        y: inner.y.saturating_add(2),
        height: inner.height.saturating_sub(4),
        ..inner
    };
    let content = Rect {
        width: body.width.saturating_sub(1),
        ..body
    };
    draw_overview(frame, inner, state, theme);
    let content_width = usize::from(content.width);
    if state
        .layout
        .as_ref()
        .is_none_or(|layout| layout.width != content_width)
    {
        state.layout = Some(build_layout(
            state.sections(),
            &state.expanded,
            content_width,
            theme,
        ));
    }
    state.viewport = usize::from(body.height);
    // 上面保证缓存存在：分块或列宽变化时刚重建，其余情况沿用上一帧。
    let layout = state.layout.as_ref().expect("布局缓存已建立");
    let max_offset = layout.max_offset(state.viewport);
    let scroll = state.scroll.min(max_offset);
    let end = (scroll + state.viewport).min(layout.lines.len());
    let visible: Vec<Line<'static>> = layout.lines[scroll..end].to_vec();
    state.scroll = scroll;
    state.scrollbar = ContextScrollbar {
        track: Rect {
            x: content.x + content.width,
            y: content.y,
            width: 1,
            height: content.height,
        },
        thumb: thumb_height(state.viewport, layout.lines.len()),
        max_offset,
    };
    frame.render_widget(Paragraph::new(visible), content);
    highlight_focus(frame, state, content, theme);
    draw_scrollbar(frame, state, theme);
    app.key_hits = draw_footer(frame, inner, state, theme);
}

/// 键盘焦点块的标题行加底色：告诉用户 `Enter` 会展开哪一块。
///
/// 底色直接铺在已画好的那一行上，不改渲染缓存——Tab 切换焦点不必重排正文。
fn highlight_focus(frame: &mut Frame, state: &ContextContentState, content: Rect, theme: &Theme) {
    let Some(index) = state.focus else {
        return;
    };
    let Some(span) = state
        .layout
        .as_ref()
        .and_then(|layout| layout.spans.get(index))
    else {
        return;
    };
    let Some(offset) = span.start.checked_sub(state.scroll) else {
        return;
    };
    let row = content.y.saturating_add(offset as u16);
    if offset >= state.viewport || row >= content.y + content.height {
        return;
    }
    let buffer = frame.buffer_mut();
    for column in content.x..content.x.saturating_add(content.width) {
        if let Some(cell) = buffer.cell_mut((column, row)) {
            cell.set_style(cell.style().bg(theme.bg_visual));
        }
    }
}

/// 总览行：合计与窗口上限、分块数，或加载 / 失败状态。
fn draw_overview(frame: &mut Frame, inner: Rect, state: &ContextContentState, theme: &Theme) {
    let row = Rect { height: 1, ..inner };
    if let Some(error) = &state.error {
        let text = format!("读取上下文内容失败：{error}");
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                crate::render::wrap::truncate(&text, usize::from(row.width)).into_owned(),
                Style::new().fg(theme.accent_error),
            ))),
            row,
        );
        return;
    }
    let Some(content) = &state.content else {
        frame.render_widget(
            Paragraph::new(Line::from(Span::styled(
                "正在读取上下文内容…",
                Style::new().fg(theme.accent_running),
            ))),
            row,
        );
        return;
    };
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                crate::render::format_tokens(content.total_tokens),
                Style::new()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!(
                    " / {}",
                    crate::render::format_tokens(content.context_window)
                ),
                Style::new().fg(theme.gray),
            ),
            Span::styled(
                format!(
                    " · {}",
                    crate::render::modal::share_percent(
                        content.total_tokens,
                        content.context_window
                    )
                ),
                Style::new().fg(theme.gray),
            ),
            Span::styled(
                format!(" · {} 个分块", content.sections.len()),
                Style::new().fg(theme.gray),
            ),
        ])),
        row,
    );
}

/// 页脚：操作提示，窄屏从左丢项、恒保留关闭键；返回可点击键位的命中区。
fn draw_footer(
    frame: &mut Frame,
    inner: Rect,
    state: &ContextContentState,
    theme: &Theme,
) -> Vec<KeyHit> {
    let width = usize::from(inner.width);
    let footer = if state.drag.is_some() {
        overlay::footer(&[("", "松开鼠标结束拖拽")], width, theme)
    } else if state.content.is_some() {
        overlay::footer(
            &[
                ("e/c", "全部展开/收起"),
                ("", "点击分块展开/收起"),
                ("tab", "切换"),
                ("↑↓/pgup/pgdn", "滚动"),
                ("enter", "展开"),
                ("esc", "关闭"),
            ],
            width,
            theme,
        )
    } else {
        overlay::footer(&[("esc", "关闭")], width, theme)
    };
    let row = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    let keys = footer.hits(row.x, row.y).collect();
    frame.render_widget(Paragraph::new(footer.line), row);
    keys
}

/// 滚动条：内容溢出时才画滑块，轨道本身始终占一列（正文折行宽度不随之跳动）。
fn draw_scrollbar(frame: &mut Frame, state: &ContextContentState, theme: &Theme) {
    let scrollbar = state.scrollbar;
    if scrollbar.track.height == 0 || scrollbar.track.width == 0 {
        return;
    }
    let thumb = if scrollbar.max_offset > 0 {
        scrollbar.thumb
    } else {
        0
    };
    overlay::paint_scrollbar(
        frame.buffer_mut(),
        scrollbar.track,
        scrollbar.thumb_top(state.scroll),
        thumb,
        theme,
    );
}

/// 滑块高度：视口占全文的比例，至少一行。
fn thumb_height(viewport: usize, total: usize) -> u16 {
    if total == 0 {
        return 0;
    }
    let viewport = viewport.clamp(1, total);
    ((viewport * viewport) / total).clamp(1, viewport) as u16
}

/// 分块列表 → 渲染行；每块一行标题、若干正文行、可选提示行，块间空一行。
fn build_layout(
    sections: &[moc_protocol::methods::ContextContentSection],
    expanded: &[bool],
    width: usize,
    theme: &Theme,
) -> ContextLayout {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut spans = Vec::with_capacity(sections.len());
    for (index, section) in sections.iter().enumerate() {
        let start = lines.len();
        let body = wrap_body(&section.text, width.saturating_sub(CONTEXT_INDENT));
        let foldable = body.len() > CONTEXT_COLLAPSED_LINES;
        let folded = foldable && !expanded.get(index).copied().unwrap_or(false);
        lines.push(title_line(
            index,
            section,
            width,
            foldable.then_some(folded),
            theme,
        ));
        let shown = if folded {
            CONTEXT_COLLAPSED_LINES
        } else {
            body.len()
        };
        for text in body.iter().take(shown) {
            lines.push(indented_line(text, CONTEXT_INDENT, theme.text_secondary));
        }
        if folded {
            let rest = body.len() - shown;
            lines.push(indented_line(
                &format!("… 还有 {rest} 行 · 点击展开"),
                CONTEXT_INDENT,
                theme.gray_dim,
            ));
        } else if foldable {
            lines.push(indented_line("… 点击收起", CONTEXT_INDENT, theme.gray_dim));
        }
        if section.truncated_bytes > 0 {
            lines.push(indented_line(
                &format!(
                    "… 正文超过单块上限，已截断 {} 字节",
                    section.truncated_bytes
                ),
                CONTEXT_INDENT,
                theme.accent_warning,
            ));
        }
        // 块间空行：折叠状态下分块边界才看得清。
        lines.push(Line::default());
        spans.push(start..lines.len());
    }
    ContextLayout {
        width,
        spans,
        lines,
    }
}

/// 标题行：`▌` 色标（分块色） + 折叠标记 + 短名（加粗同色） + 右对齐 token。
///
/// `folded` 为 `None` 表示该块全文显示（不比折叠阈值长），标记位留空以对齐名字列。
fn title_line(
    index: usize,
    section: &moc_protocol::methods::ContextContentSection,
    width: usize,
    folded: Option<bool>,
    theme: &Theme,
) -> Line<'static> {
    let marker = match folded {
        None => "  ",
        Some(true) => "▸ ",
        Some(false) => "▾ ",
    };
    let name = format!("{marker}{}", section.name);
    let tokens = crate::render::format_tokens(section.tokens);
    let head = 1 + str_width(&name);
    let pad = width.saturating_sub(head + CONTEXT_TITLE_GAP + str_width(&tokens));
    let style = Style::new()
        .fg(theme.chart(index))
        .add_modifier(Modifier::BOLD);
    Line::from(vec![
        Span::styled("▌", style),
        Span::styled(name, style),
        Span::styled(" ".repeat(pad + CONTEXT_TITLE_GAP), Style::new()),
        Span::styled(tokens, Style::new().fg(theme.gray)),
    ])
}

/// 正文折行：先按源文本行切开（工具 JSON 与系统提示的换行必须保留），再各自折行。
fn wrap_body(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    for source in text.split('\n') {
        lines.extend(wrap_text(source, width));
    }
    lines
}

/// 缩进正文行。
fn indented_line(text: &str, indent: usize, color: ratatui::style::Color) -> Line<'static> {
    Line::from(vec![
        Span::styled(" ".repeat(indent), Style::new()),
        Span::styled(text.to_owned(), Style::new().fg(color)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modal::{ContextContentState, Modal};
    use moc_protocol::methods::{ContextContentResult, ContextContentSection};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn section(name: &str, text: &str) -> ContextContentSection {
        ContextContentSection {
            name: name.into(),
            text: text.into(),
            tokens: 42,
            truncated_bytes: 0,
        }
    }

    /// 强制真彩色主题：配色断言必须拿到确定值，不能随运行环境的 TERM 漂移。
    fn test_theme() -> &'static Theme {
        static THEME: std::sync::LazyLock<Theme> =
            std::sync::LazyLock::new(|| Theme::resolved(crate::theme::ColorLevel::TrueColor));
        &THEME
    }

    fn app_with(sections: Vec<ContextContentSection>) -> App {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(test_theme(), events);
        let mut state = ContextContentState::default();
        state.set_content(ContextContentResult {
            context_window: 500_000,
            total_tokens: 12_345,
            sections,
        });
        app.modal = Some(Modal::ContextContent { state });
        app
    }

    /// 画一帧并回读整个屏幕的文本。
    ///
    /// 宽字符（CJK）在缓冲里占两格、后一格是占位空格，回读时按显示宽度对齐会夹出
    /// 空格；断言一律先过 [`compact`] 去掉空格再匹配。
    fn draw_text(app: &mut App, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| {
                let area = frame.area();
                draw(frame, app, area);
            })
            .expect("绘制成功");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|row| {
                (0..width)
                    .filter_map(|column| buffer.cell((column, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// 去掉全部空格：只用于断言内容在不在，不用于断言排版。
    fn compact(text: &str) -> String {
        text.replace(' ', "")
    }

    /// 窗口按内容收口：不铺满终端，但也要占得下正文。
    #[test]
    fn window_stays_inside_the_terminal() {
        let wide = modal_area(Rect::new(0, 0, 200, 60));
        assert!(wide.width < 200 && wide.height < 60, "宽终端下留出边距");
        assert!(wide.width <= CONTEXT_MAX_WIDTH && wide.height <= CONTEXT_MAX_HEIGHT);
        let narrow = modal_area(Rect::new(0, 0, 30, 12));
        assert!(
            narrow.width <= 28 && narrow.height <= 10,
            "窄终端下按边界收口"
        );
        assert!(narrow.width >= 24 && narrow.height >= 10);
    }

    /// 长块默认折叠成提示行，点击展开后正文补齐；短块始终全文显示。
    #[test]
    fn long_blocks_fold_until_expanded() {
        let long: Vec<String> = (1..=20).map(|index| format!("正文行{index}")).collect();
        let mut app = app_with(vec![
            section("intro", "很短的一段"),
            section("tool policy", &long.join("\n")),
        ]);
        let text = draw_text(&mut app, 100, 30);
        let flat = compact(&text);
        assert!(flat.contains("intro") && flat.contains("toolpolicy"));
        assert!(flat.contains("▸toolpolicy"), "长块折叠时带折叠标记");
        assert!(flat.contains("正文行6"), "折叠块保留前 6 行");
        assert!(!flat.contains("正文行7"), "第 7 行要折起来");
        assert!(flat.contains("还有14行·点击展开"));
        assert!(flat.contains("12.35k/500k"), "总览行沿用占用口径");

        let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
            panic!("正文窗应保持打开");
        };
        state.toggle(1);
        // 展开后整块高过视口：先重排（缓存作废），再滚到底看尾部。
        let flat = compact(&draw_text(&mut app, 100, 30));
        assert!(flat.contains("▾toolpolicy"), "展开后标记翻转");
        assert!(flat.contains("正文行7"), "折叠阈值之后的行也露出来了");
        let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
            panic!("正文窗应保持打开");
        };
        state.scroll_to_edge(true);
        let flat = compact(&draw_text(&mut app, 100, 30));
        assert!(flat.contains("正文行20"), "滚到底能读到末行");
        assert!(flat.contains("…点击收起"));
    }

    /// 正文溢出时右缘出现滑块，拖拽列始终占位。
    #[test]
    fn overflow_draws_a_draggable_scrollbar() {
        let long: Vec<String> = (1..=200).map(|index| format!("行{index}")).collect();
        let mut app = app_with(vec![section("messages", &long.join("\n"))]);
        let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
            panic!("正文窗应保持打开");
        };
        state.set_all_expanded(true);
        let _ = draw_text(&mut app, 100, 30);
        let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
            panic!("正文窗应保持打开");
        };
        assert!(state.scrollbar.max_offset > 0, "内容溢出");
        assert!(state.scrollbar.thumb > 0 && state.scrollbar.thumb < state.scrollbar.track.height);
        assert_eq!(
            state.scrollbar.track.height, state.viewport as u16,
            "轨道覆盖整个正文区"
        );
        // 滚到底部后滑块贴轨道末端。
        state.scroll = state.scrollbar.max_offset;
        let _ = draw_text(&mut app, 100, 30);
        let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
            panic!("正文窗应保持打开");
        };
        assert_eq!(
            state.scrollbar.thumb_top(state.scroll),
            state.scrollbar.track.height - state.scrollbar.thumb,
        );
    }

    /// 底色不透：底下铺满宽字符也不泄露底色，左边框也不被左邻宽字符吞掉。
    #[test]
    fn context_content_frame_renders_solid_opaque_background() {
        use ratatui::buffer::CellWidth;
        use ratatui::style::Color;
        let mut app = app_with(vec![]);

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
        let area = Rect::new(0, 0, 100, 30);
        let modal = modal_area(area);
        // 按窗左缘奇偶选起点，使 `modal.x - 1` 恰好是宽字符首格（最坏情况）。
        let start = (modal.x - 1) % 2;
        terminal
            .draw(|frame| {
                let style = ratatui::style::Style::new().bg(Color::Red);
                for y in 0..30 {
                    frame
                        .buffer_mut()
                        .set_string(start, y, "中".repeat(50), style);
                }
                draw(frame, &mut app, area);
            })
            .expect("绘制成功");

        let buffer = terminal.backend().buffer();
        for y in modal.y..modal.y + modal.height {
            let border = buffer.cell((modal.x, y)).expect("单元格存在").symbol();
            assert!(
                matches!(border, "│" | "╭" | "╰"),
                "第 {y} 行左边框被吞：{border:?}"
            );
            for x in modal.x..modal.x + modal.width {
                // 窗内宽字符的尾格由 ratatui 重置成 Reset，终端上被首格盖住，不算泄露。
                if x > modal.x && buffer.cell((x - 1, y)).is_some_and(|c| c.cell_width() > 1) {
                    continue;
                }
                let cell = buffer.cell((x, y)).expect("单元格存在");
                assert_ne!(
                    cell.bg,
                    Color::Reset,
                    "正文窗区域不应出现 Reset 穿透：({x}, {y})"
                );
                assert_ne!(
                    cell.bg,
                    Color::Red,
                    "正文窗区域不应泄露底层颜色：({x}, {y})"
                );
            }
        }
    }
}

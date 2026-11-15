use rust_i18n::t;

use crate::app::App;
use crate::render::labels;
use crate::render::labels::KeyHit;
use crate::render::overlay;
use crate::render::wrap::fit;
use crate::render::wrap::str_width;
use crate::render::wrap::truncate;
use crate::theme::Theme;
use crate::widgets::completion::CompletionState;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

/// 计算斜杠补全菜单所需高度（与 draw_completion_menu 严格一致）。
pub fn completion_menu_height(candidates_len: usize, prompt_y: u16) -> u16 {
    let max_visible = 20usize.min(usize::from(prompt_y.saturating_sub(3)));
    let visible = candidates_len.min(max_visible);
    let rows = u16::try_from(visible).unwrap_or(u16::MAX) + u16::from(candidates_len > visible);
    (rows + 3).min(prompt_y)
}

/// 斜杠补全菜单：贴着输入框上沿，选中行反色；返回 footer 上可点击键位的命中区。
pub fn draw_completion_menu(
    frame: &mut Frame,
    completion: &mut CompletionState,
    prompt_area: Rect,
) -> Vec<KeyHit> {
    let total = completion.candidates.len();
    let max_visible = 20usize.min(usize::from(prompt_area.y.saturating_sub(3)));
    let visible = total.min(max_visible);
    let width = prompt_area.width.clamp(20, 72);
    let height = completion_menu_height(total, prompt_area.y);
    if height == 0 {
        return Vec::new();
    }
    let area = Rect {
        x: prompt_area.x,
        y: prompt_area.y - height,
        width,
        height,
    };
    completion.area = area;
    completion.visible_count = visible;
    completion.adjust_scroll();

    let theme = crate::theme::Theme::global();
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    let content_width = overlay::content_width(width);
    let max_label_width = completion
        .candidates
        .iter()
        .skip(completion.offset)
        .take(visible)
        .map(|c| str_width(&c.label))
        .max()
        .unwrap_or(12)
        .max(14);
    let col_width = max_label_width + 3;
    let mut lines = Vec::with_capacity(visible + 2);
    for (rel_index, candidate) in completion
        .candidates
        .iter()
        .skip(completion.offset)
        .take(visible)
        .enumerate()
    {
        let index = completion.offset + rel_index;
        let selected = index == completion.selected;
        let prefix = if selected { "❯ " } else { "  " };
        let (prefix_style, label_style, desc_style) = if selected {
            (
                Style::new()
                    .fg(theme.accent_user)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD),
                Style::new()
                    .fg(theme.text_primary)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD),
                Style::new().fg(theme.text_secondary).bg(theme.bg_visual),
            )
        } else {
            (
                Style::new().fg(theme.text_primary),
                Style::new().fg(theme.text_primary),
                Style::new().fg(theme.gray_dim),
            )
        };
        let label_width = col_width.min(content_width.saturating_sub(2));
        let desc_width = content_width.saturating_sub(2 + label_width);
        // 选中高亮补满整行：右缘固定，不随描述长短参差
        lines.push(Line::from(vec![
            Span::styled(prefix, prefix_style),
            Span::styled(fit(&candidate.label, label_width), label_style),
            Span::styled(fit(&candidate.description, desc_width), desc_style),
        ]));
    }
    if total > visible {
        lines.push(Line::from(Span::styled(
            truncate(labels::more_items(total - visible), content_width).into_owned(),
            Style::new().fg(theme.gray_dim),
        )));
    }
    let footer = overlay::footer(
        &[
            ("↑↓", t!("tui.hint.choose")),
            ("tab/enter", t!("tui.hint.confirm")),
            ("esc", t!("tui.hint.close")),
        ],
        content_width,
        theme,
    );
    // footer 是边框内最后一行正文：左边框 + 内边距之后起排。
    let footer_y = area.y + 1 + u16::try_from(lines.len()).unwrap_or(u16::MAX);
    let keys = if footer_y < area.bottom().saturating_sub(1) {
        footer
            .hits(area.x + 1 + overlay::CARD_PAD_X, footer_y)
            .collect()
    } else {
        Vec::new()
    };
    lines.push(footer.line);
    frame.render_widget(Paragraph::new(lines).block(overlay::panel(theme)), area);
    keys
}

/// 就地引擎菜单：右缘对齐图例里的引擎名，整体贴在它上方（右键菜单那种小浮层）。
pub fn draw_engine_menu(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let anchor = app.engine_area;
    let current = app.session.as_ref().map(|session| session.engine);
    let Some(menu) = app.engine_menu.as_mut() else {
        return;
    };
    draw_legend_menu(frame, theme, menu, anchor, current);
}

/// 就地权限菜单：与引擎菜单同一浮层，锚定权限图例。
pub fn draw_permission_menu(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let anchor = app.permission_tag_area;
    let current = app.session.as_ref().map(|session| session.permission);
    let Some(menu) = app.permission_menu.as_mut() else {
        return;
    };
    draw_legend_menu(frame, theme, menu, anchor, current);
}

/// 图例菜单通用画法：窄标签列；高亮项的一句说明画成菜单侧边的独立小提示框，
/// 随方向键与悬停更新；当前生效项沿用图例里的强调色，与菜单高亮（反色）互相独立。
pub(super) fn draw_legend_menu<T: crate::app::LegendItem + PartialEq>(
    frame: &mut Frame,
    theme: &Theme,
    menu: &mut crate::app::LegendMenu<T>,
    anchor: Rect,
    current: Option<T>,
) {
    let label_width = menu
        .items
        .iter()
        .map(|item| str_width(item.label()))
        .max()
        .unwrap_or(0);
    let selected = menu.selected.min(menu.items.len().saturating_sub(1));
    let description = menu
        .items
        .get(selected)
        .map_or(std::borrow::Cow::Borrowed(""), crate::app::LegendItem::description);
    let frame_area = frame.area();
    // 正文宽 = ❯ + 标签；外宽再加两侧内边距与边框。
    let width = u16::try_from(label_width + 6)
        .unwrap_or(u16::MAX)
        .min(frame_area.width);
    let height = u16::try_from(menu.items.len() + 2).unwrap_or(u16::MAX);
    let area = if !menu.area.is_empty() {
        menu.area
    } else {
        let x = (anchor.x.saturating_add(anchor.width))
            .saturating_sub(width)
            .min(frame_area.width.saturating_sub(width))
            .max(frame_area.x);
        Rect {
            x,
            y: anchor.y.saturating_sub(height).max(frame_area.y),
            width,
            height,
        }
    };
    menu.area = area;
    let inner = overlay::content_width(area.width);
    let lines: Vec<Line<'static>> = menu
        .items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let highlighted = index == selected;
            let base = if highlighted {
                Style::new()
                    .fg(theme.text_primary)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_primary)
            };
            let label_style = if current.as_ref() == Some(item) {
                base.fg(theme.accent_model)
            } else {
                base
            };
            Line::from(vec![
                Span::styled(if highlighted { "❯ " } else { "  " }, base),
                Span::styled(fit(item.label(), inner.saturating_sub(2)), label_style),
            ])
        })
        .collect();
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    frame.render_widget(Paragraph::new(lines).block(overlay::panel(theme)), area);
    draw_legend_tip(frame, theme, area, selected, &description);
}

/// 高亮项的一句说明：贴菜单侧边的小提示框，文本行与高亮条目行垂直对齐。
/// 图例菜单锚在右缘，优先放菜单左侧；左、右都放不下才放弃。
pub(super) fn draw_legend_tip(
    frame: &mut Frame,
    theme: &Theme,
    menu_area: Rect,
    selected: usize,
    description: &str,
) {
    if description.is_empty() {
        return;
    }
    let frame_area = frame.area();
    let tip_w = u16::try_from(str_width(description) + 4).unwrap_or(u16::MAX);
    let item_y = menu_area.y + 1 + u16::try_from(selected).unwrap_or(0);
    let y = item_y.saturating_sub(1).max(frame_area.y);
    let x = if let Some(x) = menu_area.x.checked_sub(tip_w + 1) {
        x
    } else if menu_area.right() + 1 + tip_w <= frame_area.right() {
        menu_area.right() + 1
    } else {
        return;
    };
    let area = Rect::new(x, y, tip_w.min(frame_area.right().saturating_sub(x)), 3);
    if area.width < 4 {
        return;
    }
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            fit(description, overlay::content_width(area.width)),
            Style::new().fg(theme.text_secondary),
        )))
        .block(overlay::panel(theme)),
        area,
    );
}

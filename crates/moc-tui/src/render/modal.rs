//! 浮层渲染：斜杠补全菜单与模态卡片。
//!
//! 浮层画在滚动区之后（覆盖其内容）；模态居中，宽度按内容取值（帮助为双列快捷键表，
//! 普通模态上限 96 列，带选项预览的提问放宽到 120 列），高度按内容收缩。全部使用主题槽位。

mod model_picker;

use std::time::{Duration, Instant};

use moc_protocol::methods::{ContextUsageResult, PlanStatus};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Padding, Paragraph};

use crate::app::App;
use crate::modal::{Modal, PlanPane, PlanReviewState, SessionScope};
use crate::render::labels::KeyHit;
use crate::render::wrap::{
    clip_line, fit, fit_line, line_width, str_width, truncate, truncate_head,
};
use crate::render::{context_content, labels, overlay};
use crate::theme::Theme;
use crate::widgets::completion::CompletionState;
use crate::widgets::prompt::clamp_cursor;

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
        &[("↑↓", "选择"), ("tab/enter", "确认"), ("esc", "关闭")],
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
fn draw_legend_menu<T: crate::app::LegendItem + PartialEq>(
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
        .map_or("", crate::app::LegendItem::description);
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
    draw_legend_tip(frame, theme, area, selected, description);
}

/// 高亮项的一句说明：贴菜单侧边的小提示框，文本行与高亮条目行垂直对齐。
/// 图例菜单锚在右缘，优先放菜单左侧；左、右都放不下才放弃。
fn draw_legend_tip(
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

/// 通用模态的正文产物：正文行（已按正文宽度折好/截好、不超过可见行数）、
/// 底部提示条，以及可点击条目所在的正文行号。
#[derive(Default)]
struct ModalBody {
    lines: Vec<Line<'static>>,
    footer: Option<overlay::Footer>,
    scroll_max: usize,
    /// 本帧实际取用的 viewport 起点；列表模态据此把 offset 写回状态，
    /// 让滚轮从真实窗口继续滚而不是从滞后值跳。
    applied_offset: Option<usize>,
    /// (条目下标, 正文行号)。
    hits: Vec<(usize, usize)>,
}

/// 居中模态卡片。
pub fn draw_modal(frame: &mut Frame, app: &mut App) {
    app.modal_item_hits.clear();
    app.modal_detail_hits.clear();
    app.modal_menu_hits.clear();
    app.modal_card_area = Rect::default();
    app.modal_detail_area = Rect::default();
    app.modal_menu_area = Rect::default();
    // 模态是阻塞层：底下界面登记的键位一律作废，只留模态自己的。
    app.key_hits.clear();
    let area = frame.area();
    if area.width < 20 || area.height < 6 {
        return;
    }
    if matches!(app.modal, Some(Modal::Model { .. })) {
        model_picker::draw(frame, app);
        return;
    }
    let theme = app.theme;
    // 折叠的提问卡片不压暗正文：用户正在滚动的就是提问背后的上下文，
    // 调暗会伤可读性；细条本身是不透明底，照样醒目。
    let dim_background = !matches!(app.modal, Some(Modal::Ask { parked: true, .. }));
    if dim_background {
        // 保留正文轮廓但降低对比度，浮层成为唯一的视觉焦点。
        frame
            .buffer_mut()
            .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    }
    if matches!(app.modal, Some(Modal::Rewind { .. })) {
        crate::render::rewind::draw(frame, app, area);
        return;
    }
    if matches!(app.modal, Some(Modal::ContextContent { .. })) {
        // 正文窗按自己的列宽折行、按整行滚动，并自带滚动条与命中几何。
        context_content::draw(frame, app, area);
        return;
    }
    if matches!(app.modal, Some(Modal::Ask { .. })) {
        // 提问卡片自带命中几何（鼠标点选选项），同样不走通用路径。
        draw_ask_modal(frame, app, area);
        return;
    }
    let Some(modal) = &app.modal else {
        return;
    };
    let rows = overlay::max_body_rows(area, true).max(1);
    let width = match modal {
        Modal::Help { .. } => {
            // 帮助是双列快捷键表：按内容自然宽度收窄，窄终端由 help_lines 退回单列。
            let probe = overlay::content_width(overlay::card_width(area, overlay::CARD_MAX_WIDTH));
            let natural = help_lines(app, probe).1 + usize::from(2 + overlay::CARD_PAD_X * 2);
            overlay::card_width(area, u16::try_from(natural).unwrap_or(u16::MAX))
        }
        Modal::Plan { .. } => overlay::card_width(area, overlay::CARD_WIDE_MAX_WIDTH),
        _ => overlay::card_width(area, overlay::CARD_MAX_WIDTH),
    };
    let body = modal_body(app, modal, overlay::content_width(width), rows);
    let layout = overlay::Card {
        title: modal.title(),
        body: body.lines,
        footer: body.footer,
    }
    .render(frame, area, width, theme);
    // 第 i 行正文就在 `layout.body.y + i`：卡片不二次折行，行号即屏幕行。
    let row_y = |row: usize| -> Option<u16> {
        let y = layout.body.y.checked_add(u16::try_from(row).ok()?)?;
        (y < layout.body.bottom()).then_some(y)
    };
    app.modal_item_hits = body
        .hits
        .into_iter()
        .filter_map(|(index, row)| {
            Some((
                index,
                Rect::new(
                    layout.area.x + 1,
                    row_y(row)?,
                    layout.area.width.saturating_sub(2),
                    1,
                ),
            ))
        })
        .collect();
    app.modal_card_area = layout.area;
    app.key_hits = layout.keys;
    // 悬停只铺底色：pointer 坐标由事件层记录，选中项保持自己的高亮。
    {
        let (pointer, selected) = match modal {
            Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. } => {
                (picker.pointer, Some(picker.selected))
            }
            Modal::Jobs {
                pointer, selected, ..
            } => (*pointer, Some(*selected)),
            Modal::WorkflowApproval { state } => (state.pointer, Some(state.focus)),
            Modal::WorkflowRun { state } => (state.pointer, Some(state.focus)),
            _ => (None, None),
        };
        if let Some(position) = pointer
            && let Some((_, area)) = app
                .modal_item_hits
                .iter()
                .find(|(index, area)| Some(*index) != selected && area.contains(position))
        {
            frame
                .buffer_mut()
                .set_style(*area, Style::new().bg(theme.bg_hover));
        }
    }
    if let Modal::Plan { state } = modal {
        if state.pane == PlanPane::Annotate {
            // 批注视图是一行源码对一行屏幕行，鼠标拖选直接按行换算。
            let window = plan_source_window(state, rows);
            app.plan_body_area = Rect::new(
                layout.body.x,
                layout.body.y + u16::try_from(PLAN_HEADER_ROWS).expect("常量可表示为 u16"),
                layout.body.width,
                u16::try_from(window.len()).unwrap_or(u16::MAX),
            );
            app.plan_row_map = window;
        }
        if state.editor.is_some() {
            // 编辑器盖在审阅卡片上：只有它的键位可点。
            app.key_hits = draw_plan_editor(frame, app, layout.area, area);
        }
    }
    // 事件层只加不减的滚动偏移按本帧上限写回；各模态同一约定。
    // 列表模态 additionally 回写实取窗口与滚动约束，并消费掉 reveal 标记。
    match app.modal.as_mut() {
        Some(Modal::Help { scroll } | Modal::Tools { scroll, .. }) => {
            *scroll = (*scroll).min(body.scroll_max);
        }
        Some(Modal::Permission { scroll_offset, .. }) => {
            *scroll_offset = (*scroll_offset).min(body.scroll_max);
        }
        Some(Modal::Plan { state }) if state.pane == PlanPane::Read => {
            state.scroll_offset = state.scroll_offset.min(body.scroll_max);
        }
        Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
            if let Some(offset) = body.applied_offset {
                picker.offset = offset;
            }
            picker.scroll.max_offset = body.scroll_max;
            picker.scroll.reveal_selection = false;
        }
        Some(Modal::Jobs { offset, scroll, .. }) => {
            if let Some(start) = body.applied_offset {
                *offset = start;
            }
            scroll.max_offset = body.scroll_max;
            scroll.reveal_selection = false;
        }
        Some(Modal::WorkflowApproval { state }) => {
            if let Some(start) = body.applied_offset {
                state.body_scroll = start;
            }
            state.scroll.max_offset = body.scroll_max;
            state.scroll.reveal_selection = false;
        }
        Some(Modal::WorkflowRun { state }) => {
            if let Some(start) = body.applied_offset {
                state.body_scroll = start;
            }
            state.scroll.max_offset = body.scroll_max;
            state.scroll.reveal_selection = false;
        }
        _ => {}
    }
}

/// 提问卡片：渲染的同时把每个选项（含自定义行）的命中矩形记到 `ask_option_hits`，
/// 鼠标点选据此换算。带选项预览时放宽到 [`overlay::CARD_WIDE_MAX_WIDTH`]。
/// 折叠态（parked）不画卡片：收成细条占住输入框位，整屏留给转写区。
fn draw_ask_modal(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    if matches!(app.modal, Some(Modal::Ask { parked: true, .. })) {
        app.ask_option_hits.clear();
        app.ask_nav_hits.clear();
        app.ask_card_area = Rect::default();
        app.ask_question_area = Rect::default();
        app.ask_detail_area = Rect::default();
        draw_ask_strip(frame, app);
        return;
    }
    let Some(modal) = app.modal.as_ref() else {
        return;
    };
    let max_width = if modal.has_option_preview() {
        overlay::CARD_WIDE_MAX_WIDTH
    } else {
        overlay::CARD_MAX_WIDTH
    };
    let width = overlay::card_width(area, max_width);
    let max_rows = overlay::max_body_rows(area, true).max(1);
    let AskLayout {
        mut lines,
        footer,
        option_hits,
        nav_hits,
        question_area,
        detail_area,
        preview_scroll_max,
    } = ask_layout(theme, modal, overlay::content_width(width), max_rows);
    // 问题、空行、自定义行是提问卡的最小骨架：极矮终端连骨架都放不下时只截掉底部，
    // 落在可见区外的命中由下方 `row_y` 统一丢弃。
    lines.truncate(max_rows);
    let card = overlay::Card {
        title: modal.title(),
        body: lines,
        footer: Some(footer),
    }
    .render(frame, area, width, theme);
    app.ask_card_area = card.area;
    app.key_hits = card.keys;
    let body = card.body;
    let row_y = |row: usize| -> Option<u16> {
        let y = body.y.checked_add(u16::try_from(row).ok()?)?;
        (y < body.bottom()).then_some(y)
    };
    app.ask_option_hits = option_hits
        .into_iter()
        .filter_map(|(option, row)| {
            Some((
                option,
                Rect::new(
                    card.area.x + 1,
                    row_y(row)?,
                    card.area.width.saturating_sub(2),
                    1,
                ),
            ))
        })
        .collect();
    app.ask_nav_hits = nav_hits
        .into_iter()
        .filter_map(|(question, row, start, end)| {
            Some((
                question,
                Rect::new(
                    body.x + u16::try_from(start).ok()?,
                    row_y(row)?,
                    u16::try_from(end - start).ok()?,
                    1,
                ),
            ))
        })
        .collect();
    // 可滚文本区换算到屏幕：被卡片底边裁掉的行不在命中范围内。
    let visible = usize::from(body.height);
    let map_area = |text: Rect| -> Rect {
        let top = usize::from(text.y);
        if top >= visible {
            return Rect::default();
        }
        let bottom = (top + usize::from(text.height)).min(visible);
        Rect::new(
            body.x + text.x,
            body.y + text.y,
            text.width.min(body.width.saturating_sub(text.x)),
            u16::try_from(bottom - top).unwrap_or(u16::MAX),
        )
    };
    app.ask_question_area = question_area.map_or_default(map_area);
    app.ask_detail_area = detail_area.map_or_default(map_area);
    // 事件层只加不减：详情/确认页的滚动偏移按本帧上限写回。
    if let Some(Modal::Ask { preview_scroll, .. }) = app.modal.as_mut() {
        *preview_scroll = (*preview_scroll).min(preview_scroll_max);
    }
}

/// 折叠的提问细条：占据输入框位（输入框本轮不绘制），左边是进度与当前问，
/// 右边是返回/取消键；边框用运行色标示「有待答的提问」，正文垂直居中。
fn draw_ask_strip(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let Some(Modal::Ask {
        params,
        question,
        selected,
        custom,
        ..
    }) = app.modal.as_ref()
    else {
        return;
    };
    let mut area = app.prompt_area;
    if area.height < 3 {
        // 输入框区域不可用时兜底：贴画面底部三行，卡片语义不变。
        let frame_area = frame.area();
        area = Rect::new(
            frame_area.x,
            frame_area.y + frame_area.height.saturating_sub(3),
            frame_area.width,
            3.min(frame_area.height),
        );
    }
    let confirming = *question >= params.questions.len();
    let header = if confirming {
        "确认回答".to_string()
    } else {
        params
            .questions
            .get(*question)
            .and_then(|entry| entry.header.clone())
            .unwrap_or_else(|| "提问".into())
    };
    // 内宽 = 边框 2 + 左右内边距 2。
    let inner = usize::from(area.width.saturating_sub(4));
    let (right, right_keys) =
        labels::hint_layout(&[("tab", "展开作答"), ("esc", "取消")], usize::MAX);
    let right_width = str_width(&right);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "▸ ",
        Style::new()
            .fg(theme.accent_running)
            .add_modifier(Modifier::BOLD),
    )];
    let mut used = 2usize;
    if params.questions.len() > 1 {
        let counter = if confirming {
            // 确认页折叠时给已答进度：这正是提交前最该看到的信息。
            let answered = (0..params.questions.len())
                .filter(|&index| {
                    selected.get(index).is_some_and(|picked| !picked.is_empty())
                        || custom
                            .get(index)
                            .is_some_and(|answer| !answer.text.is_empty())
                })
                .count();
            format!("已答 {answered}/{} · ", params.questions.len())
        } else {
            format!("问题 {}/{} · ", question + 1, params.questions.len())
        };
        used += str_width(&counter);
        spans.push(Span::styled(counter, Style::new().fg(theme.gray)));
    }
    let header = truncate(
        &header,
        inner
            .saturating_sub(used + right_width + 2)
            .max(4)
            .min(inner.saturating_sub(used)),
    )
    .into_owned();
    used += str_width(&header);
    spans.push(Span::styled(
        header,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    ));
    let inner_rows = usize::from(area.height.saturating_sub(2));
    let top_pad = inner_rows.saturating_sub(1) / 2;
    let mut keys = Vec::new();
    if used + right_width < inner {
        // 细条右端的键位同样可点：Tab 展开作答、Esc 取消整单。
        let x = area.x + 2 + u16::try_from(inner - right_width).unwrap_or(u16::MAX);
        let y = area.y + 1 + u16::try_from(top_pad).unwrap_or(u16::MAX);
        keys.extend(right_keys.into_iter().map(|key| key.at(x, y)));
        spans.push(Span::from(" ".repeat(inner - used - right_width)));
        spans.push(Span::styled(right, Style::new().fg(theme.gray_dim)));
    }
    let mut lines = vec![Line::from(String::new()); top_pad];
    lines.push(Line::from(spans));
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_panel).fg(theme.text_primary),
    );
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.accent_running))
                .style(Style::new().bg(theme.bg_panel).fg(theme.text_primary))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
    app.ask_strip_area = area;
    app.key_hits = keys;
}

/// 快捷键帮助：键位列与描述列之间的间距（格）。
const HELP_KEY_GAP: usize = 2;
/// 快捷键帮助：两列之间的间距（格）。
const HELP_COLUMN_GAP: usize = 4;
/// 快捷键帮助：快捷键行左缩进（格）。
const HELP_INDENT: usize = 2;
/// 快捷键帮助：键位列宽上限（格）。用户自定义的多键绑定可能很长，超出上限
/// 单列时键位独占一行、描述悬挂缩进，不让一个长键位撑宽整列。
const HELP_KEY_MAX: usize = 28;

/// 一列快捷键的（键位列宽，整列宽）。
type HelpColumn = (usize, usize);

struct HelpSection {
    title: String,
    rows: Vec<(String, String)>,
}

/// 快捷键一览：通用键位、输入与编辑、旁路交互三组，组标题加粗着色；
/// 末尾一行指路斜杠命令（输入 `/` 即见全表，不占本表篇幅）。
///
/// 每个分组独立测量键位列与描述列，再各自决定双列或单列：双列按组内自然列宽
/// 左右排布；单列把描述按剩余宽度折行、悬挂缩进到描述列，超过 [`HELP_KEY_MAX`]
/// 的键位独占一行。行尾不补空格，返回内容的自然宽度，模态据此收窄到刚好容纳。
/// 只产出正文；滚动窗口与底部提示由 `modal_body` 负责。
fn help_lines(app: &App, max_width: usize) -> (Vec<Line<'static>>, usize) {
    let theme = app.theme;
    let get_key = |act: &str| -> String {
        app.preferences
            .keybindings
            .get(act)
            .map_or_else(|| "—".to_owned(), |keys| labels::keys(keys))
    };
    let fixed_keys =
        |list: &[&str]| labels::keys(&list.iter().map(|key| (*key).to_owned()).collect::<Vec<_>>());
    let mut sections = vec![HelpSection {
        title: "会话与通用".to_owned(),
        rows: vec![
            (get_key("model"), "选择模型".to_owned()),
            (get_key("engine"), "切换会话引擎".to_owned()),
            (get_key("config"), "打开设置".to_owned()),
            (get_key("agents"), "唤出子代理侧栏".to_owned()),
            (get_key("resume"), "恢复会话".to_owned()),
            (get_key("thinking"), "切换思考显示档位".to_owned()),
            (get_key("thinking_level"), "切换思考等级".to_owned()),
            (get_key("tools"), "切换工具显示档位".to_owned()),
            (get_key("help"), "快捷键一览".to_owned()),
        ],
    }];
    sections.push(HelpSection {
        title: "输入与编辑".to_owned(),
        rows: vec![
            (get_key("send"), "发送消息".to_owned()),
            (get_key("queue"), "排队发送".to_owned()),
            (get_key("newline"), "插入换行".to_owned()),
            (get_key("cancel"), "停止当前生成 / 退出".to_owned()),
            (get_key("focus"), "切换输入焦点".to_owned()),
            (get_key("clear"), "清空输入".to_owned()),
            (get_key("line_start"), "移动到行首".to_owned()),
            (get_key("line_end"), "移动到行尾".to_owned()),
            (get_key("delete_word"), "删除前一词".to_owned()),
            (get_key("delete_to_end"), "删除至行尾".to_owned()),
            (get_key("paste_image"), "粘贴图片或文本".to_owned()),
            (get_key("queue_edit"), "编辑排队中的消息".to_owned()),
        ],
    });
    sections.push(HelpSection {
        title: "旁路交互 (BTW)".to_owned(),
        rows: vec![
            (
                fixed_keys(&["alt+b", "tab"]),
                "旁路展开与焦点切换".to_owned(),
            ),
            (
                fixed_keys(&["esc", "ctrl+c"]),
                "收起旁路 / 取消生成".to_owned(),
            ),
            (fixed_keys(&["c", "x"]), "复制回答 / 清空旁路".to_owned()),
        ],
    });
    let sections = sections;

    let mut lines = Vec::new();
    // 内容自然宽度：各分组实际排版宽度的最大值。
    let mut natural = 0usize;
    for (sec_idx, section) in sections.iter().enumerate() {
        if sec_idx > 0 {
            lines.push(Line::from(String::new()));
        }
        lines.push(Line::from(Span::styled(
            section.title.clone(),
            Style::new()
                .fg(theme.accent_warning)
                .add_modifier(Modifier::BOLD),
        )));

        // 每组各自测量：组内的长键位不撑宽其他组的键位列。
        let (key_max, desc_max) = measure_section(&section.rows);
        let key_width = key_max.min(HELP_KEY_MAX);
        let column: HelpColumn = (key_width, key_width + HELP_KEY_GAP + desc_max);
        // 双列要求所有键位都装得进列宽上限，且两份列宽加间距不超过可用宽度。
        let two_columns = key_max <= HELP_KEY_MAX
            && HELP_INDENT + column.1 + HELP_COLUMN_GAP + column.1 <= max_width;

        if two_columns {
            natural = natural.max(HELP_INDENT + column.1 + HELP_COLUMN_GAP + column.1);
            let split = section.rows.len().div_ceil(2);
            for index in 0..split {
                let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
                spans.extend(help_cell(&section.rows[index], column, theme));
                if let Some(right) = section.rows.get(split + index) {
                    spans.push(Span::styled(" ".repeat(HELP_COLUMN_GAP), Style::new()));
                    spans.extend(help_cell(right, column, theme));
                }
                lines.push(Line::from(spans));
            }
            continue;
        }

        // 单列：描述按剩余宽度折行、悬挂缩进到描述列；行尾不补空格。
        natural = natural.max(HELP_INDENT + (key_width + HELP_KEY_GAP + desc_max).max(key_max));
        let desc_x = HELP_INDENT + key_width + HELP_KEY_GAP;
        let desc_w = max_width.saturating_sub(desc_x).max(1);
        for (key, description) in &section.rows {
            let mut parts = crate::render::wrap::wrap_text(description, desc_w).into_iter();
            let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
            spans.push(Span::styled(
                key.clone(),
                Style::new().fg(theme.accent_user),
            ));
            if str_width(key) <= key_width {
                // 键位装进列宽：首段描述与键位同行。
                spans.push(Span::styled(
                    " ".repeat(key_width - str_width(key) + HELP_KEY_GAP),
                    Style::new(),
                ));
                spans.push(Span::styled(
                    parts.next().unwrap_or_default(),
                    Style::new().fg(theme.text_secondary),
                ));
            }
            lines.push(Line::from(spans));
            // 超长键位独占一行；其余段落统一悬挂缩进到描述列。
            for part in parts {
                lines.push(Line::from(vec![
                    Span::styled(" ".repeat(desc_x), Style::new()),
                    Span::styled(part, Style::new().fg(theme.text_secondary)),
                ]));
            }
        }
    }
    // 末尾指路斜杠命令：输入 `/` 即见全表，不放进本表。
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "输入 / 查看全部斜杠命令",
        Style::new().fg(theme.gray_dim),
    )));
    // 极窄终端连单列都放不下时截断，卡片不二次折行。
    let lines = lines
        .into_iter()
        .map(|line| clip_line(line, max_width))
        .collect();
    (lines, natural.min(max_width))
}

/// 一组快捷键的自然宽度：最长键位与最长描述（格）。
fn measure_section(rows: &[(String, String)]) -> (usize, usize) {
    let key = rows
        .iter()
        .map(|(key, _)| str_width(key))
        .max()
        .unwrap_or(0);
    let description = rows
        .iter()
        .map(|(_, description)| str_width(description))
        .max()
        .unwrap_or(0);
    (key, description)
}

/// 帮助表的一格：键位右补齐到列内 `key_width`，整格再补齐到列宽。
fn help_cell(
    row: &(String, String),
    column: HelpColumn,
    theme: &crate::theme::Theme,
) -> [Span<'static>; 4] {
    let (key_width, column_width) = column;
    let (key, description) = row;
    let key_cells = str_width(key);
    let description_cells = str_width(description);
    // 键位先补齐到 key_width，整格再补齐到列宽，同一列的描述才能上下对齐。
    let pad = column_width.saturating_sub(key_width + HELP_KEY_GAP + description_cells);
    [
        Span::styled(key.clone(), Style::new().fg(theme.accent_user)),
        Span::styled(
            " ".repeat(key_width.saturating_sub(key_cells) + HELP_KEY_GAP),
            Style::new(),
        ),
        Span::styled(
            (*description).to_owned(),
            Style::new().fg(theme.text_secondary),
        ),
        Span::styled(" ".repeat(pad), Style::new()),
    ]
}
/// 上下文窗：分块色标与分块名的左缩进（格）。
const CONTEXT_INDENT: usize = 2;
/// 上下文窗：分块名列宽（格）。
const CONTEXT_NAME_COL: usize = 16;
/// 上下文窗：token 列宽（格，右对齐）。
const CONTEXT_VALUE_COL: usize = 7;
/// 上下文窗：占比列宽（格，右对齐）。
const CONTEXT_PERCENT_COL: usize = 6;
/// 上下文窗：token 列与占比列之间的间距（格）。
const CONTEXT_GAP: usize = 2;

/// 占比：保留一位小数；非零但不足 0.1% 显示 `<0.1%`，不缩成 0.0%。
pub(crate) fn share_percent(part: u64, total: u64) -> String {
    let percent = part as f64 * 100.0 / total.max(1) as f64;
    if percent > 0.0 && percent < 0.05 {
        "<0.1%".to_owned()
    } else {
        format!("{percent:.1}%")
    }
}

/// 上下文占用明细：分块列表 + 按窗口刻度的分块色条与边界标注。
///
/// 列表每块以 `▌` 色标起头，颜色与色条上对应那段一致；色条按上下文窗口定标，
/// 因此空会话就是一整条暗色轨道，已用部分永远落在左侧。边界标注落在各分块
/// 右界，只在不重叠处显示，并优先保住「已用总量」与窗口上限两条。
pub(super) fn context_lines(
    usage: &ContextUsageResult,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let total = usage.total_tokens;
    let window = usage.context_window;
    let mut lines = Vec::with_capacity(usage.sections.len() + 6);
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
        Span::styled(
            crate::render::format_tokens(total),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {}", crate::render::format_tokens(window)),
            Style::new().fg(theme.gray),
        ),
        Span::styled(
            format!(" · {}", share_percent(total, window)),
            Style::new().fg(theme.gray),
        ),
    ]));
    lines.push(Line::from(String::new()));
    for (index, section) in usage.sections.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
            Span::styled("▌ ", Style::new().fg(theme.chart(index))),
            Span::styled(
                format_section_name(&section.name, CONTEXT_NAME_COL),
                Style::new().fg(theme.text_secondary),
            ),
            Span::styled(
                format!(
                    "{:>CONTEXT_VALUE_COL$}",
                    crate::render::format_tokens(section.tokens)
                ),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled(" ".repeat(CONTEXT_GAP), Style::new()),
            Span::styled(
                format!(
                    "{:>CONTEXT_PERCENT_COL$}",
                    share_percent(section.tokens, total)
                ),
                Style::new().fg(theme.gray),
            ),
        ]));
    }
    lines.push(Line::from(String::new()));
    // 分块右界：累积 token 按 `max(已用, 窗口)` 折算成格位；累积单调所以格位不回头
    let cells = width.saturating_sub(CONTEXT_INDENT).max(1);
    let scale = window.max(total).max(1);
    let cells_u64 = u64::try_from(cells).unwrap_or(u64::MAX);
    let mut marks = Vec::with_capacity(usage.sections.len());
    let mut cumulative = 0u64;
    for section in &usage.sections {
        cumulative = cumulative.saturating_add(section.tokens);
        let cell = cumulative
            .saturating_mul(cells_u64)
            .saturating_add(scale / 2)
            / scale;
        marks.push((
            cumulative,
            usize::try_from(cell).unwrap_or(cells).min(cells),
        ));
    }
    lines.push(context_bar_line(&marks, cells, theme));
    lines.push(context_scale_line(&marks, total, window, cells, theme));
    lines
}

/// 分块色条：各分块按格位铺 `━`（颜色与列表色标一致），窗口剩余部分铺暗色 `─`。
fn context_bar_line(marks: &[(u64, usize)], cells: usize, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::with_capacity(marks.len() + 2);
    spans.push(Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()));
    let mut drawn = 0usize;
    for (index, &(_, boundary)) in marks.iter().enumerate() {
        if boundary > drawn {
            spans.push(Span::styled(
                "━".repeat(boundary - drawn),
                Style::new().fg(theme.chart(index)),
            ));
            drawn = boundary;
        }
    }
    if drawn < cells {
        spans.push(Span::styled(
            "─".repeat(cells - drawn),
            Style::new().fg(theme.border),
        ));
    }
    Line::from(spans)
}

/// 分块边界标注：写出各分块右界的累积用量，窗口上限贴住色条末端。
fn context_scale_line(
    marks: &[(u64, usize)],
    total: u64,
    window: u64,
    cells: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut taken = vec![false; cells];
    let mut placed: Vec<(usize, String, Color)> = Vec::with_capacity(marks.len() + 1);
    if total > 0
        && let Some(&(_, boundary)) = marks.last()
    {
        push_label(
            &mut taken,
            &mut placed,
            boundary,
            crate::render::format_tokens(total),
            theme.chart(marks.len() - 1),
        );
    }
    let cap = crate::render::format_tokens(window);
    push_label(&mut taken, &mut placed, cells, cap, theme.gray_dim);
    for (index, &(cumulative, boundary)) in marks.iter().enumerate() {
        let text = crate::render::format_tokens(cumulative);
        if placed.iter().any(|(_, existing, _)| *existing == text) {
            continue;
        }
        push_label(&mut taken, &mut placed, boundary, text, theme.chart(index));
    }
    placed.sort_unstable_by_key(|(start, ..)| *start);
    let mut spans = Vec::with_capacity(placed.len() * 2 + 2);
    spans.push(Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()));
    let mut cursor = 0usize;
    for (start, text, color) in placed {
        let width = str_width(&text);
        if start > cursor {
            spans.push(Span::styled(" ".repeat(start - cursor), Style::new()));
        }
        cursor = start + width;
        spans.push(Span::styled(text, Style::new().fg(color)));
    }
    if cursor < cells {
        spans.push(Span::styled(" ".repeat(cells - cursor), Style::new()));
    }
    Line::from(spans)
}

/// 分块显示名格式化：按终端显示宽度对齐；超过列宽时截断加省略号，不足时补空格。
fn format_section_name(name: &str, target_width: usize) -> String {
    let current_width = str_width(name);
    if current_width <= target_width {
        format!("{name}{}", " ".repeat(target_width - current_width))
    } else {
        truncate(name, target_width).into_owned()
    }
}

/// 落一个边界标注：以分块右界为锚点右对齐（使数值落位在该分块色条区间的末端）；
/// 若数值为 0k 则以起点左对齐。空间不足或与已落位的标注相碰时直接放弃。
fn push_label(
    taken: &mut [bool],
    placed: &mut Vec<(usize, String, Color)>,
    target_boundary: usize,
    text: String,
    color: Color,
) {
    let width = str_width(&text);
    let cells = taken.len();
    if width == 0 || width > cells {
        return;
    }
    let position = if target_boundary == 0 {
        0
    } else {
        target_boundary.min(cells).saturating_sub(width)
    };
    let end = (position + width).min(cells);
    let position = end.saturating_sub(width);
    let clear = !taken[position..end].iter().any(|cell| *cell)
        && (position == 0 || !taken[position - 1])
        && (end == cells || !taken[end]);
    if clear {
        taken[position..end].fill(true);
        placed.push((position, text, color));
    }
}

/// 提问预览并排布局的最小内容宽度（列）；更窄的终端把预览落到选项下方整宽渲染。
const ASK_PREVIEW_MIN_WIDTH: usize = 72;
/// 并排布局左栏（选项列表）的最小宽度（列）。
const ASK_PREVIEW_LEFT_MIN: usize = 16;
/// 并排布局两栏之间的分隔；其显示宽度即两栏的间距。
const ASK_PREVIEW_SEPARATOR: &str = " │ ";

/// 测试用：正文 + 提示条按阅读顺序拼成一列，断言文案时不必区分两处。
#[cfg(test)]
fn modal_lines(app: &App, modal: &Modal, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    let body = modal_body(app, modal, width, max_lines);
    body.lines
        .into_iter()
        .chain(body.footer.map(|footer| footer.line))
        .collect()
}

/// 列表窗口起点：沿用选择器自己的偏移，但保证选中项落在 `visible` 行之内。
///
/// 键盘处理按固定翻页步长维护 `offset`，真实可见行数只有渲染知道，两边在此对齐。
pub(crate) fn window_offset(offset: usize, selected: usize, visible: usize, total: usize) -> usize {
    let visible = visible.max(1);
    let offset = if selected < offset {
        selected
    } else if selected >= offset + visible {
        selected + 1 - visible
    } else {
        offset
    };
    offset.min(total.saturating_sub(visible))
}

/// 搜索框行：`搜索：<query>▏`；查询超宽时从头部截断，光标侧始终可见。
fn search_line(query: &str, width: usize, theme: &Theme) -> Line<'static> {
    const LABEL: &str = "搜索：";
    let budget = width.saturating_sub(str_width(LABEL) + 1);
    Line::from(vec![
        Span::styled(LABEL, Style::new().fg(theme.gray_dim)),
        Span::styled(
            truncate_head(query, budget).into_owned(),
            Style::new().fg(theme.accent_model),
        ),
        Span::styled("▏", Style::new().fg(theme.accent_model)),
    ])
}

/// 说明/错误文本按正文宽度折成整行，样式统一。
fn wrapped_lines(text: &str, width: usize, style: Style) -> Vec<Line<'static>> {
    crate::render::wrap::wrap_text(text, width)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, style)))
        .collect()
}

/// 通用模态的正文与提示条（按类型分派）。
///
/// `width` 是卡片正文宽度，`max_lines` 是正文可见行数（提示条另算）：
/// 返回的每行都不超过 `width` 列、总行数不超过 `max_lines`。
fn modal_body(app: &App, modal: &Modal, width: usize, max_lines: usize) -> ModalBody {
    let mut scroll_max = 0;
    let mut applied_offset = None;
    let theme = app.theme;
    let dim = Style::new().fg(theme.gray_dim);
    let mut hits = Vec::new();
    let (mut lines, footer) = match modal {
        Modal::Permission {
            params,
            selected,
            scroll_offset,
            ..
        } => {
            let mut all_content = Vec::new();
            if let Some(body) = &params.body {
                let style = Style::new().fg(theme.text_secondary);
                for row in body.lines() {
                    all_content.extend(
                        crate::render::wrap::wrap_text(row, width.saturating_sub(2))
                            .into_iter()
                            .map(|part| Line::from(Span::styled(format!("  {part}"), style))),
                    );
                }
            }
            if let Some(diff) = &params.diff {
                all_content.extend(crate::render::diff::render_diff(
                    diff,
                    width,
                    theme,
                    usize::MAX,
                ));
            }
            // 正文窗口之后：空行 + 选项行。超出窗口时末行是标准滚动状态行，
            // 用户能看到内容被截断而不是静默丢尾。
            let options_rows = params.options.len() + 1;
            let max_content_rows = max_lines.saturating_sub(options_rows).max(1);
            scroll_max = scroll_window_max(all_content.len(), max_content_rows);
            let mut lines =
                scroll_window(all_content, max_content_rows, *scroll_offset, width, theme);
            if lines.is_empty() {
                lines.push(Line::from(String::new()));
            }
            lines.push(Line::from(String::new()));
            for (index, option) in params.options.iter().enumerate() {
                hits.push((index, lines.len()));
                lines.push(overlay::list_row(
                    &format!("{} {}", index + 1, option.label),
                    index == *selected,
                    width,
                    theme,
                ));
            }
            let footer = overlay::footer(
                &[("1-9", "直选"), ("esc", "按拒绝处理"), ("enter", "确认")],
                width,
                theme,
            );
            (lines, footer)
        }
        Modal::Ask { .. } => {
            let layout = ask_layout(theme, modal, width, max_lines);
            (layout.lines, layout.footer)
        }
        Modal::Model { .. } => {
            let (body, _) = model_picker::body(app, modal, width, max_lines);
            hits.extend(body.hits);
            (body.lines, body.footer.unwrap_or_default())
        }
        Modal::SwitchModel {
            selector,
            overflow,
            option,
            loading,
            error,
            ..
        } => {
            let window = crate::render::format_tokens(overflow.context_window);
            // 思考关闭时省掉 `:off` 后缀，与其他位置的选择器写法一致。
            let selector = crate::app::model_selector_label(selector);
            let leading = if overflow.tokens > overflow.context_window {
                format!(
                    "当前上下文 {} 超过 {selector} 的窗口 {window}",
                    crate::render::format_tokens(overflow.tokens)
                )
            } else {
                format!(
                    "当前上下文 {} 超过 {selector} 的压缩触发线 {}（窗口 {window}）",
                    crate::render::format_tokens(overflow.tokens),
                    crate::render::format_tokens(overflow.threshold)
                )
            };
            let mut lines = wrapped_lines(&leading, width, Style::new().fg(theme.text_primary));
            lines.extend(wrapped_lines(
                &format!(
                    "确认后先切换到 {selector}，再由它压缩上下文（压缩后不超过 {}）",
                    crate::render::format_tokens(overflow.target_tokens)
                ),
                width,
                Style::new().fg(theme.text_secondary),
            ));
            lines.push(Line::from(String::new()));
            if *loading {
                lines.push(Line::from(Span::styled(
                    "正在切换并压缩…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = error {
                lines.extend(wrapped_lines(
                    &format!("错误：{error}"),
                    width,
                    Style::new().fg(theme.accent_error),
                ));
            }
            for (index, text) in ["切换并压缩", "取消"].iter().enumerate() {
                hits.push((index, lines.len()));
                lines.push(overlay::list_row(text, index == *option, width, theme));
            }
            let items: &[(&str, &str)] = if error.is_some() {
                &[("enter", "重试"), ("esc", "取消")]
            } else {
                &[("↑↓", "选择"), ("enter", "确认"), ("esc", "取消")]
            };
            (lines, overlay::footer(items, width, theme))
        }
        Modal::Jobs {
            jobs,
            selected,
            pending_kill,
            offset,
            scroll,
            ..
        } => {
            let mut lines = Vec::new();
            if jobs.is_empty() {
                lines.push(Line::from(Span::styled(
                    "当前没有后台任务",
                    Style::new().fg(theme.gray_dim),
                )));
            } else {
                // 键盘/点击选中后开窗口显露选中项；滚轮只动 offset，重绘不拉回。
                scroll_max = jobs.len().saturating_sub(max_lines);
                let offset = if scroll.reveal_selection {
                    window_offset(*offset, *selected, max_lines, jobs.len())
                } else {
                    (*offset).min(scroll.max_offset)
                };
                applied_offset = Some(offset);
                for (index, job) in jobs.iter().skip(offset).take(max_lines).enumerate() {
                    let actual = offset + index;
                    hits.push((actual, lines.len()));
                    let is_selected = actual == *selected;

                    // 状态图标（单宽非 emoji：⏱/⚡ 在 Windows Terminal 会渲染成双宽）
                    let icon = match job.status {
                        moc_protocol::methods::JobStatus::Running => "●",
                        moc_protocol::methods::JobStatus::Killing => "◌",
                        moc_protocol::methods::JobStatus::Exited => "✓",
                        moc_protocol::methods::JobStatus::Killed => "✗",
                        moc_protocol::methods::JobStatus::TimedOut => "◷",
                    };

                    // 命令（截断；按显示宽度，宽字符不切开）
                    let cmd = truncate(&job.command, width.saturating_sub(25));

                    let job_id = &job.job_id;
                    let text = if *pending_kill == Some(actual) {
                        format!("{icon} {job_id} {cmd} [按 x 或点击确认杀掉]")
                    } else {
                        format!("{icon} {job_id} {cmd}")
                    };

                    lines.push(overlay::list_row(&text, is_selected, width, theme));
                }
            }

            let footer = overlay::footer(
                &[("↑↓", "选择"), ("x x/点击已选", "杀任务"), ("esc", "关闭")],
                width,
                theme,
            );
            (lines, footer)
        }
        Modal::WorkflowApproval { state } => {
            let (lines, row_hits) = crate::workflow::approval_lines(state, width, theme);
            // 正文按 body_scroll 开窗：滚轮/翻页只动视口；键盘/点击把焦点行收进窗口。
            let total = lines.len();
            scroll_max = scroll_window_max(total, max_lines);
            let focus_row = row_hits
                .iter()
                .find(|(index, _)| *index == state.focus)
                .map(|(_, row)| *row);
            let start = if state.scroll.reveal_selection {
                focus_row
                    .map(|row| window_offset(state.body_scroll, row, max_lines, total))
                    .unwrap_or_else(|| state.body_scroll.min(scroll_max))
            } else {
                state.body_scroll.min(scroll_max)
            };
            applied_offset = Some(start);
            hits.extend(
                row_hits
                    .into_iter()
                    .filter_map(|(index, row)| row.checked_sub(start).map(|row| (index, row))),
            );
            let windowed: Vec<_> = lines.into_iter().skip(start).take(max_lines).collect();
            let footer = if state.feedback.is_some() {
                overlay::footer(&[("enter", "提交拒绝"), ("esc", "取消输入")], width, theme)
            } else {
                overlay::footer(
                    &[
                        ("↑↓", "选择 role"),
                        ("enter", "选模型"),
                        ("a", "批准"),
                        ("r", "拒绝"),
                        ("esc", "最小化"),
                    ],
                    width,
                    theme,
                )
            };
            (windowed, footer)
        }
        Modal::WorkflowRuns { runs, picker } => {
            let filtered = runs
                .iter()
                .filter(|run| {
                    let query = picker.query.to_lowercase();
                    run.run_id.to_lowercase().contains(&query)
                        || run
                            .title
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&query)
                })
                .collect::<Vec<_>>();
            let mut lines = Vec::new();
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled("没有 workflow run", dim)));
            } else {
                // 键盘/搜索显露选中项；滚轮只动 offset，重绘不拉回。
                scroll_max = filtered.len().saturating_sub(max_lines);
                let offset = if picker.scroll.reveal_selection {
                    window_offset(picker.offset, picker.selected, max_lines, filtered.len())
                } else {
                    picker.offset.min(picker.scroll.max_offset)
                };
                applied_offset = Some(offset);
                for (index, run) in filtered.iter().enumerate().skip(offset).take(max_lines) {
                    hits.push((index, lines.len()));
                    let status = crate::workflow::status_label(run.status);
                    let title = run.title.as_deref().unwrap_or(run.run_id.as_str());
                    lines.push(overlay::list_row(
                        &format!("{status:<14} {title}"),
                        index == picker.selected,
                        width,
                        theme,
                    ));
                }
            }
            let footer = overlay::footer(
                &[("↑↓", "选择"), ("enter", "打开面板"), ("esc", "关闭")],
                width,
                theme,
            );
            (lines, footer)
        }
        Modal::WorkflowRun { state } => {
            let (lines, row_hits) = crate::workflow::run_lines(state, width, theme);
            // 与审批窗同一开窗语义：正文随 body_scroll 滚动，焦点行按需显露。
            let total = lines.len();
            scroll_max = scroll_window_max(total, max_lines);
            let focus_row = row_hits
                .iter()
                .find(|(index, _)| *index == state.focus)
                .map(|(_, row)| *row);
            let start = if state.scroll.reveal_selection {
                focus_row
                    .map(|row| window_offset(state.body_scroll, row, max_lines, total))
                    .unwrap_or_else(|| state.body_scroll.min(scroll_max))
            } else {
                state.body_scroll.min(scroll_max)
            };
            applied_offset = Some(start);
            hits.extend(
                row_hits
                    .into_iter()
                    .filter_map(|(index, row)| row.checked_sub(start).map(|row| (index, row))),
            );
            let windowed: Vec<_> = lines.into_iter().skip(start).take(max_lines).collect();
            let footer = overlay::footer(
                &[
                    ("↑↓", "选择 role"),
                    ("enter", "改模型"),
                    ("s", "停止"),
                    ("r", "续跑"),
                    ("esc", "关闭"),
                ],
                width,
                theme,
            );
            (windowed, footer)
        }
        // 回退视图与统一上下文面板自带布局，`draw_modal` 在此之前就已分派走。
        Modal::Rewind { .. } | Modal::ContextContent { .. } => {
            (Vec::new(), overlay::Footer::default())
        }
        Modal::Sessions {
            sessions,
            cwd,
            scope,
            picker,
        } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query, *scope, cwd);
            let mut lines = vec![search_line(&picker.query, width, theme)];
            if picker.loading && sessions.is_empty() {
                lines.push(Line::from(Span::styled(
                    "正在加载会话…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.extend(wrapped_lines(
                    &format!("错误：{error}"),
                    width,
                    Style::new().fg(theme.accent_error),
                ));
            } else if filtered.is_empty() {
                // 搜索无结果与目录为空要分开说：后者才值得引导切范围。
                let hint = if picker.query.is_empty() && *scope == SessionScope::Folder {
                    labels::hints(&[("", "当前目录没有会话"), ("tab", "查看全部")], width)
                } else {
                    truncate("没有匹配的会话", width).into_owned()
                };
                lines.push(Line::from(Span::styled(hint, Style::new().fg(theme.gray))));
            } else {
                let visible = max_lines.saturating_sub(lines.len()).max(1);
                // 键盘/搜索/切范围显露选中项；滚轮只动 offset，重绘不拉回。
                scroll_max = filtered.len().saturating_sub(visible);
                let offset = if picker.scroll.reveal_selection {
                    window_offset(picker.offset, picker.selected, visible, filtered.len())
                } else {
                    picker.offset.min(picker.scroll.max_offset)
                };
                applied_offset = Some(offset);
                for (index, (_, session)) in filtered.iter().skip(offset).take(visible).enumerate()
                {
                    let actual = offset + index;
                    hits.push((actual, lines.len()));
                    let mut text = session.title.clone();
                    if let Some(source) = crate::modal::fork_source_label(sessions, session) {
                        text.push_str(&format!("  ← {source}"));
                    }
                    if *scope == SessionScope::All {
                        text.push_str(&format!("  {}", truncate(session.cwd.as_str(), width / 2)));
                    }
                    lines.push(overlay::list_row(
                        &text,
                        actual == picker.selected,
                        width,
                        theme,
                    ));
                }
            }
            let items: &[(&str, &str)] = if picker.error.is_some() {
                &[("enter", "重试"), ("esc", "关闭")]
            } else {
                &[
                    ("", "输入搜索"),
                    ("pgup/pgdn", "翻页"),
                    ("↑↓", "移动"),
                    ("tab", "切换范围"),
                    ("enter", "恢复"),
                    ("esc", "关闭"),
                ]
            };
            (lines, overlay::footer(items, width, theme))
        }
        Modal::Plan { state } => {
            plan_lines(state, width, max_lines, theme, &mut hits, &mut scroll_max)
        }
        Modal::Tools {
            tools,
            error,
            scroll,
            ..
        } => {
            let mut rows =
                wrapped_lines("按当前会话状态，下一轮请求将发送以下工具定义。", width, dim);
            rows.push(Line::default());
            if let Some(error) = error {
                rows.extend(wrapped_lines(&format!("读取失败：{error}"), width, dim));
            } else if let Some(tools) = tools {
                if tools.is_empty() {
                    rows.extend(wrapped_lines("当前请求不发送任何工具。", width, dim));
                }
                for tool in tools {
                    rows.extend(wrapped_lines(
                        &tool.name,
                        width,
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ));
                    if let Some(description) = &tool.description {
                        rows.extend(wrapped_lines(description, width, dim));
                    }
                    rows.push(Line::default());
                }
            } else {
                rows.push(Line::from(Span::styled("正在读取工具…", dim)));
            }
            scroll_max = scroll_window_max(rows.len(), max_lines);
            let lines = scroll_window(rows, max_lines, *scroll, width, theme);
            (
                lines,
                overlay::footer(&[("↑↓", "滚动"), ("esc", "关闭")], width, theme),
            )
        }
        Modal::Monitor => {
            let lines = monitor_lines(width, theme);
            (
                lines,
                overlay::footer(
                    &[
                        ("", "10min 窗口 · 1Hz 采样 · 随进程退出清空"),
                        ("esc", "关闭"),
                    ],
                    width,
                    theme,
                ),
            )
        }
        Modal::Help { scroll } => {
            let rows = help_lines(app, width).0;
            scroll_max = scroll_window_max(rows.len(), max_lines);
            let lines = scroll_window(rows, max_lines, *scroll, width, theme);
            (lines, overlay::footer(&[("esc", "关闭")], width, theme))
        }
    };
    lines.truncate(max_lines.max(1));
    ModalBody {
        lines,
        footer: Some(footer),
        scroll_max,
        applied_offset,
        hits,
    }
}

/// 监控卡布局：label 5 列、当前值右对齐 8 列、统计段 26 列，其余全给折线。
const MONITOR_LABEL_W: usize = 5;
const MONITOR_CUR_W: usize = 8;
const MONITOR_STATS_W: usize = 26;
/// sparkline 分档字符；0 档是空格，表示「该时间桶内没有样本」。
const SPARK_CHARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// `/monitor` 正文：内存/CPU 画瞬时值，磁盘与网络各画读/写、收/发两条速率线。
fn monitor_lines(width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme.gray_dim);
    let spark_cols = width.saturating_sub(MONITOR_LABEL_W + MONITOR_CUR_W + MONITOR_STATS_W + 3);
    if spark_cols < 4 {
        return vec![Line::from(Span::styled("终端太窄", dim))];
    }
    let samples = moc_core::procstats::samples();
    if samples.len() < 2 {
        return vec![Line::from(Span::styled("正在采样…", dim))];
    }
    let now = Instant::now();
    let window = Duration::from_secs(moc_core::procstats::WINDOW_SECS);
    let latest = *samples.last().expect("samples 已判非空");

    let row = |label: &str,
               color: Color,
               current: String,
               points: &[(Instant, f64)],
               stats: String|
     -> Line<'static> {
        let label_pad = " ".repeat(MONITOR_LABEL_W.saturating_sub(str_width(label)));
        let cur_pad = " ".repeat(MONITOR_CUR_W.saturating_sub(str_width(&current)));
        Line::from(vec![
            Span::styled(format!("{label}{label_pad}"), dim),
            Span::styled(
                format!("{cur_pad}{current}"),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled(
                format!(" {}", sparkline(points, spark_cols, window, now)),
                Style::new().fg(color),
            ),
            Span::styled(format!(" {stats}"), dim),
        ])
    };
    let summary = |points: &[(Instant, f64)]| {
        let (sum, peak) = points
            .iter()
            .fold((0.0, 0.0f64), |(sum, peak), (_, v)| (sum + v, peak.max(*v)));
        (sum / points.len().max(1) as f64, peak)
    };
    let instant = |field: fn(&moc_core::procstats::Sample) -> f64| -> Vec<(Instant, f64)> {
        samples.iter().map(|s| (s.at, field(s))).collect()
    };
    let rate_line = |label: &str,
                     color: Color,
                     field: fn(&moc_core::procstats::Sample) -> u64,
                     total: u64|
     -> Line<'static> {
        let points = moc_core::procstats::rates(&samples, field);
        let current = points.last().map(|(_, rate)| *rate).unwrap_or(0.0);
        row(
            label,
            color,
            format!("{}/s", format_bytes(current as u64)),
            &points,
            format!("total {}", format_bytes(total)),
        )
    };

    let rss = instant(|s| s.rss_bytes as f64);
    let (avg, peak) = summary(&rss);
    let mut lines = vec![row(
        "内存",
        theme.chart_1,
        format_bytes(latest.rss_bytes),
        &rss,
        format!(
            "avg {}  peak {}",
            format_bytes(avg as u64),
            format_bytes(peak as u64)
        ),
    )];
    let cpu = instant(|s| f64::from(s.cpu_pct));
    let (avg, peak) = summary(&cpu);
    lines.push(row(
        "CPU",
        theme.chart_2,
        format!("{:.0}%", latest.cpu_pct),
        &cpu,
        format!("avg {avg:.0}%  peak {peak:.0}%"),
    ));
    lines.push(Line::default());
    lines.push(rate_line(
        "磁盘↓",
        theme.chart_3,
        |s| s.disk_read_bytes,
        latest.disk_read_bytes,
    ));
    lines.push(rate_line(
        "磁盘↑",
        theme.chart_3,
        |s| s.disk_write_bytes,
        latest.disk_write_bytes,
    ));
    lines.push(Line::default());
    lines.push(rate_line(
        "网络↓",
        theme.chart_4,
        |s| s.net_rx_bytes,
        latest.net_rx_bytes,
    ));
    lines.push(rate_line(
        "网络↑",
        theme.chart_4,
        |s| s.net_tx_bytes,
        latest.net_tx_bytes,
    ));
    lines
}

/// 字节量人类可读：`512B`、`45KB`、`1.2GB`（1024 进制；不足 10 单位带一位小数）。
fn format_bytes(value: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = value as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit + 1 < UNITS.len() {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 || v >= 10.0 {
        format!("{v:.0}{}", UNITS[unit])
    } else {
        format!("{v:.1}{}", UNITS[unit])
    }
}

/// 折线：按时刻把值投进 `columns` 列的固定窗口桶（每桶取峰值，空桶空格），
/// 全序列按自身峰值归一到 8 档。points 按时刻升序。
fn sparkline(points: &[(Instant, f64)], columns: usize, window: Duration, now: Instant) -> String {
    debug_assert!(columns > 0);
    let bucket = window.as_secs_f64() / columns as f64;
    let start = now.checked_sub(window).unwrap_or(now);
    let mut peaks = vec![0.0f64; columns];
    let mut filled = vec![false; columns];
    for &(at, value) in points {
        let col = (at.saturating_duration_since(start).as_secs_f64() / bucket) as usize;
        if col < columns {
            filled[col] = true;
            peaks[col] = peaks[col].max(value);
        }
    }
    let peak = peaks.iter().copied().fold(0.0, f64::max);
    let mut out = String::with_capacity(columns * 3);
    for col in 0..columns {
        if !filled[col] {
            out.push(SPARK_CHARS[0]);
        } else if peak <= 0.0 || peaks[col] <= 0.0 {
            out.push(SPARK_CHARS[1]);
        } else {
            let level = 1 + ((peaks[col] / peak) * 7.0).round().min(7.0) as usize;
            out.push(SPARK_CHARS[level]);
        }
    }
    out
}

/// 提问卡片的布局产物：正文行 + 提示条 + 文本坐标的可点击几何，`draw_ask_modal` 换算到屏幕。
#[derive(Default)]
struct AskLayout {
    lines: Vec<Line<'static>>,
    footer: overlay::Footer,
    /// (选项下标, 行号)；`options.len()` 下标代表自定义行。
    option_hits: Vec<(usize, usize)>,
    /// (问题下标, 行号, 起始列, 结束列)：顶部问题导航条的点击范围。
    nav_hits: Vec<(usize, usize, usize, usize)>,
    /// 可滚动的问题正文窗口（文本坐标）；`None` = 未溢出，滚轮落在此区仍动光标。
    question_area: Option<Rect>,
    /// 可滚动的详情区（单栏底区或双栏右栏，文本坐标）；同上仅在溢出时给出。
    detail_area: Option<Rect>,
    /// `preview_scroll` 本帧允许的最大偏移，绘制后写回状态。
    preview_scroll_max: usize,
}

/// 问题导航条里单项的最大宽度（格）：`n. header` 超长截断。
const ASK_NAV_ITEM_MAX: usize = 24;
/// 确认页 key 列宽上限（格）：对齐回答正文，超长 header/id 截断。
const ASK_CONFIRM_KEY_MAX: usize = 20;
/// 问题正文窗口的最大行数：长问题滚动展示而不是铺满整卡、把选项挤出视野。
const ASK_QUESTION_ROWS: usize = 5;
/// 聚焦项详情区（单栏底区）内容窗口的最大行数。
const ASK_DETAIL_ROWS: usize = 4;

/// 在最终行列表里的行号，`draw_ask_modal` 据此换算鼠标点选的屏幕矩形。
/// 选项与自定义行一律截断到单行：`Wrap` 折行会让后续行号整体漂移。
fn ask_layout(theme: &Theme, modal: &Modal, width: usize, max_lines: usize) -> AskLayout {
    let Modal::Ask {
        params,
        question,
        highlight,
        selected,
        custom,
        preview_scroll,
        question_scroll,
        ..
    } = modal
    else {
        return AskLayout::default();
    };
    let mut lines = Vec::new();
    let mut nav_hits = Vec::new();
    // `preview_scroll` 的本帧上限：详情窗口或确认页各算各的，绘制后写回。
    let mut preview_scroll_max = 0usize;
    // 多问时顶部是导航条而不是计数器：`n. header` 逐项排开，当前问反白、
    // 已答带 ✓；每一项记下列范围供鼠标跳问。
    if params.questions.len() > 1 {
        let nav_row = lines.len();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        for (index, entry) in params.questions.iter().enumerate() {
            let separator = if index > 0 { str_width(labels::SEP) } else { 0 };
            let answered = selected.get(index).is_some_and(|picked| !picked.is_empty())
                || custom
                    .get(index)
                    .is_some_and(|answer| !answer.text.is_empty());
            let header = entry.header.clone().unwrap_or_else(|| "提问".into());
            let label = truncate(format!("{}. {header}", index + 1), ASK_NAV_ITEM_MAX).into_owned();
            let item_width = str_width(&label) + usize::from(answered) * 2;
            // 放不下的问题收成一个 `…`；连 `…` 都放不下就不画，导航条绝不越过正文宽度。
            if col + separator + item_width > width {
                if col + 2 <= width {
                    spans.push(Span::styled(" …", Style::new().fg(theme.gray_dim)));
                    col += 2;
                }
                break;
            }
            if index > 0 {
                spans.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
                col += separator;
            }
            nav_hits.push((index, nav_row, col, col + item_width));
            if answered {
                spans.push(Span::styled("✓ ", Style::new().fg(theme.accent_success)));
            }
            let style = if index == *question {
                Style::new()
                    .fg(theme.accent_user)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else if answered {
                Style::new().fg(theme.text_secondary)
            } else {
                Style::new().fg(theme.gray)
            };
            spans.push(Span::styled(label, style));
            col += item_width;
        }
        // 导航条末尾是确认页：越过最后一问强制经过，点击或 → 直达。
        let confirm_label = "确认";
        let confirm_width = str_width(confirm_label);
        let separator = str_width(labels::SEP);
        if col + separator + confirm_width <= width {
            spans.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
            nav_hits.push((
                params.questions.len(),
                nav_row,
                col + separator,
                col + separator + confirm_width,
            ));
            let style = if *question == params.questions.len() {
                Style::new()
                    .fg(theme.accent_user)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            spans.push(Span::styled(confirm_label, style));
        }
        lines.push(Line::from(spans));
    }
    let Some(current) = params.questions.get(*question) else {
        return ask_confirm_layout(theme, modal, lines, nav_hits, width, max_lines);
    };
    lines.push(Line::from(String::new()));

    // 问题正文窗口：常规字重、限高滚动——长问题铺满整卡会把选项挤出视野；
    // 溢出时末行给出滚动状态（滚轮/PgUp/PgDn 读全文），窗口区记为可滚命中。
    let question_rows = crate::render::wrap::wrap_text(&current.question, width);
    let question_overflow = question_rows.len() > ASK_QUESTION_ROWS;
    let question_start = lines.len();
    lines.extend(scroll_window(
        question_rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, Style::new().fg(theme.text_primary))))
            .collect(),
        ASK_QUESTION_ROWS,
        *question_scroll,
        width,
        theme,
    ));
    let question_area = question_overflow.then(|| {
        Rect::new(
            0,
            u16::try_from(question_start).unwrap_or(u16::MAX),
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(lines.len() - question_start).unwrap_or(u16::MAX),
        )
    });
    lines.push(Line::from(String::new()));

    // 当前问任一选项带预览就走双栏（问题级判定，焦点移动不跳版）：
    // 左栏恒为单行选项列表，右栏是聚焦项的详情（描述 + 预览）。
    let two_col = current.options.iter().any(|option| {
        option
            .preview
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
    }) && width >= ASK_PREVIEW_MIN_WIDTH;
    let focused_option = current.options.get(highlight[*question]);
    // 单栏时聚焦项描述进底部固定区：分隔线 + 限高窗口。它是次信息——
    // 预算不足保住两个可见选项 + 自定义行时，先压详情区再整个让掉。
    let detail_content: Vec<Line<'static>> = if two_col {
        Vec::new()
    } else {
        focused_option
            .map(|option| focused_detail_lines(option, width, theme))
            .unwrap_or_default()
    };
    // 固定行：自定义行 1（提示条在卡片底部另算）。
    let fixed_tail = lines.len() + 1;
    let mut detail_zone_rows = if detail_content.is_empty() {
        0
    } else {
        1 + detail_content.len().min(ASK_DETAIL_ROWS)
    };
    if detail_zone_rows > 0 && max_lines.saturating_sub(fixed_tail + detail_zone_rows) < 2 {
        detail_zone_rows = max_lines.saturating_sub(fixed_tail + 2);
        // 分隔线 + 至少一行内容才有意义，否则整区让掉。
        if detail_zone_rows < 2 {
            detail_zone_rows = 0;
        }
    }
    // 剩余行预算给选项列表窗口 + 自定义行。
    let opt_budget = max_lines
        .saturating_sub(lines.len() + 1 + detail_zone_rows)
        .max(1);

    // 选项每项恒单行（描述已挪进详情区）；超出预算时窗口跟随光标，
    // 两端以「▲/▼ 还有 N 项」提示被藏起的项数。
    let mut option_rows: Vec<Line<'static>> = Vec::with_capacity(current.options.len());
    for (index, option) in current.options.iter().enumerate() {
        let chosen = selected[*question].contains(&option.label);
        let focused = index == highlight[*question];
        let marker = if focused { "❯" } else { " " };
        // 单选用 radio、多选用 checkbox：勾选框在单选上暗示可多选，是误导。
        let check = match (current.multi, chosen) {
            (true, true) => "☑",
            (true, false) => "☐",
            (false, true) => "●",
            (false, false) => "○",
        };
        let style = if focused {
            Style::new()
                .fg(theme.accent_user)
                .bg(theme.bg_visual)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        // 前缀 " ❯ ☑ " 占 5 列，推荐后缀「（推荐）」占 8 列，都从标签预算里扣除。
        let recommended = current.recommended.contains(&(index as u32));
        let label_budget = width
            .saturating_sub(5)
            .saturating_sub(if recommended { 8 } else { 0 });
        let mut spans = vec![
            Span::styled(format!(" {marker} {check} "), style),
            Span::styled(
                truncate(option.label.as_str(), label_budget).into_owned(),
                style,
            ),
        ];
        if recommended {
            spans.push(Span::styled("（推荐）", Style::new().fg(theme.gray)));
        }
        // 单栏时聚焦行补满正文宽度，选中色带右缘整齐；双栏左栏由 join_columns 补齐。
        let mut row = Line::from(spans);
        if focused && !two_col {
            let used = line_width(&row);
            row.spans
                .push(Span::styled(" ".repeat(width.saturating_sub(used)), style));
        }
        option_rows.push(row);
    }
    let total_opts = current.options.len();
    let (mut start, mut take) = (0usize, total_opts.min(opt_budget));
    if total_opts > opt_budget {
        // 先按两端都有省略指示来估算窗口；落到边缘的指示行让位给选项。
        let take_max = opt_budget.saturating_sub(2).max(1);
        start = highlight[*question].min(total_opts.saturating_sub(take_max));
        take = take_max.min(total_opts - start);
        if start == 0 {
            take = (opt_budget - 1).min(total_opts);
        }
        if start + take >= total_opts {
            take = (opt_budget - usize::from(start > 0)).min(total_opts - start);
        }
    }
    let base = lines.len();
    let mut list: Vec<Line<'static>> = Vec::with_capacity(take + 3);
    let mut option_hits: Vec<(usize, usize)> = Vec::with_capacity(take + 1);
    if start > 0 {
        list.push(scroll_edge_hint('▲', start, theme));
    }
    for (index, row) in option_rows.iter().enumerate().skip(start).take(take) {
        option_hits.push((index, base + list.len()));
        list.push(row.clone());
    }
    if start + take < total_opts {
        list.push(scroll_edge_hint('▼', total_opts - start - take, theme));
    }
    let editing_here = highlight[*question] == current.options.len();
    let answer = custom.get(*question);
    let editing = answer.is_some_and(|answer| answer.editing);
    let custom_style = if editing {
        Style::new()
            .fg(theme.accent_running)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else if editing_here {
        Style::new()
            .fg(theme.accent_user)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.gray)
    };
    let custom_display = match answer {
        Some(answer) if answer.editing => {
            let cursor_pos = clamp_cursor(&answer.text, answer.cursor);
            let (before, after) = answer.text.split_at(cursor_pos);
            format!("{before}▏{after}")
        }
        Some(answer) => answer.text.clone(),
        None => String::new(),
    };
    option_hits.push((current.options.len(), base + list.len()));
    list.push(Line::from(Span::styled(
        truncate(
            format!(
                " {} ✎ 自定义：{custom_display}",
                if editing_here { "❯" } else { " " }
            ),
            width,
        )
        .into_owned(),
        custom_style,
    )));

    let mut detail_area = None;
    if two_col {
        // 双栏：左栏选项列表 + 右栏聚焦项详情（描述在前、预览在后，同一滚动窗）。
        let left_width = list
            .iter()
            .map(line_width)
            .max()
            .unwrap_or(0)
            .clamp(ASK_PREVIEW_LEFT_MIN, width / 2);
        let pane_width = width.saturating_sub(left_width + str_width(ASK_PREVIEW_SEPARATOR));
        let pane_budget = opt_budget + 1;
        let pane_content = focused_option
            .map(|option| focused_detail_lines(option, pane_width, theme))
            .unwrap_or_default();
        let pane_overflow = pane_content.len() > pane_budget;
        preview_scroll_max = scroll_window_max(pane_content.len(), pane_budget);
        let pane = scroll_window(
            pane_content,
            pane_budget,
            *preview_scroll,
            pane_width,
            theme,
        );
        let pane_rows = pane.len();
        let pane_start = lines.len();
        lines.extend(join_columns(list, pane, left_width, theme));
        if pane_overflow {
            detail_area = Some(Rect::new(
                u16::try_from(left_width + str_width(ASK_PREVIEW_SEPARATOR)).unwrap_or(u16::MAX),
                u16::try_from(pane_start).unwrap_or(u16::MAX),
                u16::try_from(pane_width).unwrap_or(u16::MAX),
                u16::try_from(pane_rows).unwrap_or(u16::MAX),
            ));
        }
    } else {
        lines.extend(list);
        if detail_zone_rows > 0 {
            // 底部固定详情区：分隔线与选项列表隔开，内容限高可滚；
            // 容量取预算核定的 detail_zone_rows - 1（可能与 ASK_DETAIL_ROWS 不同）。
            let capacity = detail_zone_rows - 1;
            let detail_start = lines.len();
            lines.push(Line::from(Span::styled(
                "─".repeat(width),
                Style::new().fg(theme.border),
            )));
            let overflow = detail_content.len() > capacity;
            preview_scroll_max = scroll_window_max(detail_content.len(), capacity);
            let windowed = scroll_window(detail_content, capacity, *preview_scroll, width, theme);
            lines.extend(windowed);
            if overflow {
                detail_area = Some(Rect::new(
                    0,
                    u16::try_from(detail_start).unwrap_or(u16::MAX),
                    u16::try_from(width).unwrap_or(u16::MAX),
                    u16::try_from(lines.len() - detail_start).unwrap_or(u16::MAX),
                ));
            }
        }
    }
    // 提示条：方向键、数字键属常规交互不教，只留反直觉的键；
    // 多选最后一问 enter 从切勾变成进确认页，提示如实切换。窄屏先丢跳过/换题。
    let last = *question + 1 >= params.questions.len();
    let mut items: Vec<(&str, &str)> = vec![("shift+x", "跳过")];
    if params.questions.len() > 1 {
        items.push(("tab", "换题"));
    }
    items.push(("esc", "折叠问题卡"));
    match (current.multi, last) {
        (true, true) => items.extend([("space", "勾选"), ("enter", "确认")]),
        (true, false) => items.push(("space/enter", "勾选")),
        (false, _) => items.push(("enter", "选择")),
    }
    AskLayout {
        lines,
        footer: overlay::footer(&items, width, theme),
        option_hits,
        nav_hits,
        question_area,
        detail_area,
        preview_scroll_max,
    }
}

/// `scroll_window` 实际采用的最大滚动偏移：状态行占去一行，偏移上限要少算它。
/// 渲染写回 `scroll` 与此用同一公式，保证事件里的大值一帧内就被收紧。
fn scroll_window_max(total: usize, capacity: usize) -> usize {
    let capacity = capacity.max(1);
    if total <= capacity {
        0
    } else {
        total - (capacity - 1).max(1)
    }
}

/// 把内容截取为最多 `capacity` 行的滚动窗口；放不下时末行换成
/// 「⋯ a-b/total 行 · 滚轮/PgUp/PgDn」状态行（各滚动区同一约定），状态行截到 `width`。
fn scroll_window(
    rows: Vec<Line<'static>>,
    capacity: usize,
    scroll: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let total = rows.len();
    let capacity = capacity.max(1);
    if total <= capacity {
        return rows;
    }
    // 只剩一行时状态行没有意义：留给内容本身。
    let visible = (capacity - 1).max(1);
    let offset = scroll.min(scroll_window_max(total, capacity));
    let mut out: Vec<Line<'static>> = rows.into_iter().skip(offset).take(visible).collect();
    if capacity > 1 {
        let status = format!(
            "{} {}-{}/{total} 行{}{}",
            labels::OMITTED,
            offset + 1,
            offset + visible,
            labels::SEP,
            labels::hints(&[("pgup/pgdn", "滚动")], usize::MAX),
        );
        out.push(Line::from(Span::styled(
            truncate(status, width).into_owned(),
            Style::new().fg(theme.gray_dim),
        )));
    }
    out
}

/// 选项列表窗口的边缘指示：上沿 `▲ 还有 N 项`、下沿 `▼ 还有 N 项`。
fn scroll_edge_hint(arrow: char, hidden: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        format!("   {arrow} 还有 {hidden} 项"),
        Style::new().fg(theme.gray_dim),
    ))
}

/// 聚焦项的详情内容：描述在前（灰字折行）、预览在后（markdown 渲染）。
/// 单栏时进底部固定区，双栏时进右栏；聚焦项两者皆无则返回空。
fn focused_detail_lines(
    option: &moc_protocol::methods::AskOption,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    if let Some(description) = option
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        rows.extend(
            crate::render::wrap::wrap_text(description, width)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, Style::new().fg(theme.gray)))),
        );
    }
    if let Some(preview) = option
        .preview
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        let mut rendered: Vec<Line<'static>> =
            crate::render::markdown::render_markdown(preview, width, theme)
                .into_iter()
                .map(|line| clip_line(line, width))
                .collect();
        while rendered
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
        {
            rendered.pop();
        }
        if !rendered.is_empty() {
            if !rows.is_empty() {
                rows.push(Line::from(String::new()));
            }
            rows.extend(rendered);
        }
    }
    rows
}

/// 确认页布局：越过最后一问强制经过，逐问一行列出全部回答，未回答的用警告色标出。
/// `preview_scroll` 复用为答案列表的滚动偏移；返回值交给 `draw_ask_modal` 照常绘制。
fn ask_confirm_layout(
    theme: &Theme,
    modal: &Modal,
    mut lines: Vec<Line<'static>>,
    nav_hits: Vec<(usize, usize, usize, usize)>,
    width: usize,
    max_lines: usize,
) -> AskLayout {
    let Modal::Ask {
        params,
        selected,
        custom,
        preview_scroll,
        ..
    } = modal
    else {
        return AskLayout {
            lines,
            nav_hits,
            ..AskLayout::default()
        };
    };
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "确认回答",
        Style::new()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::BOLD),
    )));
    let rows = max_lines.saturating_sub(lines.len()).max(1);
    // key 列对齐：取 header 兜底 id，统一截断到同一列宽。
    let key_width = params
        .questions
        .iter()
        .map(|entry| str_width(entry.header.as_deref().unwrap_or(&entry.id)))
        .max()
        .unwrap_or(0)
        .min(ASK_CONFIRM_KEY_MAX);
    let mut answers = Vec::with_capacity(params.questions.len());
    for (index, entry) in params.questions.iter().enumerate() {
        let key = truncate(entry.header.as_deref().unwrap_or(&entry.id), key_width).into_owned();
        let pad = " ".repeat(key_width.saturating_sub(str_width(&key)));
        let mut spans = vec![Span::styled(
            format!("{key}{pad}: "),
            Style::new().fg(theme.text_secondary),
        )];
        // 勾选集合 + 自定义文本都算回答；都没有就是未回答。
        let mut parts = selected.get(index).cloned().unwrap_or_default();
        if let Some(text) = custom
            .get(index)
            .map(|answer| answer.text.trim())
            .filter(|text| !text.is_empty())
        {
            parts.push(text.to_string());
        }
        if parts.is_empty() {
            spans.push(Span::styled(
                "未回答",
                Style::new()
                    .fg(theme.accent_warning)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                truncate(parts.join(", "), width.saturating_sub(key_width + 2)).into_owned(),
                Style::new().fg(theme.text_primary),
            ));
        }
        answers.push(Line::from(spans));
    }
    let scroll_max = answers.len().saturating_sub(rows);
    let scroll = (*preview_scroll).min(scroll_max);
    lines.extend(answers.into_iter().skip(scroll).take(rows));
    AskLayout {
        lines,
        footer: overlay::footer(
            &[
                ("shift+x", "跳过"),
                ("esc", "折叠问题卡"),
                ("enter", "确认提交"),
            ],
            width,
            theme,
        ),
        nav_hits,
        preview_scroll_max: scroll_max,
        ..AskLayout::default()
    }
}

/// 并排拼接左右两栏：左栏补齐到 `left_width` 列后接竖线分隔，再接右栏。
fn join_columns(
    left: Vec<Line<'static>>,
    right: Vec<Line<'static>>,
    left_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    let rows = left.len().max(right.len());
    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows);
    for _ in 0..rows {
        let mut line = fit_line(
            left.next().unwrap_or_else(|| Line::from(String::new())),
            left_width,
        );
        line.spans.push(Span::styled(
            ASK_PREVIEW_SEPARATOR,
            Style::new().fg(theme.border),
        ));
        line.spans.extend(
            right
                .next()
                .unwrap_or_else(|| Line::from(String::new()))
                .spans,
        );
        out.push(line);
    }
    out
}

// ---------- 计划审阅：阅读 / 批注视图 ----------

/// 计划审阅卡片顶部的固定行数：标题/版本、路径。
pub(crate) const PLAN_HEADER_ROWS: usize = 2;

/// 待审阅计划的动作列表；「在新会话执行」带上所选目标引擎。
fn plan_action_label(state: &PlanReviewState, index: usize) -> String {
    match index {
        0 => "执行（当前会话）".to_owned(),
        1 => "压缩当前会话后执行".to_owned(),
        2 => format!(
            "在新会话执行（{}）",
            crate::app::engine_label(state.execution_engine)
        ),
        _ => "继续计划".to_owned(),
    }
}

const PLAN_ACTIONS_LEN: usize = 4;

/// 状态文字与配色。
fn plan_status_label(theme: &Theme, status: PlanStatus) -> (&'static str, Color) {
    use labels::status;
    match status {
        PlanStatus::Draft => (status::DRAFT, theme.accent_running),
        PlanStatus::Submitted => (status::IN_REVIEW, theme.accent_success),
        PlanStatus::Revised => (status::REVISING, theme.accent_warning),
        PlanStatus::Superseded => (status::SUPERSEDED, theme.gray),
        PlanStatus::Approved => (status::APPROVED, theme.accent_success),
        PlanStatus::Rejected => (status::REJECTED, theme.accent_error),
    }
}

/// 非待审阅版本的说明行。
fn plan_status_note(status: PlanStatus) -> &'static str {
    match status {
        PlanStatus::Draft => "计划仍在起草中。",
        PlanStatus::Revised => "已提交修改意见，等待模型修订。",
        PlanStatus::Superseded => "已被新版本取代。",
        PlanStatus::Approved => "计划已批准。",
        PlanStatus::Rejected => "计划已拒绝。",
        PlanStatus::Submitted => "",
    }
}

/// 正文之外的固定行数；渲染与鼠标命中都走它，两边算出的正文窗口才会一致。
///
/// 头部 [`PLAN_HEADER_ROWS`] 行；正文之后的尾部（动作列表 / 状态说明 / 错误）
/// 非空时先空一行再排。操作提示在卡片底部的提示条里，不占正文行。
pub(crate) fn plan_fixed_rows(state: &PlanReviewState) -> usize {
    let mut tail = 0;
    if state.pane == PlanPane::Read && state.status == PlanStatus::Submitted {
        tail += PLAN_ACTIONS_LEN;
    }
    if state.status != PlanStatus::Submitted {
        tail += 1;
    }
    if state.error.is_some() || state.submitting {
        tail += 1;
    }
    PLAN_HEADER_ROWS + if tail > 0 { tail + 1 } else { 0 }
}

/// 正文可见行数。
pub(crate) fn plan_body_rows(state: &PlanReviewState, max_lines: usize) -> usize {
    max_lines.saturating_sub(plan_fixed_rows(state)).max(1)
}

/// 批注视图当前可见的源码行（0-based，与屏幕行一一对应）。
pub(crate) fn plan_source_window(state: &PlanReviewState, max_lines: usize) -> Vec<usize> {
    let rows = plan_body_rows(state, max_lines);
    let total = state.source_lines().len();
    let offset = state.scroll_offset.min(total.saturating_sub(rows));
    (offset..(offset + rows).min(total)).collect()
}

/// 计划审阅卡片的正文行与提示条。
fn plan_lines(
    state: &PlanReviewState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
    hits: &mut Vec<(usize, usize)>,
    scroll_max: &mut usize,
) -> (Vec<Line<'static>>, overlay::Footer) {
    let mut lines = Vec::with_capacity(max_lines);

    // 状态先占位、标题吃剩下的宽度：标题再长也不会把这一行挤成两行。
    let (status_label, status_color) = plan_status_label(theme, state.status);
    let status = format!("{}{status_label}", labels::SEP);
    lines.push(clip_line(
        Line::from(vec![
            Span::styled(
                truncate(
                    format!("{}{}v{}", state.title, labels::SEP, state.version),
                    width.saturating_sub(str_width(&status)),
                )
                .into_owned(),
                Style::new()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(status, Style::new().fg(status_color)),
        ]),
        width,
    ));
    let mut location = state.path.clone();
    if state.previous_plan_id.is_some() {
        location.push_str(&format!(" · 对比 v{}", state.version.saturating_sub(1)));
    }
    if !state.drafts.is_empty() || !state.submitted.is_empty() {
        location.push_str(&format!(
            " · {} 条待提交 / {} 条已提交",
            state.drafts.len(),
            state.submitted.len()
        ));
    }
    lines.push(Line::from(Span::styled(
        truncate(&location, width).into_owned(),
        Style::new().fg(theme.gray),
    )));

    let body_rows = plan_body_rows(state, max_lines);
    match state.pane {
        PlanPane::Read => {
            if state.show_diff && state.previous_plan_id.is_some() {
                if let Some(error) = &state.diff_error {
                    lines.push(Line::from(Span::styled(
                        truncate(format!("无法加载改动：{error}"), width).into_owned(),
                        Style::new().fg(theme.accent_error),
                    )));
                } else if let Some(diff) = &state.diff {
                    let rendered = crate::render::diff::render_diff(diff, width, theme, usize::MAX);
                    if rendered.is_empty() {
                        lines.push(Line::from(Span::styled(
                            "本版没有文本改动。",
                            Style::new().fg(theme.gray),
                        )));
                    } else {
                        // 事件层只加不减：渲染时按正文行数收紧偏移并写回。
                        *scroll_max = rendered.len().saturating_sub(body_rows);
                        lines.extend(
                            rendered
                                .into_iter()
                                .skip(state.scroll_offset.min(*scroll_max))
                                .take(body_rows),
                        );
                    }
                } else {
                    lines.push(Line::from(Span::styled(
                        "正在加载本版改动…",
                        Style::new().fg(theme.accent_running),
                    )));
                }
            } else {
                let rendered =
                    crate::render::markdown::render_markdown(&state.markdown, width, theme);
                if rendered.is_empty() {
                    lines.push(Line::from(String::new()));
                } else {
                    // 事件层只加不减：渲染时按正文行数收紧偏移并写回。
                    *scroll_max = rendered.len().saturating_sub(body_rows);
                    lines.extend(
                        rendered
                            .into_iter()
                            .skip(state.scroll_offset.min(*scroll_max))
                            .take(body_rows)
                            .map(|line| clip_line(line, width)),
                    );
                }
            }
        }
        PlanPane::Annotate => {
            let source = state.source_lines();
            for index in plan_source_window(state, max_lines) {
                lines.push(plan_source_line(state, &source, index, width, theme));
            }
        }
    }

    // 提示条从左丢项：引擎/模型/批注等次要键先让位，滚动与确认/关闭恒在。
    let mut items: Vec<(&str, &str)> = Vec::with_capacity(8);
    match state.pane {
        PlanPane::Read => {
            let submitted = state.status == PlanStatus::Submitted;
            if submitted {
                items.push(("m", "换引擎"));
            }
            items.push(("a", "批注"));
            if state.previous_plan_id.is_some() {
                items.push((
                    "d",
                    if state.show_diff {
                        "看计划"
                    } else {
                        "看改动"
                    },
                ));
            }
            items.push(("↑↓/pgup/pgdn", "滚动"));
            if submitted {
                items.extend([("←→/tab", "选择"), ("enter", "确认")]);
            }
            items.push(("esc", "关闭"));
        }
        PlanPane::Annotate => items.extend([
            ("ctrl+g", "建议改法"),
            ("x", "删除"),
            ("v", "选行"),
            ("↑↓", "移动"),
            ("c", "批注"),
            ("a/esc", "返回阅读"),
            ("s", "发送"),
        ]),
    }
    let footer = overlay::footer(&items, width, theme);

    let tail_start = lines.len();
    if plan_fixed_rows(state) > PLAN_HEADER_ROWS {
        lines.push(Line::from(String::new()));
    }
    if state.pane == PlanPane::Read && state.status == PlanStatus::Submitted {
        for index in 0..PLAN_ACTIONS_LEN {
            let label = plan_action_label(state, index);
            hits.push((index, lines.len()));
            lines.push(if state.submitting {
                Line::from(Span::styled(
                    fit(&format!("  {label}"), width),
                    Style::new().fg(theme.gray_dim),
                ))
            } else {
                overlay::list_row(&label, state.selected == index, width, theme)
            });
        }
    } else if state.status != PlanStatus::Submitted {
        lines.push(Line::from(Span::styled(
            truncate(plan_status_note(state.status), width).into_owned(),
            Style::new().fg(theme.gray),
        )));
    }

    if let Some(error) = &state.error {
        lines.push(Line::from(Span::styled(
            truncate(format!("错误：{error}"), width).into_owned(),
            Style::new().fg(theme.accent_error),
        )));
    } else if state.submitting {
        lines.push(Line::from(Span::styled(
            "正在提交…",
            Style::new().fg(theme.accent_running),
        )));
    }
    debug_assert_eq!(
        lines.len() - tail_start,
        plan_fixed_rows(state) - PLAN_HEADER_ROWS,
        "尾部行数必须与 plan_fixed_rows 一致，否则正文窗口与鼠标映射错位"
    );
    (lines, footer)
}

/// 批注视图的一行源码：行号 gutter + 内容，选区铺底、光标加粗、有批注的行标 `●`。
fn plan_source_line(
    state: &PlanReviewState,
    source: &[&str],
    index: usize,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let line_no = index + 1;
    let annotated = state.drafts.iter().any(|draft| draft.covers(line_no))
        || state.submitted.iter().any(|annotation| {
            annotation.start_line as usize <= line_no && line_no <= annotation.end_line as usize
        });
    let (sel_start, sel_end) = state.selection();
    let selected = index >= sel_start && index <= sel_end;
    let cursor = index == state.cursor;
    let base = if selected {
        Style::new().bg(theme.bg_visual)
    } else {
        Style::new()
    };
    let gutter = format!("{line_no:>4} {} ", if annotated { "●" } else { "│" });
    let gutter_width = str_width(&gutter);
    let mut text = truncate(
        source.get(index).copied().unwrap_or(""),
        width.saturating_sub(gutter_width),
    )
    .into_owned();
    // 补到整行宽度，让选区底色铺满整行而不是只盖住文字。
    let text_width = str_width(&text);
    if let Some(pad) = width.checked_sub(gutter_width + text_width) {
        text.push_str(&" ".repeat(pad));
    }
    let mut text_style = base.fg(theme.text_primary);
    if cursor {
        text_style = text_style.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(
            gutter,
            base.fg(if annotated {
                theme.accent_warning
            } else {
                theme.gray_dim
            }),
        ),
        Span::styled(text, text_style),
    ])
}

/// 折行后的输入内容与光标位置。
struct WrappedInput {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

/// 多行输入按宽度折行（无掩码），并给出光标所在行与列。
fn wrap_input(input: &crate::widgets::prompt::PromptState, width: usize) -> WrappedInput {
    let width = width.max(1);
    let text = input.text.as_str();
    let prefix = &input.text[..input.cursor];
    let cursor_line = prefix.matches('\n').count();
    let mut lines: Vec<String> = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_column = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == cursor_line {
            let head = prefix.rsplit('\n').next().unwrap_or_default();
            let wrapped = crate::render::wrap::wrap_text(head, width);
            cursor_row = lines.len() + wrapped.len().max(1) - 1;
            cursor_column = wrapped.last().map_or(0, |line| str_width(line));
        }
        let wrapped = crate::render::wrap::wrap_text(line, width);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped);
        }
    }
    WrappedInput {
        lines,
        cursor_row,
        cursor_column,
    }
}

/// 批注编辑器：居中覆盖在模态卡片之上，光标落在正文里；返回 footer 键位的命中区。
fn draw_plan_editor(frame: &mut Frame, app: &App, modal_area: Rect, area: Rect) -> Vec<KeyHit> {
    let theme = app.theme;
    let Some(Modal::Plan { state }) = &app.modal else {
        return Vec::new();
    };
    let Some(editor) = &state.editor else {
        return Vec::new();
    };
    let width = area
        .width
        .saturating_sub(8)
        .clamp(32, 76)
        .min(modal_area.width);
    let inner_width = overlay::content_width(width).max(1);
    let wrapped = wrap_input(&editor.body, inner_width);
    let capacity = overlay::max_body_rows(area, true).max(1);
    // 行预算紧张时先让掉引用，保住标签 + 至少一行正文。
    let quote_rows: Vec<String> = editor
        .quote
        .lines()
        .take(2.min(capacity.saturating_sub(2)))
        .map(|line| truncate(format!("> {line}"), inner_width).into_owned())
        .collect();
    let body_space = capacity.saturating_sub(quote_rows.len() + 1).max(1);
    let start = wrapped
        .cursor_row
        .saturating_sub(body_space.saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(capacity);
    for row in &quote_rows {
        lines.push(Line::from(Span::styled(
            row.clone(),
            Style::new().fg(theme.gray),
        )));
    }
    lines.push(Line::from(Span::styled(
        truncate(
            "批注正文（可用 ```suggestion 围栏给出替换文本）",
            inner_width,
        )
        .into_owned(),
        Style::new().fg(theme.text_secondary),
    )));
    for row in wrapped.lines.iter().skip(start).take(body_space) {
        lines.push(Line::from(Span::styled(
            row.clone(),
            Style::new().fg(theme.text_primary),
        )));
    }
    lines.truncate(capacity);
    let label_rows = quote_rows.len() + 1;
    let layout = overlay::Card {
        title: format!(
            "批注{}第 {}-{} 行",
            labels::SEP,
            editor.start_line,
            editor.end_line
        ),
        body: lines,
        footer: Some(overlay::footer(
            &[
                ("ctrl+g", "插入建议改法"),
                ("esc", "取消"),
                ("ctrl+s", "保存"),
            ],
            inner_width,
            theme,
        )),
    }
    .render(frame, area, width, theme);
    let cursor_row = wrapped.cursor_row.saturating_sub(start);
    let y = layout.body.y + u16::try_from(label_rows + cursor_row).unwrap_or(u16::MAX);
    let x = layout.body.x + u16::try_from(wrapped.cursor_column).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        x.min(layout.body.right().saturating_sub(1)),
        y.min(layout.body.bottom().saturating_sub(1)),
    ));
    layout.keys
}

#[cfg(test)]
mod tests {
    use super::{ask_layout, modal_body, modal_lines};
    use crate::{
        app::App,
        modal::{Modal, PickerState, SessionScope},
        theme::Theme,
    };
    use moc_protocol::methods::{
        AskOption, AskParams, AskQuestion, ContextUsageResult, ContextUsageSection, SessionEngine,
        SessionInfo, SessionPermission,
    };
    use ratatui::style::Color;
    use ratatui::text::Line;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn model_picker_separates_provider_and_context_colors() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let theme = app.theme;
        let modal = Modal::Model {
            entries: vec![crate::modal::ModelEntry {
                selector: "p/model".into(),
                label: "p/model".into(),
                display_name: "Model".into(),
                levels: vec!["off".into(), "high".into()],
                context_window: Some(500_000),
                max_output_tokens: Some(64_000),
                inputs: vec![
                    moc_protocol::methods::ModelInput::Text,
                    moc_protocol::methods::ModelInput::Image,
                ],
                pricing: Some(moc_protocol::methods::ModelPricing {
                    input: 0.15,
                    output: 0.6,
                    cache_read: Some(0.015),
                    cache_write: None,
                }),
                fast_mode: false,
            }],
            current: "p/model:high".into(),
            picker: PickerState::default(),
            level: crate::modal::LevelState::default(),
            detail: Default::default(),
            target: crate::modal::ModelTarget::Session,
        };

        // 列表卡：选中行、供应商次要色与当前模型 ✓。
        let lines = modal_lines(&app, &modal, 80, 20);
        let row = lines
            .iter()
            .find(|line| line_text(line).starts_with("❯ "))
            .expect("模型行");
        let text = line_text(row);
        assert!(text.starts_with("❯ Model"), "{text}");
        assert!(text.contains('✓'), "{text}");
        let provider = row
            .spans
            .iter()
            .find(|span| span.content.contains("p ✓"))
            .expect("供应商次要列");
        assert_eq!(provider.style.fg, Some(theme.text_secondary));

        // 详情卡：规格行带上下文窗口，思考等级控制行带当前值。
        let (detail, hits) = super::model_picker::detail_lines(&app, &modal, 30);
        let texts: Vec<String> = detail.iter().map(line_text).collect();
        assert!(texts.iter().any(|text| text.contains("500k")), "{texts:?}");
        let level_row = hits
            .iter()
            .find(|(row, _)| *row == crate::modal::DetailRow::Level)
            .map(|(_, line)| *line)
            .expect("思考等级控制行");
        assert!(texts[level_row].contains("high"), "{texts:?}");
    }

    /// 模型选择器在任何高度都要保住页脚与至少一条候选：留白随行预算递减，
    /// 页脚永远单行收尾，且每行都不超过正文宽度。
    #[test]
    fn model_picker_fits_short_terminals() {
        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.modal = Some(Modal::Model {
            entries: vec![
                crate::modal::ModelEntry {
                    selector: "p/model".into(),
                    label: "p/model".into(),
                    display_name: "Model".into(),
                    levels: vec!["off".into(), "low".into(), "high".into()],
                    context_window: Some(500_000),
                    max_output_tokens: Some(64_000),
                    inputs: vec![moc_protocol::methods::ModelInput::Text],
                    pricing: None,
                    fast_mode: false,
                },
                crate::modal::ModelEntry {
                    selector: "p/other".into(),
                    label: "p/other".into(),
                    display_name: "Other".into(),
                    levels: vec!["off".into(), "high".into()],
                    context_window: Some(128_000),
                    max_output_tokens: None,
                    inputs: Vec::new(),
                    pricing: None,
                    fast_mode: false,
                },
            ],
            current: "p/model:high".into(),
            picker: PickerState::default(),
            level: crate::modal::LevelState {
                manual: Some("high".into()),
                ..Default::default()
            },
            detail: Default::default(),
            target: crate::modal::ModelTarget::Session,
        });
        let modal = app.modal.as_ref().expect("模型选择器");

        // 提示条在正文预算之外；包括极短窗口的紧凑行。
        for (width, budget) in [(76usize, 18usize), (56, 8), (36, 6), (24, 4)] {
            let body = modal_body(&app, modal, width, budget);
            let texts: Vec<String> = body.lines.iter().map(line_text).collect();
            assert!(
                body.lines.len() <= budget,
                "{width}/{budget} 超出预算：{texts:?}"
            );
            let footer = line_text(&body.footer.as_ref().expect("缺少提示条").line);
            for text in texts.iter().chain([&footer]) {
                assert!(
                    crate::render::wrap::str_width(text) <= width,
                    "{text:?} 超过 {width} 列"
                );
            }
            assert!(texts[0].starts_with("搜索："), "搜索置顶：{texts:?}");
            assert!(
                texts.iter().any(|text| text.starts_with("❯ Model")),
                "{texts:?}"
            );
            assert!(
                footer.contains("Enter 确认") && footer.contains("Esc 关闭"),
                "{footer:?}"
            );
        }
    }

    /// 恢复会话列表：默认只看当前目录且行尾不带目录；分叉来源始终标注，
    /// 来源已不在列表时退回短 id。切到全部目录后行尾才显示 cwd。
    #[test]
    fn session_picker_scopes_to_current_folder_and_marks_fork_provenance() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let session = |id: &str, title: &str, cwd: &str, forked_from: Option<&str>| SessionInfo {
            id: id.into(),
            title: title.into(),
            model: "p/model".into(),
            permission: SessionPermission::Full,
            engine: SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: forked_from.map(str::to_owned),
            agent_preset: None,
            delegation_depth: 0,
            cwd: cwd.into(),
            created_at_ms: 1,
            updated_at_ms: 2,
            lock: None,
        };
        let rows = |scope| -> Vec<String> {
            let modal = Modal::Sessions {
                sessions: vec![
                    session("ses_plain0000001", "原会话", "/tmp/project", None),
                    session(
                        "ses_fork00000001",
                        "回退：原会话",
                        "/tmp/project",
                        Some("ses_plain0000001"),
                    ),
                    session(
                        "ses_orphan0000001",
                        "孤儿分叉",
                        "/tmp/other",
                        Some("ses_gone00000001"),
                    ),
                ],
                cwd: "/tmp/project".into(),
                scope,
                picker: PickerState::default(),
            };
            modal_lines(&app, &modal, 80, 20)
                .iter()
                .map(|line| {
                    line.spans
                        .iter()
                        .map(|span| span.content.as_ref())
                        .collect()
                })
                .collect()
        };
        // 列表行补满正文宽度（选中色带右缘整齐），比较前去掉尾部补白。
        let row = |rows: &[String], needle: &str| {
            rows.iter()
                .find(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("缺少 {needle} 行：{rows:?}"))
                .trim_end()
                .to_owned()
        };

        let folder = rows(SessionScope::Folder);
        assert_eq!(row(&folder, "回退：原会话"), "  回退：原会话  ← 原会话");
        assert!(
            !folder.iter().any(|row| row.contains("孤儿分叉")),
            "当前目录范围不得出现其它目录的会话：{folder:?}"
        );
        assert_eq!(row(&folder, "原会话"), "❯ 原会话");

        let all = rows(SessionScope::All);
        assert_eq!(
            row(&all, "回退：原会话"),
            "  回退：原会话  ← 原会话  /tmp/project"
        );
        assert_eq!(
            row(&all, "孤儿分叉"),
            "  孤儿分叉  ← ses_gone00000001  /tmp/other"
        );
        assert_eq!(row(&all, "原会话"), "❯ 原会话  /tmp/project");
    }

    /// 当前目录没有会话时给出切到全部目录的提示，而不是笼统的「没有匹配」。
    #[test]
    fn session_picker_hints_tab_when_current_folder_is_empty() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = Modal::Sessions {
            sessions: Vec::new(),
            cwd: "/tmp/project".into(),
            scope: SessionScope::Folder,
            picker: PickerState::default(),
        };
        let rows: Vec<String> = modal_lines(&app, &modal, 80, 20)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();
        assert_eq!(modal.title(), "恢复会话（当前目录）");
        assert!(
            rows.iter()
                .any(|row| row.contains("当前目录没有会话 · Tab 查看全部")),
            "{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("Tab 切换范围")),
            "页脚应提示 Tab 的范围切换：{rows:?}"
        );
    }

    /// 上下文窗：分块名称过长或包含宽字符时保持列宽对齐。
    #[test]
    fn context_modal_handles_wide_names() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let usage = ContextUsageResult {
            context_window: 100_000,
            total_tokens: 50_000,
            sections: vec![
                ContextUsageSection {
                    name: "very_long_custom_section_name_overflow".into(),
                    tokens: 30_000,
                },
                ContextUsageSection {
                    name: "中文分块名称".into(),
                    tokens: 20_000,
                },
            ],
        };

        let lines = super::context_lines(&usage, 60, app.theme);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // 验证各分块行总宽度一致，列对齐未被顶破
        assert_eq!(line_width(&texts[2]), line_width(&texts[3]));
    }

    /// 推荐项在标签后显示星号；星号与用户当前勾选状态相互独立，多选可显示多个星号。
    #[test]
    fn ask_recommendations_render_after_option_labels() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let mut modal = ask_modal(0, 0);
        let Modal::Ask {
            params, selected, ..
        } = &mut modal
        else {
            unreachable!("测试模态应为 ask");
        };
        // 多星号只可能出现在多选问题；多选不支持预览，先把 fixture 的预览清掉。
        params.questions[0].multi = true;
        params.questions[0].recommended = vec![0, 1];
        for option in &mut params.questions[0].options {
            option.preview = None;
        }
        selected[0] = vec!["上中下".to_owned()];

        let texts: Vec<String> = modal_lines(&app, &modal, 80, 24)
            .iter()
            .map(line_text)
            .collect();
        let first = texts
            .iter()
            .find(|text| text.contains("上中下（推荐）"))
            .expect("第一个推荐项应带推荐后缀");
        assert!(
            first.contains("☑ 上中下（推荐）"),
            "已选推荐项仍应保留推荐后缀：{first}"
        );
        let second = texts
            .iter()
            .find(|text| text.contains("左右（推荐）"))
            .expect("第二个推荐项应带推荐后缀");
        assert!(
            second.contains("☐ 左右（推荐）"),
            "未选推荐项也应显示推荐后缀：{second}"
        );
    }

    /// 问题正文进限高窗口；聚焦项 description 进底部详情区（非聚焦项不铺满）；
    /// 标题栏只放 header；矮终端详情区先让位，问题正文与选项行保留。
    #[test]
    fn ask_body_renders_question_and_descriptions() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let mut modal = ask_modal(0, 0);
        if let Modal::Ask { params, .. } = &mut modal {
            params.questions[0].header = Some("方案".into());
            params.questions[0].question =
                "这是一个很长很长的问题正文，应当折行展示，而不是挤在标题栏里被截断。".into();
            for option in &mut params.questions[0].options {
                option.preview = None;
            }
            params.questions[0].options[0].description = Some("第一个选项的含义说明".into());
            params.questions[0].options[1].description = Some("第二个选项的含义说明".into());
        }
        assert_eq!(modal.title(), "方案", "标题栏只放 header");

        let texts: Vec<String> = modal_lines(&app, &modal, 40, 24)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("这是一个很长很长的问题正文")),
            "问题正文应出现在正文区：{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|text| text.contains("第一个选项的含义说明")),
            "聚焦项 description 应渲染进详情区：{texts:?}"
        );
        assert!(
            !texts
                .iter()
                .any(|text| text.contains("第二个选项的含义说明")),
            "非聚焦项 description 不铺满列表（渐进披露）：{texts:?}"
        );

        let tight: Vec<String> = modal_lines(&app, &modal, 40, 8)
            .iter()
            .map(line_text)
            .collect();
        assert!(
            tight.iter().any(|text| text.contains("问题正文")),
            "矮终端仍显示问题正文：{tight:?}"
        );
        assert!(
            tight.iter().any(|text| text.contains("上中下")),
            "矮终端仍显示选项行：{tight:?}"
        );
        assert!(
            !tight.iter().any(|text| text.contains("含义说明")),
            "矮终端详情区先让位给选项列表：{tight:?}"
        );
    }

    /// 折叠态：卡片收成占据输入框位的细条，左侧进度与当前问、右侧返回/取消键，
    /// 不再绘制选项行，命中几何只剩细条自身。
    #[test]
    fn ask_parked_collapses_to_strip_over_prompt() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let mut modal = ask_modal(0, 0);
        if let Modal::Ask { params, parked, .. } = &mut modal {
            params.questions[0].header = Some("布局偏好".into());
            *parked = true;
        }
        app.modal = Some(modal);
        app.prompt_area = ratatui::layout::Rect::new(3, 20, 114, 3);
        app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
        let (width, height) = (120u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");

        assert_eq!(app.ask_strip_area, app.prompt_area, "细条应占住输入框位");
        assert!(app.ask_option_hits.is_empty(), "折叠态没有可点选项");
        let text: String = (0..width)
            .filter_map(|x| terminal.backend().buffer().cell((x, 21)))
            .map(|cell| cell.symbol())
            .collect();
        let compact = text.replace(' ', "");
        assert!(compact.contains("布局偏好"), "细条应显示当前问：{text:?}");
        assert!(
            compact.contains("Tab展开作答") && compact.contains("Esc取消"),
            "细条应给出返回与取消的路：{text:?}"
        );
    }

    /// 提问预览：宽内容宽度并排（左选项、右聚焦项详情），窄内容宽度详情进底部署名区；
    /// 预览/描述内容都跟随聚焦项。
    #[test]
    fn ask_preview_joins_columns_when_wide_and_stacks_when_narrow() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = ask_modal(0, 0);

        // 并排：聚焦项与其预览同行，分隔线左缘对齐。
        let wide: Vec<String> = modal_lines(&app, &modal, 100, 24)
            .iter()
            .map(line_text)
            .collect();
        let row = wide
            .iter()
            .find(|text| text.contains("❯ ○ 上中下"))
            .expect("选项行");
        assert!(row.contains("+--+"), "聚焦项预览应与选项同行：{row:?}");
        let offsets: Vec<usize> = wide
            .iter()
            .filter_map(|text| text.split_once(" │ "))
            .map(|(left, _)| crate::render::wrap::str_width(left))
            .collect();
        assert!(offsets.len() >= 3, "选项与预览应共行：{wide:?}");
        assert!(
            offsets.iter().all(|offset| *offset == offsets[0]),
            "左栏宽度应逐行一致：{offsets:?}"
        );
        assert!(
            wide.iter()
                .all(|text| crate::render::wrap::str_width(text) <= 100),
            "并排行不得超出内容宽度：{wide:?}"
        );

        // 窄内容宽度：预览随聚焦项进底部详情区（与描述同一去处），不分栏。
        let narrow: Vec<String> = modal_lines(&app, &modal, 60, 24)
            .iter()
            .map(line_text)
            .collect();
        assert!(!narrow.iter().any(|text| text.contains(" │ ")));
        let divider = narrow
            .iter()
            .position(|text| text.starts_with("───"))
            .expect("窄布局的详情区分隔线");
        assert!(
            narrow[divider..].iter().any(|text| text.contains("+--+")),
            "预览应渲染进分隔线下的详情区：{narrow:?}"
        );

        // 聚焦到无预览的选项：右栏空出但双栏布局保持（预览判定是问题级的）。
        let plain: Vec<String> = modal_lines(&app, &ask_modal(1, 0), 100, 24)
            .iter()
            .map(line_text)
            .collect();
        assert!(!plain.iter().any(|text| text.contains("+--+")));
    }

    /// 提问预览：内容高于窗口时按滚动偏移开窗，末行给出位置与按键，
    /// 偏移越界夹到尾部。
    #[test]
    fn ask_preview_scrolls_and_reports_position() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let mut modal = ask_modal(0, 0);
        let rows = 18;
        let code = (1..=30)
            .map(|line| format!("L{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        if let Modal::Ask { params, .. } = &mut modal {
            params.questions[0].options[0].preview = Some(format!("```\n{code}\n```"));
        }

        let texts = |modal: &Modal| -> Vec<String> {
            modal_lines(&app, modal, 60, rows)
                .iter()
                .map(line_text)
                .collect()
        };
        let head = texts(&modal);
        assert!(head.iter().any(|text| text.contains("L01")), "{head:?}");
        let status = head
            .iter()
            .position(|text| text.contains("PgUp/PgDn 滚动"))
            .unwrap_or_else(|| panic!("超窗详情区应给滚动提示：{head:?}"));
        assert_eq!(
            status,
            head.len() - 2,
            "滚动提示应在详情区末行（其后只有提示条）：{head:?}"
        );

        if let Modal::Ask { preview_scroll, .. } = &mut modal {
            *preview_scroll = 8;
        }
        let scrolled = texts(&modal);
        assert!(
            !scrolled.iter().any(|text| text.contains("L01")),
            "滚过首行后不应再显示 L01：{scrolled:?}"
        );
        assert!(
            scrolled.iter().any(|text| text.contains("L09")),
            "{scrolled:?}"
        );

        if let Modal::Ask { preview_scroll, .. } = &mut modal {
            *preview_scroll = usize::MAX;
        }
        let tail = texts(&modal);
        assert!(tail.iter().any(|text| text.contains("L30")), "{tail:?}");
        assert!(
            tail.iter()
                .all(|text| !text.contains("L00") && !text.contains("L31")),
            "{tail:?}"
        );
    }

    /// 长问题进限高窗口（滚动状态行 + question_area 命中区）；选项超预算时
    /// 窗口跟随光标、两端给出 ▲/▼ 隐藏项数提示；命中矩形只记可见选项。
    #[test]
    fn ask_windows_overflowing_question_and_options() {
        let (events, _receiver) = unbounded_channel();
        let _app = App::new(Theme::global(), events);
        let theme = Theme::global();
        let mut modal = ask_modal(0, 0);
        if let Modal::Ask { params, .. } = &mut modal {
            params.questions[0].question = "很长的问".repeat(30);
            params.questions[0].options = (1..=12)
                .map(|index| AskOption {
                    label: format!("选项{index}"),
                    description: None,
                    preview: None,
                })
                .collect();
        }

        // 首屏：问题窗限高 + 状态行；选项窗口从头开始，尾部 ▼ 提示。
        // 15 行正文：问题窗 6 + 空行 2 + 选项窗口 6 + 自定义行 1。
        let layout = ask_layout(theme, &modal, 40, 15);
        let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("行 · PgUp/PgDn 滚动")),
            "超窗问题应给滚动状态行：{texts:?}"
        );
        assert!(layout.question_area.is_some(), "溢出时问题区登记命中");
        assert!(
            texts.iter().any(|text| text.contains("▼ 还有 6 项")),
            "选项窗口尾部应提示隐藏项数：{texts:?}"
        );
        assert_eq!(
            layout.option_hits.len(),
            7,
            "命中矩形只覆盖可见选项 + 自定义行"
        );

        // 光标到底：窗口跟随，头部 ▲ 提示上方隐藏项数。
        if let Modal::Ask { highlight, .. } = &mut modal {
            highlight[0] = 11;
        }
        let layout = ask_layout(theme, &modal, 40, 15);
        let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|text| text.contains("▲ 还有 7 项")),
            "滚到底后头部应提示隐藏项数：{texts:?}"
        );

        // 问题正文窗口按偏移推进（与光标移动互不相干）；偏移越界时钳到末窗。
        if let Modal::Ask {
            highlight,
            question_scroll,
            ..
        } = &mut modal
        {
            highlight[0] = 0;
            *question_scroll = 3;
        }
        let layout = ask_layout(theme, &modal, 40, 15);
        let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|text| text.contains("⋯ 3-6/6 行")),
            "问题窗应显示钳取后的行区间：{texts:?}"
        );
    }

    /// 卡片级渲染：带预览的提问在真实终端缓冲里并排出现，卡片高度容得下
    /// 选项、预览与底部按键行（不会因换行把提示挤出边框）。
    #[test]
    fn ask_preview_draws_side_by_side_in_terminal_buffer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.modal = Some(ask_modal(0, 0));
        let (width, height) = (120u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");

        let rows: Vec<String> = (0..height)
            .map(|row| {
                (0..width)
                    .filter_map(|x| terminal.backend().buffer().cell((x, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect();
        // TestBackend 给宽字符的尾部补空格格位：比较时先去掉所有空格。
        let compact: Vec<String> = rows.iter().map(|text| text.replace(' ', "")).collect();
        let option_row = compact
            .iter()
            .find(|text| text.contains("❯○上中下"))
            .unwrap_or_else(|| {
                let shown: Vec<&String> =
                    rows.iter().filter(|text| !text.trim().is_empty()).collect();
                panic!("选项行应出现在卡片内：{shown:#?}")
            });
        assert!(
            option_row.contains("+--+"),
            "聚焦项预览应与选项同行：{option_row:?}"
        );
        assert!(
            compact.iter().any(|text| text.contains("Esc折叠问题卡")),
            "底部按键行应留在卡片内：{compact:?}"
        );
    }

    /// 渲染时把选项与自定义行的命中矩形记到 `ask_option_hits`：
    /// 下标顺序对应选项（自定义行在末位），行号自上而下递增且落在卡片内。
    #[test]
    fn ask_render_records_option_hit_areas() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.modal = Some(ask_modal(0, 0));
        let (width, height) = (120u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");

        let hits = &app.ask_option_hits;
        assert_eq!(hits.len(), 3, "两个选项 + 自定义行：{hits:?}");
        assert_eq!(
            hits.iter().map(|(index, _)| *index).collect::<Vec<_>>(),
            vec![0, 1, 2],
            "下标按选项顺序排列，末位是自定义行"
        );
        assert!(hits[0].1.y < hits[1].1.y && hits[1].1.y < hits[2].1.y);
        for (index, area) in hits {
            assert!(area.y > 0 && area.y < height, "选项 {index} 命中行在终端内");
            assert_eq!(area.height, 1);
        }

        // 命中行确实渲染了对应选项：下标 0 的行里能看到聚焦标记与标签。
        let (_, first) = &hits[0];
        let row: String = (first.x..first.right())
            .filter_map(|x| terminal.backend().buffer().cell((x, first.y)))
            .map(|cell| cell.symbol())
            .collect();
        assert!(
            row.replace(' ', "").contains("上中下"),
            "命中行渲染选项文本：{row:?}"
        );
    }

    /// 多问导航条：各问 `n. header` 逐项排开，已答带 ✓；
    /// 每项记下列范围命中矩形，供鼠标跳问。
    #[test]
    fn ask_nav_strip_marks_answered_and_records_hits() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let question = |id: &str, header: &str| AskQuestion {
            id: id.into(),
            question: "选哪个？".into(),
            header: Some(header.into()),
            multi: false,
            recommended: Vec::new(),
            options: vec![AskOption {
                label: "甲".into(),
                description: None,
                preview: None,
            }],
        };
        app.modal = Some(Modal::Ask {
            params: AskParams {
                session_id: "ses_1".into(),
                prompt_id: "prompt_1".into(),
                request_id: "ask_1".into(),
                questions: vec![
                    question("q1", "布局"),
                    question("q2", "配色"),
                    question("q3", "交互"),
                ],
            },
            question: 1,
            highlight: vec![0; 3],
            selected: vec![vec!["甲".to_owned()], Vec::new(), Vec::new()],
            custom: vec![crate::modal::CustomAnswer::default(); 3],
            preview_scroll: 0,
            question_scroll: 0,
            parked: false,
            reply: None,
        });
        let (width, height) = (120u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");

        assert_eq!(app.ask_nav_hits.len(), 4, "三问各一个命中矩形 + 确认项");
        let row = app.ask_nav_hits[0].1.y;
        assert!(app.ask_nav_hits.iter().all(|(_, area)| area.y == row));
        let texts: Vec<String> = app
            .ask_nav_hits
            .iter()
            .map(|(_, area)| {
                (area.x..area.right())
                    .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
                    .replace(' ', "")
            })
            .collect();
        assert!(texts[0].contains("✓1.布局"), "已答带 ✓：{texts:?}");
        assert!(texts[1].contains("2.配色"), "第二问 header：{texts:?}");
        assert!(texts[2].contains("3.交互"), "第三问 header：{texts:?}");
        assert_eq!(texts[3], "确认", "导航条末尾是确认页：{texts:?}");
        assert_eq!(app.ask_nav_hits[3].0, 3, "确认项的下标是 questions.len()");
        assert!(
            app.ask_nav_hits[0].1.x < app.ask_nav_hits[1].1.x
                && app.ask_nav_hits[1].1.x < app.ask_nav_hits[2].1.x,
            "命中矩形按列递增"
        );
    }

    /// 确认页：越过最后一问强制经过，逐问一行列出全部回答，未回答的醒目标出。
    #[test]
    fn ask_confirm_page_lists_answers_and_flags_unanswered() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let question = |id: &str, header: &str| AskQuestion {
            id: id.into(),
            question: "选哪个？".into(),
            header: Some(header.into()),
            multi: false,
            recommended: Vec::new(),
            options: vec![AskOption {
                label: "甲".into(),
                description: None,
                preview: None,
            }],
        };
        let modal = Modal::Ask {
            params: AskParams {
                session_id: "ses_1".into(),
                prompt_id: "prompt_1".into(),
                request_id: "ask_1".into(),
                questions: vec![
                    question("theme", "主题"),
                    question("features", "features"),
                    question("lang", "lang"),
                ],
            },
            question: 3, // 确认页 = questions.len()
            highlight: vec![0; 3],
            selected: vec![
                vec!["纸张白".to_owned()],
                Vec::new(),
                vec!["Rust".to_owned()],
            ],
            custom: vec![crate::modal::CustomAnswer::default(); 3],
            preview_scroll: 0,
            question_scroll: 0,
            parked: false,
            reply: None,
        };
        let body: String = modal_lines(&app, &modal, 100, 24)
            .iter()
            .map(line_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(body.contains("确认回答"), "确认页标题：{body}");
        assert!(body.contains("主题    : 纸张白"), "key 列补齐对齐：{body}");
        assert!(body.contains("features: 未回答"), "未回答醒目标记：{body}");
        assert!(body.contains("lang    : Rust"), "第三问回答：{body}");
        assert!(body.contains("Enter 确认提交"), "确认页页脚：{body}");
    }

    /// 回退模态：在终端缓冲里正确绘制时间线分界、回退横幅与底部 dock 操作栏。
    #[test]
    fn rewind_modal_draws_omp_timeline_and_dock_in_buffer() {
        use moc_protocol::{Message, TranscriptEntry};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);

        let mut rewind_state = crate::modal::RewindState::default();
        rewind_state.set_entries(
            vec![
                TranscriptEntry {
                    id: "u1".into(),
                    at_ms: 1700000000000,
                    message: Message::user_text("第一句提问：实现快速排序"),
                    tool: None,
                    bash: None,
                    agent: None,
                    goal_round: None,
                    reminder: None,
                },
                TranscriptEntry {
                    id: "a1".into(),
                    at_ms: 1700000001000,
                    message: Message::assistant_text("好的，已为你实现快排"),
                    tool: None,
                    bash: None,
                    agent: None,
                    goal_round: None,
                    reminder: None,
                },
            ],
            vec![],
        );
        app.modal = Some(Modal::Rewind {
            state: rewind_state,
        });

        let (width, height) = (120u16, 32u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");

        let rows: Vec<String> = (0..height)
            .map(|row| {
                (0..width)
                    .filter_map(|x| terminal.backend().buffer().cell((x, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect();
        let full_output: String = rows.join("\n").replace(' ', "");

        assert!(
            full_output.contains("回退"),
            "应包含回退标识：{full_output}"
        );
        assert!(
            full_output.contains("回退到此处") || full_output.contains("从此处继续"),
            "应包含 omp 风格的回退横幅或按钮动作：{full_output}"
        );
        assert!(
            full_output.contains("第一句提问"),
            "时间线应渲染用户提问：{full_output}"
        );
    }

    /// 单问、单选的提问模态；`highlight` 指向聚焦选项。
    fn ask_modal(highlight: usize, preview_scroll: usize) -> Modal {
        Modal::Ask {
            params: AskParams {
                session_id: "ses_1".into(),
                prompt_id: "prompt_1".into(),
                request_id: "ask_1".into(),
                questions: vec![AskQuestion {
                    id: "layout".into(),
                    question: "选哪个布局？".into(),
                    header: None,
                    multi: false,
                    recommended: Vec::new(),
                    options: vec![
                        AskOption {
                            label: "上中下".into(),
                            description: None,
                            preview: Some("```\n+--+\n|A |\n+--+\n```".into()),
                        },
                        AskOption {
                            label: "左右".into(),
                            description: None,
                            preview: None,
                        },
                    ],
                }],
            },
            question: 0,
            highlight: vec![highlight],
            selected: vec![Vec::new()],
            custom: vec![crate::modal::CustomAnswer::default()],
            preview_scroll,
            question_scroll: 0,
            parked: false,
            reply: None,
        }
    }

    /// 一行渲染后的文本。
    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// 一行渲染后的显示宽度（格）。
    fn line_width(text: &str) -> usize {
        crate::render::wrap::str_width(text)
    }

    #[test]
    fn completion_menu_renders_solid_opaque_background() {
        use crate::widgets::completion::CompletionState;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::buffer::CellWidth;
        use ratatui::layout::Rect;

        let mut app_for_completion = App::new(Theme::global(), unbounded_channel().0);
        app_for_completion.prompt.insert_str_raw("/he");
        let mut completion = CompletionState::derive(&app_for_completion).expect("构造补全状态");
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let prompt_area = Rect::new(0, 10, 80, 3);
        terminal
            .draw(|frame| {
                // 用醒目底色填满缓冲，模拟底层聊天记录或透明终端
                frame.buffer_mut().set_style(
                    Rect::new(0, 0, 80, 24),
                    ratatui::style::Style::new().bg(Color::Red),
                );
                super::draw_completion_menu(frame, &mut completion, prompt_area);
            })
            .expect("绘制成功");

        let theme = Theme::global();
        let area = completion.area;
        assert!(area.width > 0 && area.height > 0);
        let buffer = terminal.backend().buffer();
        for y in area.y..area.y + area.height {
            for x in area.x..area.x + area.width {
                if x > area.x && buffer.cell((x - 1, y)).is_some_and(|c| c.cell_width() > 1) {
                    continue;
                }
                let cell = buffer.cell((x, y)).expect("单元格存在");
                assert_ne!(
                    cell.bg,
                    Color::Reset,
                    "浮层区域不应出现 Reset 穿透：({x}, {y})"
                );
                assert_ne!(cell.bg, Color::Red, "浮层区域不应泄露底层颜色：({x}, {y})");
                assert!(
                    cell.bg == theme.bg_base || cell.bg == theme.bg_visual,
                    "单元格背景必须为实色 base 或 visual 高亮：({x}, {y}) 实际为 {:?}",
                    cell.bg
                );
            }
        }
    }

    #[test]
    fn engine_menu_renders_solid_opaque_background() {
        use crate::app::EngineMenu;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let (events, _rx) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.engine_menu = Some(EngineMenu {
            selected: 0,
            items: crate::app::ENGINES.to_vec(),
            area: Rect::default(),
        });
        app.engine_area = Rect::new(50, 20, 10, 1);

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        terminal
            .draw(|frame| {
                frame.buffer_mut().set_style(
                    Rect::new(0, 0, 80, 24),
                    ratatui::style::Style::new().bg(Color::Red),
                );
                super::draw_engine_menu(frame, &mut app);
            })
            .expect("绘制成功");

        let theme = app.theme;
        let menu_area = app.engine_menu.as_ref().unwrap().area;
        assert!(menu_area.width > 0 && menu_area.height > 0);
        let buffer = terminal.backend().buffer();
        for y in menu_area.y..menu_area.y + menu_area.height {
            for x in menu_area.x..menu_area.x + menu_area.width {
                let cell = buffer.cell((x, y)).expect("单元格存在");
                // 宽字符的尾格：渲染时被引导格盖住，diff 不冲刷，backend 里留着
                // 初始 EMPTY（Reset）；上屏时显示的是引导格的右半，不算穿透。
                if x > menu_area.x
                    && crate::render::str_width(
                        buffer.cell((x - 1, y)).expect("单元格存在").symbol(),
                    ) > 1
                {
                    continue;
                }
                assert_ne!(
                    cell.bg,
                    Color::Reset,
                    "引擎菜单不应出现 Reset 穿透：({x}, {y})"
                );
                assert_ne!(cell.bg, Color::Red, "引擎菜单不应泄露底层颜色：({x}, {y})");
                assert!(
                    cell.bg == theme.bg_base || cell.bg == theme.bg_visual,
                    "单元格背景必须为实色 base 或 visual：({x}, {y}) 实际为 {:?}",
                    cell.bg
                );
            }
        }
    }

    #[test]
    fn modal_card_renders_solid_opaque_background() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::layout::Rect;

        let (events, _rx) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.modal = Some(Modal::Help { scroll: 0 });

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
        terminal
            .draw(|frame| {
                frame.buffer_mut().set_style(
                    Rect::new(0, 0, 100, 30),
                    ratatui::style::Style::new().bg(Color::Red),
                );
                super::draw_modal(frame, &mut app);
            })
            .expect("绘制成功");

        let theme = app.theme;
        let buffer = terminal.backend().buffer();
        // 居中卡片区域：在 100x30 终端下检查中心卡片覆盖的所有单元格
        // 验证中心点必然是不透底的实色
        let center_cell = buffer.cell((50, 15)).expect("中心单元格存在");
        assert_ne!(center_cell.bg, Color::Reset, "模态卡片中心不应为 Reset");
        assert_ne!(center_cell.bg, Color::Red, "模态卡片中心不应泄露底层背景");
        assert!(
            center_cell.bg == theme.bg_base || center_cell.bg == theme.bg_visual,
            "模态卡片中心应为 bg_base，实际为 {:?}",
            center_cell.bg
        );
    }

    /// 快捷键一览排版：行不越宽、无省略号、不再列斜杠命令；末尾是指路行。
    #[test]
    fn help_layout_has_no_gaps_or_fake_ellipsis() {
        let (events, _rx) = unbounded_channel();
        let app = App::new(Theme::global(), events);

        for width in [92usize, 40] {
            let (lines, _) = super::help_lines(&app, width);
            for line in &lines {
                let text = line_text(line);
                assert!(line_width(&text) <= width, "行宽应不超过 {width}：{text:?}");
                assert!(!text.contains('…'), "{width} 宽下不应有截断：{text:?}");
                assert!(
                    !text.starts_with("  /"),
                    "{width} 宽下不应列出命令行：{text:?}"
                );
            }
            // 末尾指路斜杠命令：输入 `/` 即见全表。
            let last = lines
                .iter()
                .map(line_text)
                .rev()
                .find(|text| !text.trim().is_empty())
                .expect("末尾非空行");
            assert_eq!(last, "输入 / 查看全部斜杠命令");
        }

        // 「选择模型」出现在键位列上限之内。
        let (lines, _) = super::help_lines(&app, 92);
        let row = lines
            .iter()
            .map(line_text)
            .find(|text| text.contains("选择模型"))
            .expect("选择模型行");
        let offset = row.find("选择模型").expect("描述在行内");
        let column = line_width(&row[..offset]);
        assert!(
            column <= super::HELP_INDENT + super::HELP_KEY_MAX + super::HELP_KEY_GAP,
            "描述列起点 {column} 超过键位列上限"
        );
    }

    /// 事件层只加不减的滚动偏移由渲染写回收紧：滚到底后往上滚一格立即生效。
    #[test]
    fn help_scroll_is_clamped_on_draw() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _rx) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.modal = Some(Modal::Help { scroll: usize::MAX });
        // 矮终端：帮助正文（约 20 行）超过正文可见行数才会出现滚动。
        let mut terminal = Terminal::new(TestBackend::new(100, 14)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");
        let Some(Modal::Help { scroll: max }) = app.modal else {
            unreachable!()
        };
        assert!((4..1000).contains(&max), "写回应把偏移收紧到底部：{max}");

        app.modal = Some(Modal::Help { scroll: max - 3 });
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");
        // 状态行格式「⋯ a-b/total 行」：偏移减一，窗口起点立即跟着减一。
        let marker = format!(" {}-", max - 2);
        let screen = terminal.backend().buffer();
        let found = (0..14u16).any(|y| {
            (0..100u16)
                .filter_map(|x| screen.cell((x, y)).map(|cell| cell.symbol()))
                .collect::<String>()
                .contains(&marker)
        });
        assert!(found, "状态行应包含 {marker:?}");
    }

    /// 权限确认正文：事件层只加不减的偏移由渲染写回收紧。
    #[test]
    fn permission_scroll_is_clamped_on_draw() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _rx) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let body = (1..=60)
            .map(|index| format!("第 {index} 行命令全文"))
            .collect::<Vec<_>>()
            .join("\n");
        app.modal = Some(Modal::Permission {
            params: moc_protocol::methods::PermissionRequestParams {
                session_id: "s".into(),
                prompt_id: "p".into(),
                request_id: "r".into(),
                tool_call_id: "c".into(),
                tool_name: "bash".into(),
                title: "权限确认".into(),
                body: Some(body),
                diff: None,
                options: vec![moc_protocol::methods::PermissionOption {
                    id: "allow".into(),
                    label: "允许".into(),
                    kind: moc_protocol::methods::PermissionOptionKind::AllowOnce,
                }],
            },
            selected: 0,
            scroll_offset: usize::MAX,
            reply: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
        let snapshot = |terminal: &Terminal<TestBackend>| -> String {
            (0..30u16)
                .map(|y| {
                    (0..100u16)
                        .filter_map(|x| {
                            terminal.backend().buffer().cell((x, y)).map(|c| c.symbol())
                        })
                        .collect::<String>()
                })
                .collect()
        };
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");
        let Some(Modal::Permission {
            scroll_offset: max, ..
        }) = app.modal
        else {
            unreachable!()
        };
        assert!((1..1000).contains(&max), "写回应把偏移收紧到底部：{max}");
        let first = snapshot(&terminal);
        // 正文溢出：末行应给出标准滚动状态行而非静默截断。
        assert!(first.contains('⋯'), "溢出应显示滚动状态行");

        if let Some(Modal::Permission { scroll_offset, .. }) = &mut app.modal {
            *scroll_offset = max - 1;
        }
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");
        assert_ne!(first, snapshot(&terminal), "偏移减一应改变画面");
    }

    /// 计划阅读面板：事件层只加不减的偏移由渲染写回收紧，贴底正文不为空。
    #[test]
    fn plan_read_scroll_is_clamped_on_draw() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _rx) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let markdown = (1..=200)
            .map(|index| format!("第 {index} 行计划正文"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut state = crate::modal::PlanReviewState::new(
            moc_protocol::methods::PlanInfo {
                plan_id: "p1".into(),
                title: "计划".into(),
                path: "plan.md".into(),
                markdown,
                status: moc_protocol::methods::PlanStatus::Submitted,
                version: 1,
                previous_plan_id: None,
                annotations: Vec::new(),
            },
            SessionEngine::Standard,
        );
        state.scroll_offset = usize::MAX;
        app.modal = Some(Modal::Plan {
            state: Box::new(state),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");
        let Some(Modal::Plan { state }) = &app.modal else {
            unreachable!()
        };
        assert!(
            (1..1000).contains(&state.scroll_offset),
            "写回应把偏移收紧到底部：{}",
            state.scroll_offset
        );
        // 贴底时正文区显示计划末尾内容，不是空白。
        let text: String = (0..30u16)
            .map(|y| {
                (0..100u16)
                    .filter_map(|x| {
                        terminal
                            .backend()
                            .buffer()
                            .cell((x, y))
                            .map(|c| c.symbol().to_owned())
                    })
                    .collect::<String>()
            })
            .collect();
        assert!(
            text.replace(' ', "").contains("第200行"),
            "贴底应显示末尾正文：{text:?}"
        );
    }

    /// 防复发守卫：通用卡片压在满屏宽字符上时，左边框每一行都必须真的发到终端。
    ///
    /// 两帧绘制让 TestBackend 走真实 diff；背景起始列取 0/1 两种奇偶，保证
    /// 「宽字符引导格恰在边框左邻」这一触发条件对任意卡片 x 都会出现。
    #[test]
    fn modal_left_border_survives_wide_char_background() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        use ratatui::style::Style;

        // 帮助（双列表）、上下文占用（紧凑宽度）、会话列表（通用宽度）：覆盖三种卡宽。
        let modal = |kind: usize| match kind {
            0 => Modal::Help { scroll: 0 },
            1 => Modal::ContextContent {
                state: Default::default(),
            },
            _ => Modal::Sessions {
                sessions: Vec::new(),
                cwd: "/tmp".into(),
                scope: SessionScope::All,
                picker: PickerState::default(),
            },
        };
        for (width, height) in [(120u16, 40u16), (80, 30), (71, 24)] {
            for kind in 0..3 {
                for start in [0u16, 1] {
                    let (events, _rx) = unbounded_channel();
                    let mut app = App::new(Theme::global(), events);
                    app.modal = Some(modal(kind));
                    let background = |frame: &mut ratatui::Frame| {
                        let fill = "中".repeat(usize::from(width) / 2);
                        for y in 0..height {
                            frame.buffer_mut().set_string(start, y, &fill, Style::new());
                        }
                    };
                    let mut terminal =
                        Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
                    terminal.draw(background).expect("首帧");
                    terminal
                        .draw(|frame| {
                            background(frame);
                            super::draw_modal(frame, &mut app);
                        })
                        .expect("模态帧");
                    let screen = terminal.backend().buffer();
                    let (left, top) = (0..height)
                        .find_map(|y| {
                            (0..width)
                                .find(|&x| screen[(x, y)].symbol() == "╭")
                                .map(|x| (x, y))
                        })
                        .expect("卡片左上角");
                    let bottom = (top..height)
                        .find(|&y| screen[(left, y)].symbol() == "╰")
                        .expect("卡片左下角");
                    for y in top..=bottom {
                        let symbol = screen[(left, y)].symbol();
                        assert!(
                            matches!(symbol, "│" | "╭" | "╰"),
                            "{width}×{height} 起始列 {start} 的 {} 第 {y} 行左边框缺失：{symbol:?}",
                            modal(kind).title()
                        );
                    }
                }
            }
        }
    }
}

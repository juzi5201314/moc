//! 浮层渲染：斜杠补全菜单与模态卡片。
//!
//! 浮层画在滚动区之后（覆盖其内容）；模态居中，宽度按内容取值（帮助为双列快捷键表，
//! 普通模态上限 96 列，带选项预览的提问放宽到 120 列），高度按内容收缩。全部使用主题槽位。

mod model_picker;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

use crate::app::App;
use crate::modal::{Modal, PlanTab, SessionScope};
use crate::render::wrap::{clip_line, fit, fit_line, str_width, truncate, truncate_head};
use crate::render::{context_content, labels, overlay};
use crate::theme::Theme;
use rust_i18n::t;
use std::borrow::Cow;

mod menus;
pub use menus::completion_menu_height;
pub use menus::draw_completion_menu;
pub use menus::draw_engine_menu;
pub use menus::draw_permission_menu;
mod help;
use help::help_lines;
mod context;
pub(super) use context::context_lines;
mod monitor;
use monitor::monitor_lines;
mod ask;
use ask::ask_layout;
use ask::draw_ask_modal;
use ask::scroll_window;
use ask::scroll_window_max;
mod plan;
use plan::draw_plan_editor;
use plan::plan_lines;
pub(crate) use plan::plan_source_window;

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
    /// 计划审阅的命中几何（标签栏 / 选区条 / 动作区 / 渲染行→源码映射）。
    plan: Option<plan::PlanHits>,
}

/// 居中模态卡片。
pub fn draw_modal(frame: &mut Frame, app: &mut App) {
    app.modal_drawn = false;
    draw_modal_card(frame, app);
    app.modal_frame_seen = true;
}

fn draw_modal_card(frame: &mut Frame, app: &mut App) {
    app.modal_item_hits.clear();
    app.modal_detail_hits.clear();
    app.modal_menu_hits.clear();
    app.modal_card_area = Rect::default();
    app.modal_detail_area = Rect::default();
    app.modal_menu_area = Rect::default();
    app.plan_tab_hits.clear();
    app.plan_selbar_hits.clear();
    app.plan_control_hits.clear();
    app.plan_editor_hits.clear();
    // 模态是阻塞层：底下界面登记的键位一律作废，只留模态自己的。
    app.key_hits.clear();
    let area = frame.area();
    if area.width < 20 || area.height < 6 {
        return;
    }
    if matches!(app.modal, Some(Modal::Model { .. })) {
        // 从计划模型芯片打开的选择器：停放的审阅卡照常画在底下（递归走
        // 计划分支，含压暗），画完清掉它登记的命中区，避免与选择器抢事件。
        if let Some(state) = app.parked_plan.take() {
            let saved = app.modal.replace(Modal::Plan { state });
            draw_modal(frame, app);
            if let Some(Modal::Plan { state }) = app.modal.take() {
                app.parked_plan = Some(state);
            }
            app.modal = saved;
            app.modal_item_hits.clear();
            app.modal_detail_hits.clear();
            app.modal_menu_hits.clear();
            app.modal_card_area = Rect::default();
            app.modal_detail_area = Rect::default();
            app.modal_menu_area = Rect::default();
            app.key_hits.clear();
            app.plan_tab_hits.clear();
            app.plan_selbar_hits.clear();
            app.plan_control_hits.clear();
            app.plan_editor_hits.clear();
            app.plan_body_area = Rect::default();
            app.plan_row_map.clear();
            app.plan_line_sources.clear();
            app.plan_scrollbar_area = Rect::default();
        }
        model_picker::draw(frame, app);
        app.modal_drawn = !app.modal_card_area.is_empty();
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
        app.modal_drawn = !app.modal_card_area.is_empty();
        return;
    }
    if matches!(app.modal, Some(Modal::ContextContent { .. })) {
        // 正文窗按自己的列宽折行、按整行滚动，并自带滚动条与命中几何。
        context_content::draw(frame, app, area);
        app.modal_drawn = !app.modal_card_area.is_empty();
        return;
    }
    if matches!(app.modal, Some(Modal::Ask { .. })) {
        // 提问卡片自带命中几何（鼠标点选选项），同样不走通用路径。
        draw_ask_modal(frame, app, area);
        app.modal_drawn = true;
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
    app.modal_drawn = true;
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
        // 命中行号即屏幕行（同 modal_item_hits 的换算），x 为正文内相对列。
        let hit_rect = |row: usize, col: usize, w: usize| -> Option<Rect> {
            Some(Rect::new(
                layout.body.x.checked_add(u16::try_from(col).ok()?)?,
                row_y(row)?,
                u16::try_from(w).ok()?,
                1,
            ))
        };
        let body_y = layout.body.y + u16::try_from(PLAN_HEADER_ROWS).expect("常量可表示为 u16");
        if let Some(plan_hits) = body.plan {
            app.plan_tab_hits = plan_hits
                .tabs
                .iter()
                .filter_map(|(tab, row, col, w)| Some((*tab, hit_rect(*row, *col, *w)?)))
                .collect();
            app.plan_selbar_hits = plan_hits
                .selbar
                .iter()
                .filter_map(|(action, row, col, w)| Some((*action, hit_rect(*row, *col, *w)?)))
                .collect();
            app.plan_control_hits = plan_hits
                .controls
                .iter()
                .filter_map(|(control, row, col, w)| Some((*control, hit_rect(*row, *col, *w)?)))
                .collect();
            app.plan_line_sources = plan_hits.line_sources;
            if state.tab == PlanTab::Source {
                // 源码标签一行源码对一行屏幕行，鼠标拖选直接按行换算。
                let window =
                    plan_source_window(app, state, overlay::content_width(width), rows, theme);
                app.plan_body_area = Rect::new(
                    layout.body.x,
                    body_y,
                    layout.body.width,
                    u16::try_from(window.len()).unwrap_or(u16::MAX),
                );
                app.plan_row_map = window;
            } else {
                // 计划/改动标签：正文区 = 渲染行窗口（供 gutter/正文点击换算）。
                app.plan_body_area = Rect::new(
                    layout.body.x,
                    body_y,
                    layout.body.width,
                    u16::try_from(plan_hits.body_rows).unwrap_or(u16::MAX),
                );
            }
        }
        // 悬停底色：标签与选区条无焦点态，命中即铺 bg_hover；
        // 芯片/按钮的悬停样式由事件层写 state.focus 后在渲染期体现。
        if let Some((px, py)) = app.scrollbar_pointer {
            let pos = ratatui::layout::Position::new(px, py);
            for rect in app
                .plan_tab_hits
                .iter()
                .map(|(_, rect)| *rect)
                .chain(app.plan_selbar_hits.iter().map(|(_, rect)| *rect))
            {
                if rect.contains(pos) {
                    frame
                        .buffer_mut()
                        .set_style(rect, Style::new().bg(theme.bg_hover));
                }
            }
        }
        // 正文滚动条：溢出时画在正文最右列，轨道命中区交给鼠标拖拽。
        app.plan_scrollbar_area = Rect::default();
        app.plan_body_total = 0;
        let total = match state.tab {
            PlanTab::Plan => app.plan_line_sources.len(),
            PlanTab::Source => state.source_lines().len(),
            PlanTab::Diff => body.scroll_max + usize::from(app.plan_body_area.height),
        };
        let track_height = usize::from(app.plan_body_area.height);
        if total > track_height && track_height > 0 {
            let thumb = (track_height * track_height / total).max(1);
            let max_offset = total - track_height;
            let top_range = track_height - thumb;
            let thumb_top = state.scroll_offset.min(max_offset) * top_range / max_offset;
            let track = Rect::new(
                app.plan_body_area.right().saturating_sub(1),
                app.plan_body_area.y,
                1,
                app.plan_body_area.height,
            );
            overlay::paint_scrollbar(
                frame.buffer_mut(),
                track,
                u16::try_from(thumb_top).unwrap_or(u16::MAX),
                u16::try_from(thumb).unwrap_or(u16::MAX),
                theme,
            );
            app.plan_scrollbar_area = track;
            app.plan_body_total = total;
        }
        if state.editor.is_some() {
            // 编辑器盖在审阅卡片上：只有它的键位可点。
            let (keys, editor_hits) = draw_plan_editor(frame, app, layout.area, area);
            app.key_hits = keys;
            app.plan_editor_hits = editor_hits;
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
        Some(Modal::Plan { state }) if matches!(state.tab, PlanTab::Plan | PlanTab::Diff) => {
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
    // 「新会话引擎」芯片的就地下拉画在卡片之上，锚在芯片命中区上方。
    if let Some(Modal::Plan { state }) = app.modal.as_mut()
        && let Some(menu) = state.engine_menu.as_mut()
    {
        let anchor = app
            .plan_control_hits
            .iter()
            .find(|(control, _)| *control == crate::modal::PlanControl::Engine)
            .map_or(layout.area, |(_, rect)| *rect);
        menus::draw_legend_menu(frame, theme, menu, anchor, Some(state.execution_engine));
    }
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
    let label = t!("tui.modal.search_label").into_owned();
    let budget = width.saturating_sub(str_width(&label) + 1);
    Line::from(vec![
        Span::styled(label, Style::new().fg(theme.gray_dim)),
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
    let mut plan = None;
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
                &[
                    ("1-9", t!("tui.hint.pick")),
                    ("esc", t!("tui.hint.deny")),
                    ("enter", t!("tui.hint.confirm")),
                ],
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
            let tokens = crate::render::format_tokens(overflow.tokens);
            let leading = if overflow.tokens > overflow.context_window {
                t!(
                    "tui.modal.ctx_over_window_fmt",
                    tokens = tokens,
                    selector = selector.as_str(),
                    window = window
                )
            } else {
                t!(
                    "tui.modal.ctx_over_threshold_fmt",
                    tokens = tokens,
                    selector = selector.as_str(),
                    threshold = crate::render::format_tokens(overflow.threshold),
                    window = window
                )
            };
            let mut lines = wrapped_lines(&leading, width, Style::new().fg(theme.text_primary));
            lines.extend(wrapped_lines(
                &t!(
                    "tui.modal.switch_compact_fmt",
                    selector = selector,
                    target = crate::render::format_tokens(overflow.target_tokens)
                ),
                width,
                Style::new().fg(theme.text_secondary),
            ));
            lines.push(Line::from(String::new()));
            if *loading {
                lines.push(Line::from(Span::styled(
                    t!("tui.modal.switching"),
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = error {
                lines.extend(wrapped_lines(
                    &t!("tui.err.fmt", err = error.as_str()),
                    width,
                    Style::new().fg(theme.accent_error),
                ));
            }
            for (index, text) in [t!("tui.modal.switch_and_compact"), t!("tui.hint.cancel")]
                .iter()
                .enumerate()
            {
                hits.push((index, lines.len()));
                lines.push(overlay::list_row(text, index == *option, width, theme));
            }
            let items: Vec<(&str, Cow<'_, str>)> = if error.is_some() {
                vec![
                    ("enter", t!("tui.hint.retry")),
                    ("esc", t!("tui.hint.cancel")),
                ]
            } else {
                vec![
                    ("↑↓", t!("tui.hint.choose")),
                    ("enter", t!("tui.hint.confirm")),
                    ("esc", t!("tui.hint.cancel")),
                ]
            };
            (lines, overlay::footer(&items, width, theme))
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
                    t!("tui.modal.no_jobs"),
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
                        format!("{icon} {job_id} {cmd} {}", t!("tui.modal.kill_confirm"))
                    } else {
                        format!("{icon} {job_id} {cmd}")
                    };

                    lines.push(overlay::list_row(&text, is_selected, width, theme));
                }
            }

            let footer = overlay::footer(
                &[
                    ("↑↓", t!("tui.hint.choose")),
                    (
                        t!("tui.key.kill_selected").as_ref(),
                        t!("tui.hint.kill_task"),
                    ),
                    ("esc", t!("tui.hint.close")),
                ],
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
                overlay::footer(
                    &[
                        ("enter", t!("tui.hint.submit_deny")),
                        ("esc", t!("tui.hint.cancel_input")),
                    ],
                    width,
                    theme,
                )
            } else {
                overlay::footer(
                    &[
                        ("↑↓", t!("tui.hint.choose_role")),
                        ("enter", t!("tui.hint.pick_model")),
                        ("a", t!("tui.hint.approve")),
                        ("r", t!("tui.hint.reject")),
                        ("esc", t!("tui.hint.minimize")),
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
                lines.push(Line::from(Span::styled(t!("tui.modal.no_runs"), dim)));
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
                &[
                    ("↑↓", t!("tui.hint.choose")),
                    ("enter", t!("tui.hint.open_panel")),
                    ("esc", t!("tui.hint.close")),
                ],
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
                    ("↑↓", t!("tui.hint.choose_role")),
                    ("enter", t!("tui.hint.change_model")),
                    ("s", t!("tui.hint.stop")),
                    ("r", t!("tui.hint.resume")),
                    ("esc", t!("tui.hint.close")),
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
                    t!("tui.modal.loading_sessions"),
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.extend(wrapped_lines(
                    &t!("tui.err.fmt", err = error.as_str()),
                    width,
                    Style::new().fg(theme.accent_error),
                ));
            } else if filtered.is_empty() {
                // 搜索无结果与目录为空要分开说：后者才值得引导切范围。
                let hint = if picker.query.is_empty() && *scope == SessionScope::Folder {
                    labels::hints(
                        &[
                            ("", t!("tui.modal.no_sessions_here")),
                            ("tab", t!("tui.hint.view_all")),
                        ],
                        width,
                    )
                } else {
                    truncate(t!("tui.modal.no_match_sessions").into_owned(), width).into_owned()
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
            let items: Vec<(&str, Cow<'_, str>)> = if picker.error.is_some() {
                vec![
                    ("enter", t!("tui.hint.retry")),
                    ("esc", t!("tui.hint.close")),
                ]
            } else {
                vec![
                    ("", t!("tui.hint.type_search")),
                    ("pgup/pgdn", t!("tui.hint.paging")),
                    ("↑↓", t!("tui.hint.move")),
                    ("tab", t!("tui.hint.switch_scope")),
                    ("enter", t!("tui.hint.restore")),
                    ("esc", t!("tui.hint.close")),
                ]
            };
            (lines, overlay::footer(&items, width, theme))
        }
        Modal::Plan { state } => {
            let (lines, footer, plan_hits) =
                plan_lines(app, state, width, max_lines, theme, &mut scroll_max);
            plan = Some(plan_hits);
            (lines, footer)
        }
        Modal::Tools {
            tools,
            error,
            scroll,
            ..
        } => {
            let mut rows = wrapped_lines(&t!("tui.modal.tools_intro"), width, dim);
            rows.push(Line::default());
            if let Some(error) = error {
                rows.extend(wrapped_lines(
                    &t!("tui.modal.tools_read_failed_fmt", err = error.as_str()),
                    width,
                    dim,
                ));
            } else if let Some(tools) = tools {
                if tools.is_empty() {
                    rows.extend(wrapped_lines(&t!("tui.modal.tools_none"), width, dim));
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
                rows.push(Line::from(Span::styled(t!("tui.modal.tools_loading"), dim)));
            }
            scroll_max = scroll_window_max(rows.len(), max_lines);
            let lines = scroll_window(rows, max_lines, *scroll, width, theme);
            (
                lines,
                overlay::footer(
                    &[("↑↓", t!("tui.hint.scroll")), ("esc", t!("tui.hint.close"))],
                    width,
                    theme,
                ),
            )
        }
        Modal::Monitor => {
            let lines = monitor_lines(width, theme);
            (
                lines,
                overlay::footer(
                    &[
                        ("", t!("tui.modal.monitor_note")),
                        ("esc", t!("tui.hint.close")),
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
            (
                lines,
                overlay::footer(&[("esc", t!("tui.hint.close"))], width, theme),
            )
        }
    };
    lines.truncate(max_lines.max(1));
    ModalBody {
        lines,
        footer: Some(footer),
        scroll_max,
        applied_offset,
        hits,
        plan,
    }
}

/// 监控卡布局：label 5 列、当前值右对齐 8 列、统计段 26 列，其余全给折线。
const MONITOR_LABEL_W: usize = 5;
const MONITOR_CUR_W: usize = 8;
const MONITOR_STATS_W: usize = 26;
/// sparkline 分档字符；0 档是空格，表示「该时间桶内没有样本」。
const SPARK_CHARS: [char; 9] = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

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

// ---------- 计划审阅：阅读 / 批注视图 ----------

/// 计划审阅卡片顶部的固定行数：标题/版本、路径、标签栏。
pub(crate) const PLAN_HEADER_ROWS: usize = 3;

/// 折行后的输入内容与光标位置。
struct WrappedInput {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

#[cfg(test)]
mod tests;

//! 浮层渲染：斜杠补全菜单与模态卡片。
//!
//! 浮层画在滚动区之后（覆盖其内容）；模态居中，宽度按内容取值（帮助为双列快捷键表，
//! 普通模态上限 96 列，带选项预览的提问放宽到 120 列），高度按内容收缩。全部使用主题槽位。

use moc_protocol::Role;
use moc_protocol::methods::{ContextUsageResult, PlanStatus};
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Padding, Paragraph, Wrap};

use crate::app::App;
use crate::modal::{Modal, PlanPane, PlanReviewState, SessionScope};
use crate::render::context_content;
use crate::render::wrap::str_width;
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

/// 斜杠补全菜单：贴着输入框上沿，选中行反色。
pub fn draw_completion_menu(
    frame: &mut Frame,
    completion: &mut CompletionState,
    prompt_area: Rect,
) {
    let total = completion.candidates.len();
    let max_visible = 20usize.min(usize::from(prompt_area.y.saturating_sub(3)));
    let visible = total.min(max_visible);
    let width = prompt_area.width.clamp(20, 72);
    let height = completion_menu_height(total, prompt_area.y);
    if height == 0 {
        return;
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
    frame.render_widget(Clear, area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(Style::new().bg(theme.bg_base).fg(theme.text_primary));
            }
        }
    }
    let content_width = usize::from(width.saturating_sub(4));
    let max_label_len = completion
        .candidates
        .iter()
        .skip(completion.offset)
        .take(visible)
        .map(|c| c.label.chars().count())
        .max()
        .unwrap_or(12)
        .max(14);
    let col_width = max_label_len + 3;
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
        if 2 + col_width <= content_width {
            let desc_budget = content_width.saturating_sub(2 + col_width);
            let desc = ellipsize(&candidate.description, desc_budget);
            let label_padded = format!("{:<col_width$}", candidate.label);
            let spans = if selected {
                vec![
                    Span::styled(
                        prefix,
                        Style::new()
                            .fg(theme.accent_user)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        label_padded,
                        Style::new()
                            .fg(theme.text_primary)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::styled(
                        desc,
                        Style::new().fg(theme.text_secondary).bg(theme.bg_visual),
                    ),
                ]
            } else {
                vec![
                    Span::styled(prefix, Style::new().fg(theme.text_primary)),
                    Span::styled(label_padded, Style::new().fg(theme.text_primary)),
                    Span::styled(desc, Style::new().fg(theme.gray_dim)),
                ]
            };
            lines.push(Line::from(spans));
        } else {
            let style = if selected {
                Style::new()
                    .fg(theme.text_primary)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_primary)
            };
            lines.push(Line::from(Span::styled(
                ellipsize(
                    &format!(
                        "{prefix}{:<col_width$}{}",
                        candidate.label, candidate.description
                    ),
                    content_width,
                ),
                style,
            )));
        }
    }
    if total > visible {
        lines.push(Line::from(Span::styled(
            format!("… 还有 {} 条", total - visible),
            Style::new().fg(theme.gray_dim),
        )));
    }
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Tab/Enter 确认 · Esc 关闭",
        Style::new().fg(theme.gray_dim),
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .style(Style::new().bg(theme.bg_base).fg(theme.text_primary))
            .block(
                Block::bordered()
                    .border_set(ratatui::symbols::border::ROUNDED)
                    .border_style(Style::new().fg(theme.border_active))
                    .style(Style::new().bg(theme.bg_base).fg(theme.text_primary)),
            ),
        area,
    );
}

/// 就地引擎菜单：右缘对齐图例里的引擎名，整体贴在它上方（右键菜单那种小浮层）。
pub fn draw_engine_menu(frame: &mut Frame, app: &mut App) {
    let Some(menu) = app.engine_menu.as_ref() else {
        return;
    };
    let theme = app.theme;
    let current = app.session.as_ref().map(|session| session.engine);
    let selected = menu.selected;
    let label_width = crate::app::ENGINES
        .iter()
        .map(|engine| str_width(crate::app::engine_label(*engine)))
        .max()
        .unwrap_or(0);
    let frame_area = frame.area();
    let width = u16::try_from(2 + label_width + 2 + 2)
        .unwrap_or(u16::MAX)
        .min(frame_area.width);
    let height = u16::try_from(crate::app::ENGINES.len() + 2).unwrap_or(u16::MAX);
    let area = if !menu.area.is_empty() {
        menu.area
    } else {
        let anchor = app.engine_area;
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
    if let Some(menu_mut) = app.engine_menu.as_mut() {
        menu_mut.area = area;
    }
    let lines: Vec<Line<'static>> = crate::app::ENGINES
        .iter()
        .enumerate()
        .map(|(index, engine)| {
            let highlighted = index == selected;
            let base = if highlighted {
                Style::new()
                    .fg(theme.text_primary)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_primary)
            };
            // 当前引擎沿用图例里的强调色，与菜单高亮（反色）互相独立。
            let label_style = if Some(*engine) == current {
                base.fg(theme.accent_model)
            } else {
                base
            };
            Line::from(vec![
                Span::styled(if highlighted { "❯ " } else { "  " }, base),
                Span::styled(
                    format!(
                        "{:<label_width$}",
                        crate::app::engine_label(*engine),
                        label_width = label_width
                    ),
                    label_style,
                ),
            ])
        })
        .collect();
    frame.render_widget(Clear, area);
    frame
        .buffer_mut()
        .set_style(area, Style::new().bg(theme.bg_base).fg(theme.text_primary));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active))
                .style(Style::new().bg(theme.bg_base).fg(theme.text_primary)),
        ),
        area,
    );
    if let Some(menu) = app.engine_menu.as_mut() {
        menu.area = area;
    }
}

/// 居中模态卡片。
pub fn draw_modal(frame: &mut Frame, app: &mut App) {
    app.plan_model_area = Rect::default();
    app.modal_item_hits.clear();
    let area = frame.area();
    if area.width < 20 || area.height < 6 {
        return;
    }
    let theme = app.theme;
    // 折叠的提问卡片不压暗正文：用户正在滚动的就是提问背后的上下文，
    // 调暗会伤可读性；卡片本身有 Clear 底，照样醒目。
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
        // 正文窗按自己的列宽折行、按整行滚动，并自带滚动条与命中几何，
        // 不走 `modal_lines` 的「一次性铺满」路径。
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
    let max_content = usize::from(area.height.saturating_sub(4)).max(1);
    let (width, content_width) = if matches!(modal, Modal::Help) {
        // 帮助是双列快捷键表：按内容自然宽度取值，窄终端由 help_lines 退回单列。
        let (_, natural) = help_lines(app, usize::from(area.width).saturating_sub(6), max_content);
        let width = u16::try_from(natural + 4)
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(4));
        (width, usize::from(width.saturating_sub(4)))
    } else if matches!(modal, Modal::Context { .. }) {
        // 上下文占用卡片：收敛到紧凑宽度，避免宽屏下右侧过度留白且色条过长。
        let width = area
            .width
            .saturating_sub(4)
            .clamp(16, 58)
            .min(area.width.saturating_sub(2));
        (width, usize::from(width.saturating_sub(4)).max(8))
    } else {
        let width = if area.width < 70 {
            area.width.saturating_sub(4).max(16)
        } else {
            area.width.min(96)
        };
        (width, usize::from(width.saturating_sub(4)).max(8))
    };
    let (lines, item_hits) = modal_lines_with_hits(app, modal, content_width, max_content);
    let content_height = u16::try_from(lines.len().max(1)).unwrap_or(u16::MAX);
    // 2 行给上下边框，1 行留作底部内边距：内容顶格，下沿不贴边框也不空一大片。
    let height = content_height
        .saturating_add(3)
        .min(area.height.saturating_sub(2));
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let visible_rows = lines
        .len()
        .min(usize::from(modal_area.height.saturating_sub(2)));
    render_modal_card(frame, lines, modal.title(), theme, modal_area);
    app.modal_item_hits = item_hits
        .into_iter()
        .filter_map(|(idx, row)| {
            if row >= visible_rows {
                return None;
            }
            let y = modal_area.y + 1 + u16::try_from(row).ok()?;
            Some((
                idx,
                Rect::new(modal_area.x + 1, y, modal_area.width.saturating_sub(2), 1),
            ))
        })
        .collect();
    if let Modal::Plan { state } = modal {
        let prefix_width =
            u16::try_from(str_width("执行模型：")).expect("固定标签宽度可表示为 u16");
        let available = u16::try_from(content_width)
            .expect("正文宽度受终端 u16 宽度约束")
            .saturating_sub(prefix_width);
        let model_width = u16::try_from(str_width(crate::app::model_selector_label(
            &state.execution_model,
        )))
        .unwrap_or(u16::MAX)
        .min(available);
        app.plan_model_area = Rect::new(
            modal_area.x + 2 + prefix_width,
            modal_area.y + 1,
            model_width,
            1,
        );
        if state.pane == PlanPane::Annotate {
            // 批注视图是一行源码对一行屏幕行，鼠标拖选直接按行换算。
            let window = plan_source_window(state, max_content);
            app.plan_body_area = Rect::new(
                modal_area.x + 2,
                modal_area.y + 1 + u16::try_from(PLAN_HEADER_ROWS).unwrap_or(0),
                u16::try_from(content_width).unwrap_or(u16::MAX),
                u16::try_from(window.len()).unwrap_or(u16::MAX),
            );
            app.plan_row_map = window;
        }
        if state.editor.is_some() {
            draw_plan_editor(frame, app, modal_area, area);
        }
    }
}

/// 模态卡片的公共渲染：圆角边框、标题、水平内边距与自动折行。
fn render_modal_card(
    frame: &mut Frame,
    lines: Vec<Line<'static>>,
    title: String,
    theme: &Theme,
    modal_area: Rect,
) {
    frame.render_widget(Clear, modal_area);
    frame.buffer_mut().set_style(
        modal_area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_set(ratatui::symbols::border::ROUNDED)
                    .border_style(Style::new().fg(theme.border_active))
                    .style(Style::new().bg(theme.bg_base).fg(theme.text_primary))
                    // 左右各留 1 格内边距，正文不贴边框；标题自带同样的空格才能与正文左缘对齐。
                    .padding(Padding::horizontal(1))
                    .title(Span::styled(
                        format!(" {title} "),
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false }),
        modal_area,
    );
}

/// 提问卡片：渲染的同时把每个选项（含自定义行）的命中矩形记到 `ask_option_hits`，
/// 鼠标点选据此换算。宽度、居中与通用模态一致，带选项预览时放宽到 120 列。
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
    let max_content = usize::from(area.height.saturating_sub(4)).max(1);
    let width = if area.width < 70 {
        area.width.saturating_sub(4).max(16)
    } else if modal.has_option_preview() {
        area.width.min(ASK_PREVIEW_MODAL_WIDTH)
    } else {
        area.width.min(96)
    };
    let content_width = usize::from(width.saturating_sub(4)).max(8);
    let layout = ask_layout(theme, modal, content_width, max_content);
    let content_height = u16::try_from(layout.lines.len().max(1)).unwrap_or(u16::MAX);
    let height = content_height
        .saturating_add(3)
        .min(area.height.saturating_sub(2));
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    // 卡片被终端高度截断时，画不出来的行点不到：只记录可见行。
    let visible_rows = layout
        .lines
        .len()
        .min(usize::from(modal_area.height.saturating_sub(2)));
    render_modal_card(frame, layout.lines, modal.title(), theme, modal_area);
    app.ask_card_area = modal_area;
    // 内容文本列 = 卡片 x + 边框 1 + 内边距 1。
    let text_x = modal_area.x + 2;
    let row_y = |row: usize| -> Option<u16> {
        if row >= visible_rows {
            return None;
        }
        Some(modal_area.y + 1 + u16::try_from(row).ok()?)
    };
    app.ask_option_hits = layout
        .option_hits
        .into_iter()
        .filter_map(|(option, row)| {
            Some((
                option,
                Rect::new(
                    modal_area.x + 1,
                    row_y(row)?,
                    modal_area.width.saturating_sub(2),
                    1,
                ),
            ))
        })
        .collect();
    app.ask_nav_hits = layout
        .nav_hits
        .into_iter()
        .filter_map(|(question, row, start, end)| {
            Some((
                question,
                Rect::new(
                    text_x + u16::try_from(start).ok()?,
                    row_y(row)?,
                    u16::try_from(end - start).ok()?,
                    1,
                ),
            ))
        })
        .collect();
    // 可滚文本区换算到屏幕：被卡片底边裁掉的行不在命中范围内。
    let map_area = |area: Rect| -> Rect {
        let top = usize::from(area.y);
        let bottom = top + usize::from(area.height);
        if top >= visible_rows {
            return Rect::default();
        }
        Rect::new(
            text_x + area.x,
            modal_area.y + 1 + area.y,
            area.width.min(modal_area.width.saturating_sub(2 + area.x)),
            u16::try_from(bottom.min(visible_rows) - top).unwrap_or(u16::MAX),
        )
    };
    app.ask_question_area = layout.question_area.map(map_area).unwrap_or_default();
    app.ask_detail_area = layout.detail_area.map(map_area).unwrap_or_default();
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
    let right = "tab 展开作答 · esc 取消";
    let right_width = str_width(right);
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
    let header = ellipsize(
        &header,
        inner
            .saturating_sub(used + right_width + 2)
            .max(4)
            .min(inner.saturating_sub(used)),
    );
    used += str_width(&header);
    spans.push(Span::styled(
        header,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    ));
    if used + right_width < inner {
        spans.push(Span::from(" ".repeat(inner - used - right_width)));
        spans.push(Span::styled(right, Style::new().fg(theme.gray_dim)));
    }
    let inner_rows = usize::from(area.height.saturating_sub(2));
    let top_pad = inner_rows.saturating_sub(1) / 2;
    let mut lines = vec![Line::from(String::new()); top_pad];
    lines.push(Line::from(spans));
    frame.render_widget(Clear, area);
    frame
        .buffer_mut()
        .set_style(area, Style::new().bg(theme.bg_panel).fg(theme.text_primary));
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
}

/// 快捷键帮助：键位列与描述列之间的间距（格）。
const HELP_KEY_GAP: usize = 2;
/// 快捷键帮助：两列之间的间距（格）。
const HELP_COLUMN_GAP: usize = 4;
/// 快捷键帮助：快捷键行左缩进（格）。
const HELP_INDENT: usize = 2;

/// 一列快捷键的（键位列宽，整列宽）。
type HelpColumn = (usize, usize);

struct HelpSection {
    title: &'static str,
    rows: Vec<(String, &'static str)>,
}

/// 快捷键帮助内容：分为 # 会话与通用、# 输入与编辑、# 旁路交互 三组结构化呈现。
///
/// 默认双列（组内双列排布），宽度不足时退回单列。
/// 键位列按最长绑定取齐，返回内容的自然宽度，模态据此收窄到刚好容纳。
fn help_lines(app: &App, max_width: usize, max_lines: usize) -> (Vec<Line<'static>>, usize) {
    let theme = app.theme;
    let get_key = |act: &str| -> String {
        app.preferences
            .keybindings
            .get(act)
            .map_or_else(|| "—".to_owned(), |keys| crate::config::display_keys(keys))
    };

    let sections = [
        HelpSection {
            title: "# 会话与通用",
            rows: vec![
                (get_key("model"), "选择模型"),
                (get_key("engine"), "切换会话引擎"),
                (get_key("config"), "打开设置"),
                (get_key("agents"), "打开子代理管理"),
                (get_key("resume"), "恢复会话"),
                (get_key("thinking"), "切换思考显示档位"),
                (get_key("thinking_level"), "切换思考等级"),
                (get_key("tools"), "切换工具显示档位"),
                (get_key("help"), "帮助菜单"),
                ("/confirm-plan".into(), "重新审阅待确认的计划"),
                ("/plugins [reload]".into(), "查看/重载已加载插件"),
                ("/close".into(), "卸载会话并释放写租约"),
            ],
        },
        HelpSection {
            title: "# 输入与编辑",
            rows: vec![
                (get_key("send"), "发送消息"),
                (get_key("queue"), "排队发送"),
                (get_key("newline"), "插入换行"),
                (get_key("cancel"), "停止当前生成 / 退出"),
                (get_key("focus"), "切换输入焦点"),
                (get_key("clear"), "清空输入"),
                (get_key("line_start"), "移动到行首"),
                (get_key("line_end"), "移动到行尾"),
                (get_key("delete_word"), "删除前一词"),
                (get_key("delete_to_end"), "删除至行尾"),
                (get_key("paste_image"), "粘贴图片或文本"),
                (get_key("queue_edit"), "编辑排队中的消息"),
            ],
        },
        HelpSection {
            title: "# 旁路交互 (BTW)",
            rows: vec![
                ("/btw <问题>".into(), "旁路追问(不写历史)"),
                ("Alt+B / Tab".into(), "旁路展开与焦点切换"),
                ("Esc / Ctrl+C".into(), "收起旁路 / 取消生成"),
                ("c / x".into(), "复制回答 / 清空旁路"),
            ],
        },
    ];

    let all_rows: Vec<&(String, &'static str)> = sections.iter().flat_map(|s| &s.rows).collect();
    let unified = measure_column_refs(&all_rows);

    let two_columns = HELP_INDENT + unified.1 + HELP_COLUMN_GAP + unified.1 <= max_width;
    let (columns, width) = if two_columns {
        (
            [unified, unified],
            HELP_INDENT + unified.1 + HELP_COLUMN_GAP + unified.1,
        )
    } else {
        ([unified, unified], HELP_INDENT + unified.1)
    };

    let mut lines = Vec::new();
    for (sec_idx, section) in sections.iter().enumerate() {
        if sec_idx > 0 {
            lines.push(Line::from(String::new()));
        }
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(HELP_INDENT), Style::new()),
            Span::styled(
                section.title,
                Style::new()
                    .fg(theme.accent_warning)
                    .add_modifier(Modifier::BOLD),
            ),
        ]));

        if two_columns {
            let split = section.rows.len().div_ceil(2);
            for index in 0..split {
                let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
                spans.extend(help_cell(&section.rows[index], columns[0], theme));
                if let Some(right) = section.rows.get(split + index) {
                    spans.push(Span::styled(" ".repeat(HELP_COLUMN_GAP), Style::new()));
                    spans.extend(help_cell(right, columns[1], theme));
                }
                lines.push(Line::from(spans));
            }
        } else {
            for row in &section.rows {
                let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
                spans.extend(help_cell(row, columns[0], theme));
                lines.push(Line::from(spans));
            }
        }
    }

    lines.push(Line::from(String::new()));
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(HELP_INDENT), Style::new()),
        Span::styled("Esc 关闭帮助", Style::new().fg(theme.gray_dim)),
    ]));

    if lines.len() > max_lines && max_lines > 0 {
        lines.truncate(max_lines);
    }
    (lines, width)
}

fn measure_column_refs(rows: &[&(String, &'static str)]) -> HelpColumn {
    let key_width = rows
        .iter()
        .map(|(key, _)| str_width(key))
        .max()
        .unwrap_or(0);
    let description_width = rows
        .iter()
        .map(|(_, description)| str_width(description))
        .max()
        .unwrap_or(0);
    (key_width, key_width + HELP_KEY_GAP + description_width)
}

/// 帮助表的一格：键位右补齐到列内 `key_width`，整格再补齐到列宽。
fn help_cell(
    row: &(String, &'static str),
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
fn rewind_kind(role: Role) -> &'static str {
    match role {
        Role::User => "用户请求",
        Role::Assistant => "模型回复",
        Role::Tool => "工具结果",
        Role::Custom => "运行时提醒",
        Role::System | Role::Developer => "系统消息",
    }
}

fn rewind_preview(entry: &moc_protocol::methods::TranscriptEntry) -> String {
    let text = entry.message.text();
    if !text.trim().is_empty() {
        return text.replace('\n', " ");
    }
    let calls = entry
        .message
        .tool_calls()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>();
    if !calls.is_empty() {
        return format!("调用 {}", calls.join("、"));
    }
    entry
        .message
        .tool_results()
        .map(|result| result.content.replace('\n', " "))
        .collect::<Vec<_>>()
        .join(" ")
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
fn context_lines(usage: &ContextUsageResult, width: usize, theme: &Theme) -> Vec<Line<'static>> {
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
        ellipsize(name, target_width)
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
/// 带预览的提问卡片的最大宽度（列）：比普通模态更宽，容得下并排的预览栏。
const ASK_PREVIEW_MODAL_WIDTH: u16 = 120;

/// 模态内容行（按类型分派）。
#[allow(dead_code)]
fn modal_lines(app: &App, modal: &Modal, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    modal_lines_with_hits(app, modal, width, max_lines).0
}

/// 模态内容行及可点击项行号（按类型分派）。
fn modal_lines_with_hits(
    app: &App,
    modal: &Modal,
    width: usize,
    max_lines: usize,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let theme = app.theme;
    let mut hits = Vec::new();
    let mut lines = match modal {
        Modal::Permission {
            params,
            selected,
            scroll_offset,
            ..
        } => {
            let mut all_content = Vec::new();
            if let Some(body) = &params.body {
                all_content.extend(body.lines().map(|row| {
                    Line::from(Span::styled(
                        format!("  {row}"),
                        Style::new().fg(theme.text_secondary),
                    ))
                }));
            }
            if let Some(diff) = &params.diff {
                all_content.extend(crate::render::diff::render_diff(
                    diff,
                    width,
                    theme,
                    usize::MAX,
                ));
            }
            // 预留空行 + 选项行 + 底部提示行。
            let options_rows = params.options.len() + 3;
            let max_content_rows = max_lines.saturating_sub(options_rows).max(2);
            let offset = (*scroll_offset).min(all_content.len().saturating_sub(max_content_rows));
            let mut lines: Vec<Line<'static>> = all_content
                .into_iter()
                .skip(offset)
                .take(max_content_rows)
                .collect();
            if lines.is_empty() {
                lines.push(Line::from(String::new()));
            }
            lines.push(Line::from(String::new()));
            for (index, option) in params.options.iter().enumerate() {
                hits.push((index, lines.len()));
                let marker = if index == *selected { "❯ " } else { "  " };
                let style = if index == *selected {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}{} {}", index + 1, option.label),
                    style,
                )));
            }
            lines.push(Line::from(Span::styled(
                "1-9 直选 · enter 确认 · PgUp/PgDn 滚动预览 · esc 按拒绝处理",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Ask { .. } => ask_layout(theme, modal, width, max_lines).lines,
        Modal::Model {
            entries,
            current,
            picker,
            level,
            ..
        } => {
            let filtered = crate::modal::filtered_entries(entries, &picker.query);
            let highlighted = filtered.get(picker.selected).map(|(_, entry)| *entry);
            let resolved = highlighted.and_then(|entry| level.resolve(&entry.levels));
            let has_levels = highlighted.is_some_and(|entry| !entry.levels.is_empty());
            // 行预算优先让给候选列表：9 行以上三处留白全给（标题下、搜索与等级条
            // 之间、等级条与列表之间），7~8 行只留标题下那一处，更矮时一处不留，
            // 先保住至少一行候选和页脚。
            let full_gap = max_lines >= 9;
            let title_gap = max_lines >= 7;
            let mut lines = Vec::with_capacity(max_lines.clamp(4, 32));
            if title_gap {
                lines.push(Line::from(String::new()));
            }
            lines.push(Line::from(vec![
                Span::styled("当前  ".to_owned(), Style::new().fg(theme.gray_dim)),
                Span::styled(
                    ellipsize(
                        crate::app::model_selector_label(current),
                        width.saturating_sub(6),
                    ),
                    Style::new().fg(theme.text_secondary),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::styled("搜索  ".to_owned(), Style::new().fg(theme.gray_dim)),
                Span::styled(
                    if picker.query.is_empty() {
                        "_".to_owned()
                    } else {
                        ellipsize_tail(&picker.query, width.saturating_sub(6))
                    },
                    Style::new().fg(if picker.query.is_empty() {
                        theme.gray_dim
                    } else {
                        theme.accent_model
                    }),
                ),
            ]));
            if full_gap {
                lines.push(Line::from(String::new()));
            }
            lines.extend(level_lines(theme, highlighted, resolved.as_deref(), width));
            if full_gap {
                lines.push(Line::from(String::new()));
            }
            let visible = max_lines.saturating_sub(lines.len() + 1).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            // 规格卡片挤在候选列表右侧：正文够宽、且整张卡塞得进可见行数时才出现，
            // 否则列表独占整行，把上下文窗口压成行尾的紧凑数字。
            let card_width = if width >= META_MIN_CONTENT {
                (width / 3).clamp(META_MIN, META_MAX)
            } else {
                0
            };
            let card = model_meta_lines(theme, highlighted, card_width);
            let show_card = !card.is_empty() && card.len() <= visible;
            let list_width = if show_card {
                width - card_width - CARD_GAP
            } else {
                width
            };
            let mut rows: Vec<Line<'static>> = Vec::with_capacity(visible);

            if picker.loading {
                lines.push(Line::from(Span::styled(
                    "正在切换模型…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 重试 · Esc 关闭",
                    Style::new().fg(theme.gray_dim),
                )));
            }
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "没有匹配的模型",
                    Style::new().fg(theme.gray),
                )));
            } else {
                for (index, (_, entry)) in filtered.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let selected = actual == picker.selected;
                    let marker = if selected { "❯ " } else { "  " };
                    let style = if selected {
                        Style::new()
                            .fg(theme.accent_user)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(theme.text_primary)
                    };
                    // 卡片缺席时把上下文窗口留成行尾数字，至少不丢掉这一项。
                    let suffix = if show_card {
                        String::new()
                    } else {
                        entry
                            .context_window
                            .map(|window| format!("  {}", crate::render::format_tokens(window)))
                            .unwrap_or_default()
                    };
                    let label = ellipsize(
                        &entry.label,
                        list_width
                            .saturating_sub(str_width(marker) + str_width(&suffix))
                            .max(1),
                    );
                    let mut spans = Vec::with_capacity(8);
                    spans.push(Span::styled(marker, style));
                    if let Some((provider, model_name)) = label.split_once('/') {
                        let provider_style = if selected {
                            Style::new().fg(theme.text_secondary).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        };
                        spans.push(Span::styled(format!("{provider}/"), provider_style));
                        spans.push(Span::styled(model_name.to_owned(), style));
                    } else {
                        spans.push(Span::styled(label.clone(), style));
                    }
                    if !suffix.is_empty() {
                        spans.push(Span::styled(
                            suffix.clone(),
                            if selected {
                                Style::new().fg(theme.accent_model).bg(theme.bg_visual)
                            } else {
                                Style::new().fg(theme.accent_model)
                            },
                        ));
                    }
                    // 补齐到列表列宽：选中底色因此是一条规整的横带，不会半路截断。
                    let used = str_width(marker) + str_width(&label) + str_width(&suffix);
                    if used < list_width {
                        spans.push(Span::styled(" ".repeat(list_width - used), style));
                    }
                    rows.push(Line::from(spans));
                }
            }
            // 候选比卡片短时补齐空行，卡片照样画满整张，右边缘才对得齐。
            let block = rows.len().max(if show_card { card.len() } else { 0 });
            for index in 0..block {
                if index < rows.len() {
                    hits.push((offset + index, lines.len()));
                }
                let mut spans = rows
                    .get(index)
                    .map(|row| row.spans.clone())
                    .unwrap_or_else(|| vec![Span::raw(" ".repeat(list_width))]);
                if show_card && let Some(card_line) = card.get(index) {
                    spans.push(Span::raw(" ".repeat(CARD_GAP)));
                    spans.extend(card_line.spans.iter().cloned());
                }
                lines.push(Line::from(spans));
            }
            // 页脚按可用宽度降档：完整提示 → 去掉搜索提示的中档 → 只留关键键的紧凑档。
            // 阈值就是各串的显示宽度，保证 Paragraph 不折行、不会多占一行把列表挤掉；
            // 最后再用 ellipsize 兜住更窄的终端。
            let footer = if has_levels {
                if width >= 57 {
                    "↑↓ 移动 · Tab 切换等级 · 输入搜索 · Enter 确认 · Esc 关闭"
                } else if width >= 46 {
                    "↑↓ 移动 · Tab 切换等级 · Enter 确认 · Esc 关闭"
                } else {
                    "Tab 切换等级 · Enter 确认 · Esc 关闭"
                }
            } else if width >= 42 {
                "↑↓ 移动 · 输入搜索 · Enter 确认 · Esc 关闭"
            } else {
                "↑↓ 移动 · Enter 确认 · Esc 关闭"
            };
            lines.push(Line::from(Span::styled(
                ellipsize(footer, width),
                Style::new().fg(theme.gray_dim),
            )));
            lines
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
            let mut lines = vec![
                Line::from(Span::styled(leading, Style::new().fg(theme.text_primary))),
                Line::from(Span::styled(
                    format!(
                        "确认后先切换到 {selector}，再由它压缩上下文（压缩后不超过 {}）",
                        crate::render::format_tokens(overflow.target_tokens)
                    ),
                    Style::new().fg(theme.text_secondary),
                )),
                Line::from(String::new()),
            ];
            if *loading {
                lines.push(Line::from(Span::styled(
                    "正在切换并压缩…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
            }
            for (index, text) in ["切换并压缩", "取消"].iter().enumerate() {
                let marker = if index == *option { "❯ " } else { "  " };
                let style = if index == *option {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
            }
            lines.push(Line::from(Span::styled(
                if error.is_some() {
                    "enter 重试 · esc 取消"
                } else {
                    "↑↓ 选择 · enter 确认 · esc 取消"
                },
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Rewind { state } => {
            let mut lines = vec![Line::from(Span::styled(
                "↑↓ 选择当前用户消息内的条目 · ←→ 切换用户消息",
                Style::new().fg(theme.gray_dim),
            ))];
            if state.loading && state.prompts.is_empty() {
                lines.push(Line::from(Span::styled(
                    "正在加载消息历史…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &state.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 重试 · Esc 关闭",
                    Style::new().fg(theme.gray_dim),
                )));
            } else if state.prompts.is_empty() {
                lines.push(Line::from(Span::styled(
                    "当前会话没有可回退的消息",
                    Style::new().fg(theme.gray),
                )));
            } else {
                let prompt = &state.prompts[state.selected_prompt];
                lines.push(Line::from(Span::styled(
                    format!(
                        "用户消息 {}/{} · 当前条目 {}/{}",
                        state.selected_prompt + 1,
                        state.prompts.len(),
                        state.selected_item + 1,
                        prompt.len()
                    ),
                    Style::new().fg(theme.accent_model),
                )));
                let visible = max_lines.saturating_sub(5).max(1);
                let offset = state
                    .selected_item
                    .saturating_sub(visible.saturating_sub(1));
                for (index, entry) in prompt.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let selected = actual == state.selected_item;
                    let marker = if selected { "❯ " } else { "  " };
                    let text = format!(
                        "{marker}{}  {}",
                        rewind_kind(entry.message.role),
                        rewind_preview(entry)
                    );
                    lines.push(Line::from(Span::styled(
                        ellipsize(&text, width),
                        if selected {
                            Style::new()
                                .fg(theme.accent_user)
                                .bg(theme.bg_visual)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        },
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "Enter 回退到此处（用户消息会恢复到输入框） · Esc 取消",
                    Style::new().fg(theme.gray_dim),
                )));
            }
            lines
        }
        Modal::Sessions {
            sessions,
            cwd,
            scope,
            picker,
        } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query, *scope, cwd);
            let visible = max_lines.saturating_sub(4).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "搜索：{}",
                    if picker.query.is_empty() {
                        "_"
                    } else {
                        &picker.query
                    }
                ),
                Style::new().fg(theme.accent_model),
            ))];
            if picker.loading && sessions.is_empty() {
                lines.push(Line::from(Span::styled(
                    "正在加载会话…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 重试 · Esc 关闭",
                    Style::new().fg(theme.gray_dim),
                )));
            } else if filtered.is_empty() {
                // 搜索无结果与目录为空要分开说：后者才值得引导切范围。
                let hint = if picker.query.is_empty() && *scope == SessionScope::Folder {
                    "当前目录没有会话 · Tab 查看全部"
                } else {
                    "没有匹配的会话"
                };
                lines.push(Line::from(Span::styled(hint, Style::new().fg(theme.gray))));
            } else {
                for (index, (_, session)) in filtered.iter().skip(offset).take(visible).enumerate()
                {
                    let actual = offset + index;
                    hits.push((actual, lines.len()));
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let mut text = format!("{marker}{}", session.title);
                    if let Some(source) = crate::modal::fork_source_label(sessions, session) {
                        text.push_str(&format!("  ← {source}"));
                    }
                    if *scope == SessionScope::All {
                        text.push_str(&format!("  {}", ellipsize(&session.cwd, width / 2)));
                    }
                    lines.push(Line::from(Span::styled(
                        ellipsize(&text, width),
                        if actual == picker.selected {
                            Style::new()
                                .fg(theme.accent_user)
                                .bg(theme.bg_visual)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        },
                    )));
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ 移动 · PgUp/PgDn 翻页 · Tab 切换范围 · 输入搜索 · Enter 恢复 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Plan { state } => plan_lines(state, width, max_lines, theme, &mut hits),
        Modal::Context { usage } => {
            let mut lines = match usage {
                Some(usage) => context_lines(usage, width, theme),
                None => vec![Line::from(vec![
                    Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
                    Span::styled("正在计算上下文…", Style::new().fg(theme.accent_running)),
                ])],
            };
            lines.push(Line::from(String::new()));
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
                Span::styled("Esc 关闭", Style::new().fg(theme.gray_dim)),
            ]));
            lines
        }
        Modal::Subagents { agents, picker } => {
            let query = picker.query.to_lowercase();
            let filtered: Vec<_> = agents
                .iter()
                .filter(|agent| {
                    agent.title.to_lowercase().contains(&query) || agent.id.contains(&query)
                })
                .collect();
            let mut lines = Vec::with_capacity(filtered.len() + 2);
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "当前会话还没有子代理",
                    Style::new().fg(theme.text_secondary),
                )));
                lines.push(Line::from(Span::styled(
                    "子代理由模型在复杂任务中自动派生",
                    Style::new().fg(theme.gray_dim),
                )));
            } else {
                for (index, agent) in filtered.iter().enumerate() {
                    let selected = index == picker.selected;
                    let status_str = crate::render::scrollback::agent_status_label(agent.status);
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{}{} · {} · {}",
                            if selected { "❯ " } else { "  " },
                            agent.title,
                            status_str,
                            agent.model
                        ),
                        if selected {
                            Style::new().fg(theme.text_primary).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.text_primary)
                        },
                    )));
                    if let Some(message) = &agent.last_message {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", ellipsize(message, width.saturating_sub(4))),
                            Style::new().fg(theme.gray_dim),
                        )));
                    }
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ 选择 · Enter 聚焦 · Ctrl+S 停止 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Help => help_lines(app, width, max_lines).0,
        // 正文窗自带布局与滚动，`draw_modal` 在此之前就已分派走。
        Modal::ContextContent { .. } => Vec::new(),
    };
    lines.truncate(max_lines.max(1));
    (lines, hits)
}

/// 提问卡片的布局产物：正文行 + 文本坐标的可点击几何，`draw_ask_modal` 换算到屏幕。
struct AskLayout {
    lines: Vec<Line<'static>>,
    /// (选项下标, 行号)；`options.len()` 下标代表自定义行。
    option_hits: Vec<(usize, usize)>,
    /// (问题下标, 行号, 起始列, 结束列)：顶部问题导航条的点击范围。
    nav_hits: Vec<(usize, usize, usize, usize)>,
    /// 可滚动的问题正文窗口（文本坐标）；`None` = 未溢出，滚轮落在此区仍动光标。
    question_area: Option<Rect>,
    /// 可滚动的详情区（单栏底区或双栏右栏，文本坐标）；同上仅在溢出时给出。
    detail_area: Option<Rect>,
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
        return AskLayout {
            lines: Vec::new(),
            option_hits: Vec::new(),
            nav_hits: Vec::new(),
            question_area: None,
            detail_area: None,
        };
    };
    let mut lines = Vec::new();
    let mut nav_hits = Vec::new();
    // 多问时顶部是导航条而不是计数器：`n. header` 逐项排开，当前问反白、
    // 已答带 ✓；每一项记下列范围供鼠标跳问。
    if params.questions.len() > 1 {
        let nav_row = lines.len();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        for (index, entry) in params.questions.iter().enumerate() {
            if index > 0 {
                spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
                col += 3;
            }
            let answered = selected.get(index).is_some_and(|picked| !picked.is_empty())
                || custom
                    .get(index)
                    .is_some_and(|answer| !answer.text.is_empty());
            let header = entry.header.clone().unwrap_or_else(|| "提问".into());
            let label = ellipsize(&format!("{}. {header}", index + 1), ASK_NAV_ITEM_MAX);
            let item_width = str_width(&label) + usize::from(answered) * 2;
            if col + item_width > width {
                spans.push(Span::styled("…", Style::new().fg(theme.gray_dim)));
                break;
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
        if col + 3 + confirm_width <= width {
            spans.push(Span::styled(" · ", Style::new().fg(theme.gray_dim)));
            nav_hits.push((
                params.questions.len(),
                nav_row,
                col + 3,
                col + 3 + confirm_width,
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
    // 溢出时末行给出滚动状态（滚轮/PgUp·PgDn 读全文），窗口区记为可滚命中。
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
    // 预算不足保住两个可见选项 + 自定义行 + 页脚时，先压详情区再整个让掉。
    let detail_content: Vec<Line<'static>> = if two_col {
        Vec::new()
    } else {
        focused_option
            .map(|option| focused_detail_lines(option, width, theme))
            .unwrap_or_default()
    };
    // 固定行：自定义行 1 + 页脚 1。
    let fixed_tail = lines.len() + 2;
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
    // 剩余行预算给选项列表窗口 + 自定义行 + 页脚。
    let opt_budget = max_lines
        .saturating_sub(lines.len() + 1 + detail_zone_rows + 1)
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
            Span::styled(ellipsize(&option.label, label_budget), style),
        ];
        if recommended {
            spans.push(Span::styled("（推荐）", Style::new().fg(theme.gray)));
        }
        option_rows.push(Line::from(spans));
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
        ellipsize(
            &format!(
                " {} ✎ 自定义：{custom_display}",
                if editing_here { "❯" } else { " " }
            ),
            width,
        ),
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
        let pane = scroll_window(pane_content, pane_budget, *preview_scroll, theme);
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
            let windowed = scroll_window(detail_content, capacity, *preview_scroll, theme);
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
    // 页脚一行：方向键、数字键属常规交互不教，只留反直觉的键；
    // 多选最后一问 enter 从切勾变成进确认页，提示如实切换。
    let pick_hint = if current.multi {
        if *question + 1 >= params.questions.len() {
            "space 勾选 · enter 确认"
        } else {
            "space/enter 勾选"
        }
    } else {
        "enter 选择"
    };
    let nav_hint = (params.questions.len() > 1).then_some("tab 换题");
    let footer = [
        Some(pick_hint),
        nav_hint,
        Some("esc 折叠问题卡"),
        Some("shift+X 跳过"),
    ]
    .into_iter()
    .flatten()
    .collect::<Vec<_>>()
    .join(" · ");
    // 页脚与正文之间空一行：提示贴向卡片底部、不黏在最后一行内容上。
    // 这是装饰行——预算占满时先牺牲它，不挤掉选项/详情/页脚本身。
    if lines.len() + 1 < max_lines {
        lines.push(Line::from(String::new()));
    }
    lines.push(Line::from(Span::styled(
        ellipsize(&footer, width),
        Style::new().fg(theme.gray_dim),
    )));
    AskLayout {
        lines,
        option_hits,
        nav_hits,
        question_area,
        detail_area,
    }
}

/// 把内容截取为最多 `capacity` 行的滚动窗口；放不下时末行换成
/// 「… a-b/total 行 · 滚轮/PgUp·PgDn」状态行（各滚动区同一约定）。
fn scroll_window(
    rows: Vec<Line<'static>>,
    capacity: usize,
    scroll: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let total = rows.len();
    let capacity = capacity.max(1);
    if total <= capacity {
        return rows;
    }
    let visible = capacity - 1;
    let offset = scroll.min(total.saturating_sub(visible));
    let mut out: Vec<Line<'static>> = rows.into_iter().skip(offset).take(visible).collect();
    out.push(Line::from(Span::styled(
        format!(
            "… {}-{}/{total} 行 · 滚轮/PgUp·PgDn",
            offset + 1,
            offset + visible
        ),
        Style::new().fg(theme.gray_dim),
    )));
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
            option_hits: Vec::new(),
            nav_hits,
            question_area: None,
            detail_area: None,
        };
    };
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        "确认回答",
        Style::new()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::BOLD),
    )));
    let rows = max_lines.saturating_sub(lines.len() + 1).max(1);
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
        let key = ellipsize(entry.header.as_deref().unwrap_or(&entry.id), key_width);
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
                ellipsize(&parts.join(", "), width.saturating_sub(key_width + 2)),
                Style::new().fg(theme.text_primary),
            ));
        }
        answers.push(Line::from(spans));
    }
    let scroll = (*preview_scroll).min(answers.len().saturating_sub(rows));
    lines.extend(answers.into_iter().skip(scroll).take(rows));
    // 与作答页同款：页脚前空一行（预算占满时让位），提示行沉到卡片底部。
    if lines.len() + 1 < max_lines {
        lines.push(Line::from(String::new()));
    }
    lines.push(Line::from(Span::styled(
        ellipsize("enter 确认提交 · esc 折叠问题卡 · shift+X 跳过", width),
        Style::new().fg(theme.gray_dim),
    )));
    AskLayout {
        lines,
        option_hits: Vec::new(),
        nav_hits,
        question_area: None,
        detail_area: None,
    }
}

/// 思考等级条：`╭`／`╰` 两个圆角把标签与等级挂在同一条左轨上，等级用方括号裹成一段分段控件，
/// 固定占两行，切换条目时列表不跳动。
///
/// 标签行只负责命名，重心落在芯片行：选中项反白成强调色，其余降一档灰，`│` 分隔线再暗一档，
/// 眼睛先读等级、后读分隔。条目没有可选等级（角色伪值）时照旧占两行、显示占位。
fn level_lines(
    theme: &Theme,
    entry: Option<&crate::modal::ModelEntry>,
    resolved: Option<&str>,
    width: usize,
) -> [Line<'static>; 2] {
    let corner_style = Style::new().fg(theme.border_active);
    let label = Line::from(vec![
        Span::styled("╭ ".to_owned(), corner_style),
        Span::styled(
            "思考等级".to_owned(),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
    ]);
    let levels = entry.map_or(&[][..], |entry| entry.levels.as_slice());
    if levels.is_empty() {
        return [
            label,
            Line::from(vec![
                Span::styled("╰ ".to_owned(), corner_style),
                Span::styled(
                    if entry.is_some() {
                        "— 不适用"
                    } else {
                        "—"
                    }
                    .to_owned(),
                    Style::new().fg(theme.gray_dim),
                ),
            ]),
        ];
    }
    let bracket_style = Style::new().fg(theme.text_secondary);
    let separator_style = Style::new().fg(theme.gray_dim);
    // 芯片两侧各留 1 格底色，选中块因此在文字两侧等宽；分隔线插在两块之间，相邻等级相距 3 格。
    // 宽度预算先扣掉收尾的 `]`：宁可少显示几档，也不让右括号被挤掉。
    let mut spans = vec![
        Span::styled("╰ ".to_owned(), corner_style),
        Span::styled("[".to_owned(), bracket_style),
    ];
    let mut used = str_width("╰ [");
    let mut shown = 0usize;
    for level in levels {
        let chip = format!(" {level} ");
        let separator = usize::from(shown > 0);
        if used + str_width(&chip) + separator + 1 > width {
            break;
        }
        if shown > 0 {
            spans.push(Span::styled("│".to_owned(), separator_style));
        }
        used += str_width(&chip) + separator;
        spans.push(Span::styled(
            chip,
            if resolved == Some(level.as_str()) {
                Style::new()
                    .fg(theme.bg_dark)
                    .bg(theme.accent_model)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.gray)
            },
        ));
        shown += 1;
    }
    // 一格芯片都放不下就别留空括号，换成占位符，窄终端上不会出现不闭合的 `[`。
    if shown == 0 {
        spans.truncate(1);
        spans.push(Span::styled(
            "—".to_owned(),
            Style::new().fg(theme.gray_dim),
        ));
    } else {
        spans.push(Span::styled("]".to_owned(), bracket_style));
    }
    [label, Line::from(spans)]
}

/// 规格卡片出现的正文宽度下限：再窄就把整行让给候选列表，不再分栏。
const META_MIN_CONTENT: usize = 74;
/// 卡片外框宽度的上下限，以及两列之间的空隙。
const META_MIN: usize = 29;
const META_MAX: usize = 31;
const CARD_GAP: usize = 2;

/// 选中模型的规格卡片：只画真实生效的项，没值的整行（连同字段名）都不出现。
///
/// 字段名一律英文短标签、值靠左对齐；等级列表按值列宽度折行，续行只缩进。
fn model_meta_lines(
    theme: &Theme,
    entry: Option<&crate::modal::ModelEntry>,
    width: usize,
) -> Vec<Line<'static>> {
    let Some(entry) = entry else {
        return Vec::new();
    };
    if width < META_MIN {
        return Vec::new();
    }
    let inner = width - 2;
    // 行内布局：边框 + 1 空格 + 字段名 + 2 空格 + 值 + 1 空格 + 边框。
    let label_width = 7;
    let value_width = inner.saturating_sub(label_width + 4);
    if value_width < 8 {
        return Vec::new();
    }
    let mut rows: Vec<(String, String)> = Vec::with_capacity(8);
    if let Some(window) = entry.context_window {
        rows.push(("context".to_owned(), crate::render::format_tokens(window)));
    }
    if let Some(tokens) = entry.max_output_tokens {
        rows.push((
            "output".to_owned(),
            crate::render::format_tokens(u64::from(tokens)),
        ));
    }
    if !entry.inputs.is_empty() {
        let inputs: Vec<&str> = entry
            .inputs
            .iter()
            .map(|input| match input {
                moc_protocol::methods::ModelInput::Text => "text",
                moc_protocol::methods::ModelInput::Image => "image",
            })
            .collect();
        rows.push(("input".to_owned(), inputs.join(" · ")));
    }
    if let Some(pricing) = &entry.pricing {
        rows.push((
            "price".to_owned(),
            format!(
                "{} → {}/M",
                meta_price(pricing.input),
                meta_price(pricing.output)
            ),
        ));
        // 缓存读写各自选填，只把配置了的那一项画出来。
        let cache: Vec<String> = [pricing.cache_read, pricing.cache_write]
            .into_iter()
            .flatten()
            .map(meta_price)
            .collect();
        if !cache.is_empty() {
            rows.push(("cache".to_owned(), format!("{}/M", cache.join(" / "))));
        }
    }
    if entry.remote_compaction {
        rows.push(("compact".to_owned(), "on".to_owned()));
    }
    if !entry.levels.is_empty() {
        let mut wrapped: Vec<String> = Vec::with_capacity(2);
        let mut current = String::new();
        for level in &entry.levels {
            let candidate = if current.is_empty() {
                level.clone()
            } else {
                format!("{current} · {level}")
            };
            if str_width(&candidate) <= value_width {
                current = candidate;
                continue;
            }
            if !current.is_empty() {
                wrapped.push(std::mem::take(&mut current));
            }
            current.clone_from(level);
        }
        if !current.is_empty() {
            wrapped.push(current);
        }
        for (index, value) in wrapped.into_iter().enumerate() {
            let label = if index == 0 {
                "levels".to_owned()
            } else {
                String::new()
            };
            rows.push((label, value));
        }
    }
    let border_style = Style::new().fg(theme.border_active);
    let rule = "─".repeat(inner);
    let mut lines = Vec::with_capacity(rows.len() + 2);
    lines.push(Line::from(Span::styled(format!("╭{rule}╮"), border_style)));
    if rows.is_empty() {
        // 角色伪值没有任何可选元数据：给一行说明，不留一只空框。
        let text = pad_to(
            &format!(" {}", ellipsize(&entry.label, inner.saturating_sub(2))),
            inner,
        );
        lines.push(Line::from(vec![
            Span::styled("│", border_style),
            Span::styled(text, Style::new().fg(theme.gray_dim)),
            Span::styled("│", border_style),
        ]));
    }
    for (label, value) in rows {
        lines.push(Line::from(vec![
            Span::styled("│", border_style),
            Span::raw(" "),
            Span::styled(pad_to(&label, label_width), Style::new().fg(theme.gray_dim)),
            Span::raw("  "),
            Span::styled(
                pad_to(&ellipsize(&value, value_width), value_width),
                Style::new().fg(theme.text_primary),
            ),
            Span::raw(" "),
            Span::styled("│", border_style),
        ]));
    }
    lines.push(Line::from(Span::styled(format!("╰{rule}╯"), border_style)));
    lines
}

/// 右侧补空格到指定显示宽度；已经够宽就原样返回。
fn pad_to(text: &str, width: usize) -> String {
    let used = str_width(text);
    if used >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - used))
    }
}

/// 每百万 token 的单价：最多三位小数，去掉尾随零，`$1`、`$0.075`、`$1.25`。
fn meta_price(value: f64) -> String {
    let text = format!("{value:.3}");
    format!("${}", text.trim_end_matches('0').trim_end_matches('.'))
}

pub(crate) fn ellipsize(text: &str, width: usize) -> String {
    if crate::render::wrap::str_width(text) <= width {
        return text.to_owned();
    }
    crate::render::scrollback::truncate_to_width(text.to_owned(), width.max(1))
}

/// 保留尾部、截掉开头的省略：搜索框的光标在结尾，越靠后的字符越是正在输入的内容。
fn ellipsize_tail(text: &str, width: usize) -> String {
    if crate::render::wrap::str_width(text) <= width {
        return text.to_owned();
    }
    // `…` 占 1 列，剩下的预算从末尾往前数，凑不满的宽字符整体丢掉。
    let budget = width.saturating_sub(1);
    let mut used = 0usize;
    let mut start = text.len();
    for (index, ch) in text.char_indices().rev() {
        let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + ch_width > budget {
            break;
        }
        used += ch_width;
        start = index;
    }
    let mut out = String::with_capacity(text.len() - start + '…'.len_utf8());
    out.push('…');
    out.push_str(&text[start..]);
    out
}

/// 一行的显示宽度（列，CJK 双宽）。
fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| str_width(span.content.as_ref()))
        .sum()
}

/// 截断一行到 `width` 列，超出部分以 `…` 收尾。
fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line_width(&line) <= width {
        return line;
    }
    let budget = width.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
    let mut used = 0usize;
    for span in line.spans {
        let mut kept = String::new();
        let mut whole = true;
        for ch in span.content.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_width > budget {
                whole = false;
                break;
            }
            kept.push(ch);
            used += ch_width;
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, span.style));
        }
        if !whole {
            spans.push(Span::styled("…", span.style));
            break;
        }
    }
    Line::from(spans).style(line.style)
}

/// 补齐一行到 `width` 列，超出则截断：并排布局两栏互不越界的基础。
fn fit_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line_width(&line);
    if used > width {
        return clip_line(line, width);
    }
    if used == width {
        return line;
    }
    let mut spans = line.spans;
    spans.push(Span::raw(" ".repeat(width - used)));
    Line::from(spans).style(line.style)
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

/// 计划审阅卡片顶部的固定行数：执行模型、标题/版本、路径。
pub(crate) const PLAN_HEADER_ROWS: usize = 3;

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
    match status {
        PlanStatus::Draft => ("起草中", theme.accent_running),
        PlanStatus::Submitted => ("待审阅", theme.accent_success),
        PlanStatus::Revised => ("等待修订", theme.accent_warning),
        PlanStatus::Superseded => ("已被取代", theme.gray),
        PlanStatus::Approved => ("已批准", theme.accent_success),
        PlanStatus::Rejected => ("已拒绝", theme.accent_error),
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
pub(crate) fn plan_fixed_rows(state: &PlanReviewState) -> usize {
    let mut fixed = PLAN_HEADER_ROWS + 1;
    if state.pane == PlanPane::Read && state.status == PlanStatus::Submitted {
        fixed += PLAN_ACTIONS_LEN;
    }
    if state.status != PlanStatus::Submitted {
        fixed += 1;
    }
    if state.error.is_some() || state.submitting {
        fixed += 1;
    }
    fixed
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

/// 计划审阅卡片的内容行。
fn plan_lines(
    state: &PlanReviewState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
    hits: &mut Vec<(usize, usize)>,
) -> Vec<Line<'static>> {
    let mut lines = Vec::with_capacity(max_lines);
    let prefix = "执行模型：";
    lines.push(Line::from(vec![
        Span::styled(prefix, Style::new().fg(theme.text_secondary)),
        Span::styled(
            ellipsize(
                crate::app::model_selector_label(&state.execution_model),
                width.saturating_sub(str_width(prefix)),
            ),
            Style::new()
                .fg(theme.accent_model)
                .add_modifier(Modifier::UNDERLINED),
        ),
    ]));
    let (status_label, status_color) = plan_status_label(theme, state.status);
    lines.push(Line::from(vec![
        Span::styled(
            ellipsize(&format!("{} · v{}", state.title, state.version), width),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {status_label}"), Style::new().fg(status_color)),
    ]));
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
        ellipsize(&location, width),
        Style::new().fg(theme.gray),
    )));

    let body_rows = plan_body_rows(state, max_lines);
    match state.pane {
        PlanPane::Read => {
            if state.show_diff && state.previous_plan_id.is_some() {
                if let Some(error) = &state.diff_error {
                    lines.push(Line::from(Span::styled(
                        ellipsize(&format!("无法加载改动：{error}"), width),
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
                        lines.extend(
                            rendered
                                .into_iter()
                                .skip(state.scroll_offset)
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
                    lines.extend(
                        rendered
                            .into_iter()
                            .skip(state.scroll_offset)
                            .take(body_rows),
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

    let hint = match state.pane {
        PlanPane::Read if state.show_diff && state.previous_plan_id.is_some() => {
            "↑↓/PgUp/PgDn 滚动 · D 看计划 · A 批注 · ←→/Tab 选择 · Enter 确认 · Ctrl+P 选模型 · M 换引擎"
        }
        PlanPane::Read => {
            "↑↓/PgUp/PgDn 滚动 · D 看改动 · A 批注 · ←→/Tab 选择 · Enter 确认 · Ctrl+P 选模型 · M 换引擎"
        }
        PlanPane::Annotate => {
            "↑↓ 移动 · v 选行 · c 批注 · Ctrl+G 建议改法 · x 删除 · s 发送 · A 返回阅读 · Esc 返回"
        }
    };
    lines.push(Line::from(Span::styled(
        ellipsize(hint, width),
        Style::new().fg(theme.gray_dim),
    )));

    if state.pane == PlanPane::Read && state.status == PlanStatus::Submitted {
        for index in 0..PLAN_ACTIONS_LEN {
            let label = plan_action_label(state, index);
            hits.push((index, lines.len()));
            let style = if state.submitting {
                Style::new().fg(theme.gray_dim)
            } else if state.selected == index {
                Style::new()
                    .fg(theme.accent_success)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            lines.push(Line::from(Span::styled(
                ellipsize(
                    &format!(
                        "{} {label}",
                        if state.selected == index { "❯" } else { " " }
                    ),
                    width,
                ),
                style,
            )));
        }
    } else if state.status != PlanStatus::Submitted {
        lines.push(Line::from(Span::styled(
            ellipsize(plan_status_note(state.status), width),
            Style::new().fg(theme.gray),
        )));
    }

    if let Some(error) = &state.error {
        lines.push(Line::from(Span::styled(
            ellipsize(&format!("错误：{error}"), width),
            Style::new().fg(theme.accent_error),
        )));
    } else if state.submitting {
        lines.push(Line::from(Span::styled(
            "正在提交…",
            Style::new().fg(theme.accent_running),
        )));
    }
    lines
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
    let mut text = ellipsize(
        source.get(index).copied().unwrap_or(""),
        width.saturating_sub(gutter_width),
    );
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

/// 批注编辑器：居中覆盖在模态卡片之上，光标落在正文里。
fn draw_plan_editor(frame: &mut Frame, app: &App, modal_area: Rect, area: Rect) {
    let theme = app.theme;
    let Some(Modal::Plan { state }) = &app.modal else {
        return;
    };
    let Some(editor) = &state.editor else {
        return;
    };
    let width = area
        .width
        .saturating_sub(8)
        .clamp(32, 76)
        .min(modal_area.width);
    let inner_width = usize::from(width.saturating_sub(4)).max(8);
    let wrapped = wrap_input(&editor.body, inner_width);
    let quote_rows: Vec<String> = editor
        .quote
        .lines()
        .take(2)
        .map(|line| ellipsize(&format!("> {line}"), inner_width))
        .collect();
    let hint = "Ctrl+S 保存 · Ctrl+G 插入建议改法 · Esc 取消";
    // 边框 2 行 + 引用 + 标签 + 正文 + 提示。
    let content_rows = quote_rows.len() + 1 + wrapped.lines.len().max(1) + 1;
    let height = u16::try_from(content_rows.saturating_add(2))
        .unwrap_or(u16::MAX)
        .min(area.height.saturating_sub(4).max(6));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let visible_rows = usize::from(rect.height.saturating_sub(2));
    let fixed = quote_rows.len() + 2;
    let body_space = visible_rows.saturating_sub(fixed).max(1);
    let start = wrapped
        .cursor_row
        .saturating_sub(body_space.saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(visible_rows);
    for row in &quote_rows {
        lines.push(Line::from(Span::styled(
            row.clone(),
            Style::new().fg(theme.gray),
        )));
    }
    lines.push(Line::from(Span::styled(
        "批注正文（可用 ```suggestion 围栏给出替换文本）",
        Style::new().fg(theme.text_secondary),
    )));
    for row in wrapped.lines.iter().skip(start).take(body_space) {
        lines.push(Line::from(Span::styled(
            row.clone(),
            Style::new().fg(theme.text_primary),
        )));
    }
    lines.push(Line::from(Span::styled(
        ellipsize(hint, inner_width),
        Style::new().fg(theme.gray_dim),
    )));
    frame.render_widget(Clear, rect);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active))
                .style(Style::new().bg(theme.bg_base))
                .padding(Padding::horizontal(1))
                .title(Span::styled(
                    format!(" 批注 · 第 {}-{} 行 ", editor.start_line, editor.end_line),
                    Style::new()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                )),
        ),
        rect,
    );
    let cursor_row = wrapped.cursor_row.saturating_sub(start);
    let y = rect.y + 1 + u16::try_from(quote_rows.len() + 1 + cursor_row).unwrap_or(u16::MAX);
    let x = rect.x + 2 + u16::try_from(wrapped.cursor_column).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        x.min(rect.right().saturating_sub(2)),
        y.min(rect.bottom().saturating_sub(2)),
    ));
}

#[cfg(test)]
mod tests {
    use super::{ask_layout, modal_lines};
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
                remote_compaction: false,
                pricing: Some(moc_protocol::methods::ModelPricing {
                    input: 0.15,
                    output: 0.6,
                    cache_read: Some(0.015),
                    cache_write: None,
                }),
            }],
            current: "p/model:high".into(),
            picker: PickerState::default(),
            level: crate::modal::LevelState::default(),
            target: crate::modal::ModelTarget::Session,
        };

        let lines = modal_lines(&app, &modal, 80, 20);
        let level_label = lines
            .iter()
            .find(|line| line_text(line).starts_with("╭ 思考等级"))
            .expect("思考等级标签行");
        assert_eq!(line_text(level_label), "╭ 思考等级");
        let level_line = lines
            .iter()
            .find(|line| line_text(line).starts_with("╰ ["))
            .expect("思考等级条");
        assert_eq!(line_text(level_line), "╰ [ off │ high ]");
        let chip = |name: &str| {
            level_line
                .spans
                .iter()
                .find(|span| span.content.trim() == name)
                .unwrap_or_else(|| panic!("{name} 出现在等级条"))
        };
        assert_eq!(chip("high").content.as_ref(), " high ");
        assert_eq!(chip("high").style.bg, Some(theme.accent_model));
        assert_eq!(chip("high").style.fg, Some(theme.bg_dark));
        assert_eq!(chip("off").content.as_ref(), " off ");
        assert_eq!(chip("off").style.bg, None);
        let separator = level_line
            .spans
            .iter()
            .find(|span| span.content.as_ref() == "│")
            .expect("等级分隔线");
        assert_eq!(separator.style.fg, Some(theme.gray_dim));
        let row = lines
            .iter()
            .find(|line| line_text(line).starts_with("❯ "))
            .expect("模型行");
        let text = line_text(row);
        // 列表列只留模型名；上下文改由右侧卡片展示，行里不再堆字数。
        assert!(text.starts_with("❯ p/model"), "{text}");
        assert!(!text.contains("Ctx"), "{text}");
        assert!(text.ends_with('╮'), "选中行右侧应当接上卡片顶边：{text}");
        assert_eq!(row.spans[1].content.as_ref(), "p/");
        assert_eq!(row.spans[1].style.fg, Some(theme.text_secondary));
        assert_eq!(row.spans[2].content.as_ref(), "model");
        assert_eq!(row.spans[2].style.fg, Some(theme.accent_user));
    }

    /// 模型选择器在任何高度都要保住页脚与至少一条候选：留白随行预算递减，
    /// 等级条两行始终相邻，页脚永远单行收尾，且每行都不超过正文宽度。
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
                    remote_compaction: true,
                    pricing: None,
                },
                crate::modal::ModelEntry {
                    selector: "p/other".into(),
                    label: "p/other".into(),
                    display_name: "Other".into(),
                    levels: vec!["off".into(), "high".into()],
                    context_window: Some(128_000),
                    max_output_tokens: None,
                    inputs: Vec::new(),
                    remote_compaction: false,
                    pricing: None,
                },
            ],
            current: "p/model:high".into(),
            picker: PickerState::default(),
            level: crate::modal::LevelState {
                manual: Some("high".into()),
            },
            target: crate::modal::ModelTarget::Session,
        });
        let modal = app.modal.as_ref().expect("模型选择器");

        // (正文宽度, 行预算, 标题下留白, 内部留白)
        for (width, budget, title_gap, full_gap) in [
            (76usize, 18usize, true, true),
            (56, 8, true, false),
            (36, 6, false, false),
        ] {
            let lines = modal_lines(&app, modal, width, budget);
            let texts: Vec<String> = lines.iter().map(line_text).collect();
            assert!(
                lines.len() <= budget,
                "{width}/{budget} 超出预算：{texts:?}"
            );
            for text in &texts {
                assert!(
                    crate::render::wrap::str_width(text) <= width,
                    "{text:?} 超过 {width} 列，会被 Paragraph 折行多占一行"
                );
            }
            // 留白阶梯：预算够就三处全给，其次只留标题下那一处，更矮时一处不留。
            assert_eq!(texts[0].is_empty(), title_gap, "{texts:?}");
            let search = texts
                .iter()
                .position(|text| text.starts_with("搜索  "))
                .expect("缺少搜索行");
            let label = texts
                .iter()
                .position(|text| text.contains("思考等级"))
                .expect("缺少等级条标签行");
            assert_eq!(label, search + 1 + usize::from(full_gap), "{texts:?}");
            assert_eq!(texts[label + 2].is_empty(), full_gap, "{texts:?}");
            // 圆角与方括号：标签行 `╭ 思考等级`，芯片行 `╰ [ … ]` 首尾成对。
            assert_eq!(texts[label], "╭ 思考等级", "{texts:?}");
            assert!(texts[label + 1].starts_with("╰ ["), "{texts:?}");
            assert!(texts[label + 1].ends_with(']'), "{texts:?}");
            // 行首标记只属于候选列表，等级条不再抢 `❯`。
            assert!(texts.iter().any(|text| text.starts_with("❯ ")), "{texts:?}");
            // 正文够宽就并排卡片，否则上下文压成行尾数字。
            let row = texts
                .iter()
                .find(|text| text.starts_with("❯ "))
                .expect("模型行");
            if width >= 74 {
                assert!(row.ends_with('╮'), "{row}");
                assert!(
                    texts
                        .iter()
                        .any(|text| text.contains("context") && text.contains("500k")),
                    "{texts:?}"
                );
            } else {
                assert!(row.contains("500k") && !row.contains("context"), "{row}");
            }
            // 页脚：唯一、收尾，宽度不够时降档而不是折行。
            let footer = texts.last().expect("缺少页脚");
            assert_eq!(
                texts.iter().filter(|text| text.contains("Esc")).count(),
                1,
                "{texts:?}"
            );
            assert!(footer.contains("Enter 确认") && footer.contains("Esc 关闭"));
            if width == 76 {
                assert!(footer.contains("输入搜索") && footer.contains("Tab 切换等级"));
            } else {
                assert!(
                    !footer.contains("输入搜索"),
                    "窄宽度应换成紧凑提示：{footer:?}"
                );
            }
        }
    }

    /// 规格卡片只画真实生效的项：没值就整行消失，伪条目只留一行说明。
    #[test]
    fn model_meta_card_lists_only_effective_fields() {
        let theme = Theme::global();
        let entry = crate::modal::ModelEntry {
            selector: "p/model".into(),
            label: "p/model".into(),
            display_name: "Model".into(),
            levels: crate::modal::DEFAULT_LEVELS
                .iter()
                .map(|level| (*level).to_owned())
                .collect(),
            context_window: Some(500_000),
            max_output_tokens: Some(64_000),
            inputs: vec![
                moc_protocol::methods::ModelInput::Text,
                moc_protocol::methods::ModelInput::Image,
            ],
            remote_compaction: true,
            pricing: Some(moc_protocol::methods::ModelPricing {
                input: 0.15,
                output: 0.6,
                cache_read: Some(0.015),
                cache_write: None,
            }),
        };
        let card: Vec<String> = super::model_meta_lines(theme, Some(&entry), 30)
            .iter()
            .map(line_text)
            .collect();
        let rule = "─".repeat(28);
        assert_eq!(card[0], format!("╭{rule}╮"));
        assert_eq!(card.last().expect("卡片底边"), &format!("╰{rule}╯"));
        // 每行都恰好占满卡片宽度，右边缘才对得齐。
        for text in &card {
            assert_eq!(line_width(text), 30, "{text:?}");
        }
        for expected in [
            "│ context  500k",
            "│ output   64k",
            "│ input    text · image",
            "│ price    $0.15 → $0.6/M",
            "│ cache    $0.015/M",
            "│ compact  on",
            "│ levels   off · low",
        ] {
            assert!(
                card.iter().any(|text| text.starts_with(expected)),
                "缺少 {expected:?}：{card:?}"
            );
        }
        // 等级列表按值列折行，续行只缩进、不重复字段名。
        assert!(
            card.iter()
                .any(|text| text.starts_with("│          medium · high")),
            "{card:?}"
        );
        assert!(
            card.iter()
                .any(|text| text.starts_with("│          xhigh · max")),
            "{card:?}"
        );

        // 只配了上下文窗口的模型：其余字段连字段名都不出现。
        let bare = crate::modal::ModelEntry {
            context_window: Some(128_000),
            max_output_tokens: None,
            inputs: Vec::new(),
            remote_compaction: false,
            pricing: None,
            levels: Vec::new(),
            ..entry.clone()
        };
        let bare_card: Vec<String> = super::model_meta_lines(theme, Some(&bare), 30)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(bare_card.len(), 3, "只剩一项时卡片只占三行：{bare_card:?}");
        assert!(bare_card[1].starts_with("│ context"), "{bare_card:?}");

        // 角色伪值没有任何元数据：留一行说明，不画空框。
        let inherit = crate::modal::ModelEntry {
            selector: "inherit".into(),
            label: "沿用主模型".into(),
            display_name: String::new(),
            levels: Vec::new(),
            context_window: None,
            max_output_tokens: None,
            inputs: Vec::new(),
            remote_compaction: false,
            pricing: None,
        };
        let inherit_card: Vec<String> = super::model_meta_lines(theme, Some(&inherit), 30)
            .iter()
            .map(line_text)
            .collect();
        assert_eq!(inherit_card.len(), 3, "{inherit_card:?}");
        assert!(inherit_card[1].contains("沿用主模型"), "{inherit_card:?}");
        assert!(inherit_card[1].starts_with("│ "), "{inherit_card:?}");

        assert!(super::model_meta_lines(theme, None, 30).is_empty());
        assert!(
            super::model_meta_lines(theme, Some(&entry), 20).is_empty(),
            "窄于下限的卡片位直接不画"
        );
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
        let row = |rows: &[String], needle: &str| {
            rows.iter()
                .find(|row| row.contains(needle))
                .unwrap_or_else(|| panic!("缺少 {needle} 行：{rows:?}"))
                .clone()
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

    /// 上下文窗：分块名用英文、数值统一为 k，色标与色条同色，色条按窗口定标。
    #[test]
    fn context_modal_colors_sections_and_scales_bar_to_window() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let theme = app.theme;
        let modal = Modal::Context {
            usage: Some(ContextUsageResult {
                context_window: 500_000,
                total_tokens: 250_000,
                sections: vec![
                    ContextUsageSection {
                        name: "system".into(),
                        tokens: 100_000,
                    },
                    ContextUsageSection {
                        name: "tools".into(),
                        tokens: 50_000,
                    },
                    ContextUsageSection {
                        name: "messages".into(),
                        tokens: 100_000,
                    },
                ],
            }),
        };

        let lines = modal_lines(&app, &modal, 80, 24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "  250k / 500k · 50.0%");
        assert_eq!(texts[2], "  ▌ system             100k   40.0%");
        assert_eq!(texts[3], "  ▌ tools               50k   20.0%");
        assert_eq!(texts[4], "  ▌ messages           100k   40.0%");
        for (index, row) in [&lines[2], &lines[3], &lines[4]].iter().enumerate() {
            assert_eq!(
                row.spans[1].style.fg,
                Some(theme.chart(index)),
                "第 {index} 块色标应与色条同色"
            );
        }

        // 已用 250k 落在一半格位（39/78），剩余 39 格是空窗口
        let segments: Vec<(usize, Option<Color>)> = lines[6]
            .spans
            .iter()
            .skip(1)
            .map(|span| (span.content.chars().count(), span.style.fg))
            .collect();
        assert_eq!(
            segments,
            vec![
                (16, Some(theme.chart(0))),
                (7, Some(theme.chart(1))),
                (16, Some(theme.chart(2))),
                (39, Some(theme.border)),
            ]
        );

        // 刻度行：分块右界的累积用量 + 右端窗口上限，行宽与色条一致
        let labels: Vec<(String, Option<Color>)> = lines[7]
            .spans
            .iter()
            .filter(|span| !span.content.trim().is_empty())
            .map(|span| (span.content.to_string(), span.style.fg))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("100k".to_owned(), Some(theme.chart(0))),
                ("150k".to_owned(), Some(theme.chart(1))),
                ("250k".to_owned(), Some(theme.chart(2))),
                ("500k".to_owned(), Some(theme.gray_dim)),
            ]
        );
        assert_eq!(line_width(&texts[7]), line_width(&texts[6]));
    }

    /// 上下文窗：空会话是一整条空窗口，分块与合计都显示 0。
    #[test]
    fn context_modal_shows_empty_window_for_fresh_session() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = Modal::Context {
            usage: Some(ContextUsageResult {
                context_window: 500_000,
                total_tokens: 0,
                sections: vec![
                    ContextUsageSection {
                        name: "system".into(),
                        tokens: 0,
                    },
                    ContextUsageSection {
                        name: "messages".into(),
                        tokens: 0,
                    },
                ],
            }),
        };

        let lines = modal_lines(&app, &modal, 60, 24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "  0 / 500k · 0.0%");
        assert_eq!(
            texts[2],
            format!("  ▌ {:<16}{:>7}    0.0%", "system", "0"),
            "空会话分块也显示 0"
        );
        assert_eq!(
            texts[3],
            format!("  ▌ {:<16}{:>7}    0.0%", "messages", "0")
        );
        assert!(
            texts[5].chars().all(|ch| ch == ' ' || ch == '─'),
            "空会话的色条整条都是空窗口：{:?}",
            texts[5]
        );
        assert!(texts[6].starts_with("  0 "), "{:?}", texts[6]);
        assert!(texts[6].ends_with("500k"), "{:?}", texts[6]);
    }

    /// 上下文窗：分块名称过长或包含宽字符时保持列宽对齐，Esc 行具有缩进。
    #[test]
    fn context_modal_handles_wide_names_and_indents_footer() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = Modal::Context {
            usage: Some(ContextUsageResult {
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
            }),
        };

        let lines = modal_lines(&app, &modal, 60, 24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // 验证各分块行总宽度一致，列对齐未被顶破
        assert_eq!(line_width(&texts[2]), line_width(&texts[3]));
        // 验证底部的 Esc 说明具备 2 格左缩进
        let last_line = texts.last().expect("footer line");
        assert!(last_line.starts_with("  Esc 关闭"), "{:?}", last_line);
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
            compact.contains("tab展开作答") && compact.contains("esc取消"),
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
            .position(|text| text.contains("滚轮/PgUp·PgDn"))
            .unwrap_or_else(|| panic!("超窗详情区应给滚动提示：{head:?}"));
        assert_eq!(
            status,
            head.len() - 3,
            "滚动提示应在详情区末行（其后是空行与页脚）：{head:?}"
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
        let layout = ask_layout(theme, &modal, 40, 16);
        let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
        assert!(
            texts
                .iter()
                .any(|text| text.contains("行 · 滚轮/PgUp·PgDn")),
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
        let layout = ask_layout(theme, &modal, 40, 16);
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
        let layout = ask_layout(theme, &modal, 40, 16);
        let texts: Vec<String> = layout.lines.iter().map(line_text).collect();
        assert!(
            texts.iter().any(|text| text.contains("… 3-6/6 行")),
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
            compact.iter().any(|text| text.contains("esc折叠问题卡")),
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
        assert!(body.contains("enter 确认提交"), "确认页页脚：{body}");
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
                    reminder: None,
                },
                TranscriptEntry {
                    id: "a1".into(),
                    at_ms: 1700000001000,
                    message: Message::assistant_text("好的，已为你实现快排"),
                    tool: None,
                    bash: None,
                    agent: None,
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

        let mut completion = CompletionState::derive("/he", &[]).expect("构造补全状态");
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
                if x > area.x
                    && buffer
                        .cell((x - 1, y))
                        .map_or(false, |c| c.cell_width() > 1)
                {
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
        app.modal = Some(Modal::Help);

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
}

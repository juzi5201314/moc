//! 浮层渲染：斜杠补全菜单与模态卡片。
//!
//! 浮层画在滚动区之后（覆盖其内容）；模态居中，最大宽度为终端的 3/4，
//! 高度按内容收缩。全部使用主题槽位。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::modal::Modal;
use crate::widgets::completion::CompletionState;

/// 斜杠补全菜单：贴着输入框上沿，选中行反色。
pub fn draw_completion_menu(
    frame: &mut Frame,
    completion: &CompletionState,
    prompt_area: Rect,
) {
    let rows = completion.candidates.len() as u16;
    let width = 72.min(prompt_area.width);
    let height = (rows + 2).min(prompt_area.y);
    if height == 0 {
        return;
    }
    let area = Rect {
        x: prompt_area.x,
        y: prompt_area.y - height,
        width,
        height,
    };
    let theme = crate::theme::Theme::global();
    frame.render_widget(Clear, area);
    let mut lines = Vec::new();
    for (index, candidate) in completion.candidates.iter().enumerate() {
        let selected = index == completion.selected;
        let prefix = if selected { "❯ " } else { "  " };
        let (name_style, desc_style) = if selected {
            (
                Style::new()
                    .fg(theme.text_primary)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD),
                Style::new().fg(theme.text_secondary).bg(theme.bg_visual),
            )
        } else {
            (Style::new().fg(theme.text_primary), Style::new().fg(theme.gray))
        };
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), desc_style),
            Span::styled(format!("{:<14}", candidate.name), name_style),
            Span::styled(candidate.description.to_owned(), desc_style),
        ]));
    }
    lines.push(Line::from(Span::styled(
        "tab/enter 确认 · esc 关闭".to_owned(),
        Style::new().fg(theme.gray_dim),
    )));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active)),
        ),
        area,
    );
}

/// 居中模态卡片。
pub fn draw_modal(frame: &mut Frame, app: &App) {
    let Some(modal) = &app.modal else {
        return;
    };
    let area = frame.area();
    let max_width = area.width * 3 / 4;
    let theme = app.theme;
    let lines = modal_lines(app, modal);
    let content_height = lines.len() as u16;
    let height = (content_height + 2).min(area.height.saturating_sub(2));
    let width = max_width;
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, modal_area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_set(ratatui::symbols::border::ROUNDED)
                    .border_style(Style::new().fg(theme.border_active))
                    .title(Span::styled(
                        modal.title(),
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false }),
        modal_area,
    );
}

/// 模态内容行（按类型分派）。
fn modal_lines(app: &App, modal: &Modal) -> Vec<Line<'static>> {
    match modal {
        Modal::Permission { params, selected, .. } => {
            let theme = app.theme;
            let mut lines = Vec::new();
            if let Some(body) = &params.body {
                for row in body.lines().take(4) {
                    lines.push(Line::from(Span::styled(
                        format!("  {row}"),
                        Style::new().fg(theme.text_secondary),
                    )));
                }
            }
            if let Some(diff) = &params.diff {
                let diff_lines = crate::render::diff::render_diff(diff, 90, theme);
                lines.extend(diff_lines.into_iter().take(24));
            }
            lines.push(Line::from(String::new()));
            for (index, option) in params.options.iter().enumerate() {
                let marker = if index == *selected { "❯ " } else { "  " };
                let style = if index == *selected {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                let hint = match option.kind {
                    moc_protocol::methods::PermissionOptionKind::AllowOnce
                    | moc_protocol::methods::PermissionOptionKind::AllowAlways => theme.accent_success,
                    _ => theme.accent_error,
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}{} {}", index + 1, option.label),
                    style.fg(style.fg.unwrap_or(hint)),
                )));
            }
            lines
        }
        Modal::Ask {
            params,
            question,
            highlight,
            selected,
            custom,
            editing_custom,
            ..
        } => {
            let theme = app.theme;
            let mut lines = Vec::new();
            if params.questions.len() > 1 {
                lines.push(Line::from(Span::styled(
                    format!("问题 {}/{}", question + 1, params.questions.len()),
                    Style::new().fg(theme.gray),
                )));
            }
            let Some(current) = params.questions.get(*question) else {
                return lines;
            };
            lines.push(Line::from(String::new()));
            for (index, option) in current.options.iter().enumerate() {
                let chosen = selected[*question].contains(&option.label);
                let marker = if index == highlight[*question] { "❯" } else { " " };
                let check = if chosen { "☑" } else { "☐" };
                let style = if index == highlight[*question] {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!(" {marker} {check} {}", option.label),
                    style,
                )));
            }
            let editing_here = highlight[*question] == current.options.len();
            let custom_style = if *editing_custom {
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
            let marker = if editing_here { "❯" } else { " " };
            lines.push(Line::from(Span::styled(
                format!(" {marker} ✎ 自定义：{custom}"),
                custom_style,
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "space 切换 · enter 选择{} · tab 下一问 · esc 取消",
                    if current.multi { "（多选）" } else { "" }
                ),
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Model { models, current, selected } => {
            let theme = app.theme;
            let mut lines = vec![Line::from(Span::styled(
                format!("当前：{current}"),
                Style::new().fg(theme.gray),
            ))];
            for (index, model) in models.iter().enumerate() {
                let marker = if index == *selected { "❯ " } else { "  " };
                let style = if index == *selected {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                let mut detail = model.id.clone();
                if let Some(window) = model.context_window {
                    detail.push_str(&format!(" · {} tokens", format_number(window)));
                }
                if let Some(pricing) = &model.pricing {
                    detail.push_str(&format!(" · ${:.2}/${:.2} 每百万", pricing.input, pricing.output));
                }
                lines.push(Line::from(Span::styled(
                    format!("{marker}{detail}"),
                    style,
                )));
            }
            lines
        }
        Modal::Sessions { sessions, selected } => {
            let theme = app.theme;
            if sessions.is_empty() {
                return vec![Line::from(Span::styled(
                    "加载中…".to_owned(),
                    Style::new().fg(theme.gray),
                ))];
            }
            let mut lines = Vec::new();
            for (index, session) in sessions.iter().enumerate() {
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
                    format!("{marker}{:<40} {}", session.title, session.cwd),
                    style,
                )));
            }
            lines
        }
        Modal::Plan { markdown, selected, .. } => {
            let theme = app.theme;
            let mut lines = crate::render::markdown::render_markdown(markdown, 100, theme);
            lines.push(Line::from(String::new()));
            lines.push(Line::from(vec![
                Span::styled(
                    "  [ 执行计划 ]",
                    if *selected == 0 {
                        Style::new()
                            .fg(theme.accent_success)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(theme.text_secondary)
                    },
                ),
                Span::styled("    ", Style::new()),
                Span::styled(
                    "[ 拒绝 ]",
                    if *selected == 1 {
                        Style::new()
                            .fg(theme.accent_error)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(theme.text_secondary)
                    },
                ),
            ]));
            lines
        }
        Modal::Help => {
            let theme = app.theme;
            [
                ("ctrl+t", "展开 / 收起思考内容"),
                ("ctrl+o", "展开 / 收起工具调用"),
                ("ctrl+v", "粘贴剪贴板图片"),
                ("ctrl+m", "切换模型"),
                ("ctrl+r", "恢复历史会话"),
                ("shift+tab", "切换滚动区 / 输入框焦点"),
                ("alt+enter", "输入框内换行"),
                ("↑ / ↓", "历史消息 / 多行移动"),
                ("shift+tab(空闲)", "循环 Normal / Ask / Plan 模式"),
                ("ctrl+c", "运行中取消轮次；空闲退出"),
                ("/commands", "help new model resume mode compact quit"),
            ]
            .iter()
            .map(|(key, description)| {
                Line::from(vec![
                    Span::styled(
                        format!("  {key:<14}"),
                        Style::new().fg(theme.accent_user),
                    ),
                    Span::styled(
                        (*description).to_owned(),
                        Style::new().fg(theme.text_secondary),
                    ),
                ])
            })
            .collect()
        }
    }
}

/// token 数缩写（120000 → 120K）。
fn format_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

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
pub fn draw_completion_menu(frame: &mut Frame, completion: &CompletionState, prompt_area: Rect) {
    let rows = completion.candidates.len() as u16;
    let width = prompt_area.width.min(72).max(20);
    let height = (rows + 3).min(prompt_area.y);
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
    let content_width = usize::from(width.saturating_sub(4));
    let mut lines = Vec::with_capacity(completion.candidates.len() + 1);
    for (index, candidate) in completion.candidates.iter().enumerate() {
        let selected = index == completion.selected;
        let prefix = if selected { "❯ " } else { "  " };
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
                &format!("{prefix}{:<14}{}", candidate.name, candidate.description),
                content_width,
            ),
            style,
        )));
    }
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Tab/Enter 确认 · Esc 关闭",
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
    if area.width < 20 || area.height < 6 {
        return;
    }
    let theme = app.theme;
    // 保留正文轮廓但降低对比度，浮层成为唯一的视觉焦点。
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    let width = if area.width < 70 {
        area.width.saturating_sub(4).max(16)
    } else {
        area.width.min(96)
    };
    let max_content = usize::from(area.height.saturating_sub(6)).max(1);
    let content_width = usize::from(width.saturating_sub(4)).max(8);
    let lines = modal_lines(app, modal, content_width, max_content);
    let content_height = lines.len().max(1) as u16;
    let height = content_height
        .saturating_add(4)
        .min(area.height.saturating_sub(2));
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
fn modal_lines(app: &App, modal: &Modal, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    let theme = app.theme;
    let mut lines = match modal {
        Modal::Permission {
            params, selected, ..
        } => {
            let mut lines = Vec::new();
            if let Some(body) = &params.body {
                lines.extend(body.lines().take(4).map(|row| {
                    Line::from(Span::styled(
                        format!("  {row}"),
                        Style::new().fg(theme.text_secondary),
                    ))
                }));
            }
            if let Some(diff) = &params.diff {
                lines.extend(
                    crate::render::diff::render_diff(diff, width, theme)
                        .into_iter()
                        .take(max_lines.saturating_sub(lines.len() + 3)),
                );
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
                lines.push(Line::from(Span::styled(
                    format!("{marker}{} {}", index + 1, option.label),
                    style,
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
                let marker = if index == highlight[*question] {
                    "❯"
                } else {
                    " "
                };
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
            lines.push(Line::from(Span::styled(
                format!(
                    " {} ✎ 自定义：{custom}",
                    if editing_here { "❯" } else { " " }
                ),
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
        Modal::Model {
            models,
            current,
            picker,
        } => {
            let filtered = crate::modal::filtered_models(models, &picker.query);
            let visible = max_lines.saturating_sub(5).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            let mut lines = vec![
                Line::from(Span::styled(
                    format!("当前：{}", ellipsize(current, width)),
                    Style::new().fg(theme.gray),
                )),
                Line::from(Span::styled(
                    format!(
                        "搜索：{}",
                        if picker.query.is_empty() {
                            "_"
                        } else {
                            &picker.query
                        }
                    ),
                    Style::new().fg(theme.accent_model),
                )),
                Line::from(String::new()),
            ];
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
                for (index, (_, model)) in filtered.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let mut detail = model.id.clone();
                    if let Some(window) = model.context_window {
                        detail.push_str(&format!(" · {} tokens", format_number(window)));
                    }
                    lines.push(Line::from(Span::styled(
                        ellipsize(&format!("{marker}{detail}"), width),
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
                "↑↓ 移动 · PgUp/PgDn 翻页 · 输入搜索 · Enter 确认 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Memories {
            root_uri,
            entries,
            picker,
        } => {
            let query = picker.query.to_lowercase();
            let filtered: Vec<_> = entries
                .iter()
                .filter(|entry| entry.name.to_lowercase().contains(&query))
                .collect();
            let visible = max_lines.saturating_sub(5).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            let mut lines = vec![Line::from(Span::styled(
                ellipsize(root_uri, width),
                Style::new().fg(theme.accent_model),
            ))];
            if picker.loading {
                lines.push(Line::from(Span::styled(
                    "正在加载 OpenViking 记忆…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
            } else if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "没有记忆条目",
                    Style::new().fg(theme.gray),
                )));
            } else {
                for (index, entry) in filtered.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let kind = if entry.is_dir { "▸" } else { "·" };
                    lines.push(Line::from(Span::styled(
                        ellipsize(
                            &format!("{marker}{kind} {}  {}", entry.name, entry.abstract_text),
                            width,
                        ),
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
                "↑↓ 移动 · Enter 进入目录 · Esc 返回",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::MemoryEditor {
            uri, editor, dirty, ..
        } => {
            let mut lines = vec![Line::from(Span::styled(
                ellipsize(uri, width),
                Style::new().fg(theme.accent_model),
            ))];
            lines.push(Line::from(Span::styled(
                if *dirty {
                    "已修改 · Ctrl+S 保存"
                } else {
                    "Ctrl+S 保存 · Esc 返回"
                },
                Style::new().fg(theme.gray_dim),
            )));
            lines.extend(
                editor
                    .text
                    .lines()
                    .take(max_lines.saturating_sub(3))
                    .map(|line| {
                        Line::from(Span::styled(
                            line.to_owned(),
                            Style::new().fg(theme.text_secondary),
                        ))
                    }),
            );
            lines
        }
        Modal::Sessions { sessions, picker } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query);
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
                lines.push(Line::from(Span::styled(
                    "没有匹配的会话",
                    Style::new().fg(theme.gray),
                )));
            } else {
                for (index, (_, session)) in filtered.iter().skip(offset).take(visible).enumerate()
                {
                    let actual = offset + index;
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let text = format!(
                        "{marker}{}  {}",
                        session.title,
                        ellipsize(&session.cwd, width / 2),
                    );
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
                "↑↓ 移动 · PgUp/PgDn 翻页 · 输入搜索 · Enter 恢复 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Plan {
            markdown, selected, ..
        } => {
            let mut lines = crate::render::markdown::render_markdown(markdown, width, theme);
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
        Modal::Context { usage } => {
            let mut lines = Vec::new();
            match usage {
                Some(usage) => {
                    lines.push(Line::from(Span::styled(
                        format!(
                            "估算占用：{} / {} ({:.1}%)",
                            format_number(usage.total_tokens),
                            format_number(usage.context_window),
                            usage.total_tokens as f64 * 100.0 / usage.context_window.max(1) as f64,
                        ),
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(Span::styled(
                        "各项按当前模型请求格式估算，实际用量以响应为准。",
                        Style::new()
                            .fg(theme.gray_dim)
                            .add_modifier(Modifier::ITALIC),
                    )));
                    lines.push(Line::from(String::new()));
                    let total = usage.total_tokens.max(1) as f64;
                    for section in &usage.sections {
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {:<10}", section.name),
                                Style::new().fg(theme.text_secondary),
                            ),
                            Span::styled(
                                format!(
                                    "{} ({:.1}%)",
                                    format_number(section.tokens),
                                    section.tokens as f64 * 100.0 / total,
                                ),
                                Style::new().fg(theme.accent_model),
                            ),
                        ]));
                    }
                }
                None => lines.push(Line::from(Span::styled(
                    "正在计算上下文…",
                    Style::new().fg(theme.accent_running),
                ))),
            }
            lines.push(Line::from(Span::styled(
                "Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Help => [
            ("Ctrl+H", "打开快捷键帮助"),
            ("Ctrl+T", "展开 / 折叠思考"),
            ("Ctrl+O", "展开 / 折叠工具调用"),
            ("Ctrl+M", "切换 Normal / Ask / Plan 模式"),
            ("Shift+Tab", "切换思考预算"),
            ("Ctrl+V / P / R", "粘贴图片 / 切换模型 / 恢复会话"),
            ("Ctrl+C / Y", "取消或退出 / 复制选区"),
            ("Ctrl+A / E", "输入行首 / 行尾"),
            ("Ctrl+K / U / W", "删除至行尾 / 清空 / 删除前词"),
            ("Tab", "切换焦点；补全菜单确认候选"),
            ("Alt+Enter", "输入框内换行"),
            ("↑ / ↓", "补全候选 / 输入历史 / 多行移动"),
            ("PageUp/Down", "滚动转写区或列表"),
            ("Home / End", "转写区跳到最新 / 最旧内容"),
            ("Esc", "取消选择；再次清空输入"),
            ("/context", "查看上下文组成与占用比例"),
            (
                "/commands",
                "help new model resume mode compact context quit",
            ),
        ]
        .into_iter()
        .map(|(key, description)| {
            Line::from(vec![
                Span::styled(format!("  {key:<18}"), Style::new().fg(theme.accent_user)),
                Span::styled(description, Style::new().fg(theme.text_secondary)),
            ])
        })
        .collect(),
    };
    lines.truncate(max_lines.max(1));
    lines
}

fn ellipsize(text: &str, width: usize) -> String {
    if crate::render::wrap::str_width(text) <= width {
        return text.to_owned();
    }
    crate::render::scrollback::truncate_to_width(text.to_owned(), width.max(1))
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

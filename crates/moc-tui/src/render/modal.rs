//! 浮层渲染：斜杠补全菜单与模态卡片。
//!
//! 浮层画在滚动区之后（覆盖其内容）；模态居中，宽度按内容取值（帮助为双列快捷键表，
//! 普通模态上限 96 列），高度按内容收缩。全部使用主题槽位。

use moc_protocol::Role;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::modal::Modal;
use crate::render::wrap::str_width;
use crate::widgets::completion::CompletionState;

/// 斜杠补全菜单：贴着输入框上沿，选中行反色。
pub fn draw_completion_menu(frame: &mut Frame, completion: &CompletionState, prompt_area: Rect) {
    let visible = completion.candidates.len().min(20);
    let rows = visible as u16 + u16::from(completion.candidates.len() > visible);
    let width = prompt_area.width.clamp(20, 72);
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
    let mut lines = Vec::with_capacity(visible + 1);
    for (index, candidate) in completion.candidates.iter().take(visible).enumerate() {
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
                &format!("{prefix}{:<14}{}", candidate.label, candidate.description),
                content_width,
            ),
            style,
        )));
    }
    if completion.candidates.len() > visible {
        lines.push(Line::from(Span::styled(
            format!("… 还有 {} 条", completion.candidates.len() - visible),
            Style::new().fg(theme.gray_dim),
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
    let max_content = usize::from(area.height.saturating_sub(6)).max(1);
    let (width, content_width) = if matches!(modal, Modal::Help) {
        // 帮助是双列快捷键表：按内容自然宽度取值，窄终端由 help_lines 退回单列。
        let (_, natural) = help_lines(app, usize::from(area.width).saturating_sub(6), max_content);
        let width = u16::try_from(natural + 2)
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(4));
        (width, usize::from(width.saturating_sub(2)))
    } else {
        let width = if area.width < 70 {
            area.width.saturating_sub(4).max(16)
        } else {
            area.width.min(96)
        };
        (width, usize::from(width.saturating_sub(4)).max(8))
    };
    let lines = modal_lines(app, modal, content_width, max_content);
    let content_height = lines.len().max(1) as u16;
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

/// 快捷键帮助：键位列与描述列之间的间距（格）。
const HELP_KEY_GAP: usize = 2;
/// 快捷键帮助：两列之间的间距（格）。
const HELP_COLUMN_GAP: usize = 4;
/// 快捷键帮助：快捷键行左缩进（格）。
const HELP_INDENT: usize = 2;
/// 快捷键帮助：页脚（留白 + Esc 说明）占用的行数。
const HELP_FOOTER_ROWS: usize = 2;

/// 一列快捷键的（键位列宽，整列宽）。
type HelpColumn = (usize, usize);

/// 量出一列快捷键的键位列宽与整列宽。
fn measure_column(rows: &[(String, &'static str)]) -> HelpColumn {
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

/// 快捷键帮助内容：默认双列（左列先排满），宽度不足时退回单列。
///
/// 键位列按该列最长绑定取齐——键位可配置，固定宽度会让 `shift/alt + enter` 这类
/// 长组合挤出描述列；返回内容的自然宽度，模态据此收窄到刚好容纳。
/// 页脚只剩一行 Esc 说明，缩进与快捷键行一致，不再贴住卡片边框。
fn help_lines(app: &App, max_width: usize, max_lines: usize) -> (Vec<Line<'static>>, usize) {
    let theme = app.theme;
    let mut rows: Vec<(String, &'static str)> = crate::config::actions()
        .iter()
        .map(|(action, description, _)| {
            let keys = app
                .preferences
                .keybindings
                .get(*action)
                .map_or_else(|| "—".to_owned(), |keys| crate::config::display_keys(keys));
            (keys, *description)
        })
        .collect();
    // 双列按行序对半切开，两列各自收窄；退回单列时两列并作一列取并集。
    let unified = measure_column(&rows);
    let split = rows.len().div_ceil(2);
    let (left, right) = rows.split_at(split);
    let left_column = measure_column(left);
    let right_column = measure_column(right);
    let two_columns =
        HELP_INDENT + left_column.1 + HELP_COLUMN_GAP + right_column.1 <= max_width && split > 0;
    let (columns, width) = if two_columns {
        (
            [left_column, right_column],
            HELP_INDENT + left_column.1 + HELP_COLUMN_GAP + right_column.1,
        )
    } else {
        ([unified; 2], HELP_INDENT + unified.1)
    };
    let row_budget = max_lines
        .saturating_sub(HELP_FOOTER_ROWS)
        .max(1)
        .saturating_mul(if two_columns { 2 } else { 1 });
    rows.truncate(row_budget);
    let split = if two_columns {
        rows.len().div_ceil(2)
    } else {
        rows.len()
    };
    let mut lines: Vec<Line<'static>> = (0..split)
        .map(|index| {
            let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
            spans.extend(help_cell(&rows[index], columns[0], theme));
            if let Some(right) = rows.get(split + index) {
                spans.push(Span::styled(" ".repeat(HELP_COLUMN_GAP), Style::new()));
                spans.extend(help_cell(right, columns[1], theme));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(String::new()));
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(HELP_INDENT), Style::new()),
        Span::styled("Esc 关闭帮助", Style::new().fg(theme.gray_dim)),
    ]));
    (lines, width)
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
                    format!(
                        "当前：{}",
                        ellipsize(crate::app::model_selector_label(current), width)
                    ),
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
                    let selected = actual == picker.selected;
                    let style = if selected {
                        Style::new()
                            .fg(theme.accent_user)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(theme.text_primary)
                    };
                    let context = model
                        .context_window
                        .map(|window| format!(" · Ctx {}", crate::render::short_number(window)));
                    let row_text = ellipsize(
                        &format!(
                            "{marker}{}{}",
                            model.id,
                            context.as_deref().unwrap_or_default()
                        ),
                        width,
                    );
                    let row = row_text.strip_prefix(marker).unwrap_or(&row_text);
                    let (model_text, context_text) = context
                        .as_deref()
                        .filter(|context| row.ends_with(context))
                        .map_or((row, None), |context| {
                            (row.strip_suffix(context).unwrap_or(row), Some(context))
                        });
                    let mut spans = vec![Span::styled(marker, style)];
                    if let Some((provider, model_name)) = model_text.split_once('/') {
                        let provider_style = if selected {
                            Style::new().fg(theme.text_secondary).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        };
                        spans.push(Span::styled(format!("{provider}/"), provider_style));
                        spans.push(Span::styled(model_name.to_owned(), style));
                    } else {
                        spans.push(Span::styled(model_text.to_owned(), style));
                    }
                    if let Some(context) = context_text {
                        let context_style = if selected {
                            Style::new().fg(theme.accent_model).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.accent_model)
                        };
                        spans.push(Span::styled(context.to_owned(), context_style));
                    }
                    lines.push(Line::from(spans));
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
                    "正在加载记忆…",
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
        Modal::Rewind { state } => {
            let mut lines = vec![Line::from(Span::styled(
                "↑↓ 选择当前用户消息内的条目 · ←→ 切换用户消息",
                Style::new().fg(theme.gray_dim),
            ))];
            if state.loading && state.turns.is_empty() {
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
            } else if state.turns.is_empty() {
                lines.push(Line::from(Span::styled(
                    "当前会话没有可回退的消息",
                    Style::new().fg(theme.gray),
                )));
            } else {
                let turn = &state.turns[state.selected_turn];
                lines.push(Line::from(Span::styled(
                    format!(
                        "用户消息 {}/{} · 当前条目 {}/{}",
                        state.selected_turn + 1,
                        state.turns.len(),
                        state.selected_item + 1,
                        turn.len()
                    ),
                    Style::new().fg(theme.accent_model),
                )));
                let visible = max_lines.saturating_sub(5).max(1);
                let offset = state
                    .selected_item
                    .saturating_sub(visible.saturating_sub(1));
                for (index, entry) in turn.iter().skip(offset).take(visible).enumerate() {
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
                            "{} / {}（{:.1}%）",
                            crate::render::short_number(usage.total_tokens),
                            crate::render::short_number(usage.context_window),
                            usage.total_tokens as f64 * 100.0 / usage.context_window.max(1) as f64,
                        ),
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    )));
                    lines.push(Line::from(String::new()));
                    let total = usage.total_tokens.max(1) as f64;
                    for section in &usage.sections {
                        let pad = 14usize.saturating_sub(str_width(&section.name));
                        lines.push(Line::from(vec![
                            Span::styled(
                                format!("  {}{}", section.name, " ".repeat(pad)),
                                Style::new().fg(theme.text_secondary),
                            ),
                            Span::styled(
                                format!(
                                    "{}（{:.1}%）",
                                    crate::render::short_number(section.tokens),
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
        Modal::Help => help_lines(app, width, max_lines).0,
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

#[cfg(test)]
mod tests {
    use super::modal_lines;
    use crate::{
        app::App,
        modal::{Modal, PickerState},
        theme::Theme,
    };
    use moc_protocol::methods::ModelInfo;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn model_picker_separates_provider_and_context_colors() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let theme = app.theme;
        let modal = Modal::Model {
            models: vec![ModelInfo {
                id: "p/model".into(),
                display_name: "Model".into(),
                thinking_levels: None,
                context_window: Some(500_000),
                max_output_tokens: None,
                pricing: None,
            }],
            current: "p/model".into(),
            picker: PickerState::default(),
        };

        let lines = modal_lines(&app, &modal, 80, 20);
        let row = &lines[3];
        let text = row
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "❯ p/model · Ctx 500K");
        assert_eq!(row.spans[1].style.fg, Some(theme.text_secondary));
        assert_eq!(row.spans[2].style.fg, Some(theme.accent_user));
        assert_eq!(row.spans[3].style.fg, Some(theme.accent_model));
        assert!(!text.contains("上下文"));
    }
}

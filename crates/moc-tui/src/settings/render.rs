use super::*;
use crate::render::wrap::str_width;
use crate::theme::Theme;
use ratatui::{
    Frame,
    layout::{Alignment, Rect},
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Clear, Paragraph, Wrap},
};

/// 文件大小缩写（2411724 → 2.3 MB）。
fn short_bytes(value: u64) -> String {
    if value >= 1 << 20 {
        format!("{:.1} MB", value as f64 / f64::from(1 << 20))
    } else if value >= 1 << 10 {
        format!("{:.1} KB", value as f64 / f64::from(1 << 10))
    } else {
        format!("{value} B")
    }
}

fn cells(value: usize) -> u16 {
    u16::try_from(value).unwrap_or(u16::MAX)
}
fn label(frame: &mut Frame, area: Rect, text: impl Into<String>, style: Style) {
    frame.render_widget(Paragraph::new(text.into()).style(style), area);
}
fn button(
    frame: &mut Frame,
    hits: &mut Vec<(Rect, Hit)>,
    x: &mut u16,
    y: u16,
    end: u16,
    (text, code, modifiers): (&str, KeyCode, KeyModifiers),
    theme: &Theme,
) {
    let width = cells(str_width(text)).saturating_add(2);
    if x.saturating_add(width) > end {
        return;
    }
    let area = Rect::new(*x, y, width, 1);
    label(
        frame,
        area,
        format!(" {text} "),
        Style::new().fg(theme.text_primary).bg(theme.bg_visual),
    );
    hits.push((area, Hit::Key(code, modifiers)));
    *x = x.saturating_add(width + 1);
}

pub fn draw(frame: &mut Frame, panel: &mut SettingsPanel, saving: bool, theme: &Theme) {
    let outer = frame.area();
    frame.render_widget(Clear, outer);
    frame
        .buffer_mut()
        .set_style(outer, Style::new().bg(theme.bg_base).fg(theme.text_primary));
    let area = Rect::new(
        outer.x.saturating_add(2),
        outer.y,
        outer.width.saturating_sub(4),
        outer.height,
    );
    if area.width < 20 || area.height < 12 {
        label(
            frame,
            area,
            "终端过小；请放大窗口。Esc 返回",
            Style::new().fg(theme.text_primary),
        );
        return;
    }
    let mut hits = Vec::new();
    let scope = match panel.scope {
        ConfigScope::Global => "全局配置",
        ConfigScope::Project => "项目配置",
    };
    let count_info = if !panel.rows.is_empty() {
        format!(" ({}/{} 项)", panel.selected + 1, panel.rows.len())
    } else {
        String::new()
    };
    let filter_badge = if !panel.filter.is_empty() {
        format!(" · 过滤: \"{}\"{}", panel.filter, count_info)
    } else {
        count_info
    };
    label(
        frame,
        Rect::new(area.x, area.y, area.width, 1),
        format!(
            "设置 · {scope}{filter_badge}{}{}",
            if saving { " · 正在保存…" } else { "" },
            if panel.busy {
                " · 正在处理…"
            } else {
                ""
            }
        ),
        Style::new().add_modifier(Modifier::BOLD),
    );
    let mut x = area.x;
    let mut y = area.y + 2;
    for (index, tab) in TABS.iter().enumerate() {
        let key_hint = match index {
            0..=8 => format!("{}", index + 1),
            9 => "0".into(),
            10 => "-".into(),
            _ => "".into(),
        };
        let tab_text = if !key_hint.is_empty() {
            format!(" {key_hint}·{tab} ")
        } else {
            format!(" {tab} ")
        };
        let width = cells(str_width(&tab_text));
        if x.saturating_add(width) > area.right() {
            x = area.x;
            y += 1;
        }
        if y >= area.y + 5 {
            break;
        }
        let tab_area = Rect::new(x, y, width.min(area.width), 1);
        label(
            frame,
            tab_area,
            tab_text,
            if panel.tab == index {
                Style::new()
                    .bg(theme.bg_visual)
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.gray)
            },
        );
        hits.push((tab_area, Hit::Tab(index)));
        x = x.saturating_add(width + 1);
    }
    let toolbar_y = y + 2;
    let mut x = area.x;
    match &panel.view {
        View::Fields => {
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("撤回 Ctrl+Z", KeyCode::Char('z'), KeyModifiers::CONTROL),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("重做 Ctrl+Y", KeyCode::Char('y'), KeyModifiers::CONTROL),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("范围 F2/s", KeyCode::F(2), KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("高级 F4/v", KeyCode::F(4), KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("搜索 /", KeyCode::Char('/'), KeyModifiers::NONE),
                theme,
            );
            if !panel.filter.is_empty() {
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("清空过滤 Esc", KeyCode::Esc, KeyModifiers::NONE),
                    theme,
                );
            }
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("展开全部 E", KeyCode::Char('e'), KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("折叠全部 C", KeyCode::Char('c'), KeyModifiers::NONE),
                theme,
            );
            if matches!(TABS[panel.tab], "供应商" | "模型") {
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("添加 A", KeyCode::Char('a'), KeyModifiers::NONE),
                    theme,
                );
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("获取模型 F", KeyCode::Char('f'), KeyModifiers::NONE),
                    theme,
                );
                if TABS[panel.tab] == "模型" {
                    button(
                        frame,
                        &mut hits,
                        &mut x,
                        toolbar_y,
                        area.right(),
                        ("默认规则 Ctrl+D", KeyCode::Char('d'), KeyModifiers::CONTROL),
                        theme,
                    );
                }
            }
            if TABS[panel.tab] == "MCP" {
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("添加服务 A", KeyCode::Char('a'), KeyModifiers::NONE),
                    theme,
                );
            }
            if TABS[panel.tab] == "OpenViking" {
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("工作区 W", KeyCode::Char('w'), KeyModifiers::NONE),
                    theme,
                );
            }
            if TABS[panel.tab] == "会话" {
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("维护预览 M", KeyCode::Char('m'), KeyModifiers::NONE),
                    theme,
                );
            }
        }
        View::Models { .. } => {
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("添加勾选 Enter", KeyCode::Enter, KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("全选/全消 A", KeyCode::Char('a'), KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("返回 Esc", KeyCode::Esc, KeyModifiers::NONE),
                theme,
            );
        }
        View::Workspaces { .. } => {
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("改绑 Enter", KeyCode::Enter, KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("删除记忆 Del", KeyCode::Delete, KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("返回 Esc", KeyCode::Esc, KeyModifiers::NONE),
                theme,
            );
        }
        View::Maintenance(report) => {
            if !report.applied {
                button(
                    frame,
                    &mut hits,
                    &mut x,
                    toolbar_y,
                    area.right(),
                    ("执行维护 Enter", KeyCode::Enter, KeyModifiers::NONE),
                    theme,
                );
            }
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("返回 Esc", KeyCode::Esc, KeyModifiers::NONE),
                theme,
            );
        }
        View::Confirm { .. } => {
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("确认 Y", KeyCode::Char('y'), KeyModifiers::NONE),
                theme,
            );
            button(
                frame,
                &mut hits,
                &mut x,
                toolbar_y,
                area.right(),
                ("取消 Esc", KeyCode::Esc, KeyModifiers::NONE),
                theme,
            );
        }
    }
    let bottom = area.bottom();
    let desc_height = if area.height >= 24 { 3 } else { 2 };
    let desc_area = Rect::new(
        area.x,
        bottom.saturating_sub(desc_height + 2),
        area.width,
        desc_height,
    );
    let hint_area = Rect::new(area.x, bottom - 2, area.width, 1);
    let body = Rect::new(
        area.x,
        toolbar_y + 2,
        area.width,
        (bottom.saturating_sub(desc_height + 3)).saturating_sub(toolbar_y + 2),
    );
    let (description, hint) = match &panel.view {
        View::Fields => draw_fields(frame, panel, theme, body, &mut hits),
        View::Models { .. } => draw_models(frame, panel, theme, body, &mut hits),
        View::Workspaces { .. } => draw_workspaces(frame, panel, theme, body, &mut hits),
        View::Maintenance(report) => {
            let mut lines = vec![Line::from(if report.applied {
                format!(
                    "维护完成：归档 {} 个会话，清理 {} 个临时文件（{}），保留 {} 项。",
                    report.archived.len(),
                    report.removed.len(),
                    short_bytes(report.bytes),
                    report.protected
                )
            } else {
                format!(
                    "预计归档 {} 个会话，清理 {} 个临时文件（{}），保留 {} 项。",
                    report.archived.len(),
                    report.removed.len(),
                    short_bytes(report.bytes),
                    report.protected
                )
            })];
            lines.extend(
                report
                    .archived
                    .iter()
                    .map(|path| Line::from(format!("归档 {path}")))
                    .chain(
                        report
                            .removed
                            .iter()
                            .map(|path| Line::from(format!("删除 {path}"))),
                    )
                    .skip(panel.offset)
                    .take(usize::from(body.height)),
            );
            frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), body);
            (
                "归档的会话可以恢复；只清理过期且没有被引用的临时文件。".into(),
                if report.applied {
                    "Esc 返回".into()
                } else {
                    "Enter 执行维护 · ↑↓ 浏览清单 · Esc 返回".into()
                },
            )
        }
        View::Confirm { text, .. } => {
            frame.render_widget(
                Paragraph::new(vec![
                    Line::from(text.as_str()),
                    Line::from(""),
                    Line::from(Span::styled(
                        "按 Y 或 Enter 确认；N 或 Esc 取消",
                        Style::new()
                            .fg(theme.accent_model)
                            .add_modifier(Modifier::BOLD),
                    )),
                ])
                .wrap(Wrap { trim: false }),
                body,
            );
            (String::new(), "Esc 取消".into())
        }
    };
    if !description.is_empty() {
        frame.render_widget(
            Paragraph::new(description)
                .style(Style::new().fg(theme.text_secondary))
                .wrap(Wrap { trim: false }),
            desc_area,
        );
    }
    frame.render_widget(
        Paragraph::new(hint)
            .style(Style::new().fg(theme.gray))
            .wrap(Wrap { trim: false }),
        hint_area,
    );
    draw_edit_popup(frame, panel, theme, outer, &mut hits);
    if let Some(error) = &panel.error {
        label(
            frame,
            Rect::new(area.x, bottom - 1, area.width, 1),
            format!(" ✗ {error} "),
            Style::new()
                .fg(theme.text_primary)
                .bg(theme.accent_error)
                .add_modifier(Modifier::BOLD),
        );
    } else if let Some(message) = &panel.message {
        label(
            frame,
            Rect::new(area.x, bottom - 1, area.width, 1),
            format!(" ✓ {message} "),
            Style::new()
                .fg(theme.accent_model)
                .add_modifier(Modifier::BOLD),
        );
    }
    panel.hits = hits;
}

/// 字段树：分组头可折叠，字段行可原地编辑。返回说明与提示两行文案。
fn draw_fields(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> (String, String) {
    if panel.data.is_none() {
        label(frame, body, "正在读取设置…", Style::new().fg(theme.gray));
        return (String::new(), "Esc 返回".into());
    }
    if panel.rows.is_empty() {
        label(
            frame,
            body,
            if !panel.filter.is_empty() {
                "没有匹配的设置项；按 / 修改搜索内容，或按 Esc 清空过滤。"
            } else {
                match TABS[panel.tab] {
                    "供应商" => "还没有供应商。按 A 添加，填写服务地址与密钥后按 F 获取模型。",
                    "模型" => "还没有模型。按 A 填写模型 ID，或到“供应商”页按 F 获取。",
                    "MCP" => "还没有 MCP 服务。按 A 添加。",
                    _ => "这一页暂时没有可配置项。",
                }
            },
            Style::new().fg(theme.gray),
        );
        let empty_hint = if !panel.filter.is_empty() {
            "Esc 清空过滤 · / 重新搜索"
        } else {
            "1-9/[] 切换选项卡 · Esc 返回"
        };
        return (String::new(), empty_hint.into());
    }
    let height = usize::from(body.height);
    if panel.selected < panel.offset {
        panel.offset = panel.selected;
    }
    if panel.selected >= panel.offset.saturating_add(height) {
        panel.offset = panel.selected.saturating_sub(height.saturating_sub(1));
    }
    let has_scrollbar = panel.rows.len() > height && height > 0;
    let scrollbar_margin = if has_scrollbar { 1 } else { 0 };
    let content_width = body.width.saturating_sub(scrollbar_margin);
    let label_width = content_width.saturating_mul(3) / 5;
    let value_area = Rect::new(
        body.x + label_width + 1,
        body.y,
        content_width.saturating_sub(label_width + 1),
        1,
    );
    for (row_index, row) in panel
        .rows
        .iter()
        .enumerate()
        .skip(panel.offset)
        .take(height)
    {
        let y = body.y + cells(row_index - panel.offset);
        let row_area = Rect::new(body.x, y, content_width, 1);
        let selected = row_index == panel.selected;
        let cursor_marker = if selected { "▶" } else { " " };
        let style = if selected {
            Style::new().bg(theme.bg_visual).fg(theme.text_primary)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        frame.render_widget(Block::default().style(style), row_area);
        let indent = " ".repeat(usize::from(row.depth) * 2);
        match &row.kind {
            RowKind::Group(group) => {
                let marker = if panel.is_expanded(&group.key, usize::from(row.depth)) {
                    "▾"
                } else {
                    "▸"
                };
                label(
                    frame,
                    Rect::new(row_area.x, y, label_width, 1),
                    format!("{cursor_marker} {indent}{marker} {}", group.title),
                    style.add_modifier(Modifier::BOLD),
                );
                if !group.detail.is_empty() {
                    label(
                        frame,
                        Rect::new(value_area.x, y, value_area.width, 1),
                        group.detail.clone(),
                        style.fg(theme.gray),
                    );
                }
            }
            RowKind::Field(index) => {
                let field = &panel.fields[*index];
                let is_explicit = panel.explicit(&field.path);
                let custom_marker = if is_explicit { "● " } else { "  " };
                let search_tab = if !panel.filter.is_empty() {
                    format!(" [{}]", field.tab)
                } else {
                    String::new()
                };
                let label_text = format!(
                    "{cursor_marker} {indent}{custom_marker}{}{search_tab}",
                    field.label
                );
                let label_style = if is_explicit && !selected {
                    style.fg(theme.text_primary).add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                label(
                    frame,
                    Rect::new(row_area.x, y, label_width, 1),
                    label_text,
                    label_style,
                );
                let editing = panel.edit.as_ref().filter(|edit| {
                    !edit.popup
                        && matches!(&edit.target, EditTarget::Field(path) if path == &field.path)
                });
                match editing {
                    Some(edit) => {
                        let masked = matches!(edit.kind, FieldKind::Secret) && !edit.reveal;
                        let (text, column) = edit_line(edit, masked, usize::from(value_area.width));
                        label(
                            frame,
                            Rect::new(value_area.x, y, value_area.width, 1),
                            text,
                            style.fg(theme.accent_model),
                        );
                        frame.set_cursor_position((
                            value_area.x + cells(column).min(value_area.width.saturating_sub(1)),
                            y,
                        ));
                    }
                    None => {
                        let value = panel.value(&field.path).cloned().unwrap_or(Value::Null);
                        match &value {
                            Value::Bool(true) => {
                                label(
                                    frame,
                                    Rect::new(value_area.x, y, value_area.width, 1),
                                    "● 开启",
                                    style.fg(theme.accent_model).add_modifier(Modifier::BOLD),
                                );
                            }
                            Value::Bool(false) => {
                                label(
                                    frame,
                                    Rect::new(value_area.x, y, value_area.width, 1),
                                    "○ 关闭",
                                    style.fg(theme.gray),
                                );
                            }
                            Value::Null
                                if field.path.last().is_some_and(|k| k == "thinking_levels") =>
                            {
                                label(
                                    frame,
                                    Rect::new(value_area.x, y, value_area.width, 1),
                                    "默认 (low..max)",
                                    style.fg(theme.gray),
                                );
                            }
                            Value::Null => {
                                label(
                                    frame,
                                    Rect::new(value_area.x, y, value_area.width, 1),
                                    "未设置",
                                    style.fg(theme.gray_dim),
                                );
                            }
                            _ => {
                                label(
                                    frame,
                                    Rect::new(value_area.x, y, value_area.width, 1),
                                    display_value(&value, field),
                                    style,
                                );
                            }
                        }
                    }
                }
            }
        }
        hits.push((row_area, Hit::Row(row_index)));
    }
    if has_scrollbar {
        let total = panel.rows.len();
        let thumb_height = (height * height / total).clamp(1, height);
        let max_offset = total.saturating_sub(height);
        let max_thumb_top = height.saturating_sub(thumb_height);
        let thumb_top = if max_offset > 0 {
            panel.offset * max_thumb_top / max_offset
        } else {
            0
        };
        let scroll_x = body.right().saturating_sub(1);
        for i in 0..height {
            let y = body.y + cells(i);
            let in_thumb = i >= thumb_top && i < thumb_top + thumb_height;
            let (ch, style) = if in_thumb {
                ("█", Style::new().fg(theme.accent_model))
            } else {
                ("│", Style::new().fg(theme.gray_dim))
            };
            frame.render_widget(
                Paragraph::new(ch).style(style),
                Rect::new(scroll_x, y, 1, 1),
            );
            let clicked_row = (i * total / height).min(total.saturating_sub(1));
            hits.push((Rect::new(scroll_x, y, 1, 1), Hit::Row(clicked_row)));
        }
    }
    if let Some(edit) = panel.edit.as_ref().filter(|edit| !edit.popup) {
        return (String::new(), edit.hint.clone());
    }
    match panel.row() {
        Some(RowKind::Group(group)) => (
            group_description(group),
            "Enter/Space 展开收起 · ←→/hl 层级导航 · e/c 展收全部 · Del 删除 · / 搜索 · Esc 返回"
                .into(),
        ),
        Some(RowKind::Field(index)) => {
            let field = &panel.fields[*index];
            let is_explicit = panel.explicit(&field.path);
            let state = if field.path.first().is_some_and(|part| part == "tui") {
                "（界面偏好设置）"
            } else if is_explicit {
                "（● 已在本文件自定义）"
            } else if matches!(panel.scope, ConfigScope::Project) {
                "（○ 沿用全局配置或默认值）"
            } else {
                "（○ 使用系统默认值）"
            };
            let description = format!("{} {}", field.description, state);
            // 模型角色字段由共用的模型选择器接管：Enter 打开搜索框，可同时挑思考等级。
            let is_model_role = field.path.starts_with(&["models".into(), "roles".into()]);
            let is_choice = matches!(field.kind, FieldKind::Choice(_))
                || (field.path.last().is_some_and(|k| k == "input")
                    && field.path.iter().any(|p| p == "models"));
            let action_hint = if let FieldKind::Bool = field.kind {
                "Space/Enter 切换开关"
            } else if is_model_role {
                "Space/Enter 打开模型选择器（可搜索、可选思考等级）"
            } else if is_choice {
                "Space 快速轮换 · Enter 弹窗选择"
            } else {
                "Enter 编辑 · 留空恢复默认"
            };
            let hint = format!(
                "{action_hint} · r 恢复默认 · ←/h 跳至分组 · / 搜索 · 1-9/[] 切页 · Esc 返回"
            );
            (description, hint)
        }

        None => (String::new(), "1-9/[] 切换标签 · / 搜索 · Esc 返回".into()),
    }
}

fn group_description(group: &Group) -> String {
    let path = group.path.as_deref().unwrap_or_default();
    match path.first().map(String::as_str) {
        Some("providers") if path.len() == 2 => format!("供应商“{}”的设置。", group.title),
        Some("providers") if path.len() == 4 => {
            format!("模型“{}”：{}。", group.title, group.detail)
        }
        Some("defaults") => format!(
            "名称匹配“{}”的模型默认设置。",
            path.get(2).map_or("", String::as_str)
        ),
        Some("mcp") => format!("MCP 服务“{}”。", group.title),
        Some("session") => "会话的自动归档与清理策略。".into(),
        Some(_) => format!("“{}”相关的设置。", group.title),

        None => "会话使用的各类模型；候选来自已添加的模型。".into(),
    }
}

/// 模型候选勾选列表。
fn draw_models(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> (String, String) {
    let View::Models {
        provider,
        rows,
        checked,
        selected,
    } = &mut panel.view
    else {
        return (String::new(), String::new());
    };
    label(
        frame,
        Rect::new(body.x, body.y, body.width, 1),
        format!("{provider} 的可用模型 · 已选 {} 个", checked.len()),
        Style::new().fg(theme.accent_model),
    );
    if rows.is_empty() {
        label(
            frame,
            Rect::new(body.x, body.y + 2, body.width, 1),
            if panel.busy {
                "正在获取模型列表…"
            } else {
                "没有返回可用的模型；可返回后手动添加。"
            },
            Style::new().fg(theme.gray),
        );
    }
    let height = usize::from(body.height.saturating_sub(2));
    if *selected < panel.offset {
        panel.offset = *selected;
    }
    if *selected >= panel.offset.saturating_add(height) {
        panel.offset = selected.saturating_sub(height.saturating_sub(1));
    }
    let has_scrollbar = rows.len() > height && height > 0;
    let scrollbar_margin = if has_scrollbar { 1 } else { 0 };
    let row_width = body.width.saturating_sub(scrollbar_margin);
    for (index, row) in rows.iter().enumerate().skip(panel.offset).take(height) {
        let exists = panel
            .effective
            .get("providers")
            .and_then(|providers| providers.get(provider.as_str()))
            .and_then(|provider| provider.get("models"))
            .and_then(|models| models.get(&row.id))
            .is_some();
        let text = format!(
            "{} {}  {}",
            if exists {
                "[已添加]"
            } else if checked.contains(&row.id) {
                "[✓]"
            } else {
                "[ ]"
            },
            row.id,
            if row.name == row.id { "" } else { &row.name }
        );
        let area = Rect::new(
            body.x,
            body.y + 2 + cells(index - panel.offset),
            row_width,
            1,
        );
        label(
            frame,
            area,
            text,
            if index == *selected {
                Style::new().bg(theme.bg_visual)
            } else {
                Style::new().fg(theme.gray)
            },
        );
        hits.push((area, Hit::Check(index)));
    }
    if has_scrollbar {
        let total = rows.len();
        let thumb_height = (height * height / total).clamp(1, height);
        let max_offset = total.saturating_sub(height);
        let max_thumb_top = height.saturating_sub(thumb_height);
        let thumb_top = if max_offset > 0 {
            panel.offset * max_thumb_top / max_offset
        } else {
            0
        };
        let scroll_x = body.right().saturating_sub(1);
        for i in 0..height {
            let y = body.y + 2 + cells(i);
            let in_thumb = i >= thumb_top && i < thumb_top + thumb_height;
            let (ch, style) = if in_thumb {
                ("█", Style::new().fg(theme.accent_model))
            } else {
                ("│", Style::new().fg(theme.gray_dim))
            };
            frame.render_widget(
                Paragraph::new(ch).style(style),
                Rect::new(scroll_x, y, 1, 1),
            );
            let clicked_row = (i * total / height).min(total.saturating_sub(1));
            hits.push((Rect::new(scroll_x, y, 1, 1), Hit::Check(clicked_row)));
        }
    }
    (
        "勾选需要的模型后按 Enter 添加；也可以返回后手动填写模型 ID。".into(),
        "↑↓/jk 移动 · Space 勾选 · A 全选/全消 · Enter 添加 · Esc 返回".into(),
    )
}

/// 工作区记忆列表。
fn draw_workspaces(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> (String, String) {
    let View::Workspaces { rows, selected } = &panel.view else {
        return (String::new(), String::new());
    };
    label(
        frame,
        Rect::new(body.x, body.y, body.width, 1),
        "工作区记忆",
        Style::new().fg(theme.accent_model),
    );
    let height = usize::from(body.height.saturating_sub(2));
    if *selected < panel.offset {
        panel.offset = *selected;
    }
    if *selected >= panel.offset.saturating_add(height) {
        panel.offset = selected.saturating_sub(height.saturating_sub(1));
    }
    let has_scrollbar = rows.len() > height && height > 0;
    let scrollbar_margin = if has_scrollbar { 1 } else { 0 };
    let row_width = body.width.saturating_sub(scrollbar_margin);
    for (index, row) in rows.iter().enumerate().skip(panel.offset).take(height) {
        let area = Rect::new(
            body.x,
            body.y + 2 + cells(index - panel.offset),
            row_width,
            1,
        );
        label(
            frame,
            area,
            format!(
                "{}{}{}",
                row.path,
                if row.exists {
                    ""
                } else {
                    " · 目录已失效"
                },
                if row.shared { " · 共享记忆" } else { "" }
            ),
            if index == *selected {
                Style::new().bg(theme.bg_visual)
            } else {
                Style::new().fg(theme.gray)
            },
        );
        hits.push((area, Hit::Check(index)));
    }
    if has_scrollbar {
        let total = rows.len();
        let thumb_height = (height * height / total).clamp(1, height);
        let max_offset = total.saturating_sub(height);
        let max_thumb_top = height.saturating_sub(thumb_height);
        let thumb_top = if max_offset > 0 {
            panel.offset * max_thumb_top / max_offset
        } else {
            0
        };
        let scroll_x = body.right().saturating_sub(1);
        for i in 0..height {
            let y = body.y + 2 + cells(i);
            let in_thumb = i >= thumb_top && i < thumb_top + thumb_height;
            let (ch, style) = if in_thumb {
                ("█", Style::new().fg(theme.accent_model))
            } else {
                ("│", Style::new().fg(theme.gray_dim))
            };
            frame.render_widget(
                Paragraph::new(ch).style(style),
                Rect::new(scroll_x, y, 1, 1),
            );
            let clicked_row = (i * total / height).min(total.saturating_sub(1));
            hits.push((Rect::new(scroll_x, y, 1, 1), Hit::Check(clicked_row)));
        }
    }
    let description = rows.get(*selected).map_or_else(
        || "还没有登记的工作区记忆；启用 OpenViking 后会按工作区登记。".into(),
        |row| {
            if row.shared {
                format!("{}：与其他工作区共享记忆。", row.path)
            } else {
                format!("{}：独占的记忆。", row.path)
            }
        },
    );
    (
        description,
        "↑↓/jk 移动 · Enter 改绑目录 · Del 删除该工作区记忆 · Esc 返回".into(),
    )
}

/// 小弹窗编辑器：单选候选列表居中卡片，单行输入压成两行卡片，长文本用半屏卡片。
fn draw_edit_popup(
    frame: &mut Frame,
    panel: &SettingsPanel,
    theme: &Theme,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    let Some(edit) = panel.edit.as_ref().filter(|edit| edit.popup) else {
        return;
    };
    if let FieldKind::Choice(options) = &edit.kind {
        let count = options.len();
        let max_visible = usize::from((area.height * 3 / 5).max(6).saturating_sub(4));
        let visible_count = count.min(max_visible).max(1);
        let height = cells(visible_count + 4);
        let width = area.width.saturating_sub(8).clamp(32, 72);
        let rect = Rect {
            x: area.x + (area.width.saturating_sub(width)) / 2,
            y: area.y + (area.height.saturating_sub(height)) / 2,
            width,
            height,
        };
        // 弹窗外点击关闭弹窗
        if rect.y > area.y {
            hits.push((
                Rect::new(area.x, area.y, area.width, rect.y - area.y),
                Hit::DismissPopup,
            ));
        }
        if area.bottom() > rect.bottom() {
            hits.push((
                Rect::new(
                    area.x,
                    rect.bottom(),
                    area.width,
                    area.bottom() - rect.bottom(),
                ),
                Hit::DismissPopup,
            ));
        }
        if rect.x > area.x {
            hits.push((
                Rect::new(area.x, rect.y, rect.x - area.x, rect.height),
                Hit::DismissPopup,
            ));
        }
        if area.right() > rect.right() {
            hits.push((
                Rect::new(
                    rect.right(),
                    rect.y,
                    area.right() - rect.right(),
                    rect.height,
                ),
                Hit::DismissPopup,
            ));
        }
        let close_btn_x = rect.right().saturating_sub(6);
        hits.push((Rect::new(close_btn_x, rect.y, 5, 1), Hit::DismissPopup));

        frame
            .buffer_mut()
            .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
        frame.render_widget(Clear, rect);
        let start = if edit.choice_selected >= visible_count {
            edit.choice_selected + 1 - visible_count
        } else {
            0
        };
        let mut lines = Vec::new();
        for (index, option) in options.iter().enumerate().skip(start).take(visible_count) {
            let is_sel = index == edit.choice_selected;
            let radio = if is_sel { "● " } else { "○ " };
            let text = if option.label == option.value {
                option.label.clone()
            } else {
                format!("{} ({})", option.label, option.value)
            };
            let style = if is_sel {
                Style::new()
                    .bg(theme.bg_visual)
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            lines.push(Line::from(vec![
                Span::styled(
                    format!("  {radio}"),
                    if is_sel {
                        Style::new().fg(theme.accent_model)
                    } else {
                        Style::new().fg(theme.gray)
                    },
                ),
                Span::styled(text, style),
            ]));
            let line_y = rect.y + 1 + cells(index - start);
            let option_area = Rect::new(rect.x + 1, line_y, rect.width.saturating_sub(2), 1);
            hits.push((option_area, Hit::Choice(index)));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            edit.hint.clone(),
            Style::new().fg(theme.gray_dim),
        )));
        frame.render_widget(
            Paragraph::new(lines).block(
                Block::bordered()
                    .border_set(ratatui::symbols::border::ROUNDED)
                    .border_style(Style::new().fg(theme.border_active))
                    .title(Span::styled(
                        format!(" {} ", edit.title),
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    ))
                    .title(
                        Line::from(vec![Span::styled(" [×] ", Style::new().fg(theme.gray_dim))])
                            .alignment(Alignment::Right),
                    ),
            ),
            rect,
        );
        return;
    }
    let multiline = matches!(
        edit.kind,
        FieldKind::Multiline | FieldKind::Strings | FieldKind::Object
    );
    let width = area.width.saturating_sub(8).clamp(24, 72);
    let masked = matches!(edit.kind, FieldKind::Secret) && !edit.reveal;
    let inner_width = usize::from(width.saturating_sub(4));
    let wrapped = multiline.then(|| wrap_editor(&edit.input, masked, inner_width));
    // 高度按内容自适应，最多占半屏；内容变长时随输入增长，超出后滚动。
    let cap = (area.height * 3 / 5).max(6);
    let height = if let Some(wrapped) = &wrapped {
        let lines = u16::try_from(wrapped.lines.len()).unwrap_or(u16::MAX);
        lines.saturating_add(3).clamp(4, cap)
    } else {
        4
    };
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    // 弹窗外点击关闭弹窗
    if rect.y > area.y {
        hits.push((
            Rect::new(area.x, area.y, area.width, rect.y - area.y),
            Hit::DismissPopup,
        ));
    }
    if area.bottom() > rect.bottom() {
        hits.push((
            Rect::new(
                area.x,
                rect.bottom(),
                area.width,
                area.bottom() - rect.bottom(),
            ),
            Hit::DismissPopup,
        ));
    }
    if rect.x > area.x {
        hits.push((
            Rect::new(area.x, rect.y, rect.x - area.x, rect.height),
            Hit::DismissPopup,
        ));
    }
    if area.right() > rect.right() {
        hits.push((
            Rect::new(
                rect.right(),
                rect.y,
                area.right() - rect.right(),
                rect.height,
            ),
            Hit::DismissPopup,
        ));
    }
    let close_btn_x = rect.right().saturating_sub(6);
    hits.push((Rect::new(close_btn_x, rect.y, 5, 1), Hit::DismissPopup));

    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    frame.render_widget(Clear, rect);
    let inner = Rect {
        x: rect.x + 2,
        y: rect.y + 1,
        width: rect.width.saturating_sub(4),
        height: rect.height.saturating_sub(2),
    };
    let width = usize::from(inner.width);
    let mut lines = Vec::new();
    let mut cursor = None;
    if let Some(wrapped) = wrapped {
        let rows = usize::from(inner.height).saturating_sub(1).max(1);
        let cursor_row = wrapped.cursor_row;
        let start = cursor_row.saturating_sub(rows.saturating_sub(1));
        for (index, line) in wrapped.lines.into_iter().enumerate().skip(start).take(rows) {
            if index == cursor_row {
                cursor = Some((cells(wrapped.cursor_column), cells(index - start)));
            }
            lines.push(Line::from(line));
        }
    } else {
        let (text, column) = edit_line(edit, masked, width);
        cursor = Some((cells(column), 0));
        lines.push(Line::from(text));
    }
    lines.push(Line::from(Span::styled(
        edit.hint.clone(),
        Style::new().fg(theme.gray_dim),
    )));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active))
                .title(Span::styled(
                    format!(" {} ", edit.title),
                    Style::new()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::BOLD),
                ))
                .title(
                    Line::from(vec![Span::styled(" [×] ", Style::new().fg(theme.gray_dim))])
                        .alignment(Alignment::Right),
                ),
        ),
        rect,
    );
    if let Some((column, row)) = cursor {
        let hint_row = inner.y + inner.height.saturating_sub(1);
        let y = (inner.y + row).min(hint_row.saturating_sub(1));
        frame.set_cursor_position((inner.x + column.min(inner.width.saturating_sub(1)), y));
    }
}

/// 行内编辑的一行文本：横向滚动到光标可见，返回文本与光标列。
fn edit_line(edit: &EditState, masked: bool, width: usize) -> (String, usize) {
    if masked {
        let text: String = edit
            .input
            .text
            .chars()
            .map(|character| if character == '\n' { ' ' } else { '•' })
            .collect();
        let prefix: String = text
            .chars()
            .take(edit.input.text[..edit.input.cursor].chars().count())
            .collect();
        return (text, str_width(&prefix));
    }
    tail(&edit.input.text, edit.input.cursor, width)
}

/// 光标处可见的文本尾巴：起点尽量靠后，保证光标不超出宽度。
fn tail(text: &str, cursor: usize, width: usize) -> (String, usize) {
    let width = width.max(1);
    let cursor = cursor.min(text.len());
    let boundaries: Vec<usize> = text
        .char_indices()
        .map(|(index, _)| index)
        .filter(|index| *index <= cursor)
        .collect();
    let mut start = 0;
    for boundary in boundaries.iter().rev() {
        if str_width(&text[*boundary..cursor]) + 1 > width {
            break;
        }
        start = *boundary;
    }
    (text[start..].to_owned(), str_width(&text[start..cursor]))
}

struct Wrapped {
    lines: Vec<String>,
    cursor_row: usize,
    cursor_column: usize,
}

/// 多行输入按宽度折行，并给出光标所在行与列。
fn wrap_editor(input: &PromptState, masked: bool, width: usize) -> Wrapped {
    let width = width.max(1);
    let mask = |text: &str| -> String {
        if masked {
            text.chars()
                .map(|character| if character == '\n' { '\n' } else { '•' })
                .collect()
        } else {
            text.to_owned()
        }
    };
    let text = mask(&input.text);
    let prefix = mask(&input.text[..input.cursor]);
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
    Wrapped {
        lines,
        cursor_row,
        cursor_column,
    }
}

/// 数字千分位格式化（200000 -> 200,000）。
pub(crate) fn format_number(value: u64) -> String {
    let s = value.to_string();
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut result = String::with_capacity(len + len / 3);
    for (i, &b) in bytes.iter().enumerate() {
        if i > 0 && (len - i) % 3 == 0 {
            result.push(',');
        }
        result.push(b as char);
    }
    result
}

/// 值的展示形式：布尔、密钥、候选与容器都换成用户可读文字。
pub(super) fn display_value(value: &Value, field: &ConfigField) -> String {
    if field
        .path
        .last()
        .is_some_and(|key| key == "thinking_levels")
    {
        if value.is_null() {
            return "默认 (low, medium, high, xhigh, max)".into();
        }
        if let Some(arr) = value.as_array() {
            if arr.is_empty() {
                return "无".into();
            }
            return arr
                .iter()
                .map(|v| v.as_str().unwrap_or_default())
                .collect::<Vec<_>>()
                .join("、");
        }
    }
    if field.path.last().is_some_and(|key| key == "input")
        && field.path.iter().any(|part| part == "models")
    {
        let has_image = value
            .as_array()
            .is_some_and(|arr| arr.iter().any(|v| v == "image"))
            || value.as_str().is_some_and(|s| s.contains("image"));
        return if has_image {
            "‹ 文本与图片 / 多模态 (text, image) ›".into()
        } else {
            "‹ 仅文本 (text) ›".into()
        };
    }
    if value.is_null() {
        return "未设置".into();
    }
    if matches!(field.kind, FieldKind::Secret) {
        return if value.as_str().is_some_and(|text| !text.is_empty()) {
            "••••••"
        } else {
            "未设置"
        }
        .into();
    }
    if field.path.last().is_some_and(|key| key == "endpoint") && value.as_str() == Some("Default") {
        return "默认".into();
    }
    // 字节预算按 KB/MB 显示，避免一串数字看不出量级。
    if let Some(bytes) = value.as_u64()
        && field.path.last().is_some_and(|key| key.ends_with("_bytes"))
    {
        return short_bytes(bytes);
    }
    // 上下文与回复上限等 Token 数显示千分位与缩写（200,000 (200k)）。
    if let Some(num) = value.as_u64()
        && field.path.last().is_some_and(|key| {
            matches!(
                key.as_str(),
                "context_window" | "max_output_tokens" | "max_tokens"
            )
        })
    {
        let compact = crate::render::format_tokens(num);
        let plain = format_number(num);
        // 不足 1000 时两者相同，没必要写成「512 (512)」。
        return if compact == plain {
            plain
        } else {
            format!("{plain} ({compact})")
        };
    }
    // 大于等于一万的整数加上千分位。
    if let Some(num) = value.as_u64()
        && num >= 10_000
    {
        return format_number(num);
    }
    if let FieldKind::Choice(options) = &field.kind
        && let Some(text) = value.as_str()
        && let Some(option) = options.iter().find(|option| option.value == text)
    {
        return format!("‹ {} ›", option.label);
    }
    match value {
        Value::Bool(true) => "● 开启".into(),
        Value::Bool(false) => "○ 关闭".into(),
        Value::String(text) if text.is_empty() => "未设置".into(),
        Value::String(text) => text.lines().next().unwrap_or_default().into(),
        Value::Object(values) if values.is_empty() => "无".into(),
        Value::Object(values) => format!("{{ {} 项 }}", values.len()),
        Value::Array(values) if values.is_empty() => "无".into(),
        Value::Array(values) => values
            .iter()
            .map(|value| value.as_str().unwrap_or_default())
            .collect::<Vec<_>>()
            .join("、"),
        _ => value.to_string(),
    }
}

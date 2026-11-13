use super::super::EditTarget;
use super::super::Group;
use super::super::Hit;
use super::super::RowKind;
use super::super::SettingsPanel;
use super::super::TABS;
use super::super::controls;
use super::super::controls::Control;
use super::super::editors::Editor;
use super::super::providers::ProviderAction;
use super::Footer;
use super::cells;
use super::editor::edit_line;
use super::label;
use super::paint_lines;
use super::prose;
use super::scrollbar;
use crate::render::labels;
use crate::render::modal::window_offset;
use crate::render::wrap::clip_line;
use crate::render::wrap::str_width;
use crate::settings::render::display_value;
use crate::theme::Theme;
use moc_protocol::ConfigScope;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

/// 标签列宽：取整页最宽标签（含光标、缩进与自定义标记），夹在 24–44 列，
/// 且不超过一半宽度；按整页而非可见行计算，滚动时列线不跳。
pub(super) fn label_column(panel: &SettingsPanel, width: usize) -> usize {
    let widest = panel
        .rows
        .iter()
        .map(|row| {
            let text = match &row.kind {
                RowKind::Group(group) => str_width(&group.title),
                RowKind::Action(_) => 0,
                RowKind::Field(index) => {
                    let field = &panel.fields[*index];
                    let badge = if panel.filter.is_empty() {
                        0
                    } else {
                        str_width(&field.tab) + 3
                    };
                    str_width(&field.label) + badge
                }
            };
            4 + usize::from(row.depth) * 2 + text
        })
        .max()
        .unwrap_or(0);
    (widest + 2).clamp(24, 44).min(width / 2)
}

/// 字段树：分组头可折叠，字段行可原地编辑。
pub(super) fn draw_fields(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    let width = usize::from(body.width);
    let hints = |items: &[(&str, &str)]| labels::hint_layout(items, width);
    if panel.data.is_none() {
        label(
            frame,
            Rect { height: 1, ..body },
            "正在读取设置…",
            Style::new().fg(theme.accent_running),
        );
        return Footer {
            hint: hints(&[("esc", "返回")]),
            ..Footer::default()
        };
    }
    if panel.rows.is_empty() {
        let text = if !panel.filter.is_empty() {
            "没有匹配的设置项；按 / 修改搜索内容，或按 Esc 清空过滤。"
        } else {
            match TABS[panel.tab] {
                "供应商" => "还没有供应商。按 A 添加，填写服务地址与密钥后按 F 获取模型。",
                "模型" => "还没有模型。按 A 填写模型 ID，或到“供应商”页按 F 获取。",
                "MCP" => "还没有 MCP 服务。按 A 添加。",
                "插件" => "当前没有可配置的插件项。",
                _ => "这一页暂时没有可配置项。",
            }
        };
        let lines = prose(
            text,
            width,
            usize::from(body.height),
            Style::new().fg(theme.gray),
            theme,
        );
        paint_lines(frame, body, &lines);
        let hint = if !panel.filter.is_empty() {
            hints(&[("/", "重新搜索"), ("esc", "清空过滤")])
        } else {
            hints(&[("1-9/[]", "切页"), ("esc", "返回")])
        };
        return Footer {
            hint,
            ..Footer::default()
        };
    }
    let height = usize::from(body.height);
    let total = panel.rows.len();
    // 键盘/点击选中了才显露选中项；滚轮手动滚到的位置普通重绘不拉回。
    panel.offset = if panel.scroll.reveal_selection {
        window_offset(panel.offset, panel.selected, height, total)
    } else {
        panel.offset.min(total.saturating_sub(height))
    };
    panel.scroll.max_offset = total.saturating_sub(height);
    panel.scroll.reveal_selection = false;
    let label_width = label_column(panel, width);
    let value_x = body.x + cells(label_width);
    let value_width = body.width.saturating_sub(cells(label_width));
    for (row_index, row) in panel
        .rows
        .iter()
        .enumerate()
        .skip(panel.offset)
        .take(height)
    {
        let y = body.y + cells(row_index - panel.offset);
        let row_area = Rect::new(body.x, y, body.width, 1);
        let label_area = Rect::new(body.x, y, cells(label_width.saturating_sub(1)), 1);
        let value_area = Rect::new(value_x, y, value_width, 1);
        let selected = row_index == panel.selected;
        let style = if selected {
            Style::new().bg(theme.bg_visual).fg(theme.text_primary)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        frame.buffer_mut().set_style(row_area, style);
        // 选中标记与浮层列表同一个 `❯`。
        let lead = format!(
            "{} {}",
            if selected { "❯" } else { " " },
            " ".repeat(usize::from(row.depth) * 2)
        );
        match &row.kind {
            RowKind::Action(action) => {
                let inline = panel
                    .edit
                    .as_ref()
                    .filter(|edit| !edit.popup)
                    .and_then(|edit| {
                        let matches = matches!(
                            (&edit.target, action),
                            (EditTarget::Model(_), ProviderAction::Manual)
                                | (EditTarget::ModelFilter, ProviderAction::Search)
                        );
                        if matches && let Editor::Text(editor) = &edit.editor {
                            Some(editor)
                        } else {
                            None
                        }
                    });
                if let Some(editor) = inline {
                    let prompt = if *action == ProviderAction::Manual {
                        "模型 ID："
                    } else {
                        "搜索："
                    };
                    let prefix = format!("{lead}{prompt}");
                    let start = cells(str_width(&prefix)).min(row_area.width);
                    label(frame, row_area, &prefix, style);
                    let input_area = Rect::new(
                        row_area.x + start,
                        y,
                        row_area.width.saturating_sub(start),
                        1,
                    );
                    let (text, column) = edit_line(editor, usize::from(input_area.width));
                    label(frame, input_area, &text, style.fg(theme.accent_running));
                    if input_area.width > 0 {
                        frame.set_cursor_position((
                            input_area.x + cells(column).min(input_area.width - 1),
                            y,
                        ));
                    }
                } else {
                    label(
                        frame,
                        row_area,
                        &format!("{lead}{}", panel.provider_action_text(action)),
                        style.fg(theme.accent_model),
                    );
                }
            }
            RowKind::Group(group) => {
                let marker = if panel.is_expanded(&group.key, usize::from(row.depth)) {
                    "▾"
                } else {
                    "▸"
                };
                label(
                    frame,
                    label_area,
                    &format!("{lead}{marker} {}", group.title),
                    style.add_modifier(Modifier::BOLD),
                );
                label(frame, value_area, &group.detail, style.fg(theme.gray));
            }
            RowKind::Field(index) => {
                let field = &panel.fields[*index];
                let is_explicit = panel.explicit(&field.path);
                let label_style = if is_explicit && !selected {
                    style.fg(theme.text_primary).add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                let mut spans = vec![Span::styled(
                    format!(
                        "{lead}{}{}",
                        if is_explicit { "● " } else { "  " },
                        field.label
                    ),
                    label_style,
                )];
                // 搜索结果标出所在选项卡：独立的暗色徽标，不和标签同色。
                if !panel.filter.is_empty() {
                    spans.push(Span::styled(
                        format!("  [{}]", field.tab),
                        style.fg(theme.gray),
                    ));
                }
                let line = clip_line(Line::from(spans), usize::from(label_area.width));
                frame
                    .buffer_mut()
                    .set_line(label_area.x, y, &line, label_area.width);
                let inline =
                    panel
                        .edit
                        .as_ref()
                        .and_then(|edit| match (&edit.target, &edit.editor) {
                            (EditTarget::Field(path), Editor::Text(text))
                                if !edit.popup && *path == field.path =>
                            {
                                Some(text)
                            }
                            _ => None,
                        });
                match inline {
                    Some(editor) => {
                        let (text, column) = edit_line(editor, usize::from(value_width));
                        label(frame, value_area, &text, style.fg(theme.accent_running));
                        frame.set_cursor_position((
                            value_x + cells(column).min(value_width.saturating_sub(1)),
                            y,
                        ));
                    }
                    None => {
                        let value = panel.value(&field.path).unwrap_or(&Value::Null);
                        let control = &panel.controls[*index];
                        let (text, value_style) = match (control, value) {
                            (Control::Toggle, Value::Bool(true)) => (
                                "● 开启".to_owned(),
                                style.fg(theme.accent_success).add_modifier(Modifier::BOLD),
                            ),
                            (Control::Toggle, Value::Bool(false)) => {
                                ("○ 关闭".to_owned(), style.fg(theme.gray))
                            }
                            _ => {
                                let text = controls::summary(control, value)
                                    .unwrap_or_else(|| display_value(value, field));
                                let dim = if value.is_null() {
                                    style.fg(theme.gray)
                                } else {
                                    style
                                };
                                (text, dim)
                            }
                        };
                        label(frame, value_area, &text, value_style);
                    }
                }
            }
        }
        hits.push((row_area, Hit::Row(row_index)));
    }
    scrollbar(
        frame,
        body,
        panel.offset,
        panel.rows.len(),
        theme,
        Some((hits, Hit::Scroll)),
    );
    if let Some(edit) = panel.edit.as_ref().filter(|edit| !edit.popup) {
        let note = match &edit.target {
            EditTarget::Model(_) => "模型 ID 原样保存，可包含 /",
            EditTarget::ModelFilter => "实时过滤，已有勾选会保留",
            _ => "留空恢复默认",
        };
        let mut items = vec![("", note), ("tab", "下一项")];
        items.extend(edit.editor.hints());
        return Footer {
            hint: hints(&items),
            ..Footer::default()
        };
    }
    // 提示条从左丢项：最可省的写在前面，Esc 恒在末尾。
    match panel.row() {
        Some(RowKind::Action(_)) => Footer {
            description: "模型操作只作用于当前供应商；新增模型后可直接展开编辑参数。".into(),
            source: None,
            hint: hints(&[
                ("tab", "切换焦点"),
                ("↑↓", "移动"),
                ("enter/space", "操作"),
                ("esc", "返回"),
            ]),
        },
        Some(RowKind::Group(group)) => {
            let mut items = vec![
                ("1-9/[]", "切页"),
                ("e/c", "展收全部"),
                ("←→/hl", "层级导航"),
            ];
            if group.path.is_some() {
                items.push(("del", "删除"));
            }
            items.extend([("enter/space", "展开收起"), ("esc", "返回")]);
            Footer {
                description: group_description(group),
                source: None,
                hint: hints(&items),
            }
        }
        Some(RowKind::Field(index)) => {
            let field = &panel.fields[*index];
            let source = if field.path.first().is_some_and(|part| part == "tui") {
                "界面偏好设置"
            } else if panel.explicit(&field.path) {
                "● 已在本文件自定义"
            } else if matches!(panel.scope, ConfigScope::Project) {
                "○ 沿用全局配置或默认值"
            } else {
                "○ 使用系统默认值"
            };
            let items = [
                ("1-9/[]", "切页"),
                ("←/h", "跳至分组"),
                ("r", "恢复默认"),
                ("", controls::action_hint(&panel.controls[*index])),
                ("esc", "返回"),
            ];
            Footer {
                description: field.description.clone(),
                source: Some(source),
                hint: hints(&items),
            }
        }
        None => Footer {
            hint: hints(&[("1-9/[]", "切页"), ("esc", "返回")]),
            ..Footer::default()
        },
    }
}

pub(super) fn group_description(group: &Group) -> String {
    let path = group.grouping.as_slice();
    match path.first().map(String::as_str) {
        Some("providers") if path.len() == 2 => format!("供应商“{}”的设置。", group.title),
        Some("providers") if group.detail.is_empty() => format!("模型“{}”的设置。", group.title),
        Some("providers") => format!("模型“{}”：{}。", group.title, group.detail),
        Some("models") if path.get(1).is_some_and(|part| part == "roles") => {
            "会话使用的各类模型；候选来自已添加的模型。".into()
        }
        Some("models") => {
            "主导模型负责规划与评审，执行模型负责实现与测试；两者都配置才生效。".into()
        }
        Some("defaults") => format!(
            "名称匹配“{}”的模型默认设置。",
            path.get(2).map_or("", String::as_str)
        ),
        Some("mcp") => format!("MCP 服务“{}”。", group.title),
        _ => format!("“{}”相关的设置。", group.title),
    }
}

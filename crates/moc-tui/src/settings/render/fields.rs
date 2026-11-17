use super::super::EditTarget;
use super::super::Group;
use super::super::Hit;
use super::super::RowKind;
use super::super::SettingsPanel;
use super::super::TABS;
use super::super::controls;
use super::super::controls::Control;
use super::super::editors::Editor;
use super::super::forms;
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
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use rust_i18n::t;
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
    let hints = |items: &[(&str, String)]| labels::hint_layout(items, width);
    let h = |key| t!(key).into_owned();
    if panel.data.is_none() {
        label(
            frame,
            Rect { height: 1, ..body },
            &t!("tui.settings.loading"),
            Style::new().fg(theme.accent_running),
        );
        return Footer {
            hint: hints(&[("esc", h("tui.hint.back"))]),
            ..Footer::default()
        };
    }
    if panel.rows.is_empty() {
        let text = if !panel.filter.is_empty() {
            t!("tui.settings.no_match").into_owned()
        } else {
            match TABS[panel.tab] {
                "models" => t!("tui.settings.empty_models").into_owned(),
                "providers" => t!("tui.settings.empty_providers").into_owned(),
                "mcp" => t!("tui.settings.empty_mcp").into_owned(),
                "plugins" => t!("tui.settings.empty_plugins").into_owned(),
                _ => t!("tui.settings.empty_page").into_owned(),
            }
        };
        let lines = prose(
            &text,
            width,
            usize::from(body.height),
            Style::new().fg(theme.gray),
            theme,
        );
        paint_lines(frame, body, &lines);
        let hint = if !panel.filter.is_empty() {
            hints(&[
                ("/", h("tui.hint.research")),
                ("esc", h("tui.hint.clear_filter")),
            ])
        } else {
            hints(&[("esc", h("tui.hint.back"))])
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
                        );
                        if matches && let Editor::Text(editor) = &edit.editor {
                            Some(editor)
                        } else {
                            None
                        }
                    });
                if let Some(editor) = inline {
                    let prefix = format!("{lead}{}", t!("tui.settings.model_id_prefix"));
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
                } else if matches!(action, ProviderAction::More) {
                    let count = t!(
                        "tui.settings.item_count",
                        count = panel.provider_more_count()
                    )
                    .into_owned();
                    let detail_style = if selected {
                        style.fg(theme.gray_bright)
                    } else {
                        style.fg(theme.gray)
                    };
                    label(
                        frame,
                        label_area,
                        &format!("{lead}{}", panel.provider_action_text(action)),
                        style.fg(theme.accent_running).add_modifier(Modifier::BOLD),
                    );
                    label(frame, value_area, &count, detail_style);
                } else {
                    let accent = match action {
                        ProviderAction::Fetch => match &panel.providers.discovery {
                            Some(discovery) if discovery.pending.is_some() => theme.accent_running,
                            Some(discovery) if discovery.error.is_some() => theme.accent_error,
                            _ => theme.accent_success,
                        },
                        ProviderAction::Manual => theme.accent_tool,
                        ProviderAction::New => theme.accent_success,
                        ProviderAction::Remove => theme.accent_error,
                        ProviderAction::More => theme.accent_running,
                    };
                    let pill_bg = if selected {
                        theme.bg_selection
                    } else {
                        theme.bg_highlight
                    };
                    let text_fg = if selected { theme.text_primary } else { accent };
                    let button = format!(" {} ", panel.provider_action_text(action));
                    let spans = vec![
                        Span::styled(lead, style),
                        Span::styled("│", Style::new().fg(accent).bg(pill_bg)),
                        Span::styled(
                            button,
                            Style::new()
                                .fg(text_fg)
                                .bg(pill_bg)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled("│", Style::new().fg(accent).bg(pill_bg)),
                    ];
                    let line = clip_line(Line::from(spans), usize::from(row_area.width));
                    frame
                        .buffer_mut()
                        .set_line(row_area.x, y, &line, row_area.width);
                }
            }
            RowKind::Group(group) => {
                let marker = if panel.is_expanded(&group.key, usize::from(row.depth)) {
                    "▾"
                } else {
                    "▸"
                };
                let is_advanced = group.grouping.last().is_some_and(|p| p == "__advanced");
                let muted_detail = if selected {
                    style.fg(theme.gray_bright)
                } else {
                    style.fg(theme.gray)
                };
                let (title_style, detail_style) = if is_advanced {
                    (
                        style.fg(theme.accent_running).add_modifier(Modifier::BOLD),
                        muted_detail,
                    )
                } else if is_model_card(group) {
                    (
                        style.fg(theme.accent_model).add_modifier(Modifier::BOLD),
                        if selected {
                            style.fg(theme.text_primary)
                        } else {
                            style.fg(theme.text_secondary)
                        },
                    )
                } else {
                    let level_color = match row.depth {
                        0 => theme.accent_tool,
                        1 => theme.accent_assistant,
                        _ => theme.chart_7,
                    };
                    (
                        style.fg(level_color).add_modifier(Modifier::BOLD),
                        muted_detail,
                    )
                };
                label(
                    frame,
                    label_area,
                    &format!("{lead}{marker} {}", group.title),
                    title_style,
                );
                label(frame, value_area, &group.detail, detail_style);
            }
            RowKind::Field(index) => {
                let field = &panel.fields[*index];
                let is_explicit = panel.explicit(&field.path);
                let label_style = if is_explicit && !selected {
                    style.fg(theme.text_primary).add_modifier(Modifier::BOLD)
                } else {
                    style
                };
                let mut spans = vec![
                    Span::styled(lead, style),
                    if is_explicit {
                        Span::styled("● ", style.fg(theme.accent_success))
                    } else {
                        Span::styled("  ", style)
                    },
                    Span::styled(field.label.clone(), label_style),
                ];
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
                                t!("tui.settings.toggle_on").into_owned(),
                                style.fg(theme.accent_success).add_modifier(Modifier::BOLD),
                            ),
                            (Control::Toggle, Value::Bool(false)) => (
                                t!("tui.settings.toggle_off").into_owned(),
                                style.fg(theme.gray),
                            ),
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
        let note = match (&edit.target, &edit.editor) {
            (EditTarget::Model(_), _) => t!("tui.settings.model_id_hint").into_owned(),
            // 普通文本字段留空写入空值；请求参数与数值、JSON 一样删键回默认。
            (EditTarget::Field(path), Editor::Text(editor))
                if forms::keeps_empty(&editor.kind, path) =>
            {
                t!("tui.settings.keep_empty").into_owned()
            }
            _ => t!("tui.settings.empty_default").into_owned(),
        };
        let mut items = vec![("", note), ("tab", h("tui.hint.next_field"))];
        items.extend(edit.editor.hints());
        return Footer {
            hint: hints(&items),
            ..Footer::default()
        };
    }
    // 提示条从左丢项：最可省的写在前面，Esc 恒在末尾。
    match panel.row() {
        Some(RowKind::Action(action)) => Footer {
            description: match action {
                ProviderAction::More => t!("tui.settings.act_more"),
                ProviderAction::Fetch => t!("tui.settings.act_fetch"),
                ProviderAction::Manual => t!("tui.settings.act_manual"),
                ProviderAction::New => t!("tui.settings.act_new"),
                ProviderAction::Remove => t!("tui.settings.act_remove"),
            }
            .into_owned(),
            source: None,
            hint: hints(&[
                ("tab", h("tui.hint.focus")),
                ("↑↓", h("tui.hint.move")),
                ("enter/space", h("tui.hint.operate")),
                ("esc", h("tui.hint.back")),
            ]),
        },
        Some(RowKind::Group(group)) => {
            let mut items = Vec::new();
            if TABS[panel.tab] == "mcp" {
                items.push(("a", h("tui.hint.add_server")));
            } else if TABS[panel.tab] == "session" {
                items.push(("m", h("tui.hint.maintenance")));
            } else if TABS[panel.tab] == "models" {
                items.push(("ctrl+d", h("tui.hint.default_rules")));
            }
            if group.path.is_some() {
                items.push(("del", h("tui.hint.delete")));
            }
            items.extend([
                ("enter/space", h("tui.hint.expand")),
                ("esc", h("tui.hint.back")),
            ]);
            Footer {
                description: group_description(group),
                source: None,
                hint: hints(&items),
            }
        }
        Some(RowKind::Field(index)) => {
            let field = &panel.fields[*index];
            let source = if field.path.first().is_some_and(|part| part == "tui") {
                t!("tui.settings.src_ui").into_owned()
            } else if panel.explicit(&field.path) {
                t!("tui.settings.src_custom").into_owned()
            } else {
                t!("tui.settings.src_default").into_owned()
            };
            let mut items = Vec::new();
            if TABS[panel.tab] == "mcp" {
                items.push(("a", h("tui.hint.add_server")));
            } else if TABS[panel.tab] == "session" {
                items.push(("m", h("tui.hint.maintenance")));
            } else if TABS[panel.tab] == "models" {
                items.push(("ctrl+d", h("tui.hint.default_rules")));
            }
            items.extend([
                ("/", h("tui.hint.search")),
                ("r", h("tui.hint.reset")),
                ("", controls::action_hint(&panel.controls[*index])),
                ("esc", h("tui.hint.back")),
            ]);
            Footer {
                description: field.description.clone(),
                source: Some(source),
                hint: hints(&items),
            }
        }
        None => Footer {
            hint: hints(&[("esc", h("tui.hint.back"))]),
            ..Footer::default()
        },
    }
}

fn is_model_card(group: &Group) -> bool {
    matches!(
        group.grouping.as_slice(),
        [p, _, m, _] if p == "providers" && m == "models"
    ) || matches!(
        group.grouping.as_slice(),
        [d, m, _] if d == "defaults" && m == "models"
    )
}

pub(super) fn group_description(group: &Group) -> String {
    let path = group.grouping.as_slice();
    if path
        .iter()
        .any(|part| matches!(part.as_str(), "extra_body" | "headers"))
    {
        return t!("tui.group.extra_body", title = group.title).into_owned();
    }
    if path.last().is_some_and(|part| part == "__advanced") {
        return match path.first().map(String::as_str) {
            Some("providers") => t!("tui.group.advanced_model").into_owned(),
            _ => t!("tui.group.advanced").into_owned(),
        };
    }
    match path.first().map(String::as_str) {
        Some("providers") if path.len() == 2 => {
            t!("tui.group.provider", title = group.title).into_owned()
        }
        Some("providers") if group.detail.is_empty() => {
            t!("tui.group.model", title = group.title).into_owned()
        }
        Some("providers") => t!(
            "tui.group.model_detail",
            title = group.title,
            detail = group.detail
        )
        .into_owned(),
        Some("models") if path.get(1).is_some_and(|part| part == "roles") => {
            t!("tui.group.roles").into_owned()
        }
        Some("defaults") => t!(
            "tui.group.defaults",
            pattern = path.get(2).map_or("", String::as_str)
        )
        .into_owned(),
        Some("agents") => t!("tui.group.agents").into_owned(),
        Some("context_files") => t!("tui.group.context_files").into_owned(),
        Some("system_prompt") => t!("tui.group.system_prompt").into_owned(),
        Some("skills") => t!("tui.group.skills").into_owned(),
        Some("web_fetch") => t!("tui.group.web_fetch").into_owned(),
        Some("workspace") => t!("tui.group.workspace").into_owned(),
        Some("tools") => match path.get(1).map(String::as_str) {
            Some("ptc") => t!("tui.group.tools_ptc").into_owned(),
            _ => t!("tui.group.tools").into_owned(),
        },
        Some("sandbox") => t!("tui.group.sandbox").into_owned(),
        Some("compaction") if path.get(1).is_some_and(|p| p == "shake") => {
            t!("tui.group.shake").into_owned()
        }
        Some("compaction") => t!("tui.group.compaction").into_owned(),
        Some("session") => t!("tui.group.session").into_owned(),
        Some("workflow") => t!("tui.group.workflow").into_owned(),
        Some("features") => t!("tui.group.features").into_owned(),
        Some("limits") => t!("tui.group.limits").into_owned(),
        Some("tui") if path.get(1).is_some_and(|p| p == "keybindings") => {
            t!("tui.group.keybindings").into_owned()
        }
        Some("tui") => t!("tui.group.tui").into_owned(),
        Some("mcp") if path.len() == 1 => t!("tui.group.mcp").into_owned(),
        Some("mcp") => t!("tui.group.mcp_one", title = group.title).into_owned(),
        Some("plugins") if path.len() == 1 => t!("tui.group.plugins").into_owned(),
        _ => t!("tui.group.generic", title = group.title).into_owned(),
    }
}

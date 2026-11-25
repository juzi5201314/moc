use super::super::Hit;
use super::super::SettingsPanel;
use super::super::View;
use super::super::providers::{
    APIS, DRAFT_CONTROLS, ModelSubTab, ProviderAction, ProviderSubTab, api_short_label,
};
use super::Footer;
use super::cells;
use super::editor::edit_line;
use super::fields::draw_fields;
use super::flow;
use super::label;
use super::scrollbar;
use super::short_bytes;
use crate::render::labels;
use crate::render::modal::window_offset;
use crate::render::overlay;
use crate::render::wrap::{clip_line, str_width, truncate, wrap_text};
use crate::theme::Theme;
use moc_core::config::presets::PROVIDER_PRESETS;
use moc_i18n::t;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

/// 供应商列表和详情共用一个页面，窄终端用可切换的单栏布局。
pub(super) fn draw_providers(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    if panel.data.is_none() {
        return draw_fields(frame, panel, theme, body, hits);
    }
    let ids = panel.provider_ids();
    let wide = body.width >= 72;
    let sidebar_w: u16 = if body.width >= 88 { 26 } else { 22 };
    let sidebar = if wide {
        Rect {
            width: sidebar_w,
            ..body
        }
    } else {
        body
    };
    if wide || panel.providers.sidebar {
        let height = usize::from(sidebar.height);
        let cursor = panel.providers.cursor;
        let total = ids.len() + 1;
        panel.providers.offset = if panel.providers.scroll.reveal_selection {
            window_offset(panel.providers.offset, cursor, height, total)
        } else {
            panel.providers.offset.min(total.saturating_sub(height))
        };
        panel.providers.scroll.max_offset = total.saturating_sub(height);
        panel.providers.scroll.reveal_selection = false;
        for index in (panel.providers.offset..=ids.len()).take(height) {
            let (text, disabled) = match ids.get(index) {
                Some(id) => {
                    let prov = &panel.effective["providers"][id];
                    let count = prov["models"]
                        .as_object()
                        .map_or(0, serde_json::Map::len);
                    let disabled = prov.get("enabled").and_then(Value::as_bool) == Some(false);
                    let api = prov.get("api").and_then(Value::as_str).unwrap_or("");
                    let proto = api_short_label(api);
                    let inherited = panel.inherited_from_global(&["providers".into(), id.clone()]);
                    let mut suffix = format!(" · {count}");
                    if !proto.is_empty() && sidebar_w >= 26 {
                        suffix.push_str(&format!(" ({proto})"));
                    }
                    if disabled {
                        suffix.push_str(" off");
                    } else if inherited {
                        suffix.push_str(" [G]");
                    }
                    (format!("{id}{suffix}"), disabled)
                }
                None => (t!("tui.providers.add").into_owned(), false),
            };
            let selected = panel.provider_selected(&ids, index);
            let style = if selected {
                Style::new()
                    .bg(theme.bg_visual)
                    .fg(if panel.providers.sidebar {
                        theme.accent_model
                    } else {
                        theme.text_primary
                    })
                    .add_modifier(Modifier::BOLD)
            } else if disabled {
                Style::new().fg(theme.gray)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            let area = Rect::new(
                sidebar.x,
                sidebar.y + cells(index - panel.providers.offset),
                sidebar.width,
                1,
            );
            label(
                frame,
                area,
                &format!("{} {text}", if selected { "❯" } else { " " }),
                style,
            );
            hits.push((area, Hit::Provider(index)));
        }
    }
    if !wide && panel.providers.sidebar {
        return Footer {
            description: t!("tui.providers.pick_desc").into_owned(),
            source: None,
            hint: labels::hint_layout(
                &[
                    ("↑↓", t!("tui.hint.choose").into_owned()),
                    ("enter/→", t!("tui.hint.open").into_owned()),
                    ("x", t!("tui.hint.toggle_enabled").into_owned()),
                    ("c", t!("tui.hint.clone").into_owned()),
                    ("del", t!("tui.hint.delete").into_owned()),
                    ("s", t!("tui.hint.scope").into_owned()),
                    ("esc", t!("tui.hint.back").into_owned()),
                ],
                usize::from(body.width),
            ),
        };
    }
    let detail = if wide {
        let gap = 2u16;
        Rect::new(
            body.x + sidebar_w + gap,
            body.y,
            body.width.saturating_sub(sidebar_w + gap),
            body.height,
        )
    } else {
        body
    };
    if panel.providers.draft.is_some() {
        return draw_provider_draft(frame, panel, theme, detail, hits);
    }
    let Some(id) = panel.providers.selected.clone() else {
        let area = Rect {
            height: 1,
            ..detail
        };
        label(
            frame,
            area,
            &t!("tui.providers.add_hint"),
            Style::new().fg(theme.accent_model),
        );
        hits.push((area, Hit::ProviderAction(ProviderAction::New)));
        return Footer {
            description: t!("tui.providers.new_desc").into_owned(),
            source: None,
            hint: labels::hint_layout(
                &[
                    ("n", t!("tui.hint.new").into_owned()),
                    ("s", t!("tui.hint.scope").into_owned()),
                    ("esc", t!("tui.hint.back").into_owned()),
                ],
                usize::from(body.width),
            ),
        };
    };

    // 模型钻取编辑视图
    if let Some(model_id) = panel.providers.editing_model.clone() {
        let model_path = vec![
            "providers".into(),
            id.clone(),
            "models".into(),
            model_id.clone(),
        ];
        let disabled = panel.value(&[
            "providers".into(),
            id.clone(),
            "models".into(),
            model_id.clone(),
            "enabled".into(),
        ]) == Some(&Value::Bool(false));
        let mut header = format!("← {id} / {model_id}");
        if panel.inherited_from_global(&model_path) {
            header.push_str("  [Global]");
        }
        if disabled {
            header.push_str("  [Disabled]");
        }
        label(
            frame,
            Rect {
                height: 1,
                ..detail
            },
            &header,
            Style::new()
                .fg(theme.accent_model)
                .add_modifier(Modifier::BOLD),
        );
        let configured = panel.configured_param_count();
        let subtabs_end = if detail.height >= 4 {
            let chips = ModelSubTab::ALL.into_iter().map(|tab| {
                let text = match tab {
                    ModelSubTab::General => " General ".to_owned(),
                    ModelSubTab::Compat => " Compat ".to_owned(),
                    ModelSubTab::Params if configured > 0 => format!(" Params ({configured}) "),
                    ModelSubTab::Params => " Params ".to_owned(),
                };
                let style = if tab == panel.providers.model_subtab {
                    Style::new()
                        .bg(theme.bg_visual)
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.gray)
                };
                (text, style, Hit::ModelSubTab(tab))
            });
            flow(frame, detail, detail.y + 1, chips, hits)
        } else {
            detail.y
        };
        let top = (subtabs_end + 2).min(detail.bottom());
        let content = Rect::new(
            detail.x,
            top,
            detail.width,
            detail.bottom().saturating_sub(top),
        );
        let mut footer = draw_fields(frame, panel, theme, content, hits);
        if panel.edit.is_none() {
            let mut items: Vec<(&str, String)> = vec![
                ("tab", t!("tui.hint.subtab").into_owned()),
                ("↑↓", t!("tui.hint.move").into_owned()),
                ("enter/space", t!("tui.hint.edit_operate").into_owned()),
                ("x", t!("tui.hint.toggle_enabled").into_owned()),
                ("c", t!("tui.hint.clone").into_owned()),
            ];
            if panel.field().is_some() {
                items.push(("r", t!("tui.hint.reset").into_owned()));
            }
            items.push(("esc/←", t!("tui.hint.back_models").into_owned()));
            footer.hint = labels::hint_layout(&items, usize::from(body.width));
        }
        return footer;
    }

    // 供应商详情视图
    let prov = &panel.effective["providers"][&id];
    let api_raw = prov.get("api").and_then(Value::as_str).unwrap_or("");
    let api_name = APIS
        .iter()
        .find(|(k, _)| *k == api_raw)
        .map_or(api_raw, |(_, v)| *v);
    let disabled = prov.get("enabled").and_then(Value::as_bool) == Some(false);
    let inherited = panel.inherited_from_global(&["providers".into(), id.clone()]);
    let mut title = if api_name.is_empty() {
        id.clone()
    } else {
        format!("{id} · {api_name}")
    };
    if inherited {
        title.push_str("  [Global]");
    }
    if disabled {
        title.push_str("  [Disabled]");
    }
    label(
        frame,
        Rect {
            height: 1,
            ..detail
        },
        &title,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );

    let model_count = panel.provider_model_ids(&id).len();
    let param_count = panel.configured_param_count();
    let subtabs_end = if detail.height >= 4 {
        let chips = ProviderSubTab::ALL.into_iter().map(|tab| {
            let text = match tab {
                ProviderSubTab::Models => format!(" Models ({model_count}) "),
                ProviderSubTab::Connection => " Connection ".to_owned(),
                ProviderSubTab::Params if param_count > 0 => format!(" Params ({param_count}) "),
                ProviderSubTab::Params => " Params ".to_owned(),
            };
            let style = if tab == panel.providers.subtab {
                Style::new()
                    .bg(theme.bg_visual)
                    .fg(theme.accent_model)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.gray)
            };
            (text, style, Hit::ProviderSubTab(tab))
        });
        flow(frame, detail, detail.y + 1, chips, hits)
    } else {
        detail.y
    };

    let mut next_y = subtabs_end + 1;
    // 展示测试连接状态或精简连接摘要
    if let Some(state) = &panel.providers.test
        && next_y < detail.bottom()
    {
        let (text, color) = if state.pending.is_some() {
            (
                t!("tui.providers.testing").into_owned(),
                theme.accent_running,
            )
        } else if let Some(res) = &state.result {
            let mark = if res.ok { "✓" } else { "✗" };
            (
                format!("{mark} {}", res.message),
                if res.ok {
                    theme.accent_success
                } else {
                    theme.accent_error
                },
            )
        } else {
            (String::new(), theme.gray)
        };
        if !text.is_empty() {
            label(
                frame,
                Rect::new(detail.x, next_y, detail.width, 1),
                &text,
                Style::new().fg(color),
            );
            next_y += 1;
        }
    } else if panel.providers.subtab == ProviderSubTab::Models && next_y + 3 < detail.bottom() {
        let url = prov.get("base_url").and_then(Value::as_str).unwrap_or("");
        let key = prov.get("api_key").and_then(Value::as_str).unwrap_or("");
        let key_desc = if key.is_empty() {
            t!("tui.providers.key_none").into_owned()
        } else if key.starts_with("env:") || key.starts_with('!') {
            key.to_owned()
        } else {
            t!("tui.providers.key_set").into_owned()
        };
        let summary = format!(
            "{} · {key_desc}",
            if url.is_empty() { "—" } else { url }
        );
        label(
            frame,
            Rect::new(detail.x, next_y, detail.width, 1),
            &summary,
            Style::new().fg(theme.gray),
        );
        next_y += 1;
    }

    let content_y = (next_y + 1).min(detail.bottom());
    let content = Rect::new(
        detail.x,
        content_y,
        detail.width,
        detail.bottom().saturating_sub(content_y),
    );
    let mut footer = draw_fields(frame, panel, theme, content, hits);
    if panel.edit.is_none() {
        let items: Vec<(&str, String)> = if panel.providers.sidebar {
            vec![
                ("↑↓", t!("tui.hint.choose").into_owned()),
                ("→/enter", t!("tui.hint.open").into_owned()),
                ("n", t!("tui.hint.new").into_owned()),
                ("t", t!("tui.hint.test").into_owned()),
                ("x", t!("tui.hint.toggle_enabled").into_owned()),
                ("c", t!("tui.hint.clone").into_owned()),
                ("del", t!("tui.hint.delete").into_owned()),
                ("s", t!("tui.hint.scope").into_owned()),
                ("esc", t!("tui.hint.back").into_owned()),
            ]
        } else {
            let mut list = vec![
                ("←", t!("tui.hint.sidebar").into_owned()),
                ("tab", t!("tui.hint.subtab").into_owned()),
                ("t", t!("tui.hint.test").into_owned()),
                ("f", t!("tui.hint.fetch").into_owned()),
                ("a", t!("tui.hint.add_manual").into_owned()),
                ("x", t!("tui.hint.toggle_enabled").into_owned()),
                ("c", t!("tui.hint.clone").into_owned()),
                ("s", t!("tui.hint.scope").into_owned()),
            ];
            if panel.field().is_some() {
                list.push(("r", t!("tui.hint.reset").into_owned()));
            }
            list.push(("esc", t!("tui.hint.back").into_owned()));
            list
        };
        footer.hint = labels::hint_layout(&items, usize::from(body.width));
    }
    footer
}

pub(super) fn draw_provider_draft(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    let draft = panel.providers.draft.as_mut().expect("新建表单");
    label(
        frame,
        Rect { height: 1, ..body },
        &t!("tui.providers.draft_title"),
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let mut list_top = body.y + 2;
    if let Some(state) = &panel.providers.test
        && body.height >= 6
    {
        let (text, color) = if state.pending.is_some() {
            (
                t!("tui.providers.testing").into_owned(),
                theme.accent_running,
            )
        } else if let Some(res) = &state.result {
            let mark = if res.ok { "✓" } else { "✗" };
            (
                format!("{mark} {}", res.message),
                if res.ok {
                    theme.accent_success
                } else {
                    theme.accent_error
                },
            )
        } else {
            (String::new(), theme.gray)
        };
        if !text.is_empty() {
            label(
                frame,
                Rect::new(body.x, body.y + 1, body.width, 1),
                &text,
                Style::new().fg(color),
            );
            list_top = body.y + 3;
        }
    }
    let list = Rect::new(
        body.x,
        list_top.min(body.bottom()),
        body.width,
        body.bottom().saturating_sub(list_top),
    );
    let height = usize::from(list.height);
    draft.offset = if draft.scroll.reveal_selection {
        window_offset(draft.offset, draft.focus, height, DRAFT_CONTROLS)
    } else {
        draft.offset.min(DRAFT_CONTROLS.saturating_sub(height))
    };
    draft.scroll.max_offset = DRAFT_CONTROLS.saturating_sub(height);
    draft.scroll.reveal_selection = false;
    let url_bad = {
        let u = draft.url.input.text.trim();
        !u.is_empty() && !u.starts_with("http://") && !u.starts_with("https://")
    };
    for index in (draft.offset..DRAFT_CONTROLS).take(height) {
        let area = Rect::new(list.x, list.y + cells(index - draft.offset), list.width, 1);
        let selected = draft.focus == index;
        let style = if selected {
            Style::new().bg(theme.bg_visual).fg(theme.text_primary)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        let title = t!([
            "tui.providers.field_preset",
            "tui.providers.field_id",
            "tui.providers.field_api",
            "tui.providers.field_url",
            "tui.providers.field_key",
            "tui.providers.field_test",
            "tui.providers.field_add_fetch",
            "tui.providers.field_add_only",
        ][index]);
        let row_style = if index >= 5 {
            if selected {
                style.fg(theme.accent_model).add_modifier(Modifier::BOLD)
            } else {
                style.fg(theme.accent_tool)
            }
        } else {
            style
        };
        label(
            frame,
            area,
            &format!("{} {title}", if selected { "❯" } else { " " }),
            row_style,
        );
        let start = 14.min(area.width);
        let input_area = Rect::new(area.x + start, area.y, area.width.saturating_sub(start), 1);
        if index == 0 {
            let preset_label = PROVIDER_PRESETS[draft.preset % PROVIDER_PRESETS.len()].label;
            label(frame, input_area, &format!("‹ {preset_label} ›"), style);
        } else if index == 2 {
            label(frame, input_area, &format!("‹ {} ›", APIS[draft.api].1), style);
        } else if let Some(input) = match index {
            1 => Some(&draft.id),
            3 => Some(&draft.url),
            4 => Some(&draft.key),
            _ => None,
        } {
            let (text, column) = edit_line(input, usize::from(input_area.width));
            let val_style = if index == 3 && url_bad {
                style.fg(theme.accent_error)
            } else {
                style
            };
            label(frame, input_area, &text, val_style);
            if selected && input_area.width > 0 && draft.pending.is_none() {
                frame.set_cursor_position((
                    input_area.x + cells(column).min(input_area.width - 1),
                    area.y,
                ));
            }
        }
        hits.push((area, Hit::ProviderInput(index)));
    }
    Footer {
        description: if draft.pending.is_some() {
            t!("tui.providers.saving").into_owned()
        } else if draft.focus == 3 && url_bad {
            t!("tui.err.url_scheme").into_owned()
        } else {
            t!([
                "tui.providers.field_preset_desc",
                "tui.providers.field_id_desc",
                "tui.providers.field_api_desc",
                "tui.providers.field_url_desc",
                "tui.providers.field_key_desc",
                "tui.providers.field_test_desc",
                "tui.providers.field_add_fetch_desc",
                "tui.providers.field_add_only_desc",
            ][draft.focus.min(DRAFT_CONTROLS - 1)])
            .into_owned()
        },
        source: None,
        hint: labels::hint_layout(
            &[
                ("tab/↑↓", t!("tui.hint.switch_field").into_owned()),
                ("←→", t!("tui.hint.switch_option").into_owned()),
                ("f6", t!("tui.hint.reveal_key").into_owned()),
                ("ctrl+s", t!("tui.hint.add_fetch").into_owned()),
                ("esc", t!("tui.hint.cancel").into_owned()),
            ],
            usize::from(body.width),
        ),
    }
}

/// 维护预览：摘要 + 可滚动的删除清单。
pub(super) fn draw_maintenance(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    body: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) -> Footer {
    let View::Maintenance { report, offset } = &panel.view else {
        return Footer::default();
    };
    let summary = t!(
        "tui.maint.summary",
        prefix = if report.applied {
            t!("tui.maint.done").into_owned()
        } else {
            t!("tui.maint.preview").into_owned()
        },
        count = report.removed.len(),
        bytes = short_bytes(report.bytes),
        protected = report.protected
    );
    label(
        frame,
        Rect { height: 1, ..body },
        &summary,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let list = Rect::new(
        body.x,
        body.y + 2,
        body.width,
        body.height.saturating_sub(2),
    );
    let height = usize::from(list.height);
    let applied = report.applied;
    let total = report.removed.len();
    let offset = (*offset).min(total.saturating_sub(height));
    for (row, path) in report.removed.iter().skip(offset).take(height).enumerate() {
        label(
            frame,
            Rect::new(list.x, list.y + cells(row), list.width, 1),
            &t!("tui.maint.delete_fmt", path = path.as_str()),
            Style::new().fg(theme.text_secondary),
        );
    }
    scrollbar(frame, list, offset, total, theme, Some((hits, Hit::Scroll)));
    if let View::Maintenance {
        offset: current, ..
    } = &mut panel.view
    {
        *current = offset;
    }
    let mut items = Vec::new();
    if !applied {
        items.push(("enter", t!("tui.hint.run_maint").into_owned()));
    }
    if total > height {
        items.push(("↑↓", t!("tui.hint.browse").into_owned()));
    }
    items.push(("esc", t!("tui.hint.back").into_owned()));
    Footer {
        description: t!("tui.maint.desc").into_owned(),
        source: None,
        hint: labels::hint_layout(&items, usize::from(body.width)),
    }
}

/// 格式化候选模型的能力摘要（如 `400k · Thinking · Image`）。
fn candidate_meta_badge(model: &moc_protocol::CatalogModel) -> String {
    if model.image_gen == Some(true) {
        return "Image Gen".into();
    }
    let mut parts = Vec::new();
    if let Some(ctx) = model.context_window {
        parts.push(crate::render::format_tokens(ctx));
    }
    if model.reasoning == Some(true) {
        parts.push("Thinking".into());
    }
    if model
        .input
        .as_ref()
        .is_some_and(|list| list.iter().any(|v| v == "image"))
    {
        parts.push("Image".into());
    }
    parts.join(" · ")
}

/// 模型获取与多选弹窗：独立卡片居中展示，支持实时输入搜索、勾选、全选、批量添加（不强制关闭）与多行错误折行。
pub(super) fn draw_discovery_popup(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if !panel.provider_page() || panel.providers.discovery.is_none() || panel.edit.is_some() {
        return;
    }
    let Some(provider_id) = panel.providers.selected.clone() else {
        return;
    };
    hits.clear();
    let card_width = overlay::card_width(area, 80);
    let width = overlay::content_width(card_width);
    let rows = overlay::max_body_rows(area, true).max(1);
    let visible = panel.visible_candidates();
    let added_flags: Vec<bool> = visible
        .iter()
        .map(|idx| panel.candidate_added(*idx))
        .collect();
    let (query, pending, error, status_note, total_rows, checked_count) = {
        let d = panel.providers.discovery.as_ref().expect("发现列表存在");
        (
            d.query.clone(),
            d.pending.is_some(),
            d.error.clone(),
            d.status.clone(),
            d.rows.len(),
            d.checked.len(),
        )
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut search_cursor_col: Option<usize> = None;
    if rows >= 2 {
        let prefix = t!("tui.providers.search_prefix").into_owned();
        let prefix_w = str_width(&prefix);
        let badge = if checked_count > 0 {
            t!("tui.providers.selected_fmt", count = checked_count).into_owned()
        } else {
            String::new()
        };
        let query_w = width.saturating_sub(prefix_w + str_width(&badge));
        let query_span = if query.is_empty() {
            search_cursor_col = Some(prefix_w);
            Span::styled(
                truncate(t!("tui.providers.filter_hint").into_owned(), query_w).into_owned(),
                Style::new().fg(theme.gray),
            )
        } else {
            let (shown, col) = super::editor::tail(&query, query.len(), query_w.max(1));
            search_cursor_col = Some(prefix_w + col);
            Span::styled(shown, Style::new().fg(theme.accent_running))
        };
        let mut spans = vec![
            Span::styled(prefix, Style::new().fg(theme.text_secondary)),
            query_span,
        ];
        if !badge.is_empty() {
            spans.push(Span::styled(badge, Style::new().fg(theme.accent_model)));
        }
        lines.push(clip_line(Line::from(spans), width));
    }
    if pending {
        if lines.len() < rows {
            lines.push(clip_line(
                Line::styled(
                    t!("tui.providers.fetching").into_owned(),
                    Style::new().fg(theme.accent_running),
                ),
                width,
            ));
        }
    } else if let Some(err) = &error {
        let full = t!("tui.providers.fetch_failed", err = err.as_str()).into_owned();
        for wrapped in wrap_text(&full, width).into_iter().take(4) {
            if lines.len() < rows {
                lines.push(Line::styled(wrapped, Style::new().fg(theme.accent_error)));
            }
        }
    } else if let Some(note) = &status_note {
        if lines.len() < rows {
            lines.push(clip_line(
                Line::styled(note.clone(), Style::new().fg(theme.accent_success)),
                width,
            ));
        }
    } else if total_rows == 0 {
        if lines.len() < rows {
            lines.push(clip_line(
                Line::styled(
                    t!("tui.providers.no_models").into_owned(),
                    Style::new().fg(theme.gray),
                ),
                width,
            ));
        }
    } else if visible.is_empty() {
        if lines.len() < rows {
            lines.push(clip_line(
                Line::styled(
                    t!("tui.providers.no_match").into_owned(),
                    Style::new().fg(theme.gray),
                ),
                width,
            ));
        }
    } else if rows >= 3 {
        lines.push(Line::default());
    }
    let header_rows = lines.len();
    let list_rows = rows.saturating_sub(header_rows).max(1);
    let start = {
        let d = panel.providers.discovery.as_mut().expect("发现列表存在");
        if visible.is_empty() {
            d.cursor = 0;
            d.offset = 0;
            d.scroll.max_offset = 0;
            d.scroll.reveal_selection = false;
            0
        } else {
            d.cursor = d.cursor.min(visible.len() - 1);
            super::editor::viewport(
                &mut d.scroll,
                &mut d.offset,
                d.cursor,
                list_rows,
                visible.len(),
            )
        }
    };
    let mut row_hits: Vec<(usize, usize)> = Vec::new();
    {
        let d = panel.providers.discovery.as_ref().expect("发现列表存在");
        for (vis_idx, &cand_idx) in visible
            .iter()
            .enumerate()
            .skip(start)
            .take(rows.saturating_sub(header_rows))
        {
            let model = &d.rows[cand_idx];
            let added = added_flags[vis_idx];
            let checked = !added && d.checked.contains(&model.id);
            let focused = vis_idx == d.cursor;
            let (mark, mark_style) = if added {
                ("✓  ", Style::new().fg(theme.gray))
            } else if checked {
                (
                    "☑  ",
                    Style::new()
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::BOLD),
                )
            } else {
                ("☐  ", Style::new().fg(theme.gray))
            };
            let mut spans = vec![
                Span::styled(mark, mark_style),
                Span::styled(
                    model.id.clone(),
                    if added {
                        Style::new().fg(theme.gray)
                    } else {
                        Style::new()
                    },
                ),
            ];
            let meta = candidate_meta_badge(model);
            if !meta.is_empty() {
                spans.push(Span::styled(
                    format!("  [{meta}]"),
                    Style::new().fg(theme.gray),
                ));
            } else if !model.name.is_empty() && model.name != model.id {
                spans.push(Span::styled(
                    format!(" · {}", model.name),
                    Style::new().fg(theme.gray),
                ));
            }
            if added {
                spans.push(Span::styled(
                    t!("tui.providers.added").into_owned(),
                    Style::new().fg(theme.gray),
                ));
            }
            let body_row = lines.len();
            lines.push(super::editor::option_row(focused, spans, width, theme));
            row_hits.push((body_row, vis_idx));
        }
    }
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    let layout = overlay::Card {
        title: t!("tui.providers.fetch_title", id = provider_id.as_str()).into_owned(),
        body: lines,
        footer: Some(overlay::footer(
            &[
                ("↑↓/ctrl+jk", t!("tui.hint.move").into_owned()),
                ("space", t!("tui.hint.check").into_owned()),
                ("ctrl+a", t!("tui.hint.select_all").into_owned()),
                ("enter", t!("tui.hint.add_selected").into_owned()),
                ("f5", t!("tui.hint.retry").into_owned()),
                ("esc", t!("tui.hint.close").into_owned()),
            ],
            width,
            theme,
        )),
    }
    .render(frame, area, card_width, theme);
    hits.extend(
        layout
            .keys
            .iter()
            .map(|key| (key.area, Hit::Hint(key.code, key.modifiers))),
    );
    for (body_row, vis_idx) in row_hits {
        let y = layout.body.y + cells(body_row);
        if y < layout.body.bottom() {
            hits.push((
                Rect::new(layout.body.x, y, layout.body.width, 1),
                Hit::DiscoveryItem(vis_idx),
            ));
        }
    }
    super::editor::dismiss_around(layout.area, area, hits);
    if let Some(col) = search_cursor_col
        && layout.body.height > 0
        && layout.body.width > 0
    {
        frame.set_cursor_position((
            layout.body.x + cells(col).min(layout.body.width - 1),
            layout.body.y,
        ));
    }
}

/// 按需添加 API 请求参数的搜索弹窗。
pub(super) fn draw_param_picker_popup(
    frame: &mut Frame,
    panel: &mut SettingsPanel,
    theme: &Theme,
    area: Rect,
    hits: &mut Vec<(Rect, Hit)>,
) {
    if !panel.provider_page() || panel.providers.param_picker.is_none() || panel.edit.is_some() {
        return;
    }
    hits.clear();
    let card_width = overlay::card_width(area, 76);
    let width = overlay::content_width(card_width);
    let rows = overlay::max_body_rows(area, true).max(1);
    let visible = panel.visible_param_picks();
    let query = panel
        .providers
        .param_picker
        .as_ref()
        .expect("参数弹窗存在")
        .query
        .clone();
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut search_cursor_col: Option<usize> = None;
    if rows >= 2 {
        let prefix = t!("tui.providers.search_prefix").into_owned();
        let prefix_w = str_width(&prefix);
        let query_w = width.saturating_sub(prefix_w);
        let query_span = if query.is_empty() {
            search_cursor_col = Some(prefix_w);
            Span::styled(
                truncate(t!("tui.providers.param_filter_hint").into_owned(), query_w).into_owned(),
                Style::new().fg(theme.gray),
            )
        } else {
            let (shown, col) = super::editor::tail(&query, query.len(), query_w.max(1));
            search_cursor_col = Some(prefix_w + col);
            Span::styled(shown, Style::new().fg(theme.accent_running))
        };
        lines.push(clip_line(
            Line::from(vec![
                Span::styled(prefix, Style::new().fg(theme.text_secondary)),
                query_span,
            ]),
            width,
        ));
    }
    if visible.is_empty() {
        if lines.len() < rows {
            lines.push(clip_line(
                Line::styled(
                    t!("tui.providers.no_param_match").into_owned(),
                    Style::new().fg(theme.gray),
                ),
                width,
            ));
        }
    } else if rows >= 3 {
        lines.push(Line::default());
    }
    let header_rows = lines.len();
    let list_rows = rows.saturating_sub(header_rows).max(1);
    let start = {
        let picker = panel.providers.param_picker.as_mut().expect("参数弹窗存在");
        if visible.is_empty() {
            picker.cursor = 0;
            picker.offset = 0;
            picker.scroll.max_offset = 0;
            picker.scroll.reveal_selection = false;
            0
        } else {
            picker.cursor = picker.cursor.min(visible.len() - 1);
            super::editor::viewport(
                &mut picker.scroll,
                &mut picker.offset,
                picker.cursor,
                list_rows,
                visible.len(),
            )
        }
    };
    let cursor = panel
        .providers
        .param_picker
        .as_ref()
        .map_or(0, |p| p.cursor);
    let mut row_hits: Vec<(usize, usize)> = Vec::new();
    for (vis_idx, &field_idx) in visible
        .iter()
        .enumerate()
        .skip(start)
        .take(rows.saturating_sub(header_rows))
    {
        let field = &panel.fields[field_idx];
        let mut spans = vec![Span::raw(field.label.clone())];
        if !field.description.is_empty() {
            spans.push(Span::styled(
                format!("  {}", field.description),
                Style::new().fg(theme.gray),
            ));
        }
        let body_row = lines.len();
        lines.push(super::editor::option_row(
            vis_idx == cursor,
            spans,
            width,
            theme,
        ));
        row_hits.push((body_row, vis_idx));
    }
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    let layout = overlay::Card {
        title: t!("tui.providers.add_param").into_owned(),
        body: lines,
        footer: Some(overlay::footer(
            &[
                ("↑↓/ctrl+jk", t!("tui.hint.move").into_owned()),
                ("enter", t!("tui.hint.add").into_owned()),
                ("esc", t!("tui.hint.close").into_owned()),
            ],
            width,
            theme,
        )),
    }
    .render(frame, area, card_width, theme);
    hits.extend(
        layout
            .keys
            .iter()
            .map(|key| (key.area, Hit::Hint(key.code, key.modifiers))),
    );
    for (body_row, vis_idx) in row_hits {
        let y = layout.body.y + cells(body_row);
        if y < layout.body.bottom() {
            hits.push((
                Rect::new(layout.body.x, y, layout.body.width, 1),
                Hit::ParamPickItem(vis_idx),
            ));
        }
    }
    super::editor::dismiss_around(layout.area, area, hits);
    if let Some(col) = search_cursor_col
        && layout.body.height > 0
        && layout.body.width > 0
    {
        frame.set_cursor_position((
            layout.body.x + cells(col).min(layout.body.width - 1),
            layout.body.y,
        ));
    }
}

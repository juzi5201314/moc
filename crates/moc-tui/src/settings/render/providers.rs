use super::super::Hit;
use super::super::SettingsPanel;
use super::super::View;
use super::super::providers::ProviderAction;
use super::Footer;
use super::cells;
use super::editor::edit_line;
use super::fields::draw_fields;
use super::label;
use super::scrollbar;
use super::short_bytes;
use crate::render::labels;
use crate::render::modal::window_offset;
use crate::render::overlay;
use crate::render::wrap::clip_line;
use crate::render::wrap::str_width;
use crate::render::wrap::truncate;
use crate::settings::providers::APIS;
use crate::theme::Theme;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use moc_i18n::t;

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
    let sidebar = if wide {
        Rect { width: 22, ..body }
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
            let text = match ids.get(index) {
                Some(id) => {
                    let count = panel.effective["providers"][id]["models"]
                        .as_object()
                        .map_or(0, serde_json::Map::len);
                    format!("{id} · {count}")
                }
                None => t!("tui.providers.add").into_owned(),
            };
            let selected = panel.provider_selected(&ids, index);
            let style = if selected {
                Style::new().bg(theme.bg_visual).fg(theme.accent_model)
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
                    ("enter/tab", t!("tui.hint.open").into_owned()),
                    ("esc", t!("tui.hint.back").into_owned()),
                ],
                usize::from(body.width),
            ),
        };
    }
    let detail = if wide {
        Rect::new(body.x + 24, body.y, body.width - 24, body.height)
    } else {
        body
    };
    if panel.providers.draft.is_some() {
        return draw_provider_draft(frame, panel, theme, detail, hits);
    }
    let Some(id) = panel.providers.selected.as_ref() else {
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
                    ("esc", t!("tui.hint.back").into_owned()),
                ],
                usize::from(body.width),
            ),
        };
    };
    label(
        frame,
        Rect {
            height: 1,
            ..detail
        },
        &t!("tui.providers.title_fmt", id = id.as_str()),
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    );
    let content = Rect::new(
        detail.x,
        detail.y + 2,
        detail.width,
        detail.height.saturating_sub(2),
    );
    let mut footer = draw_fields(frame, panel, theme, content, hits);
    if panel.edit.is_none() {
        let mut items: Vec<(&str, String)> = vec![
            ("n", t!("tui.hint.new").into_owned()),
            ("f", t!("tui.hint.fetch").into_owned()),
            ("a", t!("tui.hint.add_manual").into_owned()),
            ("ctrl+x", t!("tui.hint.remove_provider").into_owned()),
            ("tab", t!("tui.hint.focus").into_owned()),
            ("↑↓", t!("tui.hint.move").into_owned()),
            ("enter/space", t!("tui.hint.edit_operate").into_owned()),
        ];
        // 本页整体替换字段提示条；字段行要单独把恢复默认露出来，放在 Esc 前优先保留。
        if panel.field().is_some() {
            items.push(("r", t!("tui.hint.reset").into_owned()));
        }
        items.push(("esc", t!("tui.hint.back").into_owned()));
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
    let list = Rect::new(
        body.x,
        body.y + 2,
        body.width,
        body.height.saturating_sub(2),
    );
    let height = usize::from(list.height);
    const CONTROLS: usize = 6;
    draft.offset = if draft.scroll.reveal_selection {
        window_offset(draft.offset, draft.focus, height, CONTROLS)
    } else {
        draft.offset.min(CONTROLS.saturating_sub(height))
    };
    draft.scroll.max_offset = CONTROLS.saturating_sub(height);
    draft.scroll.reveal_selection = false;
    for index in (draft.offset..CONTROLS).take(height) {
        let area = Rect::new(list.x, list.y + cells(index - draft.offset), list.width, 1);
        let selected = draft.focus == index;
        let style = if selected {
            Style::new().bg(theme.bg_visual).fg(theme.text_primary)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        let title = t!([
            "tui.providers.field_id",
            "tui.providers.field_api",
            "tui.providers.field_url",
            "tui.providers.field_key",
            "tui.providers.field_add_fetch",
            "tui.providers.field_add_only",
        ][index]);
        label(
            frame,
            area,
            &format!("{} {title}", if selected { "❯" } else { " " }),
            style,
        );
        let start = 14.min(area.width);
        let input_area = Rect::new(area.x + start, area.y, area.width.saturating_sub(start), 1);
        if index == 1 {
            label(frame, input_area, APIS[draft.api].1, style);
        } else if let Some(input) = match index {
            0 => Some(&draft.id),
            2 => Some(&draft.url),
            3 => Some(&draft.key),
            _ => None,
        } {
            let (text, column) = edit_line(input, usize::from(input_area.width));
            label(frame, input_area, &text, style);
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
            t!("tui.providers.saving")
        } else {
            t!([
                "tui.providers.field_id_desc",
                "tui.providers.field_api_desc",
                "tui.providers.field_url_desc",
                "tui.providers.field_key_desc",
                "tui.providers.field_add_fetch_desc",
                "tui.providers.field_add_only_desc",
            ][draft.focus.min(CONTROLS - 1)])
        }
        .into_owned(),
        source: None,
        hint: labels::hint_layout(
            &[
                ("tab/↑↓", t!("tui.hint.switch_field").into_owned()),
                ("←→", t!("tui.hint.switch_api").into_owned()),
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

/// 模型获取与多选弹窗：独立卡片居中展示，支持实时输入搜索、勾选、全选与批量添加。
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
    let card_width = overlay::card_width(area, 76);
    let width = overlay::content_width(card_width);
    let rows = overlay::max_body_rows(area, true).max(1);
    let visible = panel.visible_candidates();
    let added_flags: Vec<bool> = visible
        .iter()
        .map(|idx| panel.candidate_added(*idx))
        .collect();
    let (query, pending, error, total_rows, checked_count) = {
        let d = panel.providers.discovery.as_ref().expect("发现列表存在");
        (
            d.query.clone(),
            d.pending.is_some(),
            d.error.clone(),
            d.rows.len(),
            d.checked.len(),
        )
    };
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut search_cursor_col: Option<usize> = None;
    if rows >= 2 {
        let badge = if checked_count > 0 {
            t!("tui.providers.selected_fmt", count = checked_count).into_owned()
        } else {
            String::new()
        };
        let query_w = width.saturating_sub(6 + str_width(&badge));
        let query_span = if query.is_empty() {
            search_cursor_col = Some(6);
            Span::styled(
                truncate(t!("tui.providers.filter_hint").into_owned(), query_w).into_owned(),
                Style::new().fg(theme.gray),
            )
        } else {
            let (shown, col) = super::editor::tail(&query, query.len(), query_w.max(1));
            search_cursor_col = Some(6 + col);
            Span::styled(shown, Style::new().fg(theme.accent_running))
        };
        let mut spans = vec![
            Span::styled(
                t!("tui.providers.search_prefix").into_owned(),
                Style::new().fg(theme.text_secondary),
            ),
            query_span,
        ];
        if !badge.is_empty() {
            spans.push(Span::styled(badge, Style::new().fg(theme.accent_model)));
        }
        lines.push(clip_line(Line::from(spans), width));
    }
    let status = if pending {
        Some((
            t!("tui.providers.fetching").into_owned(),
            theme.accent_running,
        ))
    } else if let Some(err) = &error {
        Some((
            t!("tui.providers.fetch_failed", err = err.as_str()).into_owned(),
            theme.accent_error,
        ))
    } else if total_rows == 0 {
        Some((t!("tui.providers.no_models").into_owned(), theme.gray))
    } else if visible.is_empty() {
        Some((t!("tui.providers.no_match").into_owned(), theme.gray))
    } else {
        None
    };
    if let Some((text, color)) = status {
        if lines.len() < rows {
            lines.push(clip_line(Line::styled(text, Style::new().fg(color)), width));
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
            if !model.name.is_empty() && model.name != model.id {
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
                ("↑↓", t!("tui.hint.move").into_owned()),
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

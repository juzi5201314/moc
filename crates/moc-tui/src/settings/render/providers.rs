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
                None => "+ 添加供应商".into(),
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
            description: "选择供应商后配置连接参数与模型列表。".into(),
            source: None,
            hint: labels::hint_layout(
                &[("↑↓", "选择"), ("enter/tab", "打开"), ("esc", "返回")],
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
            "+ 添加供应商：配置连接参数后拉取或手动添加模型",
            Style::new().fg(theme.accent_model),
        );
        hits.push((area, Hit::ProviderAction(ProviderAction::New)));
        return Footer {
            description: "按 N 新建供应商。".into(),
            source: None,
            hint: labels::hint_layout(&[("n", "新建"), ("esc", "返回")], usize::from(body.width)),
        };
    };
    label(
        frame,
        Rect {
            height: 1,
            ..detail
        },
        &format!("{id} · 连接与模型"),
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
        let mut items = vec![
            ("n", "新建"),
            ("f", "获取模型"),
            ("a", "手动添加"),
            ("ctrl+x", "删除供应商"),
            ("tab", "切换焦点"),
            ("↑↓", "移动"),
            ("enter/space", "编辑/操作"),
        ];
        // 本页整体替换字段提示条；字段行要单独把恢复默认露出来，放在 Esc 前优先保留。
        if panel.field().is_some() {
            items.push(("r", "恢复默认"));
        }
        items.push(("esc", "返回"));
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
        "添加供应商",
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
        let title = [
            "标识",
            "API 格式",
            "服务地址",
            "API 密钥",
            "添加并获取模型",
            "仅添加供应商",
        ][index];
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
            "正在保存供应商…"
        } else {
            "地址示例：https://api.example.com/v1；本地服务无需密钥时可留空。"
        }
        .into(),
        source: None,
        hint: labels::hint_layout(
            &[
                ("tab/↑↓", "切换字段"),
                ("←→", "切换协议"),
                ("f6", "显示密钥"),
                ("ctrl+s", "添加并获取"),
                ("esc", "取消"),
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
    let summary = format!(
        "{}清理 {} 个临时文件（{}），保留 {} 项。",
        if report.applied {
            "维护完成："
        } else {
            "预计"
        },
        report.removed.len(),
        short_bytes(report.bytes),
        report.protected
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
            &format!("删除 {path}"),
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
        items.push(("enter", "执行维护"));
    }
    if total > height {
        items.push(("↑↓", "浏览清单"));
    }
    items.push(("esc", "返回"));
    Footer {
        description: "只清理过期且未被会话引用的临时文件。".into(),
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
            format!("  已选 {checked_count}")
        } else {
            String::new()
        };
        let query_w = width.saturating_sub(6 + str_width(&badge));
        let query_span = if query.is_empty() {
            search_cursor_col = Some(6);
            Span::styled(
                truncate("输入模型名或 ID 过滤", query_w).into_owned(),
                Style::new().fg(theme.gray),
            )
        } else {
            let (shown, col) = super::editor::tail(&query, query.len(), query_w.max(1));
            search_cursor_col = Some(6 + col);
            Span::styled(shown, Style::new().fg(theme.accent_running))
        };
        let mut spans = vec![
            Span::styled("搜索：", Style::new().fg(theme.text_secondary)),
            query_span,
        ];
        if !badge.is_empty() {
            spans.push(Span::styled(badge, Style::new().fg(theme.accent_model)));
        }
        lines.push(clip_line(Line::from(spans), width));
    }
    let status = if pending {
        Some(("正在获取模型列表…".to_owned(), theme.accent_running))
    } else if let Some(err) = &error {
        Some((format!("获取失败：{err}"), theme.accent_error))
    } else if total_rows == 0 {
        Some((
            "供应商未返回模型，可关闭后手动添加。".to_owned(),
            theme.gray,
        ))
    } else if visible.is_empty() {
        Some((
            "没有匹配的模型，按 Backspace 修改过滤词。".to_owned(),
            theme.gray,
        ))
    } else {
        None
    };
    if let Some((text, color)) = status {
        if lines.len() < rows {
            lines.push(clip_line(
                Line::styled(text, Style::new().fg(color)),
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
            if !model.name.is_empty() && model.name != model.id {
                spans.push(Span::styled(
                    format!(" · {}", model.name),
                    Style::new().fg(theme.gray),
                ));
            }
            if added {
                spans.push(Span::styled(" (已添加)", Style::new().fg(theme.gray)));
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
        title: format!("获取模型 · {provider_id}"),
        body: lines,
        footer: Some(overlay::footer(
            &[
                ("↑↓", "移动"),
                ("space", "勾选"),
                ("ctrl+a", "全选"),
                ("enter", "添加所选"),
                ("f5", "重试"),
                ("esc", "关闭"),
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

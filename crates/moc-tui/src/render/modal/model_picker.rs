//! 模型选择器：锚定触发标签的紧凑浮层，列表几何不随元信息变化。

use super::*;
use crate::modal::{FUSION_ENTRY_SELECTOR, ModelEntry, ModelTarget, filtered_entries};

/// 优先向上展开；空间不足时选择下方。无可见标签时贴输入框右上方。
fn popup_rect(screen: Rect, anchor: Rect) -> Rect {
    let width = if screen.width >= 96 { 86 } else { 56 }.min(screen.width);
    let above = anchor.y.saturating_sub(screen.y).min(screen.height);
    let below = screen.bottom().saturating_sub(anchor.bottom());
    let up = above >= 18 || above >= below;
    let height = 18.min(if up { above } else { below });
    Rect::new(
        anchor
            .right()
            .saturating_sub(width)
            .clamp(screen.x, screen.right().saturating_sub(width)),
        if up {
            anchor.y.saturating_sub(height)
        } else {
            anchor.bottom()
        },
        width,
        height,
    )
}

pub(super) fn draw(frame: &mut Frame, app: &mut App) {
    let screen = frame.area();
    let anchor = if app.model_tag_area.is_empty() {
        Rect::new(
            screen.right().saturating_sub(1),
            screen.bottom().saturating_sub(1),
            1,
            1,
        )
    } else {
        app.model_tag_area
    };
    let rect = popup_rect(screen, anchor);
    if rect.height < 4 {
        return;
    }
    let width = overlay::content_width(rect.width);
    let rows = usize::from(rect.height.saturating_sub(3));
    let modal = app.modal.as_ref().expect("模型浮层已打开");
    let ModalBody {
        lines,
        footer,
        hits,
        level_hits,
        ..
    } = super::modal_body(app, modal, width, rows);
    let footer = footer.unwrap_or_default();
    let theme = app.theme;
    overlay::paint_opaque(
        frame.buffer_mut(),
        rect,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    let panel = overlay::panel(theme).title(overlay::title_span(&modal.title(), width, theme));
    frame.render_widget(panel, rect);
    let content = Rect::new(
        rect.x + 2,
        rect.y + 1,
        rect.width.saturating_sub(4),
        rect.height.saturating_sub(3),
    );
    frame.render_widget(Paragraph::new(lines), content);
    let y = rect.bottom() - 2;
    app.key_hits = footer.hits(content.x, y).collect();
    frame.render_widget(
        Paragraph::new(footer.line),
        Rect::new(content.x, y, content.width, 1),
    );
    let list_width =
        if matches!(modal, Modal::Model { .. }) && width >= META_MIN_CONTENT && rows >= 8 {
            width - META_MAX - CARD_GAP
        } else {
            width
        };
    app.modal_item_hits = hits
        .into_iter()
        .filter(|(_, row)| *row < rows)
        .map(|(index, row)| {
            (
                index,
                Rect::new(content.x, content.y + row as u16, list_width as u16, 1),
            )
        })
        .collect();
    app.modal_level_hits = level_hits
        .into_iter()
        .filter(|(_, row, _, _)| *row < rows)
        .map(|(index, row, x, width)| {
            (
                index,
                Rect::new(content.x + x, content.y + row as u16, width, 1),
            )
        })
        .collect();
    app.modal_card_area = rect;
}

pub(super) fn body(app: &App, modal: &Modal, width: usize, max_lines: usize) -> ModalBody {
    let Modal::Model {
        entries,
        current,
        picker,
        level,
        target,
    } = modal
    else {
        unreachable!()
    };
    let theme = app.theme;
    let filtered = filtered_entries(entries, &picker.query);
    let highlighted = filtered.get(picker.selected).map(|(_, entry)| *entry);
    let resolved = highlighted.and_then(|entry| level.resolve(&entry.levels));
    let wide = width >= META_MIN_CONTENT && max_lines >= 8;
    let list_width = if wide {
        width - META_MAX - CARD_GAP
    } else {
        width
    };
    let detail_width = if wide { META_MAX } else { width };
    let current_id = if crate::modal::parse_fusion(current).is_some() {
        FUSION_ENTRY_SELECTOR
    } else {
        current
            .rsplit_once(':')
            .map_or(current.as_str(), |(id, _)| id)
    };
    let mut body = ModalBody::default();
    let count = format!(
        " {}/{}",
        if filtered.is_empty() {
            0
        } else {
            picker.selected + 1
        },
        filtered.len()
    );
    let mut search = search_line(
        &picker.query,
        width.saturating_sub(str_width(&count)),
        theme,
    );
    search = fit_line(search, width.saturating_sub(str_width(&count)));
    search
        .spans
        .push(Span::styled(count, Style::new().fg(theme.gray)));
    body.lines.push(search);
    let fusion_index = filtered
        .iter()
        .position(|(_, entry)| entry.selector == FUSION_ENTRY_SELECTOR);
    let pinned_fusion = fusion_index.filter(|_| max_lines >= 6);
    if let Some(index) = pinned_fusion {
        body.hits.push((index, body.lines.len()));
        body.lines.push(clip_line(
            overlay::list_row(
                if current_id == FUSION_ENTRY_SELECTOR {
                    "单模型 / Fusion 组合… ✓"
                } else {
                    "单模型 / Fusion 组合…"
                },
                picker.selected == index,
                list_width,
                theme,
            ),
            width,
        ));
    }
    let header_rows = body.lines.len();
    let fusion = highlighted.is_some_and(|entry| entry.selector == FUSION_ENTRY_SELECTOR);
    let mut detail = Vec::new();
    let mut chips = Vec::new();
    let chip_row;
    if fusion {
        let draft = crate::app::initial_fusion_draft(app, current);
        detail.push(Line::from(Span::styled(
            "Fusion · 组合模型",
            Style::new().fg(theme.accent_model),
        )));
        for (label, value) in [("Lead", draft.lead), ("Sidekick", draft.sidekick)] {
            detail.push(Line::from(format!(
                "{label}: {}",
                if value.is_empty() {
                    "未选择".into()
                } else {
                    crate::app::model_selector_label(&value)
                }
            )));
        }
        chip_row = 0;
    } else {
        if wide {
            detail.push(Line::from(Span::styled(
                highlighted
                    .map_or("", |entry| entry.selector.as_str())
                    .to_owned(),
                Style::new().fg(theme.text_secondary),
            )));
        }
        let (levels, level_chips) =
            level_lines(theme, highlighted, resolved.as_deref(), detail_width);
        chip_row = detail.len() + 1;
        chips = level_chips;
        detail.extend(levels);
        if wide {
            detail.extend(model_meta_lines(theme, highlighted, detail_width));
        } else {
            let meta = highlighted
                .map(|entry| {
                    let mut parts = vec![entry.provider().to_owned()];
                    if let Some(window) = entry.context_window {
                        parts.push(format!("{} 上下文", crate::render::format_tokens(window)));
                    }
                    if let Some(output) = entry.max_output_tokens {
                        parts.push(format!(
                            "{} 输出",
                            crate::render::format_tokens(u64::from(output))
                        ));
                    }
                    parts.join(" · ")
                })
                .unwrap_or_default();
            detail.push(Line::from(Span::styled(
                meta,
                Style::new().fg(theme.text_secondary),
            )));
        }
    }
    // 极短窗口保留搜索、一条候选、选中的等级和操作提示。
    let available = max_lines.saturating_sub(header_rows);
    if !wide && available < 5 {
        detail = vec![Line::from(Span::styled(
            format!("思考: {}", resolved.as_deref().unwrap_or("不适用")),
            Style::new().fg(theme.accent_model),
        ))];
        chips.clear();
    }
    let status = if picker.loading {
        Some("正在切换模型…".to_owned())
    } else {
        picker.error.as_ref().map(|error| format!("错误：{error}"))
    };
    let status_rows = usize::from(status.is_some() && available >= 3);
    let detail_rows = if wide {
        0
    } else {
        detail.len().min(available.saturating_sub(1 + status_rows))
    };
    let visible = available.saturating_sub(detail_rows + status_rows).max(1);
    let mut list = Vec::<(Option<usize>, Line<'static>)>::new();
    let mut last_group = String::new();
    for (index, (_, entry)) in filtered.iter().enumerate() {
        if pinned_fusion == Some(index) {
            continue;
        }
        let group = if entry.selector == FUSION_ENTRY_SELECTOR {
            "模式"
        } else if entry.provider().is_empty() {
            "继承"
        } else if app.recent_models.rank(&entry.selector).is_some() {
            "最近使用"
        } else {
            entry.provider()
        };
        if picker.query.trim().is_empty() && group != last_group && visible >= 4 {
            list.push((
                None,
                Line::from(Span::styled(
                    format!("─ {group}"),
                    Style::new().fg(theme.gray),
                )),
            ));
            last_group = group.to_owned();
        }
        list.push((
            Some(index),
            model_row(
                entry,
                index == picker.selected,
                entry.selector == current_id,
                &picker.query,
                list_width,
                theme,
            ),
        ));
    }
    let selected_row = list
        .iter()
        .position(|(index, _)| *index == Some(picker.selected))
        .unwrap_or(0);
    let initial_offset = list
        .iter()
        .position(|(index, _)| *index == Some(picker.offset))
        .unwrap_or(0)
        .saturating_sub(1);
    let offset = window_offset(initial_offset, selected_row, visible, list.len());
    for row in 0..visible {
        let mut line = if let Some((index, line)) = list.get(offset + row) {
            if let Some(index) = index {
                body.hits.push((*index, body.lines.len()));
            }
            fit_line(line.clone(), list_width)
        } else if row == 0 && filtered.is_empty() {
            Line::from(Span::styled(
                fit("没有匹配的模型", list_width),
                Style::new().fg(theme.gray),
            ))
        } else {
            Line::from(" ".repeat(list_width))
        };
        if wide {
            line.spans.push(Span::raw(" ".repeat(CARD_GAP)));
            if let Some(detail_line) = detail.get(row) {
                line.spans
                    .extend(clip_line(detail_line.clone(), detail_width).spans);
            }
        }
        body.lines.push(line);
    }
    let detail_start = if wide { header_rows } else { body.lines.len() };
    if !wide {
        body.lines.extend(
            detail
                .into_iter()
                .take(detail_rows)
                .map(|line| clip_line(line, width)),
        );
    }
    let chips_y = detail_start + chip_row;
    if (wide || detail_rows >= 2) && chips_y < max_lines {
        let dx = if wide {
            (list_width + CARD_GAP) as u16
        } else {
            0
        };
        body.level_hits.extend(
            chips
                .into_iter()
                .map(|(index, x, w)| (index, chips_y, dx + x, w)),
        );
    }
    if status_rows > 0 {
        body.lines.push(Line::from(Span::styled(
            truncate(status.unwrap(), width).into_owned(),
            Style::new().fg(if picker.loading {
                theme.accent_running
            } else {
                theme.accent_error
            }),
        )));
    }
    let enter = if fusion {
        "配置组合"
    } else if picker.error.is_some() {
        "重试"
    } else {
        "确认"
    };
    let back = if matches!(target, ModelTarget::FusionSlot { .. }) {
        "返回"
    } else {
        "关闭"
    };
    body.footer = Some(overlay::footer(
        &[
            ("↑↓", "选择"),
            ("tab", "等级"),
            ("enter", enter),
            ("esc", back),
        ],
        width,
        theme,
    ));
    body.lines.truncate(max_lines);
    body
}

fn model_row(
    entry: &ModelEntry,
    selected: bool,
    current: bool,
    query: &str,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let style = if selected {
        Style::new()
            .fg(theme.accent_user)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text_primary)
    };
    let provider_width = str_width(entry.provider()).min(width / 3);
    let name_width = width.saturating_sub(5 + provider_width);
    let name = fit(entry.name(), name_width);
    let mut spans = vec![Span::styled(if selected { "❯ " } else { "  " }, style)];
    spans.extend(highlight(&name, query, style, theme));
    spans.push(Span::styled(
        format!(
            " {} {}",
            fit(entry.provider(), provider_width),
            if current { "✓" } else { " " }
        ),
        style.fg(theme.text_secondary),
    ));
    fit_line(Line::from(spans), width)
}

/// 多词在可见名称中高亮；连续命中优先，否则标出子序列命中的字符。
fn highlight(text: &str, query: &str, style: Style, theme: &Theme) -> Vec<Span<'static>> {
    let chars: Vec<char> = text.chars().collect();
    let mut marked = vec![false; chars.len()];
    for term in query.to_lowercase().split_whitespace() {
        let needle: Vec<char> = term.chars().collect();
        let lower: Vec<char> = chars
            .iter()
            .map(|ch| ch.to_lowercase().next().unwrap_or(*ch))
            .collect();
        if let Some(start) = lower
            .windows(needle.len())
            .position(|window| window == needle)
        {
            marked[start..start + needle.len()].fill(true);
        } else {
            let mut positions = Vec::new();
            let mut next = 0;
            for ch in &needle {
                if let Some(index) = lower[next..].iter().position(|candidate| candidate == ch) {
                    next += index;
                    positions.push(next);
                    next += 1;
                } else {
                    positions.clear();
                    break;
                }
            }
            for index in positions {
                marked[index] = true;
            }
        }
    }
    chars
        .into_iter()
        .zip(marked)
        .map(|(ch, marked)| {
            Span::styled(
                ch.to_string(),
                if marked {
                    style
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::UNDERLINED)
                } else {
                    style
                },
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{Terminal, backend::TestBackend};

    #[test]
    fn model_picker_anchor_stays_inside_terminal() {
        for (w, h) in [(120, 40), (80, 24), (40, 12), (20, 6)] {
            let screen = Rect::new(0, 0, w, h);
            for anchor in [
                Rect::new(1, h - 1, 4, 1),
                Rect::new(w - 5, h - 1, 4, 1),
                Rect::new(w - 5, 0, 4, 1),
            ] {
                let rect = popup_rect(screen, anchor);
                assert_eq!(rect.intersection(screen), rect);
                assert!(rect.bottom() <= anchor.y || rect.y >= anchor.bottom());
                assert!(rect.height >= 4);
            }
        }
    }

    #[test]
    fn model_picker_preserves_background_and_separates_detail_hits() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let entry = ModelEntry {
            selector: "p/model".into(),
            label: "p/model".into(),
            display_name: "模型示例".into(),
            levels: crate::modal::DEFAULT_LEVELS
                .iter()
                .map(|level| (*level).into())
                .collect(),
            context_window: Some(128_000),
            max_output_tokens: None,
            inputs: Vec::new(),
            pricing: None,
        };
        app.modal = Some(Modal::Model {
            entries: vec![entry],
            current: "p/model:max".into(),
            picker: Default::default(),
            level: Default::default(),
            target: ModelTarget::Session,
        });
        app.model_tag_area = Rect::new(90, 28, 8, 1);
        let mut terminal = Terminal::new(TestBackend::new(110, 30)).unwrap();
        terminal
            .draw(|frame| {
                frame
                    .buffer_mut()
                    .set_string(0, 0, "正文", Style::new().fg(Color::Red));
                super::super::draw_modal(frame, &mut app);
            })
            .unwrap();
        assert_eq!(terminal.backend().buffer()[(0, 0)].symbol(), "正");
        assert_eq!(terminal.backend().buffer()[(0, 0)].fg, Color::Red);
        assert_eq!(app.modal_card_area.bottom(), app.model_tag_area.y);
        let (_, max_hit) = app
            .modal_level_hits
            .iter()
            .find(|(index, _)| *index == 5)
            .expect("max 档位必须可见");
        for (_, hit) in &app.modal_item_hits {
            assert!(hit.intersection(*max_hit).is_empty());
        }
        let before = app.modal_card_area;
        if let Some(Modal::Model { entries, .. }) = &mut app.modal {
            entries[0].context_window = None;
        }
        terminal
            .draw(|frame| super::super::draw_modal(frame, &mut app))
            .unwrap();
        assert_eq!(app.modal_card_area, before, "元信息变化不能移动浮层");
    }
}

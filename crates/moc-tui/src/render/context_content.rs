//! 统一上下文面板：首屏紧凑总览，目录与正文按需进入，返回保留位置。
use super::{
    labels::KeyHit,
    overlay,
    wrap::{str_width, wrap_text},
};
use crate::{
    app::App,
    modal::{ContextContentState, ContextLayout, ContextScrollbar, Modal},
    theme::Theme,
};
use ratatui::{
    Frame,
    layout::Rect,
    style::{Modifier, Style},
    text::{Line, Span},
    widgets::Paragraph,
};

fn modal_area(area: Rect, detail: bool, rows: usize) -> Rect {
    let width = if detail {
        (area.width / 4 * 3).clamp(24, 100)
    } else {
        64
    };
    let height = if detail {
        (area.height / 5 * 4).clamp(10, 40)
    } else {
        u16::try_from(rows).expect("总览分类不超过六项") + 10
    };
    let width = width.min(area.width.saturating_sub(2));
    let height = height.min(area.height.saturating_sub(2));
    Rect::new(
        area.x + (area.width - width) / 2,
        area.y + (area.height - height) / 2,
        width,
        height,
    )
}

pub fn draw(frame: &mut Frame, app: &mut App, area: Rect) {
    let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
        return;
    };
    let theme = app.theme;
    let area = modal_area(area, state.current.is_some(), state.count());
    app.modal_card_area = area;
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    let title = state
        .current
        .as_ref()
        .map_or("上下文", |node| node.name.as_str());
    let panel = overlay::panel(theme).title(overlay::title_span(
        title,
        usize::from(area.width.saturating_sub(4)),
        theme,
    ));
    let inner = panel.inner(area);
    frame.render_widget(panel, area);
    let status = if state.stale {
        "有更新 · r 刷新"
    } else {
        ""
    };
    let source = state
        .current
        .as_ref()
        .and_then(|node| node.source.as_deref())
        .unwrap_or("");
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled(
                super::wrap::truncate(
                    source,
                    usize::from(inner.width).saturating_sub(str_width(status) + 2),
                )
                .into_owned(),
                Style::new().fg(theme.gray),
            ),
            Span::styled(format!("  {status}"), Style::new().fg(theme.accent_running)),
        ])),
        Rect { height: 1, ..inner },
    );
    let body = Rect {
        y: inner.y.saturating_add(1),
        width: inner.width.saturating_sub(1),
        height: inner.height.saturating_sub(3),
        ..inner
    };
    let width = usize::from(body.width);
    if state
        .layout
        .as_ref()
        .is_none_or(|layout| layout.width != width)
    {
        state.layout = Some(build_layout(state, width, theme));
    }
    state.viewport = usize::from(body.height);
    let layout = state.layout.as_ref().expect("已构造布局");
    let max_offset = layout.max_offset(state.viewport);
    state.scroll = state.scroll.min(max_offset);
    let end = (state.scroll + state.viewport).min(layout.lines.len());
    frame.render_widget(
        Paragraph::new(layout.lines[state.scroll..end].to_vec()),
        body,
    );
    if let Some(span) = state.focus.and_then(|index| layout.spans.get(index))
        && let Some(offset) = span.start.checked_sub(state.scroll)
        && offset < state.viewport
    {
        for column in body.x..body.x + body.width {
            if let Some(cell) = frame.buffer_mut().cell_mut((
                column,
                body.y + u16::try_from(offset).expect("偏移小于终端视口"),
            )) {
                cell.set_style(cell.style().bg(theme.bg_visual));
            }
        }
    }
    let total = layout.lines.len().max(1);
    state.scrollbar = ContextScrollbar {
        track: Rect {
            x: body.x + body.width,
            width: 1,
            ..body
        },
        thumb: u16::try_from(
            (state.viewport * state.viewport / total).clamp(1, state.viewport.max(1)),
        )
        .expect("滑块不超过终端视口"),
        max_offset,
    };
    if max_offset > 0 {
        overlay::paint_scrollbar(
            frame.buffer_mut(),
            state.scrollbar.track,
            state.scrollbar.thumb_top(state.scroll),
            state.scrollbar.thumb,
            theme,
        );
    }
    app.key_hits = draw_footer(frame, inner, state, theme);
}

fn draw_footer(
    frame: &mut Frame,
    inner: Rect,
    state: &ContextContentState,
    theme: &Theme,
) -> Vec<KeyHit> {
    let mut keys = Vec::new();
    if let Some(node) = &state.current {
        if state
            .pages
            .get(&node.id)
            .is_some_and(|page| page.next_cursor.is_some())
        {
            keys.push(("n", "下页"));
        }
        if state
            .previous
            .get(&node.id)
            .is_some_and(|pages| !pages.is_empty())
        {
            keys.push(("p", "上页"));
        }
        if state.is_body() {
            keys.extend([
                ("v", if state.raw { "阅读" } else { "原文" }),
                ("y", "复制"),
            ]);
        }
    }
    if !state.is_body() {
        keys.push(("enter", "查看"));
    }
    keys.extend([
        ("r", "刷新"),
        (
            "esc",
            if state.current.is_some() {
                "返回"
            } else {
                "关闭"
            },
        ),
    ]);
    let footer = overlay::footer(&keys, usize::from(inner.width), theme);
    let row = Rect {
        y: inner.y + inner.height.saturating_sub(1),
        height: 1,
        ..inner
    };
    let hits = footer.hits(row.x, row.y).collect();
    frame.render_widget(Paragraph::new(footer.line), row);
    hits
}

fn build_layout(state: &ContextContentState, width: usize, theme: &Theme) -> ContextLayout {
    let mut lines = Vec::new();
    let mut spans = Vec::new();
    if let Some(error) = &state.error {
        lines.extend(
            wrap_text(error, width)
                .into_iter()
                .map(|line| Line::styled(line, Style::new().fg(theme.accent_error))),
        );
    } else if state.content.is_none() {
        lines.push(Line::raw("正在读取上下文…"));
    } else if let Some(node) = &state.current {
        if let Some(error) = state.node_errors.get(&node.id) {
            lines.extend(
                wrap_text(&format!("{error} · n 重试"), width)
                    .into_iter()
                    .map(Line::raw),
            );
        } else if state.pending.contains(&node.id) {
            lines.push(Line::raw("正在读取…"));
        } else if let Some(page) = state.pages.get(&node.id) {
            if let Some(text) = &page.text {
                if state.raw {
                    for line in text.split('\n') {
                        lines.extend(wrap_text(line, width).into_iter().map(Line::raw));
                    }
                } else {
                    lines = super::markdown::render_markdown(text, width, theme);
                }
            } else {
                for row in &page.children {
                    let start = lines.len();
                    let marker = if row.hidden.is_some() { "  " } else { "› " };
                    let tokens = super::format_tokens(row.tokens);
                    let name = super::wrap::truncate(
                        &row.name,
                        width.saturating_sub(str_width(&tokens) + 4),
                    );
                    let pad = width.saturating_sub(str_width(&name) + str_width(&tokens) + 2);
                    lines.push(Line::from(vec![
                        Span::styled(
                            format!("{marker}{name}"),
                            Style::new()
                                .fg(theme.text_primary)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!("{}{tokens}", " ".repeat(pad)),
                            Style::new().fg(theme.gray),
                        ),
                    ]));
                    if let Some(note) = row.hidden.as_deref().or(row.source.as_deref()) {
                        lines.push(Line::styled(
                            super::wrap::truncate(note, width).into_owned(),
                            Style::new().fg(theme.gray),
                        ));
                    }
                    spans.push(start..lines.len());
                    lines.push(Line::default());
                }
                if page.children.is_empty() {
                    lines.push(Line::raw("暂无内容"));
                }
            }
        }
    } else if let Some(content) = &state.content {
        let usage = moc_protocol::methods::ContextUsageResult {
            context_window: content.context_window,
            total_tokens: content.total_tokens,
            sections: content
                .sections
                .iter()
                .map(|node| moc_protocol::methods::ContextUsageSection {
                    name: node.name.clone(),
                    tokens: node.tokens,
                })
                .collect(),
        };
        lines = super::modal::context_lines(&usage, width, theme);
        spans = (0..content.sections.len())
            .map(|index| index + 2..index + 3)
            .collect();
    }
    ContextLayout {
        width,
        lines,
        spans,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::{ContextContentResult, ContextContentSection, ContextReadResult};

    #[test]
    fn hidden_rows_cannot_open_and_return_restores_directory_position() {
        let mut state = ContextContentState::default();
        state.set_content(ContextContentResult {
            sections: vec![ContextContentSection {
                id: "root".into(),
                name: "messages".into(),
                kind: "category".into(),
                children: 2,
                ..Default::default()
            }],
            ..Default::default()
        });
        state.enter(0);
        state.apply_page(ContextReadResult {
            node_id: "root".into(),
            children: vec![
                ContextContentSection {
                    id: "hidden".into(),
                    hidden: Some("工具输出已隐藏".into()),
                    ..Default::default()
                },
                ContextContentSection {
                    id: "visible".into(),
                    kind: "markdown".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        });
        state.loads.clear();
        state.enter(0);
        assert_eq!(state.current.as_ref().unwrap().id, "root");
        assert!(state.loads.is_empty());
        state.scroll = 12;
        state.enter(1);
        state.apply_page(ContextReadResult {
            node_id: "visible".into(),
            text: Some("行\n".repeat(200)),
            ..Default::default()
        });
        let layout = build_layout(&state, 60, Theme::global());
        assert!(layout.max_offset(20) > 0);
        assert!(state.back());
        assert_eq!(state.scroll, 12);
        assert_eq!(state.focus, Some(1));
        assert_eq!(state.current.as_ref().unwrap().id, "root");
    }
}

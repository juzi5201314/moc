//! 旁路 dock：宽屏右侧分栏，窄屏底部保留主转写，不占用 Modal。

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::Paragraph,
};

use super::labels::{self, SEP};
use super::overlay;
use super::wrap::{truncate, wrap_text};

use crate::{
    app::{App, Focus},
    btw::Status,
};

/// 从主滚动区划出 dock；收起时留一行可点击状态条。
pub fn split(app: &mut App, area: Rect) -> Rect {
    app.btw.area = Rect::default();
    // 空线程收起后不占行：它只是「面板曾打开过」，不代表有可回顾的内容。
    let exists = app.session.as_ref().is_some_and(|session| {
        app.btw.threads.get(&session.id).is_some_and(|thread| {
            app.btw.expanded
                || !thread.turns.is_empty()
                || app
                    .btw
                    .flight
                    .as_ref()
                    .is_some_and(|flight| flight.session_id == session.id)
        })
    });
    if !exists {
        return area;
    }
    let (main, dock) = if !app.btw.expanded {
        let [main, dock] =
            Layout::vertical([Constraint::Min(2), Constraint::Length(1)]).areas(area);
        (main, dock)
    } else if area.width >= 104 {
        let [main, dock] =
            Layout::horizontal([Constraint::Percentage(58), Constraint::Percentage(42)])
                .areas(area);
        (main, dock)
    } else {
        let height = (area.height / 2).clamp(1, 16);
        let [main, dock] =
            Layout::vertical([Constraint::Min(2), Constraint::Length(height)]).areas(area);
        (main, dock)
    };
    app.btw.area = dock;
    main
}

pub fn draw(frame: &mut Frame, app: &mut App) {
    let area = app.btw.area;
    app.btw.key_hits.clear();
    if area.is_empty() {
        return;
    }
    let Some(session_id) = app.session.as_ref().map(|session| session.id.as_str()) else {
        return;
    };
    let Some(thread) = app.btw.threads.get_mut(session_id) else {
        return;
    };
    let theme = app.theme;
    let status = thread
        .turns
        .back()
        .map_or("空线程", |turn| turn.status.label());
    let turns = thread.turns.len();
    let focused = app.focus == Focus::Btw && app.modal.is_none();
    let border = if focused {
        theme.border_active
    } else {
        theme.border
    };
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_base).fg(theme.text_primary),
    );
    if !app.btw.expanded {
        let head = format!("▸ BTW{SEP}{status}{SEP}{turns} 轮{SEP}");
        let hints = labels::hints(
            &[("", "点击展开"), ("alt+b", "展开")],
            usize::from(area.width).saturating_sub(super::wrap::str_width(&head)),
        );
        frame.render_widget(
            Paragraph::new(truncate(head + &hints, usize::from(area.width)).into_owned())
                .style(Style::new().bg(theme.bg_base).fg(theme.text_secondary)),
            area,
        );
        return;
    }
    let panel = overlay::panel(theme).border_style(Style::new().fg(border));
    let inner = panel.inner(area);
    let width = usize::from(inner.width);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if thread.turns.is_empty() {
        lines.extend(
            wrap_text(
                "在主输入使用 /btw 问题开始；后续 /btw 问题沿用本线程。仅内存，不调用工具，不写主上下文或历史。",
                width,
            )
            .into_iter()
            .map(|text| Line::styled(text, Style::new().fg(theme.text_secondary))),
        );
    }
    for turn in &thread.turns {
        lines.extend(
            wrap_text(&format!("你 › {}", turn.question), width)
                .into_iter()
                .map(|text| Line::styled(text, Style::new().fg(theme.text_primary))),
        );
        if !turn.reasoning.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("◆ ", Style::new().fg(theme.accent_thinking)),
                Span::styled(
                    labels::title::THOUGHT,
                    Style::new()
                        .fg(theme.gray_bright)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
            ]));
            let inner_width = width.saturating_sub(2).max(1);
            for row in wrap_text(&turn.reasoning, inner_width) {
                lines.push(Line::from(vec![
                    Span::styled("▏ ", Style::new().fg(theme.accent_thinking)),
                    Span::styled(row, Style::new().fg(theme.gray)),
                ]));
            }
        }
        lines.extend(super::markdown::render_markdown(&turn.answer, width, theme));
        let mut footer = format!(
            "{} · 回放 {} 轮",
            turn.status.label(),
            turn.history_turns_used
        );
        if !turn.model.is_empty() {
            footer.push_str(&format!(" · {}", turn.model));
        }
        if let Some(usage) = &turn.usage {
            footer.push_str(&format!(
                " · 输入 {} / 输出 {} token",
                usage.input_tokens, usage.output_tokens
            ));
        }
        lines.extend(
            wrap_text(&footer, width)
                .into_iter()
                .map(|text| Line::styled(text, Style::new().fg(theme.text_secondary))),
        );
        if let Status::Failed(error) = &turn.status {
            lines.extend(
                wrap_text(error, width)
                    .into_iter()
                    .map(|text| Line::styled(text, Style::new().fg(theme.text_secondary))),
            );
        }
        lines.push(Line::default());
    }

    let inner_height = usize::from(inner.height);
    app.btw.scroll_max = lines.len().saturating_sub(inner_height);
    thread.scroll = thread.scroll.min(app.btw.scroll_max);

    // `scroll` 是距底部的行数；百分比按「已读到哪」从顶部算，到底为 100%。
    let mut title = format!("BTW{SEP}{status}{SEP}{turns} 轮");
    if let Some(percent) =
        ((app.btw.scroll_max - thread.scroll) * 100).checked_div(app.btw.scroll_max)
    {
        title.push_str(&format!("{SEP}{percent}%"));
    }
    let edge = usize::from(area.width.saturating_sub(4));
    let (footer, footer_keys) = labels::hint_layout(
        &[
            ("x", "清空"),
            ("c", "复制"),
            ("ctrl+c", "取消"),
            ("tab", "主输入"),
            ("esc", "收起"),
        ],
        edge.saturating_sub(2),
    );
    // 底边标题从左角之后起排、前面补一格空白：键位从 `area.x + 2` 开始。
    let footer_y = area.bottom().saturating_sub(1);
    let key_hits: Vec<_> = footer_keys
        .into_iter()
        .map(|key| key.at(area.x + 2, footer_y))
        .collect();
    let block = panel
        .title(overlay::title_span(&title, edge, theme))
        .title_bottom(Line::styled(
            format!(" {footer} "),
            Style::new().fg(theme.text_secondary),
        ));
    frame.render_widget(block, area);
    app.btw.key_hits = key_hits;
    if inner.is_empty() {
        return;
    }

    let start = app.btw.scroll_max.saturating_sub(thread.scroll);
    // 按 usize 裁出视口，不用 Paragraph 的 u16 scroll，长回答也可浏览全文。
    let visible: Vec<_> = lines
        .into_iter()
        .skip(start)
        .take(usize::from(inner.height))
        .collect();
    frame.render_widget(
        Paragraph::new(visible).style(Style::new().bg(theme.bg_base).fg(theme.text_primary)),
        inner,
    );
}

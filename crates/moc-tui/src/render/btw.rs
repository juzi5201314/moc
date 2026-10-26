//! 旁路 dock：宽屏右侧分栏，窄屏底部保留主转写，不占用 Modal。

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::Style,
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph},
};

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
    let title = format!(" BTW · {status} · {} 轮 ", thread.turns.len());
    let focused = app.focus == Focus::Btw && app.modal.is_none();
    let border = if focused {
        theme.border_active
    } else {
        theme.border
    };
    frame.render_widget(Clear, area);
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            if let Some(cell) = frame.buffer_mut().cell_mut((x, y)) {
                cell.set_char(' ');
                cell.set_style(Style::new().bg(theme.bg_base).fg(theme.text_primary));
            }
        }
    }
    if !app.btw.expanded {
        frame.render_widget(
            Paragraph::new(format!("▸{title} Alt+B / 点击展开"))
                .style(Style::new().bg(theme.bg_base).fg(theme.text_secondary)),
            area,
        );
        return;
    }
    let width = usize::from(area.width.saturating_sub(2));
    let mut lines: Vec<Line<'static>> = Vec::new();
    if thread.turns.is_empty() {
        lines.extend(super::wrap::wrap_text("在主输入使用 /btw 问题开始；后续 /btw 问题沿用本线程。仅内存，不调用工具，不写主上下文或历史。", width)
            .into_iter().map(|text| Line::styled(text, Style::new().fg(theme.text_secondary))));
    }
    for turn in &thread.turns {
        lines.extend(
            super::wrap::wrap_text(&format!("你 › {}", turn.question), width)
                .into_iter()
                .map(|text| Line::styled(text, Style::new().fg(theme.text_primary))),
        );
        if !turn.reasoning.is_empty() {
            lines.push(Line::from(vec![
                Span::styled("◆ ", Style::new().fg(theme.accent_thinking)),
                Span::styled(
                    "思考",
                    Style::new()
                        .fg(theme.gray_bright)
                        .add_modifier(ratatui::style::Modifier::BOLD),
                ),
            ]));
            let inner_width = width.saturating_sub(2).max(1);
            for row in super::wrap::wrap_text(&turn.reasoning, inner_width) {
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
            super::wrap::wrap_text(&footer, width)
                .into_iter()
                .map(|text| Line::styled(text, Style::new().fg(theme.text_secondary))),
        );
        if let Status::Failed(error) = &turn.status {
            lines.extend(
                super::wrap::wrap_text(error, width)
                    .into_iter()
                    .map(|text| Line::styled(text, Style::new().fg(theme.text_secondary))),
            );
        }
        lines.push(Line::default());
    }

    let inner_height = usize::from(area.height.saturating_sub(2));
    app.btw.scroll_max = lines.len().saturating_sub(inner_height);
    thread.scroll = thread.scroll.min(app.btw.scroll_max);

    let progress_suffix = if app.btw.scroll_max > 0 {
        let percent = (thread.scroll * 100) / app.btw.scroll_max;
        format!(" [{percent}%]")
    } else {
        String::new()
    };
    let title = format!(" BTW · {status} · {} 轮{progress_suffix} ", thread.turns.len());

    let footer_text = if area.width >= 56 {
        " Tab 主输入 · Esc 收起 · Ctrl+C 取消 · c 复制 · x 清空 "
    } else if area.width >= 38 {
        " Tab 输入 · Esc 收起 · c 复制 · x 清空 "
    } else {
        " Tab/Esc 切换 · c 复制 "
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(ratatui::symbols::border::ROUNDED)
        .border_style(Style::new().fg(border))
        .style(Style::new().bg(theme.bg_base).fg(theme.text_primary))
        .title(Line::styled(
            title,
            Style::new().bg(theme.bg_base).fg(theme.text_primary),
        ))
        .title_bottom(Line::styled(
            footer_text,
            Style::new().bg(theme.bg_base).fg(theme.text_secondary),
        ));
    let inner = block.inner(area);
    frame.render_widget(block, area);
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

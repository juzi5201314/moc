//! TUI 子代理窄侧栏绘制。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph, Wrap};

use crate::app::App;

pub fn draw_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    app.subagent_area = area;
    app.subagent_hits.clear();
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let mut lines = Vec::with_capacity(app.subagents.len().saturating_mul(3).saturating_add(1));
    lines.push(Line::from(Span::styled(
        "子代理",
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )));
    for (agent_id, view) in &app.subagents {
        let selected = app.subagent_selected.as_deref() == Some(agent_id.as_str());
        let status = view.status_label();
        let style = if selected {
            Style::new().fg(theme.text_primary).bg(theme.bg_visual)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        lines.push(Line::from(Span::styled(
            format!("{} {}", if view.running { "●" } else { "○" }, status),
            style,
        )));
        lines.push(Line::from(Span::styled(
            crate::render::scrollback::truncate_to_width(
                view.info.title.clone(),
                usize::from(area.width.saturating_sub(2)),
            ),
            Style::new().fg(theme.text_primary),
        )));
        let model = view
            .info
            .model
            .rsplit('/')
            .next()
            .unwrap_or(&view.info.model);
        lines.push(Line::from(Span::styled(
            crate::render::scrollback::truncate_to_width(
                model.to_owned(),
                usize::from(area.width.saturating_sub(2)),
            ),
            Style::new().fg(theme.gray_dim),
        )));
        if let Some(message) = &view.info.last_message {
            lines.push(Line::from(Span::styled(
                crate::render::scrollback::truncate_to_width(
                    message.clone(),
                    usize::from(area.width.saturating_sub(2)),
                ),
                Style::new().fg(theme.gray_dim),
            )));
        }
        app.subagent_hits.push((
            agent_id.clone(),
            Rect::new(
                area.x,
                area.y + u16::try_from(lines.len().saturating_sub(4)).unwrap_or(u16::MAX),
                area.width,
                1,
            ),
        ));
    }
    let block = Block::default()
        .borders(Borders::LEFT)
        .border_style(Style::new().fg(theme.border));
    frame.render_widget(
        Paragraph::new(lines).block(block).wrap(Wrap { trim: true }),
        area,
    );
}

pub fn draw_detail(frame: &mut Frame, app: &App, area: Rect, agent_id: &str, scroll_offset: usize) {
    let Some(view) = app.subagents.get(agent_id) else {
        return;
    };
    let theme = app.theme;
    let mut lines = vec![Line::from(vec![
        Span::styled(
            view.info.title.clone(),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} · {}", view.status_label(), view.info.model),
            Style::new().fg(theme.gray_dim),
        ),
    ])];
    for entry in view
        .blocks
        .iter()
        .skip(scroll_offset)
        .take(usize::from(area.height.saturating_sub(2)))
    {
        let text = match &entry.block {
            crate::transcript::Block::User { text }
            | crate::transcript::Block::Assistant { text }
            | crate::transcript::Block::Thinking { text, .. }
            | crate::transcript::Block::Note { text, .. } => text.clone(),
            crate::transcript::Block::Tool(tool) => format!(
                "{} · {}",
                tool.name,
                tool.result.as_deref().unwrap_or("运行中")
            ),
            crate::transcript::Block::Todo { .. } => "Todo 已更新".into(),
        };
        lines.push(Line::from(text));
    }
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: false }), area);
}

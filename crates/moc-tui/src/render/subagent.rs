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
    let many_agents = app.subagents.len() > 3;

    if !many_agents {
        app.subagent_scroll = 0;
        let mut lines = Vec::with_capacity(app.subagents.len().saturating_mul(3).saturating_add(1));
        lines.push(Line::from(Span::styled(
            "子代理",
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        )));
        for (agent_id, view) in &app.subagents {
            let selected = app.subagent_selected.as_deref() == Some(agent_id.as_str());
            let focused = app.focused.is_some()
                && app
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == *agent_id);
            let block_top = lines.len();
            let status = view.status_label();
            let style = if selected {
                Style::new().fg(theme.text_primary).bg(theme.bg_visual)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "{}{} {}",
                    if focused { "▸ " } else { "" },
                    if view.running { "●" } else { "○" },
                    status
                ),
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
            // 命中区覆盖本代理这一块的全部可见行：块高随 last_message 有无变化，
            // 不能按固定行数折算，否则会落到相邻代理（或表头）上。
            let bottom = area.bottom();
            let top = area
                .y
                .saturating_add(u16::try_from(block_top).unwrap_or(u16::MAX))
                .min(bottom.saturating_sub(1));
            let rows = u16::try_from(lines.len().saturating_sub(block_top)).unwrap_or(u16::MAX);
            app.subagent_hits.push((
                agent_id.clone(),
                Rect::new(
                    area.x,
                    top,
                    area.width,
                    rows.min(bottom.saturating_sub(top)).max(1),
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
    } else {
        // > 3 个子代理：加入视觉分割线与纵向滚动指示
        let mut raw_blocks: Vec<(String, Vec<Line<'static>>)> = Vec::new();
        let sep_width = usize::from(area.width.saturating_sub(2));
        for (i, (agent_id, view)) in app.subagents.iter().enumerate() {
            let selected = app.subagent_selected.as_deref() == Some(agent_id.as_str());
            let focused = app.focused.is_some()
                && app
                    .session
                    .as_ref()
                    .is_some_and(|session| session.id == *agent_id);
            let status = view.status_label();
            let style = if selected {
                Style::new().fg(theme.text_primary).bg(theme.bg_visual)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            let mut block_lines = Vec::new();
            if i > 0 {
                block_lines.push(Line::from(Span::styled(
                    "─".repeat(sep_width),
                    Style::new().fg(theme.border),
                )));
            }
            block_lines.push(Line::from(Span::styled(
                format!(
                    "{}{} {}",
                    if focused { "▸ " } else { "" },
                    if view.running { "●" } else { "○" },
                    status
                ),
                style,
            )));
            block_lines.push(Line::from(Span::styled(
                crate::render::scrollback::truncate_to_width(view.info.title.clone(), sep_width),
                Style::new().fg(theme.text_primary),
            )));
            let model = view
                .info
                .model
                .rsplit('/')
                .next()
                .unwrap_or(&view.info.model);
            block_lines.push(Line::from(Span::styled(
                crate::render::scrollback::truncate_to_width(model.to_owned(), sep_width),
                Style::new().fg(theme.gray_dim),
            )));
            if let Some(message) = &view.info.last_message {
                block_lines.push(Line::from(Span::styled(
                    crate::render::scrollback::truncate_to_width(message.clone(), sep_width),
                    Style::new().fg(theme.gray_dim),
                )));
            }
            raw_blocks.push((agent_id.clone(), block_lines));
        }

        let total_content_lines: usize = raw_blocks.iter().map(|(_, b)| b.len()).sum();
        let visible_height = usize::from(area.height.saturating_sub(1)); // 扣除表头 1 行
        let max_scroll = total_content_lines.saturating_sub(visible_height);
        app.subagent_scroll = app.subagent_scroll.min(max_scroll);

        let mut lines = Vec::new();
        let has_scroll_up = app.subagent_scroll > 0;
        let has_scroll_down = app.subagent_scroll < max_scroll;

        let title_span = if has_scroll_up {
            Span::styled(
                "子代理 ▲",
                Style::new()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                "子代理",
                Style::new()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            )
        };
        lines.push(Line::from(title_span));

        let mut flat_lines: Vec<(Option<String>, Line<'static>)> = Vec::new();
        for (agent_id, blk) in raw_blocks {
            for (idx, line) in blk.into_iter().enumerate() {
                let owner = if idx == 0 && line.spans.iter().any(|s| s.content.starts_with('─')) {
                    None
                } else {
                    Some(agent_id.clone())
                };
                flat_lines.push((owner, line));
            }
        }

        let skipped = flat_lines
            .into_iter()
            .skip(app.subagent_scroll)
            .take(visible_height);
        let bottom = area.bottom();
        for (line_idx, (owner, line)) in skipped.enumerate() {
            let row_y = area
                .y
                .saturating_add(1 + u16::try_from(line_idx).unwrap_or(u16::MAX));
            if row_y < bottom {
                if let Some(agent_id) = owner {
                    if let Some((_, rect)) =
                        app.subagent_hits.iter_mut().find(|(id, _)| *id == agent_id)
                    {
                        rect.height = rect
                            .height
                            .saturating_add(1)
                            .min(bottom.saturating_sub(rect.y));
                    } else {
                        app.subagent_hits
                            .push((agent_id, Rect::new(area.x, row_y, area.width, 1)));
                    }
                }
            }
            lines.push(line);
        }

        if has_scroll_down && lines.len() > 1 {
            let last_idx = lines.len() - 1;
            lines[last_idx] = Line::from(Span::styled(
                "▼ 向下滚动查看更多",
                Style::new().fg(theme.gray_dim),
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::{AgentInfo, AgentStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn agent(id: &str, last_message: Option<&str>) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            parent_id: "ses_parent".into(),
            title: format!("任务 {id}"),
            model: "test/model".into(),
            cwd: "/tmp".into(),
            status: AgentStatus::Running,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_message: last_message.map(str::to_owned),
        }
    }

    /// 小5：命中区如实覆盖每个代理的整块行——缺少 last_message 的代理块少一行，
    /// 按固定行数折算会把点击落到相邻代理或表头上。
    #[test]
    fn hit_rows_follow_each_agent_block() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.subagents.insert(
            "a1".into(),
            crate::subagent::SubagentView::new(agent("a1", None)),
        );
        app.subagents.insert(
            "a2".into(),
            crate::subagent::SubagentView::new(agent("a2", Some("正在读取文件"))),
        );

        let area = Rect::new(60, 4, 30, 12);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("构造测试终端");
        terminal
            .draw(|frame| draw_sidebar(frame, &mut app, area))
            .expect("绘制侧栏");

        let hit = |agent: &str| {
            app.subagent_hits
                .iter()
                .find(|(id, _)| id == agent)
                .map(|(_, rect)| *rect)
                .expect("每个代理都有命中区")
        };
        // 表头 1 行 + a1 三行（无 last_message）+ a2 四行
        assert_eq!(hit("a1"), Rect::new(area.x, area.y + 1, area.width, 3));
        assert_eq!(hit("a2"), Rect::new(area.x, area.y + 4, area.width, 4));
        assert!(
            !hit("a1").contains((area.x + 1, area.y).into()),
            "表头不属于任何代理"
        );
        assert!(
            hit("a2").contains((area.x + 1, area.y + 7).into()),
            "末行（last_message）仍命中该代理"
        );
    }
}

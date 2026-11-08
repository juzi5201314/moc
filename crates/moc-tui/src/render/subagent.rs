//! TUI 子代理侧栏绘制：停靠在转写区右侧，显隐由 `App::subagent_sidebar` 决定。

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use crate::app::App;
use crate::render::wrap::truncate;

/// 侧栏卡片的状态符号与颜色。
fn status_glyph(
    status: moc_protocol::methods::AgentStatus,
    theme: &crate::theme::Theme,
) -> (&'static str, ratatui::style::Color) {
    use moc_protocol::methods::AgentStatus;
    match status {
        AgentStatus::Queued | AgentStatus::Running => ("●", theme.accent_running),
        AgentStatus::Completed | AgentStatus::Idle => ("✓", theme.accent_success),
        AgentStatus::Failed => ("✘", theme.accent_error),
        AgentStatus::Stopped => ("■", theme.gray),
    }
}

pub fn draw_sidebar(frame: &mut Frame, app: &mut App, area: Rect) {
    app.subagent_area = area;
    app.subagent_hits.clear();
    app.subagent_close_area = Rect::default();
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let buffer = frame.buffer_mut();

    // 左边框 + 一格内边距，内容从 x+2 起。
    for y in area.y..area.bottom() {
        buffer[(area.x, y)]
            .set_char('│')
            .set_style(Style::new().fg(theme.border));
    }
    let content = Rect::new(
        area.x + 2,
        area.y,
        area.width.saturating_sub(2),
        area.height,
    );
    if content.is_empty() {
        return;
    }
    let cw = usize::from(content.width);

    // 标题行：`子代理 N` + 运行中计数，右端可点击的 ×；向上滚动时标 ▲。
    let running = app.subagents.values().filter(|view| view.running).count();
    let mut title = vec![
        Span::styled(
            "子代理",
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" {}", app.subagents.len()),
            Style::new().fg(theme.gray),
        ),
    ];
    if running > 0 {
        title.push(Span::styled(
            format!(" · {running} running"),
            Style::new().fg(theme.accent_running),
        ));
    }
    if app.subagent_scroll > 0 {
        title.push(Span::styled(" ▲", Style::new().fg(theme.gray_dim)));
    }
    let used: usize = title.iter().map(Span::width).sum();
    title.push(Span::raw(" ".repeat(cw.saturating_sub(used + 1))));
    title.push(Span::styled("×", Style::new().fg(theme.gray)));
    buffer.set_line(content.x, content.y, &Line::from(title), content.width);
    app.subagent_close_area = Rect::new(content.x + content.width - 1, content.y, 1, 1);

    // 扁平行：每张卡片 = 状态行 + 标题 + 模型 +（可选）last_message；卡片间一行 ─ 分隔。
    let truncate_to = |text: String| truncate(text, cw).into_owned();
    let mut flat: Vec<(Option<String>, Line<'static>)> = Vec::new();
    for (index, (agent_id, view)) in app.subagents.iter().enumerate() {
        if index > 0 {
            flat.push((
                None,
                Line::from(Span::styled("─".repeat(cw), Style::new().fg(theme.border))),
            ));
        }
        let owner = || Some(agent_id.clone());
        let focused = app.focused.is_some()
            && app
                .session
                .as_ref()
                .is_some_and(|session| session.id == *agent_id);
        let (symbol, color) = status_glyph(view.info.status, theme);
        flat.push((
            owner(),
            Line::from(vec![
                if focused {
                    Span::raw("▸ ")
                } else {
                    Span::raw("")
                },
                Span::styled(
                    format!("{symbol} {}", view.status_label()),
                    Style::new().fg(color),
                ),
            ]),
        ));
        flat.push((
            owner(),
            Line::from(Span::styled(
                truncate_to(view.info.title.clone()),
                Style::new()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            )),
        ));
        let model = view
            .info
            .model
            .rsplit('/')
            .next()
            .unwrap_or(&view.info.model);
        flat.push((
            owner(),
            Line::from(Span::styled(
                truncate_to(model.to_owned()),
                Style::new().fg(theme.gray_dim),
            )),
        ));
        if let Some(message) = &view.info.last_message {
            flat.push((
                owner(),
                Line::from(Span::styled(
                    truncate_to(message.clone()),
                    Style::new().fg(theme.gray_bright),
                )),
            ));
        }
    }

    // 标题行 + 空一行后开始正文；放不下就滚动，底行让给 ▼ 指示。
    let body_y = content.y + 2;
    let body_h = usize::from(content.height.saturating_sub(2));
    let max_scroll = flat.len().saturating_sub(body_h);
    app.subagent_scroll = app.subagent_scroll.min(max_scroll);
    let skip = app.subagent_scroll;
    let selected = app.subagent_selected.as_deref();
    for (row, (owner, line)) in flat.iter().skip(skip).take(body_h).enumerate() {
        let y = body_y + u16::try_from(row).unwrap_or(u16::MAX);
        if y >= content.bottom() {
            break;
        }
        let has_more_below = skip + body_h < flat.len();
        if has_more_below && row + 1 == body_h {
            buffer.set_line(
                content.x,
                y,
                &Line::from(Span::styled("▼", Style::new().fg(theme.gray_dim))),
                content.width,
            );
            break;
        }
        if let Some(id) = owner {
            if selected == Some(id.as_str()) {
                // 选中卡片整行铺高亮（铺满内容宽度）。
                buffer.set_style(
                    Rect::new(content.x, y, content.width, 1),
                    Style::new().bg(theme.bg_visual),
                );
            }
            // 命中区逐可见行累积：同一代理的行是连续的，命中区跟着长。
            let bottom = area.bottom();
            if let Some((_, rect)) = app.subagent_hits.iter_mut().find(|(hit, _)| hit == id) {
                rect.height = rect
                    .height
                    .saturating_add(1)
                    .min(bottom.saturating_sub(rect.y));
            } else {
                app.subagent_hits
                    .push((id.clone(), Rect::new(area.x, y, area.width, 1)));
            }
        }
        buffer.set_line(content.x, y, line, content.width);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::{AgentInfo, AgentStatus};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn agent(id: &str, status: AgentStatus, last_message: Option<&str>) -> AgentInfo {
        AgentInfo {
            id: id.into(),
            parent_id: "ses_parent".into(),
            title: format!("任务 {id}"),
            model: "test/model".into(),
            cwd: "/tmp".into(),
            preset: None,
            status,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_message: last_message.map(str::to_owned),
        }
    }

    fn app_with(entries: Vec<(&str, AgentStatus, Option<&str>)>) -> App {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        for (id, status, message) in entries {
            app.subagents.insert(
                id.to_owned(),
                crate::subagent::SubagentView::new(agent(id, status, message)),
            );
        }
        app.sync_subagent_sidebar();
        app
    }

    /// 命中区如实覆盖每个代理卡片的全部可见行；分隔线不属于任何代理。
    #[test]
    fn hit_rows_follow_each_agent_block() {
        let mut app = app_with(vec![
            ("a1", AgentStatus::Running, None),
            ("a2", AgentStatus::Completed, Some("正在读取文件")),
        ]);
        app.subagent_selected = Some("a2".into());

        let area = Rect::new(60, 4, 32, 12);
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
        // 标题 1 行 + 空行 + a1 三行（无 last_message）+ 分隔 1 行 + a2 四行。
        assert_eq!(hit("a1"), Rect::new(area.x, area.y + 2, area.width, 3));
        assert_eq!(hit("a2"), Rect::new(area.x, area.y + 6, area.width, 4));
        assert!(
            !hit("a1").contains((area.x + 2, area.y + 5).into()),
            "分隔线不属于任何代理"
        );
        assert!(
            hit("a2").contains((area.x + 2, area.y + 9).into()),
            "末行（last_message）仍命中该代理"
        );
        // 标题右端的 × 有命中区。
        assert_eq!(
            app.subagent_close_area,
            Rect::new(area.x + area.width - 1, area.y, 1, 1),
            "× 落在侧栏内容右端"
        );
    }

    /// 显隐状态机：自动模式跟 running，手动覆盖持续到新一轮 running。
    #[test]
    fn sidebar_visibility_follows_running_and_override() {
        let mut app = app_with(vec![("a1", AgentStatus::Completed, None)]);
        assert!(!app.subagent_sidebar_visible(), "无 running → 隐藏");

        app.subagents.insert(
            "a2".into(),
            crate::subagent::SubagentView::new(agent("a2", AgentStatus::Running, None)),
        );
        app.sync_subagent_sidebar();
        assert!(app.subagent_sidebar_visible(), "有 running → 显示");

        app.subagent_sidebar = Some(false);
        assert!(
            !app.subagent_sidebar_visible(),
            "× / Alt+A 关闭后即使 running 也隐藏"
        );

        app.subagents
            .get_mut("a2")
            .expect("存在")
            .set_status(AgentStatus::Completed, None);
        app.sync_subagent_sidebar();
        assert!(
            !app.subagent_sidebar_visible(),
            "全部结束 → 隐藏（覆盖仍在）"
        );

        app.subagents.insert(
            "a3".into(),
            crate::subagent::SubagentView::new(agent("a3", AgentStatus::Running, None)),
        );
        app.sync_subagent_sidebar();
        assert!(
            app.subagent_sidebar_visible(),
            "新一轮 running → 回自动模式并显示"
        );
        assert_eq!(app.subagent_sidebar, None);
    }

    /// 侧栏不画的帧必须清掉 hits 与 × 命中区，防止旧位置还点得到。
    #[test]
    fn hidden_sidebar_clears_hit_regions() {
        let mut app = app_with(vec![("a1", AgentStatus::Running, None)]);
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("构造测试终端");
        let mut view = crate::render::scrollback::ScrollbackView::new();
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("宽屏绘制");
        assert!(!app.subagent_hits.is_empty(), "宽屏下侧栏画出了命中区");

        app.subagent_sidebar = Some(false);
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("隐藏后绘制");
        assert!(app.subagent_hits.is_empty(), "侧栏隐藏后 hits 清空");
        assert_eq!(app.subagent_close_area, Rect::default(), "× 命中区清空");
    }

    /// 真实渲染快照：一个 running、一个选中且已完成的代理。
    #[test]
    fn sidebar_render_snapshot() {
        let mut app = app_with(vec![
            ("a1", AgentStatus::Running, Some("读取代码")),
            ("a2", AgentStatus::Completed, None),
        ]);
        app.subagent_selected = Some("a2".into());
        let area = Rect::new(68, 2, 32, 11);
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).expect("构造测试终端");
        terminal
            .draw(|frame| draw_sidebar(frame, &mut app, area))
            .expect("绘制侧栏");
        let backend = terminal.backend();
        let mut dump = String::new();
        for y in area.y..area.y + area.height {
            let mut row = String::new();
            for x in area.x..area.x + area.width {
                row.push_str(backend.buffer().cell((x, y)).expect("cell").symbol());
            }
            dump.push_str(&row);
            dump.push('\n');
        }
        eprintln!("\n{dump}");
        // dump 按 cell 逐格输出，CJK 双宽字符后带一个空 cell。
        assert!(dump.contains("子 代 理  2"), "标题计数：{dump}");
        assert!(dump.contains("· 1 running"), "运行中计数：{dump}");
        assert!(dump.contains("●"), "running 符号：{dump}");
        assert!(dump.contains("✓"), "完成符号：{dump}");
        assert!(dump.contains("─"), "卡片分隔线：{dump}");
        assert!(dump.contains("×"), "关闭按钮：{dump}");
    }
}

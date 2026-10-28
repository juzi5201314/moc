//! 右上角 todo HUD：折叠胶囊 + 可拖动的展开卡片。
//!
//! 胶囊贴转写区首行右端，任务名超宽时按显示宽度横向滚动；悬停或点击固定后
//! 展开卡片。卡片是临时浮层，不参与转写区折行；拖动可把它移开正文。

use std::time::{Duration, Instant};

use moc_protocol::methods::{TodoPhase, TodoStatus};
use ratatui::Frame;
use ratatui::layout::{Alignment, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph};

use super::scrollback::todo_status_marker;
use super::wrap::{display_window, str_width, wrap_text};
use crate::app::App;
use crate::theme::Theme;

/// marquee 每前进一步的时间。
pub const MARQUEE_TICK: Duration = Duration::from_millis(250);
/// 任务名开头停留时间，先让人看清从哪里开始。
const MARQUEE_HOLD: Duration = Duration::from_millis(1200);
/// 循环滚动时任务名之间的分隔。
const MARQUEE_SEPARATOR: &str = "   ·   ";
/// 胶囊最大占宽；更宽的任务名在内部滚动，保持在右上角紧凑呈现不遮挡转写正文。
const CHIP_MAX_WIDTH: u16 = 34;
/// 展开卡片最大占宽。
const CARD_MAX_WIDTH: u16 = 66;
/// 低于该宽度/高度时整个 HUD 隐藏，避免出现打不开的入口。
const HUD_MIN_WIDTH: u16 = 28;
const HUD_MIN_HEIGHT: u16 = 4;
/// 任务名至少留出的显示宽度；不够就隐藏胶囊。
const MIN_TASK_BUDGET: usize = 6;

/// 绘制 HUD；`area` 是主转写区（已扣除子代理侧栏）。
pub fn draw(frame: &mut Frame, app: &mut App, area: Rect, header_visible: bool) {
    if app.todos.is_empty() {
        app.todo_hud.reset();
        return;
    }
    // 矮终端连 header 都保不住时，不再往转写区上叠浮层。
    if !header_visible || area.width < HUD_MIN_WIDTH || area.height < HUD_MIN_HEIGHT {
        app.todo_hud.on_hidden();
        return;
    }
    let (done, total) = app.todo_progress();
    let Some((task, status)) = headline(&app.todos) else {
        app.todo_hud.on_hidden();
        return;
    };
    let task = sanitize(&task);
    let theme = app.theme;
    // warning 也贴转写区首行右端：让胶囊下移一行，告警保持可读；优先占用顶部间隙区不遮挡正文。
    let warning_shift = u16::from(app.warning_visible() && area.height > 1);
    let chip_y = if header_visible && area.y > 0 {
        area.y.saturating_sub(1).saturating_add(warning_shift)
    } else {
        area.y.saturating_add(warning_shift)
    };
    let Some(chip) = build_chip(app, &task, status, done, total, area.width) else {
        app.todo_hud.on_hidden();
        return;
    };
    let chip_width = chip.width.min(area.width);
    let chip_area = Rect::new(
        area.right().saturating_sub(chip_width),
        chip_y,
        chip_width,
        1,
    );
    app.todo_hud.chip_area = chip_area;
    frame
        .buffer_mut()
        .set_style(chip_area, Style::new().bg(theme.bg_highlight));
    frame.render_widget(
        Paragraph::new(Line::from(chip.spans)).style(Style::new().bg(theme.bg_highlight)),
        chip_area,
    );

    if !app.todo_hud.is_open() {
        return;
    }
    draw_card(frame, app, area, chip_y, done, total);
}

/// 折叠胶囊：当前任务 + 进度；超宽时返回 marquee 窗口。
struct Chip {
    spans: Vec<Span<'static>>,
    width: u16,
}

fn build_chip(
    app: &mut App,
    task: &str,
    status: TodoStatus,
    done: usize,
    total: usize,
    width: u16,
) -> Option<Chip> {
    let theme = app.theme;
    let max_width = usize::from(width.min(CHIP_MAX_WIDTH));
    let all_done = total > 0 && done == total;
    let accent = if all_done {
        theme.accent_success
    } else {
        theme.accent_user
    };
    let count_style = Style::new()
        .fg(if all_done {
            theme.accent_success
        } else {
            theme.text_secondary
        })
        .bg(theme.bg_highlight);
    let (glyph, marker_style, _) = todo_status_marker(status, theme);
    let prefix: Vec<Span<'static>> = vec![
        Span::styled(
            " ▌ ".to_owned(),
            Style::new().fg(accent).bg(theme.bg_highlight),
        ),
        Span::styled(
            "◆ ".to_owned(),
            Style::new().fg(accent).bg(theme.bg_highlight),
        ),
        Span::styled(
            "任务 ".to_owned(),
            Style::new().fg(theme.text_secondary).bg(theme.bg_highlight),
        ),
        Span::styled(format!("{done}/{total}"), count_style),
        Span::styled(
            " · ".to_owned(),
            Style::new().fg(theme.border).bg(theme.bg_highlight),
        ),
        Span::styled(glyph.to_owned(), marker_style.bg(theme.bg_highlight)),
    ];
    let prefix_width: usize = prefix.iter().map(|span| str_width(&span.content)).sum();
    let task_budget = max_width.saturating_sub(prefix_width + 1);
    if task_budget < MIN_TASK_BUDGET {
        return None;
    }

    let overflow = str_width(task) > task_budget;
    let mut display = if overflow {
        if app.todo_hud.is_open() {
            // 卡片展开时胶囊通常被盖住；即使被拖开也保持静态，省掉无意义的定时重绘。
            display_window(task, 0, task_budget)
        } else {
            if app.todo_hud.marquee_key != task {
                app.todo_hud.marquee_key.clear();
                app.todo_hud.marquee_key.push_str(task);
                app.todo_hud.marquee_start = Some(Instant::now());
            }
            let start = *app.todo_hud.marquee_start.get_or_insert_with(Instant::now);
            let cycle = format!("{task}{MARQUEE_SEPARATOR}");
            let cycle_chars = cycle.chars().count().max(1);
            let offset = marquee_offset(start, Instant::now(), cycle_chars);
            app.todo_hud.marquee_active = true;
            marquee_view(task, offset, task_budget)
        }
    } else {
        app.todo_hud.marquee_key.clear();
        app.todo_hud.marquee_start = None;
        task.to_owned()
    };
    if overflow {
        // 宽字符边界可能让窗口少一列；补空格保持胶囊宽度稳定，避免滚动时抖动。
        let pad = task_budget.saturating_sub(str_width(&display));
        display.push_str(&" ".repeat(pad));
    }

    let mut task_style = Style::new().fg(theme.text_primary).bg(theme.bg_highlight);
    if status == TodoStatus::Completed {
        task_style = task_style.add_modifier(Modifier::CROSSED_OUT);
    }
    let mut spans = prefix;
    spans.push(Span::styled(display, task_style));
    spans.push(Span::styled(
        " ".to_owned(),
        Style::new().bg(theme.bg_highlight),
    ));
    let total_width: usize = spans.iter().map(|span| str_width(&span.content)).sum();
    Some(Chip {
        spans,
        width: u16::try_from(total_width).unwrap_or(width),
    })
}

/// 展开卡片：按内容定高，限制在转写区内；固定后使用拖动记忆的位置。
fn draw_card(
    frame: &mut Frame,
    app: &mut App,
    area: Rect,
    default_y: u16,
    done: usize,
    total: usize,
) {
    let theme = app.theme;
    let card_width = area.width.min(CARD_MAX_WIDTH);
    let inner_width = usize::from(card_width.saturating_sub(2));
    let mut rows = card_rows(&app.todos, inner_width, theme);
    let inner_max = usize::from(area.height.saturating_sub(2));
    if inner_max < 2 {
        app.todo_hud.on_hidden();
        return;
    }
    let (visible, hidden) = if rows.len() > inner_max {
        let visible = inner_max - 1;
        (visible, rows.len() - visible)
    } else {
        (rows.len(), 0)
    };
    let mut shown: Vec<Line<'static>> = rows.drain(..visible).collect();
    if hidden > 0 {
        shown.push(Line::from(Span::styled(
            format!("… 还有 {hidden} 行"),
            Style::new().fg(theme.gray_dim),
        )));
    }
    let height = u16::try_from(shown.len() + 2).unwrap_or(area.height);
    let pinned = app.todo_hud.pinned;
    let default_origin = (area.right().saturating_sub(card_width), default_y);
    let origin = if pinned {
        app.todo_hud.drag_origin.unwrap_or(default_origin)
    } else {
        default_origin
    };
    let max_x = area.right().saturating_sub(card_width).max(area.x);
    let max_y = area.bottom().saturating_sub(height).max(area.y);
    let card_area = Rect::new(
        origin.0.clamp(area.x, max_x),
        origin.1.clamp(area.y, max_y),
        card_width,
        height,
    );
    app.todo_hud.card_area = card_area;
    app.todo_hud.card_visible = true;

    let all_done = total > 0 && done == total;
    let accent = if all_done {
        theme.accent_success
    } else {
        theme.accent_user
    };
    let count_style = Style::new().fg(if all_done {
        theme.accent_success
    } else {
        theme.text_secondary
    });
    let title_left = Line::from(vec![
        Span::styled(
            " ◆ 任务 ".to_owned(),
            Style::new().fg(accent).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!("{done}/{total} "), count_style),
    ]);
    let hint = if pinned {
        " 已固定 · 点击取消 "
    } else {
        " 拖动 · 点击固定 "
    };
    let title_right = Line::from(Span::styled(
        hint.to_owned(),
        Style::new().fg(theme.gray_dim),
    ))
    .alignment(Alignment::Right);
    let block = Block::bordered()
        .border_set(ratatui::symbols::border::ROUNDED)
        .style(Style::new().bg(theme.bg_base).fg(theme.text_primary))
        .border_style(Style::new().fg(if pinned {
            theme.border_active
        } else {
            theme.border
        }))
        .title(title_left)
        .title(title_right);

    frame.render_widget(Clear, card_area);
    frame.render_widget(Paragraph::new(shown).block(block), card_area);
}

/// 卡片正文：阶段行 + 条目行，符号/颜色与转写树共用。
fn card_rows(phases: &[TodoPhase], inner_width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let phase_count = phases.len();
    let mut rows = Vec::new();
    for (index, phase) in phases.iter().enumerate() {
        let is_last = index + 1 == phase_count;
        let done = phase
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();
        let total = phase.items.len();
        let branch = if is_last { "└─ " } else { "├─ " };
        rows.push(Line::from(vec![
            Span::styled(branch.to_owned(), Style::new().fg(theme.border)),
            Span::styled(
                sanitize(&phase.phase),
                Style::new().fg(theme.text_secondary),
            ),
            Span::styled(
                format!(" · {done}/{total}"),
                Style::new().fg(if done == total && total > 0 {
                    theme.accent_success
                } else {
                    theme.gray_dim
                }),
            ),
        ]));
        let item_branch = if is_last { "    " } else { "│   " };
        for item in &phase.items {
            let (glyph, marker_style, text_style) = todo_status_marker(item.status, theme);
            let task = sanitize(&item.task);
            let budget = inner_width.saturating_sub(item_branch.len() + 6).max(8);
            for (row_index, row) in wrap_text(&task, budget).into_iter().enumerate() {
                if row_index == 0 {
                    rows.push(Line::from(vec![
                        Span::styled(item_branch.to_owned(), Style::new().fg(theme.border)),
                        Span::styled(glyph.to_owned(), marker_style),
                        Span::styled(row, text_style),
                    ]));
                } else {
                    rows.push(Line::from(vec![
                        Span::styled(item_branch.to_owned(), Style::new().fg(theme.border)),
                        Span::styled("    ".to_owned(), Style::new()),
                        Span::styled(row, text_style),
                    ]));
                }
            }
        }
    }
    rows
}

/// 折叠态标题：优先进行中，其次阻塞/待办/已完成，最后回退到任意条目。
fn headline(phases: &[TodoPhase]) -> Option<(String, TodoStatus)> {
    let mut blocked = None;
    let mut pending = None;
    let mut completed = None;
    let mut last = None;
    for phase in phases {
        for item in &phase.items {
            last = Some((item.task.as_str(), item.status));
            match item.status {
                TodoStatus::InProgress => return Some((item.task.clone(), item.status)),
                TodoStatus::Blocked => {
                    if blocked.is_none() {
                        blocked = Some((item.task.as_str(), item.status));
                    }
                }
                TodoStatus::Pending => {
                    if pending.is_none() {
                        pending = Some((item.task.as_str(), item.status));
                    }
                }
                TodoStatus::Completed => {
                    completed = Some((item.task.as_str(), item.status));
                }
                TodoStatus::Abandoned => {}
            }
        }
    }
    blocked
        .or(pending)
        .or(completed)
        .or(last)
        .map(|(task, status)| (task.to_owned(), status))
}

/// 控制字符换成空格：todo 文本可能来自模型，不能破坏单行布局。
fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

/// marquee 偏移：先停留，再按 tick 步进；由时间推导，不累积漂移。
pub(crate) fn marquee_offset(start: Instant, now: Instant, cycle_chars: usize) -> usize {
    if cycle_chars == 0 {
        return 0;
    }
    let elapsed = now.saturating_duration_since(start);
    if elapsed < MARQUEE_HOLD {
        return 0;
    }
    let steps = (elapsed - MARQUEE_HOLD).as_millis() / MARQUEE_TICK.as_millis();
    usize::try_from(steps % cycle_chars as u128).unwrap_or(0)
}

/// 循环拼接任务名，再按显示宽度截取窗口。
pub(crate) fn marquee_view(task: &str, offset_chars: usize, width: usize) -> String {
    if width == 0 {
        return String::new();
    }
    let cycle = format!("{task}{MARQUEE_SEPARATOR}");
    let cycle_chars = cycle.chars().count();
    if cycle_chars == 0 {
        return String::new();
    }
    let cycle_cols = str_width(&cycle).max(1);
    let repeated = cycle.repeat(width / cycle_cols + 2);
    display_window(&repeated, offset_chars % cycle_chars, width)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phase(name: &str, items: &[(TodoStatus, &str)]) -> TodoPhase {
        TodoPhase {
            phase: name.to_owned(),
            items: items
                .iter()
                .map(|(status, task)| moc_protocol::methods::TodoItem {
                    status: *status,
                    task: (*task).to_owned(),
                    reason: None,
                })
                .collect(),
        }
    }

    #[test]
    fn headline_prefers_in_progress_then_blocked() {
        let phases = vec![phase(
            "阶段",
            &[
                (TodoStatus::Pending, "待办"),
                (TodoStatus::Blocked, "阻塞"),
                (TodoStatus::Completed, "完成"),
                (TodoStatus::InProgress, "进行中"),
            ],
        )];
        assert_eq!(
            headline(&phases).map(|(task, _)| task),
            Some("进行中".into())
        );

        let no_active = vec![phase(
            "阶段",
            &[(TodoStatus::Pending, "待办"), (TodoStatus::Blocked, "阻塞")],
        )];
        assert_eq!(
            headline(&no_active).map(|(task, _)| task),
            Some("阻塞".into())
        );
    }

    #[test]
    fn headline_falls_back_to_last_completed_or_any() {
        let done = vec![phase(
            "阶段",
            &[
                (TodoStatus::Completed, "完成一"),
                (TodoStatus::Completed, "完成二"),
            ],
        )];
        assert_eq!(headline(&done).map(|(task, _)| task), Some("完成二".into()));

        let abandoned = vec![phase("阶段", &[(TodoStatus::Abandoned, "放弃")])];
        assert_eq!(
            headline(&abandoned).map(|(task, _)| task),
            Some("放弃".into())
        );
        assert!(headline(&[]).is_none());
    }

    #[test]
    fn marquee_holds_then_steps_and_wraps() {
        let start = Instant::now();
        assert_eq!(marquee_offset(start, start, 10), 0);
        assert_eq!(
            marquee_offset(start, start + MARQUEE_HOLD - Duration::from_millis(1), 10),
            0
        );
        assert_eq!(marquee_offset(start, start + MARQUEE_HOLD, 10), 0);
        assert_eq!(
            marquee_offset(start, start + MARQUEE_HOLD + MARQUEE_TICK, 10),
            1
        );
        assert_eq!(
            marquee_offset(start, start + MARQUEE_HOLD + MARQUEE_TICK * 12, 10),
            2,
            "超过周期后回绕"
        );
    }

    #[test]
    fn marquee_view_never_splits_wide_chars() {
        for offset in 0..20 {
            let view = marquee_view("中文任务abc", offset, 7);
            assert!(str_width(&view) <= 7, "窗口不得超过显示宽度");
            assert!(!view.contains('\u{FFFD}'));
        }
    }

    #[test]
    fn marquee_loops_with_separator() {
        let view = marquee_view("abc", 0, 10);
        assert!(view.starts_with("abc"));
        assert!(view.len() > 3);
        let wrapped = marquee_view("abc", 6, 10);
        assert!(wrapped.starts_with(MARQUEE_SEPARATOR.trim_start()));
    }

    #[test]
    fn sanitize_flattens_control_chars() {
        assert_eq!(sanitize("a\nb\tc\rd"), "a b c d");
    }

    #[test]
    fn card_rows_keep_phase_and_item_markers() {
        let theme = Theme::global();
        let phases = vec![phase(
            "实现",
            &[
                (TodoStatus::Completed, "已完成"),
                (TodoStatus::InProgress, "进行中"),
                (TodoStatus::Pending, "待办"),
            ],
        )];
        let rows = card_rows(&phases, 40, theme);
        let text = rows
            .iter()
            .flat_map(|line| line.spans.iter().map(|span| span.content.as_ref()))
            .collect::<String>();
        assert!(text.contains("└─ "));
        assert!(text.contains("☑ "));
        assert!(text.contains("▶ "));
        assert!(text.contains("☐ "));
        assert!(text.contains("1/3"));
    }

    #[test]
    fn card_renders_with_rounded_borders() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = vec![phase("阶段", &[(TodoStatus::InProgress, "进行中")])];
        app.todo_hud.pinned = true;
        let area = Rect::new(0, 0, 80, 20);
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 20)).unwrap();
        terminal
            .draw(|frame| draw(frame, &mut app, area, true))
            .unwrap();
        let card_area = app.todo_hud.card_area;
        assert!(card_area.width >= 4 && card_area.height >= 3);
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer.cell((card_area.x, card_area.y)).unwrap().symbol(),
            "╭"
        );
        assert_eq!(
            buffer
                .cell((card_area.right() - 1, card_area.y))
                .unwrap()
                .symbol(),
            "╮"
        );
        assert_eq!(
            buffer
                .cell((card_area.x, card_area.bottom() - 1))
                .unwrap()
                .symbol(),
            "╰"
        );
        assert_eq!(
            buffer
                .cell((card_area.right() - 1, card_area.bottom() - 1))
                .unwrap()
                .symbol(),
            "╯"
        );
    }
}

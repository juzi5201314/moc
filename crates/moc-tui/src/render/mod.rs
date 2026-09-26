//! 帧绘制：布局切分 + 各区域渲染。
//!
//! 布局自上而下：滚动区（转写，唯一 `Min` 约束）→ 输入框 → statusline。
//! 整帧先铺主题底色，滚动区由 [`ScrollbackView`] 按行缓存直写 buffer。

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod modal;
pub mod scrollback;
pub mod wrap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

pub use scrollback::ScrollbackView;

use crate::app::{App, Focus};

/// 输入框行数（多行编辑阶段会动态化）。
const PROMPT_ROWS: u16 = 3;
/// statusline 行数。
const STATUS_ROWS: u16 = 1;
/// 终端低于该高度时砍掉 statusline。
const SHORT_TERMINAL_ROWS: u16 = 12;

pub fn draw(frame: &mut Frame, app: &mut App, view: &mut ScrollbackView) {
    let theme = app.theme;
    let area = frame.area();
    let rows = area.height;
    let status_rows = if rows > SHORT_TERMINAL_ROWS { STATUS_ROWS } else { 0 };
    let [scroll_area, prompt_area, status_area] = Layout::vertical([
        Constraint::Min(3),
        Constraint::Length(PROMPT_ROWS.min(rows)),
        Constraint::Length(status_rows),
    ])
    .areas(area);

    // 整帧铺底色（备用屏模式下终端自身背景与主题保持一致）
    frame
        .buffer_mut()
        .set_style(area, Style::new().bg(theme.bg_base));

    let total = view.sync(app, scroll_area.width);
    let viewport = scroll_area.height as usize;
    // 滚动语义：offset = 距底部行数；0 表示跟随尾部
    let max_offset = total.saturating_sub(viewport);
    if app.scroll_offset > max_offset {
        app.scroll_offset = max_offset;
    }
    let skip_rows = max_offset - app.scroll_offset;
    app.mouse_hits.clear();
    app.paint_lines.clear();
    view.paint(
        frame.buffer_mut(),
        scroll_area,
        skip_rows,
        &mut app.mouse_hits,
        &mut app.paint_lines,
    );
    highlight_selection(frame, app, scroll_area);
    draw_notice(frame, app, scroll_area);
    draw_scrollbar(frame, app, scroll_area, max_offset);

    // 浮层：补全菜单贴输入框上沿，模态居中（最后画，覆盖滚动区）
    if let Some(completion) = &app.completion {
        modal::draw_completion_menu(frame, completion, prompt_area);
    }
    if app.modal.is_some() {
        modal::draw_modal(frame, app);
    }

    draw_prompt(frame, app, prompt_area);
    if status_rows > 0 {
        draw_statusline(frame, app, status_area);
    }
}

/// 拖选高亮：选中行铺 `bg_visual`（在内容绘制之后覆盖前景/背景对调）。
fn highlight_selection(frame: &mut Frame, app: &App, area: Rect) {
    let Some(((_, sy), (_, ey))) = app.selection else {
        return;
    };
    let (start_row, end_row) = if sy <= ey { (sy, ey) } else { (ey, sy) };
    let theme = app.theme;
    let buffer = frame.buffer_mut();
    for row in start_row..=end_row {
        if row < area.y || row >= area.y + area.height {
            continue;
        }
        for col in area.x..area.x + area.width {
            let Some(cell) = buffer.cell_mut((col, row)) else {
                continue;
            };
            let style = cell.style();
            cell.set_style(
                Style::new()
                    .fg(style.bg.unwrap_or(theme.text_primary))
                    .bg(theme.bg_visual),
            );
        }
    }
}

/// 一次性提示 toast：覆盖滚动区底行，TTL 由 [`App::notice_visible`] 决定。
fn draw_notice(frame: &mut Frame, app: &App, area: Rect) {
    use unicode_width::UnicodeWidthChar;
    if !app.notice_visible() {
        return;
    }
    let Some(text) = app.notice.as_deref() else {
        return;
    };
    let theme = app.theme;
    let row = area.y + area.height - 1;
    let buffer = frame.buffer_mut();
    let end = area.x + area.width;
    // 先物化 (字符, 颜色, 显示宽度)：◆ 前缀强调，正文按显示宽度排布
    let mut runs: Vec<(char, ratatui::style::Color, u16)> =
        Vec::with_capacity(text.chars().count() + 1);
    runs.push(('◆', theme.accent_success, 1));
    runs.extend(
        text.chars()
            .map(|ch| (ch, theme.text_primary, ch.width().unwrap_or(1).max(1) as u16)),
    );
    let mut col = area.x;
    for (ch, fg, width) in runs {
        if col >= end {
            break;
        }
        if let Some(cell) = buffer.cell_mut((col, row)) {
            cell.set_char(ch).set_style(Style::new().fg(fg).bg(theme.bg_panel));
        }
        // 宽字符右侧补空格铺底，避免残留
        for offset in 1..width {
            if col + offset >= end {
                break;
            }
            if let Some(pad) = buffer.cell_mut((col + offset, row)) {
                pad.set_char(' ').set_style(Style::new().bg(theme.bg_panel));
            }
        }
        col += width;
    }
    while col < end {
        if let Some(cell) = buffer.cell_mut((col, row)) {
            cell.set_char(' ').set_style(Style::new().bg(theme.bg_panel));
        }
        col += 1;
    }
}

/// 右缘自绘滚动条：跟随态半暗，浏览态亮起。
fn draw_scrollbar(frame: &mut Frame, app: &App, area: Rect, max_offset: usize) {
    if max_offset == 0 || area.width < 2 || area.height < 2 {
        return;
    }
    let theme = app.theme;
    let track = area.height;
    let total = (max_offset as u32) + area.height as u32;
    let thumb_height = ((area.height as u32 * area.height as u32) / total.max(1))
        .clamp(1, track as u32) as u16;
    let position_ratio = app.scroll_offset as f64 / max_offset as f64;
    let thumb_top = ((track - thumb_height) as f64 * position_ratio).round() as u16;
    let x = area.x + area.width - 1;
    let color = if app.scroll_offset == 0 {
        theme.scrollbar_bg
    } else {
        theme.scrollbar_fg
    };
    for row in 0..track {
        let in_thumb = row >= thumb_top && row < thumb_top + thumb_height;
        frame.buffer_mut().set_string(
            x,
            area.y + row,
            "▐",
            Style::new().fg(if in_thumb { theme.scrollbar_fg } else { color }),
        );
    }
}

fn draw_prompt(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let border = match app.focus {
        Focus::Prompt => theme.border_active,
        Focus::Scrollback => theme.border,
    };
    let mut widget = Paragraph::new(prompt_lines(app, area)).block(
        Block::bordered()
            .border_set(ratatui::symbols::border::ROUNDED)
            .border_style(Style::new().fg(border)),
    );
    if app.prompt_text().is_empty() {
        widget = widget.style(Style::new().fg(theme.gray_dim));
    }
    frame.render_widget(widget, area);
}

/// 输入框内容行：chip 占位高亮、空输入给 placeholder。
fn prompt_lines(app: &App, area: Rect) -> Vec<Line<'static>> {
    let theme = app.theme;
    let text = app.prompt_text();
    if text.is_empty() {
        return vec![Line::from(Span::styled(
            "输入消息，/ 唤起命令…".to_owned(),
            Style::new().fg(theme.gray_dim),
        ))];
    }
    let content_width = area.width.saturating_sub(4) as usize;
    text.split('\n')
        .flat_map(|line| wrap::wrap_text(line, content_width.max(4)))
        .map(|row| {
            // chip 占位（[paste#N] / [image#N]）着色提示
            let mut spans: Vec<Span<'static>> = Vec::new();
            let mut rest = row.as_str();
            while let Some(index) = rest.find("[paste#").or_else(|| rest.find("[image#")) {
                if index > 0 {
                    spans.push(Span::styled(
                        rest[..index].to_owned(),
                        Style::new().fg(theme.text_primary),
                    ));
                }
                let end = rest[index..].find(']').map_or(rest.len(), |end| index + end + 1);
                spans.push(Span::styled(
                    rest[index..end].to_owned(),
                    Style::new().fg(theme.accent_warning),
                ));
                rest = &rest[end..];
            }
            if !rest.is_empty() {
                spans.push(Span::styled(
                    rest.to_owned(),
                    Style::new().fg(theme.text_primary),
                ));
            }
            Line::from(spans)
        })
        .collect()
}

fn draw_statusline(frame: &mut Frame, app: &App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let line = match &app.statusline_template {
        Some(template) => statusline_from_template(app, template),
        None => statusline_builtin(app, area.width as usize),
    };
    frame.render_widget(Paragraph::new(line), area);
}

/// statusline 数据快照（内置渲染与模板共用）。
struct StatuslineData {
    model: String,
    mode: String,
    /// 当前上下文占窗口的百分比（0-100，含小数）。
    context_percent: Option<f64>,
    workdir: String,
    branch: Option<String>,
    todo: Option<(usize, usize)>,
    tokens_in: u64,
    tokens_out: u64,
    tok_s: f64,
    turn_elapsed: Option<std::time::Duration>,
    cost: Option<f64>,
}

fn statusline_data(app: &App) -> StatuslineData {
    let mode = app
        .session
        .as_ref()
        .map(|session| format!("{:?}", session.mode).to_lowercase())
        .unwrap_or_else(|| "idle".into());
    let context_percent = app
        .context_window()
        .filter(|window| *window > 0)
        .map(|window| app.usage.input_tokens as f64 * 100.0 / window as f64);
    let workdir = app
        .session
        .as_ref()
        .map(|session| session.cwd.clone())
        .map(|cwd| {
            std::path::Path::new(&cwd)
                .file_name()
                .map_or_else(|| cwd.clone(), |name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_default();
    let turn_elapsed = if app.turn_running {
        app.turn_started_at.map(|at| at.elapsed())
    } else {
        None
    };
    let cost = (app.cost_usd > 0.0).then_some(app.cost_usd);
    StatuslineData {
        model: app.model_display().to_owned(),
        mode,
        context_percent,
        workdir,
        branch: app.git_branch.clone(),
        todo: {
            let (done, total) = app.todo_progress();
            (total > 0).then_some((done, total))
        },
        tokens_in: app.usage.input_tokens,
        tokens_out: app.usage.output_tokens,
        tok_s: app.tokens_per_second(),
        turn_elapsed,
        cost,
    }
}

/// 内置渲染（omp 风格）：模型·模式 › 上下文进度 › 目录 › 分支 › todo ‖ 统计 › 费用。
fn statusline_builtin(app: &App, width: usize) -> Line<'static> {
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = vec![
        Span::styled("● ", Style::new().fg(theme.accent_model)),
        Span::styled(
            data.model.clone(),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" · {}", data.mode), Style::new().fg(theme.gray)),
    ];
    let push = |spans: &mut Vec<Span<'static>>, text: String, color: ratatui::style::Color| {
        spans.push(Span::styled(" › ", Style::new().fg(theme.border)));
        spans.push(Span::styled(text, Style::new().fg(color)));
    };
    // 上下文进度条
    if let Some(percent) = data.context_percent {
        let slots = 10;
        let filled = (((percent / 100.0) * slots as f64).round() as usize).min(slots);
        let bar_color = if percent >= 85.0 {
            theme.accent_error
        } else if percent >= 70.0 {
            theme.accent_warning
        } else {
            theme.accent_success
        };
        push(
            &mut spans,
            format!("『{}{}』{}", "━".repeat(filled), "─".repeat(slots - filled), format_context_percent(percent)),
            bar_color,
        );
    }
    if !data.workdir.is_empty() {
        push(&mut spans, data.workdir.clone(), theme.path);
    }
    if let Some(branch) = &data.branch {
        push(&mut spans, format!("⎇ {branch}"), theme.accent_success);
    }
    if let Some((done, total)) = data.todo {
        push(&mut spans, format!("☑ {done}/{total}"), theme.accent_success);
    }
    // 右侧统计段
    let right = format!(
        " ⇅ {} › {} › {} tok/s{}",
        format_number(data.tokens_in),
        format_number(data.tokens_out),
        data.tok_s as u64,
        data.turn_elapsed
            .map(|elapsed| format!(" › ⏱ {}s", elapsed.as_secs()))
            .unwrap_or_default(),
    );
    let right_cost = data
        .cost
        .map(|cost| format!(" › ${cost:.2}"))
        .unwrap_or_default();
    let used: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let remaining = width.saturating_sub(used + right.len() + right_cost.len() + 4);
    spans.push(Span::styled(
        " ".repeat(2.min(remaining / 2)),
        Style::new(),
    ));
    spans.push(Span::styled(right, Style::new().fg(theme.gray)));
    spans.push(Span::styled(
        right_cost,
        Style::new().fg(theme.accent_warning),
    ));
    Line::from(spans)
}

/// 模板渲染：`{model} · {mode} → {workdir}`，占位符按语义着色。
fn statusline_from_template(app: &App, template: &str) -> Line<'static> {
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        plain.push_str(&rest[..start]);
        let placeholder = &rest[start + 1..start + end];
        let value: Option<(String, ratatui::style::Color)> = match placeholder {
            "model" => Some((data.model.clone(), theme.accent_model)),
            "mode" => Some((data.mode.clone(), theme.accent_tool)),
            "context" => data
                .context_percent
                .map(|_| (format_number(app.usage.input_tokens), theme.text_secondary)),
            "context_percent" => data
                .context_percent
                .map(|percent| (format_context_percent(percent), theme.text_secondary)),
            "workdir" => Some((data.workdir.clone(), theme.path)),
            "branch" => data
                .branch
                .clone()
                .map(|branch| (format!("⎇ {branch}"), theme.accent_success)),
            "todo" => data
                .todo
                .map(|(done, total)| (format!("☑ {done}/{total}"), theme.accent_success)),
            "tokens_in" => Some((format_number(data.tokens_in), theme.text_secondary)),
            "tokens_out" => Some((format_number(data.tokens_out), theme.text_secondary)),
            "tok_s" => Some((format!("{:.1} tok/s", data.tok_s), theme.accent_running)),
            "turn_elapsed" => Some((
                data.turn_elapsed
                    .map(|elapsed| format!("⏱ {}s", elapsed.as_secs()))
                    .unwrap_or_else(|| "⏱ -".into()),
                theme.gray,
            )),
            "cost" => data.cost.map(|cost| (format!("${cost:.2}"), theme.accent_warning)),
            other => {
                // 未知占位符按原文保留
                plain.push_str(&format!("{{{other}}}"));
                None
            }
        };
        if let Some((text, color)) = value {
            if !plain.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut plain),
                    Style::new().fg(theme.gray),
                ));
            }
            spans.push(Span::styled(text, Style::new().fg(color)));
        }
        rest = &rest[start + end + 1..];
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        spans.push(Span::styled(plain, Style::new().fg(theme.gray)));
    }
    Line::from(spans)
}

/// token 数缩写（120000 → 120K）。
fn format_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

/// 上下文百分比标签：非零但不足 1% 时显示 `<1%`，避免截断成 0%。
fn format_context_percent(percent: f64) -> String {
    if percent > 0.0 && percent < 1.0 {
        "<1%".into()
    } else {
        format!("{percent:.0}%")
    }
}

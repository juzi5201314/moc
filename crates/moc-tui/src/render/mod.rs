//! 帧绘制：布局切分 + 各区域渲染。
//!
//! 留白 → 输入框 → statusline。所有内容左右各留 [`MARGIN_X`] 格边距，行与行
//! 之间靠留白分隔；输入框随多行输入长高，其下边框兼作图例行（左＝帮助键提示，右＝模型 · 模式）。
//! 整帧先铺主题底色，滚动区由 [`ScrollbackView`] 按行缓存直写 buffer。

pub mod diff;
pub mod highlight;
pub mod markdown;
pub mod modal;
pub mod scrollback;
pub mod wrap;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

pub use scrollback::ScrollbackView;

use self::wrap::str_width;
use crate::app::{App, Focus};
use crate::modal::Modal;
use crate::transcript::Block as TranscriptBlock;

/// 内容区左右边距（格）。
const MARGIN_X: u16 = 3;
/// 转写块标题（bullet / 工具名 / TODO）左缩进（格）。
pub const TITLE_INDENT: usize = 1;
/// 转写块正文（助手 markdown、工具内容、思考正文）左缩进（格）。
pub const PROSE_INDENT: usize = 3;
/// 顶部信息条行数。
const HEADER_ROWS: u16 = 1;
/// 输入框上下边框行数。
const PROMPT_BORDER_ROWS: u16 = 2;
/// 输入框正文行数上限（多行输入按需增长，再长就滚动查看）。
const PROMPT_MAX_ROWS: usize = 6;
/// 输入框内左右内边距（格）。
const PROMPT_PAD: usize = 1;
/// 输入框提示符宽度（`❯ `）。
const PROMPT_MARKER: usize = 2;
/// statusline 行数。
const STATUS_ROWS: u16 = 1;
/// 终端低于该高度时砍掉 statusline。
const SHORT_TERMINAL_ROWS: u16 = 12;
/// 更矮时连顶部信息条一起砍，把空间全留给转写区。
const NARROW_TERMINAL_ROWS: u16 = 9;

/// 左右内缩：内容一律留出边距，不贴终端边缘。
fn inset(area: Rect, margin: u16) -> Rect {
    let x = area.x + margin.min(area.width);
    Rect {
        x,
        width: area.width.saturating_sub(margin.saturating_mul(2)),
        ..area
    }
}

/// 输入框正文行数：按当前输入换行所需行数（1 行起，上限 [`PROMPT_MAX_ROWS`]）。
fn prompt_content_rows(app: &App, width: u16) -> usize {
    let text_width = usize::from(width)
        .saturating_sub(usize::from(PROMPT_BORDER_ROWS) + PROMPT_PAD + PROMPT_MARKER)
        .max(4);
    let text = app.prompt_text();
    if text.is_empty() {
        return 1;
    }
    text.split('\n')
        .map(|line| wrap::wrap_text(line, text_width).len())
        .sum::<usize>()
        .clamp(1, PROMPT_MAX_ROWS)
}

pub fn draw(frame: &mut Frame, app: &mut App, view: &mut ScrollbackView) {
    let saving = app.saving_config();
    if let Some(panel) = &mut app.settings {
        crate::settings::draw(frame, panel, saving, app.theme);
        if app.modal.is_some() {
            modal::draw_modal(frame, app);
        }
        return;
    }
    let theme = app.theme;
    let frame_area = frame.area();
    let rows = frame_area.height;
    let header_rows = if rows > NARROW_TERMINAL_ROWS {
        HEADER_ROWS
    } else {
        0
    };
    let status_rows = if rows > SHORT_TERMINAL_ROWS {
        STATUS_ROWS
    } else {
        0
    };
    let content_area = inset(frame_area, MARGIN_X);
    // 输入框随多行输入长高，但不能挤掉转写区（至少留 3 行）。
    let chrome = header_rows + status_rows + PROMPT_BORDER_ROWS + 2;
    let prompt_cap = rows.saturating_sub(chrome + 3).max(1);
    let content_rows = prompt_content_rows(app, content_area.width);
    let content_rows = u16::try_from(content_rows)
        .unwrap_or(PROMPT_BORDER_ROWS)
        .min(prompt_cap);
    let [
        header_area,
        _gap_top,
        scroll_area,
        _gap_bottom,
        prompt_area,
        status_area,
    ] = Layout::vertical([
        Constraint::Length(header_rows),
        // 信息条与转写区之间留白；没有信息条时转写区自己顶格。
        Constraint::Length(u16::from(header_rows > 0)),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(PROMPT_BORDER_ROWS + content_rows),
        Constraint::Length(status_rows),
    ])
    .areas(frame_area);
    let scroll_area = inset(scroll_area, MARGIN_X);
    let prompt_area = inset(prompt_area, MARGIN_X);
    app.scrollback_area = scroll_area;
    app.prompt_area = prompt_area;

    // 整帧铺底色（备用屏模式下终端自身背景与主题保持一致）
    frame
        .buffer_mut()
        .set_style(frame_area, Style::new().bg(theme.bg_base));

    let total = view.sync(app, scroll_area.width);
    let viewport = scroll_area.height as usize;
    // 滚动语义：offset = 距底部行数；0 表示跟随尾部
    let max_offset = total.saturating_sub(viewport);
    if app.scroll_offset > max_offset {
        app.scroll_offset = max_offset;
    }
    let skip_rows = max_offset - app.scroll_offset;
    view.ensure_visible(app, skip_rows, viewport);
    app.mouse_hits.clear();
    app.paint_lines.clear();
    let collect_lines = app.selection.is_some() || app.pending_click.is_some();
    view.paint(
        frame.buffer_mut(),
        scroll_area,
        skip_rows,
        collect_lines,
        &mut app.mouse_hits,
        &mut app.paint_lines,
    );
    view.trim_outside(skip_rows, viewport);
    highlight_selection(frame, app, scroll_area);
    draw_notice(frame, app, scroll_area);
    draw_warning(frame, app, scroll_area);
    // 滚动条贴终端右缘：与内容边距无关，避免压住正文
    draw_scrollbar(frame, app, scroll_area, max_offset);

    draw_header(frame, app, inset(header_area, MARGIN_X));
    draw_prompt(frame, app, prompt_area);
    if status_rows > 0 {
        draw_statusline(frame, app, inset(status_area, MARGIN_X));
    }

    crate::queue::draw(frame, app);
    // 浮层最后画：补全菜单贴输入框上沿，模态居中（覆盖正文与信息条）
    if let Some(completion) = &app.completion {
        modal::draw_completion_menu(frame, completion, prompt_area);
    }
    if app.modal.is_some() {
        modal::draw_modal(frame, app);
    }
}

/// 拖选高亮：选中行统一使用高对比度前景和 `bg_visual`，避免继承内容底色后变暗。
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
            cell.set_style(Style::new().fg(theme.text_primary).bg(theme.bg_visual));
        }
    }
}

/// 一次性提示 toast：贴滚动区右下角的胶囊，TTL 由 [`App::notice_visible`] 决定。
fn draw_notice(frame: &mut Frame, app: &App, area: Rect) {
    if !app.notice_visible() || area.height == 0 {
        return;
    }
    let Some(text) = app.notice.as_deref() else {
        return;
    };
    draw_chip(
        frame.buffer_mut(),
        app,
        area,
        area.y + area.height - 1,
        text,
        app.theme.accent_success,
    );
}

/// 上游配置不一致告警：贴滚动区右上角的胶囊，TTL 由 [`App::warning_visible`] 决定。
///
/// 与 notice 共用 [`draw_chip`]，位置与强调色不同：同屏最多一条，新告警替换旧告警。
fn draw_warning(frame: &mut Frame, app: &App, area: Rect) {
    if !app.warning_visible() || area.height == 0 {
        return;
    }
    let Some(text) = app.warning.as_deref() else {
        return;
    };
    draw_chip(
        frame.buffer_mut(),
        app,
        area,
        area.y,
        text,
        app.theme.accent_warning,
    );
}

/// 右对齐的一行胶囊：抬升底色 + 左侧强调条 + 菱形图标 + 正文。
///
/// 只铺自己那块，不整行压黑底，也不盖正文；`accent` 同时用于强调条与图标。
/// 直接写 buffer 以便单测断言落位与配色。
fn draw_chip(
    buffer: &mut ratatui::buffer::Buffer,
    app: &App,
    area: Rect,
    row: u16,
    text: &str,
    accent: Color,
) {
    if area.width < 12 || row < area.y || row >= area.y + area.height {
        return;
    }
    let theme = app.theme;
    // `▌ ◆ ` 前缀 + 左右各 1 格内边距
    const PREFIX: usize = 5;
    let max_text = usize::from(area.width).saturating_sub(PREFIX + 2);
    let text = scrollback::truncate_to_width(text.to_owned(), max_text);
    let chip_width = str_width(&text) + PREFIX + 2;
    let chip_width = u16::try_from(chip_width)
        .unwrap_or(area.width)
        .min(area.width);
    let x = area.x + area.width - chip_width;
    let fill = Style::new().bg(theme.bg_highlight);
    buffer.set_style(
        Rect {
            x,
            y: row,
            width: chip_width,
            height: 1,
        },
        fill,
    );
    buffer.set_string(
        x,
        row,
        " ▌ ",
        Style::new().fg(accent).bg(theme.bg_highlight),
    );
    buffer.set_string(
        x + 3,
        row,
        "◆ ",
        Style::new().fg(accent).bg(theme.bg_highlight),
    );
    buffer.set_string(
        x + 5,
        row,
        &text,
        Style::new().fg(theme.text_secondary).bg(theme.bg_highlight),
    );
}

/// 滚动条滑块顶端行号（0 = 轨道顶）。
///
/// 位置语义与 [`App::scroll_offset`] 相反：`scroll_offset == 0` 表示视口跟随尾部，
/// 滑块应贴底；`scroll_offset == max_offset` 表示停在历史顶端，滑块贴顶。
fn scrollbar_thumb_top(
    track: u16,
    thumb_height: u16,
    max_offset: usize,
    scroll_offset: usize,
) -> u16 {
    debug_assert!(max_offset > 0, "无溢出空间时不应绘制滑块");
    debug_assert!(thumb_height <= track);
    let travel = f64::from(track - thumb_height);
    // 距历史顶端的比例：行数规模远小于 2^53，f64 表示无精度损失
    let distance_from_top = max_offset.saturating_sub(scroll_offset) as f64 / max_offset as f64;
    // 结果已 clamp 到 [0, travel]，且 travel ≤ u16::MAX，窄化不会截断
    (travel * distance_from_top).round().clamp(0.0, travel) as u16
}

/// 右缘自绘滚动条：跟随态半暗，浏览态亮起。
fn draw_scrollbar(frame: &mut Frame, app: &App, area: Rect, max_offset: usize) {
    if max_offset == 0 || area.width < 2 || area.height < 2 {
        return;
    }
    let theme = app.theme;
    let track = area.height;
    let total = (max_offset as u32) + area.height as u32;
    let thumb_height =
        ((area.height as u32 * area.height as u32) / total.max(1)).clamp(1, track as u32) as u16;
    let thumb_top = scrollbar_thumb_top(track, thumb_height, max_offset, app.scroll_offset);
    // 贴终端右缘（在内容边距之外），不压内容
    let x = frame.area().width.saturating_sub(1);
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

fn draw_prompt(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let border = match app.focus {
        Focus::Prompt => theme.border_active,
        Focus::Scrollback => theme.border,
    };
    frame.render_widget(
        Paragraph::new(prompt_lines(app, area)).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(border)),
        ),
        area,
    );
    draw_prompt_legends(frame, app, area);
    if app.focus == Focus::Prompt {
        let (x, y) = prompt_cursor_position(app, area);
        frame.set_cursor_position((x, y));
    }
}

fn prompt_cursor_position(app: &App, area: Rect) -> (u16, u16) {
    let text_width = usize::from(area.width)
        .saturating_sub(2 + PROMPT_PAD + PROMPT_MARKER)
        .max(4);
    let before = &app.prompt.text[..app.prompt.cursor];
    let mut visual_row = 0usize;
    let mut current = "";
    for line in before.split('\n') {
        current = line;
        visual_row += wrap::wrap_text(line, text_width).len();
    }
    visual_row = visual_row.saturating_sub(1);
    let current_rows = wrap::wrap_text(current, text_width);
    let column = current_rows
        .last()
        .map(|row| str_width(row))
        .unwrap_or(0)
        .min(text_width);
    let total_rows = app
        .prompt
        .text
        .split('\n')
        .map(|line| wrap::wrap_text(line, text_width).len())
        .sum::<usize>()
        .max(1);
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    let start = total_rows.saturating_sub(visible_rows);
    let row = visual_row.saturating_sub(start).min(visible_rows - 1);
    let x = area
        .x
        .saturating_add(1)
        .saturating_add(PROMPT_PAD as u16)
        .saturating_add(PROMPT_MARKER as u16)
        .saturating_add(column as u16);
    let y = area.y.saturating_add(1).saturating_add(row as u16);
    (
        x.min(area.x.saturating_add(area.width.saturating_sub(2))),
        y,
    )
}

/// 输入框下边框上的图例：运行时显示停止操作，否则显示帮助键。
///
/// 图例直接写在边框行上，因此不额外占用终端行；空间不足时先让掉左侧提示，
/// 右侧的当前模型与模式仍然保留。
fn draw_prompt_legends(frame: &mut Frame, app: &App, area: Rect) {
    if area.height < 3 || area.width < 24 {
        return;
    }
    let theme = app.theme;
    let inner = usize::from(area.width).saturating_sub(2);
    let y = area.y + area.height - 1;

    let model = app.model_display();
    let mode = app
        .session
        .as_ref()
        .map_or("idle", |session| crate::app::mode_label(session.mode));
    let right = [
        (model, theme.gray),
        (" | ", theme.border),
        (mode, theme.accent_model),
    ];
    let right_width: usize = right.iter().map(|(text, _)| str_width(text)).sum();
    if right_width + 4 > inner {
        return;
    }

    let help_key = app
        .preferences
        .keybindings
        .get("help")
        .and_then(|keys| keys.first())
        .map_or("—", String::as_str);
    let (left_key, left_tip, left_color) = if app.turn_running {
        (
            "Esc",
            if app.cancel_requested {
                "Stopping"
            } else {
                "Stop"
            },
            if app.cancel_requested {
                theme.accent_warning
            } else {
                theme.accent_running
            },
        )
    } else {
        (help_key, "Help", theme.gray_dim)
    };
    let left_separator = " | ";
    let tips_width = str_width(left_key) + 1 + str_width(left_tip) + str_width(left_separator);
    let show_tips = tips_width + 4 + right_width + 4 <= inner;

    let buffer = frame.buffer_mut();
    if show_tips {
        let mut x = area.x + 3;
        buffer.set_string(area.x + 2, y, " ", Style::new());
        buffer.set_string(x, y, left_key, Style::new().fg(theme.gray_bright));
        x += cell_width(left_key);
        buffer.set_string(x, y, " ", Style::new());
        x += 1;
        buffer.set_string(x, y, left_tip, Style::new().fg(left_color));
        x += cell_width(left_tip);
        buffer.set_string(x, y, left_separator, Style::new().fg(theme.border));
    }
    let right_cells = u16::try_from(right_width).unwrap_or(u16::MAX);
    let mut x = area.x + area.width - 2 - right_cells;
    buffer.set_string(x.saturating_sub(1), y, " ", Style::new());
    for (text, color) in right {
        buffer.set_string(x, y, text, Style::new().fg(color));
        x += cell_width(text);
    }
    buffer.set_string(x, y, " ", Style::new());
}

/// 显示宽度 → 格数（超出 u16 的宽度按终端上限饱和）。
fn cell_width(text: &str) -> u16 {
    u16::try_from(str_width(text)).unwrap_or(u16::MAX)
}

/// 输入框内容行：`❯` 提示符、chip 占位高亮，空输入只有提示符。
///
/// 首行 `❯ ` 起头，续行按提示符宽度悬挂对齐，与正文的缩进节奏一致。
fn prompt_lines(app: &App, area: Rect) -> Vec<Line<'static>> {
    let theme = app.theme;
    let text = app.prompt_text();
    let inner_width = area.width.saturating_sub(2) as usize;
    let text_width = inner_width
        .saturating_sub(PROMPT_PAD + PROMPT_MARKER)
        .max(4);
    let marker = || {
        Span::styled(
            "❯ ".to_owned(),
            Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        )
    };
    if text.is_empty() {
        return vec![Line::from(vec![Span::raw(" "), marker()])];
    }
    let lines: Vec<Line<'static>> = text
        .split('\n')
        .flat_map(|line| wrap::wrap_text(line, text_width))
        .enumerate()
        .map(|(index, row)| {
            let mut spans: Vec<Span<'static>> = Vec::with_capacity(4);
            spans.push(Span::styled(
                " ".to_owned(),
                Style::new().fg(theme.text_primary),
            ));
            spans.push(if index == 0 {
                marker()
            } else {
                Span::styled(" ".repeat(PROMPT_MARKER), Style::new())
            });
            let mut rest = row.as_str();
            while let Some(index) = rest.find("[paste#").or_else(|| rest.find("[image#")) {
                if index > 0 {
                    spans.push(Span::styled(
                        rest[..index].to_owned(),
                        Style::new().fg(theme.text_primary),
                    ));
                }
                let end = rest[index..]
                    .find(']')
                    .map_or(rest.len(), |end| index + end + 1);
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
        .collect();
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let start = lines.len().saturating_sub(visible);
    lines.into_iter().skip(start).collect()
}

/// 顶部信息条：左＝品牌 · git 分支 · 工作目录，右＝上下文进度 · 累计费用。
fn draw_header(frame: &mut Frame, app: &mut App, area: Rect) {
    app.context_area = Rect::default();
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "moc".to_owned(),
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(branch) = &data.branch {
        spans.push(Span::styled(
            "  ⎇ ".to_owned(),
            Style::new().fg(theme.border),
        ));
        spans.push(Span::styled(
            scrollback::truncate_to_width(branch.clone(), 24),
            Style::new().fg(theme.accent_success),
        ));
    }
    if !data.workdir.is_empty() {
        spans.push(Span::styled("  ".to_owned(), Style::new()));
        let used = spans
            .iter()
            .map(|span| str_width(&span.content))
            .sum::<usize>();
        let max_workdir = usize::from(area.width).saturating_sub(used + 18).max(8);
        spans.push(Span::styled(
            scrollback::truncate_to_width(data.workdir.clone(), max_workdir),
            Style::new().fg(theme.gray),
        ));
    }

    // 右侧：上下文进度条 + 已用/窗口 + 费用
    let mut right: Vec<Span<'static>> = Vec::new();
    if let (Some(percent), Some(window)) = (data.context_percent, data.context_window) {
        let bar_color = context_bar_color(percent, theme);
        right.push(Span::styled(
            context_bar(data.context_used, window, 10),
            Style::new().fg(bar_color),
        ));
        right.push(Span::styled(
            format!(
                " {} / {}",
                short_number(data.context_used),
                short_number(window)
            ),
            Style::new().fg(theme.gray),
        ));
    }
    if let Some(cost) = data.cost {
        right.push(Span::styled(
            format!("  ${cost:.2}"),
            Style::new().fg(theme.accent_warning),
        ));
    }
    if !right.is_empty() {
        let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
        let right_width: usize = right.iter().map(|span| str_width(&span.content)).sum();
        let width = usize::from(area.width);
        // 挤不下就整段让位：宁可不显示，也不截半截进度条。
        if used + right_width + 2 <= width {
            let right_x = area.x + u16::try_from(width - right_width).unwrap_or(u16::MAX);
            if data.context_percent.is_some() {
                let context_width = right
                    .iter()
                    .take(2)
                    .map(|span| str_width(&span.content))
                    .sum::<usize>();
                app.context_area = Rect {
                    x: right_x,
                    y: area.y,
                    width: u16::try_from(context_width).unwrap_or(u16::MAX),
                    height: area.height,
                };
            }
            spans.push(Span::styled(
                " ".repeat(width - used - right_width),
                Style::new(),
            ));
            spans.extend(right);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
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

/// statusline 数据快照（内置渲染、信息条与模板共用）。
struct StatuslineData {
    model: String,
    mode: String,
    /// 当前上下文占窗口的百分比（0-100，含小数）。
    context_percent: Option<f64>,
    /// 上下文已用 token（`input_tokens`）。
    context_used: u64,
    /// 模型上下文窗口。
    context_window: Option<u64>,
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
        .map(|session| crate::app::mode_label(session.mode).to_owned())
        .unwrap_or_else(|| "idle".into());
    let context_window = app.context_window().filter(|window| *window > 0);
    let context_used = app.context_used();
    let context_percent = context_window.map(|window| context_used as f64 * 100.0 / window as f64);
    let workdir = app
        .session
        .as_ref()
        .map(|session| short_workdir(&session.cwd))
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
        context_used,
        context_window,
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

/// 工作目录缩写：`$HOME` 前缀折叠成 `~`。
fn short_workdir(cwd: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return cwd.to_owned();
    };
    if home.is_empty() {
        return cwd.to_owned();
    }
    match cwd.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => cwd.to_owned(),
    }
}

/// 上下文进度条：整数运算算占位（`round(used / window * slots)`），最后一格保留给分母。
fn context_bar(used: u64, window: u64, slots: usize) -> String {
    let slots_u64 = u64::try_from(slots).unwrap_or(1);
    let filled = used
        .saturating_mul(slots_u64)
        .saturating_add(window / 2)
        .checked_div(window)
        .map_or(0, |filled| filled.min(slots_u64));
    let filled = usize::try_from(filled).unwrap_or(slots);
    format!("『{}{}』", "━".repeat(filled), "─".repeat(slots - filled))
}

/// 上下文进度条按占用率变色。
fn context_bar_color(percent: f64, theme: &crate::theme::Theme) -> ratatui::style::Color {
    if percent >= 85.0 {
        theme.accent_error
    } else if percent >= 70.0 {
        theme.accent_warning
    } else {
        theme.accent_success
    }
}

/// token 数缩写（120000 → 120K）。
pub fn short_number(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{:.1}M", value as f64 / 1_000_000.0)
    } else if value >= 1_000 {
        format!("{}K", value / 1_000)
    } else {
        value.to_string()
    }
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct Activity {
    label: &'static str,
    color: Color,
}

fn activity(app: &App) -> Option<Activity> {
    if !app.turn_running {
        return None;
    }
    if app.cancel_requested {
        return Some(Activity {
            label: "Stopping",
            color: app.theme.accent_warning,
        });
    }
    if app
        .modal
        .as_ref()
        .is_some_and(|modal| matches!(modal, Modal::Permission { .. } | Modal::Ask { .. }))
    {
        return Some(Activity {
            label: "Waiting",
            color: app.theme.accent_warning,
        });
    }
    if app
        .blocks
        .iter()
        .rev()
        .any(|entry| matches!(&entry.block, TranscriptBlock::Tool(tool) if tool.running))
    {
        return Some(Activity {
            label: "Running",
            color: app.theme.accent_running,
        });
    }
    let label = match app.blocks.last().map(|entry| &entry.block) {
        Some(TranscriptBlock::Thinking { .. }) => "Thinking",
        Some(TranscriptBlock::Assistant { .. }) => "Responding",
        _ => "Working",
    };
    Some(Activity {
        label,
        color: app.theme.accent_running,
    })
}

fn spinner_frame(app: &App) -> &'static str {
    let elapsed_ms = app
        .turn_started_at
        .map_or(0, |started| started.elapsed().as_millis());
    let frame_count = u128::try_from(SPINNER_FRAMES.len()).expect("spinner frame count fits u128");
    let frame = (elapsed_ms / 120) % frame_count;
    let index = usize::try_from(frame).expect("spinner frame index fits usize");
    SPINNER_FRAMES[index]
}

/// 内置渲染：左侧运行状态与 todo 进度，右侧 token 统计。
fn statusline_builtin(app: &App, width: usize) -> Line<'static> {
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut has_left = false;
    if let Some(activity) = activity(app) {
        spans.push(Span::styled(
            spinner_frame(app),
            Style::new().fg(activity.color),
        ));
        spans.push(Span::styled(" ", Style::new()));
        spans.push(Span::styled(
            activity.label,
            Style::new().fg(activity.color),
        ));
        if let Some(elapsed) = data.turn_elapsed {
            spans.push(Span::styled(
                format!(" · {}s", elapsed.as_secs()),
                Style::new().fg(theme.gray),
            ));
        }
        has_left = true;
    }
    if let Some((done, total)) = data.todo {
        if has_left {
            spans.push(Span::styled(" · ", Style::new().fg(theme.border)));
        }
        spans.push(Span::styled("☑ ", Style::new().fg(theme.border)));
        spans.push(Span::styled(
            format!("{done}/{total}"),
            Style::new().fg(if done == total {
                theme.accent_success
            } else {
                theme.text_secondary
            }),
        ));
    }
    // 右侧统计段
    let right = format!(
        " ⇅ {} › {} › {:.0} tok/s",
        short_number(data.tokens_in),
        short_number(data.tokens_out),
        data.tok_s,
    );
    let mut right_cost = data
        .cost
        .map(|cost| format!(" › ${cost:.2}"))
        .unwrap_or_default();
    let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
    let available = width.saturating_sub(used + 2);
    if str_width(&right) + str_width(&right_cost) > available {
        right_cost.clear();
    }
    let right =
        scrollback::truncate_to_width(right, available.saturating_sub(str_width(&right_cost)));
    let right_width = str_width(&right) + str_width(&right_cost);
    let gap = width.saturating_sub(used + right_width).max(1);
    spans.push(Span::styled(" ".repeat(gap), Style::new()));
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
                .map(|_| (short_number(data.context_used), theme.text_secondary)),
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
            "tokens_in" => Some((short_number(data.tokens_in), theme.text_secondary)),
            "tokens_out" => Some((short_number(data.tokens_out), theme.text_secondary)),
            "tok_s" => Some((format!("{:.1} tok/s", data.tok_s), theme.accent_running)),
            "turn_elapsed" => Some((
                data.turn_elapsed
                    .map(|elapsed| format!("⏱ {}s", elapsed.as_secs()))
                    .unwrap_or_else(|| "⏱ -".into()),
                theme.gray,
            )),
            "cost" => data
                .cost
                .map(|cost| (format!("${cost:.2}"), theme.accent_warning)),
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

/// 上下文百分比标签：非零但不足 1% 时显示 `<1%`，避免截断成 0%。
fn format_context_percent(percent: f64) -> String {
    if percent > 0.0 && percent < 1.0 {
        "<1%".into()
    } else {
        format!("{percent:.0}%")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::Theme;

    /// 输入框正文行数随多行输入增长，并按显示宽度换行、受上限约束。
    #[test]
    fn prompt_rows_follow_input_wrapping() {
        let (events, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        assert_eq!(prompt_content_rows(&app, 60), 1, "空输入占一行");

        app.prompt.text = "第一行\n第二行\n第三行".into();
        assert_eq!(prompt_content_rows(&app, 60), 3);

        app.prompt.text = "一二三四五六七八九十一二三四五六七八九十".into();
        assert_eq!(prompt_content_rows(&app, 60), 1, "60 列放得下 40 格 CJK");
        assert_eq!(prompt_content_rows(&app, 30), 2, "30 列按显示宽度折成两行");

        app.prompt.text = "长输入\n".repeat(40);
        assert_eq!(prompt_content_rows(&app, 60), PROMPT_MAX_ROWS, "再长也封顶");
    }

    /// 滑块方向：跟随尾部贴底，滚向历史顶端时上移；滚轮上滚让滑块上移。
    #[test]
    fn scrollbar_thumb_tracks_offset_from_history_top() {
        // 轨道 20 行、滑块 4 行 → 可移动 16 行
        assert_eq!(scrollbar_thumb_top(20, 4, 100, 0), 16, "跟随尾部时滑块贴底");
        assert_eq!(
            scrollbar_thumb_top(20, 4, 100, 100),
            0,
            "历史顶端时滑块贴顶"
        );
        assert_eq!(scrollbar_thumb_top(20, 4, 100, 50), 8);
        assert!(scrollbar_thumb_top(20, 4, 100, 75) < scrollbar_thumb_top(20, 4, 100, 25));
        // 滑块与轨道等高（无溢出空间）时位置恒定
        assert_eq!(scrollbar_thumb_top(10, 10, 3, 3), 0);
    }

    #[test]
    fn running_statusline_uses_english_activity_labels() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.turn_running = true;
        app.turn_started_at = Some(std::time::Instant::now());

        let line = statusline_builtin(&app, 80);
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Working"));

        app.cancel_requested = true;
        let line = statusline_builtin(&app, 80);
        let rendered = line
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(rendered.contains("Stopping"));
    }

    /// 告警贴滚动区右上角、notice 贴右下角；两者同屏共存且各自只占一行。
    ///
    /// 同时校验强调色：告警用 warning、notice 用 success，用户才能一眼区分。
    #[test]
    fn warning_chip_sits_top_right_and_notice_bottom_right() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let area = Rect {
            x: 2,
            y: 1,
            width: 40,
            height: 10,
        };
        let mut terminal = Terminal::new(TestBackend::new(60, 20)).expect("构造测试终端");

        let row_text = |terminal: &Terminal<TestBackend>, row: u16| {
            (0..60)
                .filter_map(|x| terminal.backend().buffer().cell((x, row)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        };

        app.set_warning("model model → model-lite");
        app.set_notice("clipboard image attached");
        terminal
            .draw(|frame| {
                draw_warning(frame, &app, area);
                draw_notice(frame, &app, area);
            })
            .expect("绘制成功");

        let top = row_text(&terminal, area.y);
        assert!(top.contains("▌ ◆ model model → model-lite"), "{top:?}");
        let bottom = row_text(&terminal, area.y + area.height - 1);
        assert!(
            bottom.contains("▌ ◆ clipboard image attached"),
            "{bottom:?}"
        );
        assert!(
            row_text(&terminal, area.y + 1).trim().is_empty(),
            "告警只占一行"
        );

        // 右对齐：胶囊右端贴 area 右边；图标格配色区分告警与提示
        let chip_width =
            u16::try_from(str_width("model model → model-lite") + 7).expect("宽度有界");
        let icon_x = area.x + area.width - chip_width + 3;
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((icon_x, area.y))
                .expect("图标格存在")
                .fg,
            Theme::global().accent_warning
        );
        let notice_width =
            u16::try_from(str_width("clipboard image attached") + 7).expect("宽度有界");
        let notice_icon_x = area.x + area.width - notice_width + 3;
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((notice_icon_x, area.y + area.height - 1))
                .expect("图标格存在")
                .fg,
            Theme::global().accent_success
        );
    }
}

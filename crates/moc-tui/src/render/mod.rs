//! 帧绘制：布局切分 + 各区域渲染。
//!
//! 留白 → 输入框 → statusline。所有内容左右各留 [`MARGIN_X`] 格边距，行与行
//! 之间靠留白分隔；输入框随多行输入长高，其下边框兼作图例行（左＝帮助键提示，右＝模型 · 模式）。
//! 整帧先铺主题底色，滚动区由 [`ScrollbackView`] 按行缓存直写 buffer。

pub mod canvas;
pub mod chart;
pub mod diff;
pub mod graphics;
pub mod highlight;
pub mod image;
pub mod markdown;
pub mod modal;
pub mod scrollback;
pub mod subagent;
pub mod todo_hud;
pub mod wrap;

pub(crate) mod context_content;
pub(crate) mod rewind;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};

pub use scrollback::ScrollbackView;

use self::wrap::str_width;
use crate::app::{App, Focus, ScrollbackScrollbar};
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
    app.plan_model_area = Rect::default();
    app.todo_hud.begin_frame();
    let saving = app.saving_config();
    if let Some(panel) = &mut app.settings {
        app.todo_hud.on_hidden();
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
        Constraint::Length(u16::from(header_rows > 0)),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(PROMPT_BORDER_ROWS + content_rows),
        Constraint::Length(status_rows),
    ])
    .areas(frame_area);
    let scroll_area = inset(scroll_area, MARGIN_X);
    let prompt_area = inset(prompt_area, MARGIN_X);
    let sidebar_enabled = frame_area.width >= 100 && !app.subagents.is_empty();
    let (scroll_area, sidebar_area) = if sidebar_enabled {
        let [main, sidebar] =
            Layout::horizontal([Constraint::Min(60), Constraint::Length(30)]).areas(scroll_area);
        (main, sidebar)
    } else {
        (scroll_area, Rect::default())
    };
    app.scrollback_area = scroll_area;
    app.subagent_area = sidebar_area;
    app.prompt_area = prompt_area;

    // 整帧铺底色（备用屏模式下终端自身背景与主题保持一致）
    frame
        .buffer_mut()
        .set_style(frame_area, Style::new().bg(theme.bg_base));

    view.sync(app, scroll_area.width);
    let viewport = usize::from(scroll_area.height);
    // 窗口由懒渲染按渲染后高度重算：`skip_rows`/`scroll_offset` 与绘制同源。
    let window = view.ensure_visible(app, viewport, app.scroll_offset);
    app.scroll_max_offset = window.max_offset;
    app.scroll_offset = window.scroll_offset;
    app.mouse_hits.clear();
    app.chart_hits.clear();
    app.image_slots.clear();
    app.paint_lines.clear();
    let collect_lines = app.selection.is_some() || app.pending_click.is_some();
    let hover = app.hover_chart;
    view.paint(
        frame.buffer_mut(),
        scroll_area,
        window.skip_rows,
        collect_lines,
        theme,
        hover,
        scrollback::PaintSinks {
            hits: &mut app.mouse_hits,
            charts: &mut app.chart_hits,
            slots: &mut app.image_slots,
            lines: &mut app.paint_lines,
        },
    );
    view.trim_outside(window.skip_rows, viewport);
    highlight_selection(frame, app, scroll_area);
    // 滚动条贴终端右缘：与内容边距无关，避免压住正文
    draw_scrollbar(frame, app, scroll_area, window.max_offset);
    // todo HUD 在转写之上；notice/warning 最后画，临时提示始终可读。
    if app.modal.is_none() {
        todo_hud::draw(frame, app, scroll_area, header_rows > 0);
    } else {
        app.todo_hud.on_hidden();
    }
    draw_notice(frame, app, scroll_area);
    draw_warning(frame, app, scroll_area);

    draw_header(frame, app, inset(header_area, MARGIN_X));
    draw_prompt(frame, app, prompt_area);
    if status_rows > 0 {
        draw_statusline(frame, app, inset(status_area, MARGIN_X));
    }
    if sidebar_enabled {
        subagent::draw_sidebar(frame, app, sidebar_area);
    }

    crate::queue::draw(frame, app);
    // 浮层最后画：补全菜单贴输入框上沿，就地模式菜单贴模式图例，居中模态覆盖全部。
    if let Some(completion) = &mut app.completion {
        modal::draw_completion_menu(frame, completion, prompt_area);
    }
    if app.modal.is_none() {
        modal::draw_mode_menu(frame, app);
    } else {
        // 模态是阻塞层：不再保留就地菜单，避免两层浮层叠加。
        app.mode_menu = None;
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

/// 右缘自绘滚动条：轨道常暗，滑块常亮。
///
/// 滑块行数按视口占全文的比例定长，只有位置随滚动偏移移动——滚动时长度不变。
/// 几何写回 [`App::scrollbar`]，鼠标点击与拖拽按同一份几何换算滚动位置。
fn draw_scrollbar(frame: &mut Frame, app: &mut App, area: Rect, max_offset: usize) {
    app.scrollbar = ScrollbackScrollbar::default();
    if max_offset == 0 || area.width < 2 || area.height < 2 {
        return;
    }
    let theme = app.theme;
    let track_height = area.height;
    let total = (max_offset as u32) + track_height as u32;
    let thumb = ((track_height as u32 * track_height as u32) / total.max(1))
        .clamp(1, track_height as u32) as u16;
    // 贴终端右缘（在内容边距之外），不压内容
    let x = frame.area().width.saturating_sub(1);
    app.scrollbar = ScrollbackScrollbar {
        track: Rect::new(x, area.y, 1, track_height),
        thumb,
        max_offset,
    };
    let thumb_top = app.scrollbar.thumb_top(app.scroll_offset);
    for row in 0..track_height {
        let in_thumb = row >= thumb_top && row < thumb_top.saturating_add(thumb);
        frame.buffer_mut().set_string(
            x,
            area.y + row,
            "▐",
            Style::new().fg(if in_thumb {
                theme.scrollbar_fg
            } else {
                theme.scrollbar_bg
            }),
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

fn prompt_scroll_offset(visual_row: usize, total_rows: usize, visible_rows: usize) -> usize {
    if total_rows <= visible_rows {
        0
    } else {
        let max_start = total_rows.saturating_sub(visible_rows);
        if visual_row >= max_start {
            max_start
        } else {
            visual_row
        }
    }
}

/// 输入框光标的屏幕坐标（贴图后把光标送回这里）。
pub(crate) fn prompt_cursor_position(app: &App, area: Rect) -> (u16, u16) {
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
        .map_or(0, |row| str_width(row))
        .min(text_width);
    let total_rows = app
        .prompt
        .text
        .split('\n')
        .map(|line| wrap::wrap_text(line, text_width).len())
        .sum::<usize>()
        .max(1);
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    let start = prompt_scroll_offset(visual_row, total_rows, visible_rows);
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

pub(crate) fn prompt_cursor_from_point(
    text: &str,
    area: Rect,
    col: u16,
    row: u16,
) -> Option<usize> {
    if area.width < 6 || area.height < 3 {
        return None;
    }
    let inner_width = usize::from(area.width.saturating_sub(2));
    let text_width = inner_width
        .saturating_sub(PROMPT_PAD + PROMPT_MARKER)
        .max(4);
    let top = area.y + 1;
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    if row < top || row >= top + visible_rows as u16 {
        return None;
    }
    let left = area.x + 1 + PROMPT_PAD as u16 + PROMPT_MARKER as u16;
    let target_col = usize::from(col.saturating_sub(left));

    let mut visual_rows: Vec<(usize, usize, String)> = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        let wrapped = wrap::wrap_text(line, text_width);
        let mut line_offset = offset;
        for sub in wrapped {
            let len = sub.len();
            visual_rows.push((line_offset, line_offset + len, sub));
            line_offset += len;
            if line_offset < text.len() && text.as_bytes()[line_offset] == b' ' {
                line_offset += 1;
            }
        }
        offset += line.len() + 1;
    }
    let total_rows = visual_rows.len().max(1);
    let start = total_rows.saturating_sub(visible_rows);
    let clicked_visual_row = start + usize::from(row - top);
    if let Some((start_offset, _, sub_text)) = visual_rows.get(clicked_visual_row) {
        let mut accumulated_width = 0;
        let mut char_offset = 0;
        for ch in sub_text.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if accumulated_width + w > target_col {
                break;
            }
            accumulated_width += w;
            char_offset += ch.len_utf8();
        }
        Some((*start_offset + char_offset).min(text.len()))
    } else {
        Some(text.len())
    }
}

/// 输入框下边框上的图例：运行时显示停止操作，否则显示帮助键。
///
/// 图例直接写在边框行上，因此不额外占用终端行；空间不足时先让掉左侧提示，
/// 右侧的当前模型与模式仍然保留。
fn draw_prompt_legends(frame: &mut Frame, app: &mut App, area: Rect) {
    app.mode_area = Rect::default();
    if area.height < 3 || area.width < 24 {
        return;
    }
    let theme = app.theme;
    let inner = usize::from(area.width).saturating_sub(2);
    let y = area.y + area.height - 1;

    let model = app.model_display();
    let mode = app.display_mode().map_or("idle", crate::app::mode_label);
    let permission = app
        .session
        .as_ref()
        .map_or("full", |session| session.permission.as_str());
    let right = [
        (model, theme.gray),
        (" | ", theme.border),
        (mode, theme.accent_model),
        (" | ", theme.border),
        (permission, theme.accent_model),
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
    let (left_key, left_tip, left_color) = if app.prompt_running {
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
    let right_start = area.x + area.width - 2 - right_cells;
    let mode_start = right_start + cell_width(model) + cell_width(" | ");
    let mode_cells = cell_width(mode);
    let mut x = right_start;
    buffer.set_string(x.saturating_sub(1), y, " ", Style::new());
    for (text, color) in right {
        buffer.set_string(x, y, text, Style::new().fg(color));
        x += cell_width(text);
    }
    buffer.set_string(x, y, " ", Style::new());
    // 模式名单独留命中区：鼠标点击它就地图例弹出模式菜单，图例本身不变。
    app.mode_area = Rect {
        x: mode_start,
        y,
        width: mode_cells,
        height: 1,
    };
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
    let before = &app.prompt.text[..app.prompt.cursor.min(app.prompt.text.len())];
    let mut visual_row = 0usize;
    for line in before.split('\n') {
        visual_row += wrap::wrap_text(line, text_width).len();
    }
    visual_row = visual_row.saturating_sub(1);
    let start = prompt_scroll_offset(visual_row, lines.len().max(1), visible);
    lines.into_iter().skip(start).take(visible).collect()
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
    // 聚焦子代理：头部常驻标识 + 返回手势，避免忘了自己在谁的会话里。
    if app.focused.is_some() {
        let title = app
            .session
            .as_ref()
            .map_or_else(|| "子代理".to_owned(), |session| session.title.clone());
        spans.push(Span::styled(
            "  ▸ 子代理 ".to_owned(),
            Style::new().fg(theme.accent_tool),
        ));
        spans.push(Span::styled(
            scrollback::truncate_to_width(title, 24),
            Style::new().fg(theme.text_primary),
        ));
        spans.push(Span::styled(
            "  esc 返回".to_owned(),
            Style::new().fg(theme.gray_dim),
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
            format!(" {} / {}", format_tokens(data.context_used), format_tokens(window)),
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
    /// 会话累计输入 token（与 `/usage` 同源）。
    tokens_in: u64,
    /// 会话累计输出 token（与 `/usage` 同源）。
    tokens_out: u64,
    /// 会话累计输出速率：输出 token 总量 / 模型激活时间。
    tok_s: f64,
    prompt_elapsed: Option<std::time::Duration>,
    cost: Option<f64>,
}

fn statusline_data(app: &App) -> StatuslineData {
    let mode = app.display_mode().map_or_else(
        || "idle".into(),
        |mode| crate::app::mode_label(mode).to_owned(),
    );
    let context_window = app.context_window().filter(|window| *window > 0);
    let context_used = app.context_used();
    let context_percent = context_window.map(|window| context_used as f64 * 100.0 / window as f64);
    let workdir = app
        .session
        .as_ref()
        .map(|session| short_workdir(&session.cwd))
        .unwrap_or_default();
    let prompt_elapsed = if app.prompt_running {
        app.prompt_started_at.map(|at| at.elapsed())
    } else {
        None
    };
    let cost = (app.cost_usd > 0.0).then_some(app.cost_usd);
    // 右下角统计是整个会话的累计值（与 `/usage` 同源），不是单次调用。
    let (tokens_in, tokens_out) = app.session_tokens();
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
        tokens_in,
        tokens_out,
        tok_s: app.session_tokens_per_second(),
        prompt_elapsed,
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

/// 数量分档：`(一单位多少 token, 后缀)`，按升序排列。
const TOKEN_UNITS: [(u64, &str); 3] = [(1_000, "k"), (1_000_000, "m"), (1_000_000_000, "b")];

/// token 数按 1k / 1m / 1b 分档显示：`999 → 999`、`1500 → 1.5k`、`1_200_000 → 1.2m`。
///
/// 够得着下一个单位就换单位，绝不写成 `1200k`；不足 1000 直接给原数。进位后
/// 保留两位小数并去掉多余的 0（`1m`、`1.5k`）；舍入把 `999.99k` 抬成 1000 时
/// 一并完成跨档进位（`999_999 → 1m`）。四舍五入在整数上完成，不引入浮点误差。
pub fn format_tokens(value: u64) -> String {
    if value < TOKEN_UNITS[0].0 {
        return value.to_string();
    }
    let index = TOKEN_UNITS
        .iter()
        .rposition(|(scale, _)| value >= *scale)
        .unwrap_or(0);
    let (scale, unit) = TOKEN_UNITS[index];
    // 以 0.01 个当前单位为步长四舍五入。
    let hundredths = (u128::from(value) * 100 + u128::from(scale) / 2) / u128::from(scale);
    let (hundredths, unit) = match TOKEN_UNITS.get(index + 1) {
        // 各档之间正好差 1000 倍，跨档时直接少三位小数。
        Some((_, next)) if hundredths >= 100_000 => (hundredths / 1_000, *next),
        _ => (hundredths, unit),
    };
    let whole = hundredths / 100;
    let frac = hundredths % 100;
    match frac {
        0 => format!("{whole}{unit}"),
        frac if frac.is_multiple_of(10) => format!("{whole}.{}{unit}", frac / 10),
        frac => format!("{whole}.{frac:02}{unit}"),
    }
}

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct Activity {
    label: &'static str,
    color: Color,
}

fn activity(app: &App) -> Option<Activity> {
    if !app.prompt_running {
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
        .prompt_started_at
        .map_or(0, |started| started.elapsed().as_millis());
    let frame_count = u128::try_from(SPINNER_FRAMES.len()).expect("spinner frame count fits u128");
    let frame = (elapsed_ms / 120) % frame_count;
    let index = usize::try_from(frame).expect("spinner frame index fits usize");
    SPINNER_FRAMES[index]
}

/// 内置渲染：左侧运行状态，右侧会话累计 token 统计（与 `/usage` 同源）。
fn statusline_builtin(app: &App, width: usize) -> Line<'static> {
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if app.read_only {
        spans.push(Span::styled("只读", Style::new().fg(theme.accent_warning)));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray)));
    }
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
        if let Some(elapsed) = data.prompt_elapsed {
            spans.push(Span::styled(
                format!(" · {}s", elapsed.as_secs()),
                Style::new().fg(theme.gray),
            ));
        }
    }
    // 右侧统计段
    let right = format!(
        " ⇅ {} › {} › {:.0} tok/s",
        format_tokens(data.tokens_in),
        format_tokens(data.tokens_out),
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
                .map(|_| (format_tokens(data.context_used), theme.text_secondary)),
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
            "tokens_in" => Some((format_tokens(data.tokens_in), theme.text_secondary)),
            "tokens_out" => Some((format_tokens(data.tokens_out), theme.text_secondary)),
            "tok_s" => Some((format!("{:.1} tok/s", data.tok_s), theme.accent_running)),
            "prompt_elapsed" => Some((
                data.prompt_elapsed
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

    /// 滑块方向：跟随尾部贴底，滚向历史顶端时上移；拖拽按同一份几何换算。
    #[test]
    fn scrollbar_thumb_tracks_offset_from_history_top() {
        let bar = |track: u16, thumb: u16, max_offset: usize| ScrollbackScrollbar {
            track: Rect::new(0, 0, 1, track),
            thumb,
            max_offset,
        };
        // 轨道 20 行、滑块 4 行 → 可移动 16 行
        assert_eq!(bar(20, 4, 100).thumb_top(0), 16, "跟随尾部时滑块贴底");
        assert_eq!(bar(20, 4, 100).thumb_top(100), 0, "历史顶端时滑块贴顶");
        assert_eq!(bar(20, 4, 100).thumb_top(50), 8);
        assert!(bar(20, 4, 100).thumb_top(75) < bar(20, 4, 100).thumb_top(25));
        // 滑块与轨道等高（无溢出空间）时位置恒定
        assert_eq!(bar(10, 10, 3).thumb_top(3), 0);
    }

    /// 拖拽换算：轨道顶端对应历史顶端，轨道末端对应跟随尾部；抓取点保持。
    #[test]
    fn scrollbar_drag_maps_rows_to_offsets() {
        let bar = ScrollbackScrollbar {
            track: Rect::new(79, 4, 1, 10),
            thumb: 3,
            max_offset: 20,
        };
        // 轨道 10 行、滑块 3 行 → 可移动 7 行
        assert_eq!(bar.offset_at(4, 0), 20, "轨道顶端 = 历史顶端");
        assert_eq!(bar.offset_at(11, 0), 0, "轨道末端 = 跟随尾部");
        assert_eq!(bar.offset_at(8, 0), 9, "轨道中线落在中间偏移附近");
        // 抓取点：按在滑块第 2 行时，该行保持指向原偏移。
        assert_eq!(bar.offset_at(9, 1), 9);
        // 轨道之外的行夹到两端，不越界。
        assert_eq!(bar.offset_at(0, 0), 20);
        assert_eq!(bar.offset_at(30, 0), 0);
        // 滑块与轨道等高：永远回到底部（无溢出空间）。
        let flat = ScrollbackScrollbar {
            track: Rect::new(79, 4, 1, 10),
            thumb: 10,
            max_offset: 0,
        };
        assert_eq!(flat.offset_at(8, 4), 0);
    }

    /// 滚动只移动滑块，不改它的长度：整列仍是「一行亮滑块 + 九行暗轨道」。
    #[test]
    fn scrollbar_thumb_keeps_length_when_scrolled() {
        use crate::theme::ColorLevel;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        fn paint(
            terminal: &mut Terminal<TestBackend>,
            app: &mut App,
            area: Rect,
            max_offset: usize,
        ) {
            terminal
                .draw(|frame| draw_scrollbar(frame, app, area, max_offset))
                .expect("绘制成功");
        }
        fn column(terminal: &Terminal<TestBackend>, area: Rect) -> Vec<Color> {
            (area.y..area.y + area.height)
                .map(|row| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((79, row))
                        .expect("滚动条贴在终端右缘")
                        .fg
                })
                .collect()
        }

        // 终端能力固定为真彩色：无色能力下轨道与滑块都退化成默认前景色。
        let theme: &'static Theme = Box::leak(Box::new(Theme::resolved(ColorLevel::TrueColor)));
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(theme, events);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let area = Rect::new(3, 2, 60, 10);
        // 总量 100 行、视口 10 行 → 滑块 1 行。
        let max_offset = 90;

        paint(&mut terminal, &mut app, area, max_offset);
        assert_eq!(app.scrollbar.track, Rect::new(79, area.y, 1, area.height));
        assert_eq!(app.scrollbar.thumb, 1);
        let following = column(&terminal, area);
        assert_eq!(
            following
                .iter()
                .filter(|fg| **fg == theme.scrollbar_fg)
                .count(),
            1,
            "跟随尾部时只有滑块是亮色"
        );
        assert_eq!(following[9], theme.scrollbar_fg, "滑块贴底");
        assert!(
            following[..9].iter().all(|fg| *fg == theme.scrollbar_bg),
            "轨道保持暗色"
        );

        app.scroll_offset = max_offset / 2;
        paint(&mut terminal, &mut app, area, max_offset);
        let scrolled = column(&terminal, area);
        assert_eq!(
            scrolled
                .iter()
                .filter(|fg| **fg == theme.scrollbar_fg)
                .count(),
            1,
            "滚动后滑块长度不变"
        );
        assert_eq!(scrolled[5], theme.scrollbar_fg, "滑块移到轨道中部");
        assert_eq!(
            scrolled
                .iter()
                .filter(|fg| **fg == theme.scrollbar_bg)
                .count(),
            9,
            "轨道其余行仍是暗色，不随滚动整列点亮"
        );
    }

    /// 整帧绘制把滚动条几何写回 `App`：命中区与拖拽换算读的就是这份几何。
    #[test]
    fn frame_draw_records_scrollbar_geometry() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let text = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_block(TranscriptBlock::Assistant { text });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        assert!(app.scroll_max_offset > 0, "长内容应可滚动");
        assert_eq!(app.scrollbar.track.x, 79, "滚动条贴在终端右缘");
        assert_eq!(app.scrollbar.track.height, app.scrollback_area.height);
        assert_eq!(app.scrollbar.max_offset, app.scroll_max_offset);
        assert!(
            app.scrollbar.thumb < app.scrollbar.track.height,
            "滑块只占视口比例，不铺满轨道"
        );
    }

    /// 模式图例的命中区正好盖住模式名；就地菜单右上对齐图例、整体贴在其上方。
    #[test]
    fn mode_legend_hit_area_carries_the_menu() {
        use moc_protocol::methods::SessionInfo;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.session = Some(SessionInfo {
            id: "ses_test".into(),
            title: "测试会话".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            mode: moc_protocol::SessionMode::Plan,
            parent_session_id: None,
            forked_from: None,
            cwd: "/tmp".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        let row_text = |terminal: &Terminal<TestBackend>, area: Rect| {
            (area.x..area.x.saturating_add(area.width))
                .filter_map(|x| terminal.backend().buffer().cell((x, area.y)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        };
        let legend = app.mode_area;
        assert_eq!(legend.height, 1);
        assert_eq!(row_text(&terminal, legend), "plan", "命中区正好是模式名");

        app.open_mode_menu();
        assert_eq!(
            app.mode_menu.as_ref().map(|menu| menu.selected),
            Some(1),
            "默认高亮当前模式"
        );
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        let menu = app.mode_menu.as_ref().expect("菜单保持打开");
        assert_eq!(menu.area.right(), legend.right(), "右缘对齐模式图例");
        assert_eq!(menu.area.bottom(), legend.y, "整体贴在图例上方");
        assert_eq!(menu.entry_at(menu.area.x + 4, menu.area.y + 1), Some(0));
        // 当前模式行位于图例上方第 4 行；CLM 作为第四个可持久模式加入菜单。
        let plan_y = legend.y - 4;
        assert!(
            row_text(
                &terminal,
                Rect {
                    x: menu.area.x + 1,
                    y: plan_y,
                    width: 12,
                    height: 1,
                },
            )
            .starts_with("❯ plan"),
            "当前模式行同时是菜单高亮行"
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((menu.area.x + 3, plan_y))
                .expect("表头格存在")
                .fg,
            Theme::global().accent_model
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((menu.area.x + 3, menu.area.y + 1))
                .expect("首行格存在")
                .fg,
            Theme::global().text_primary
        );

        // 本次提示临时问答：图例改显 `ask`，会话持久模式不变；关闭菜单后即恢复 plan。
        app.mode_menu = None;
        app.transient_mode = Some(moc_protocol::SessionMode::Ask);
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert_eq!(row_text(&terminal, app.mode_area), "ask");
        assert_eq!(
            app.session.as_ref().map(|session| session.mode),
            Some(moc_protocol::SessionMode::Plan),
            "临时覆盖不得改写会话模式"
        );
        app.transient_mode = None;
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert_eq!(row_text(&terminal, app.mode_area), "plan");
    }

    #[test]
    fn running_statusline_uses_english_activity_labels() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.prompt_running = true;
        app.prompt_started_at = Some(std::time::Instant::now());

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

    /// 右下角统计读会话累计（与 `/usage` 同源），单次请求的 usage 不能冒充总量。
    #[test]
    fn statusline_reports_session_totals_not_last_call() {
        use moc_protocol::{ModelUsageSummary, SessionUsageResult};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.usage = moc_protocol::Usage {
            input_tokens: 400,
            output_tokens: 80,
            ..Default::default()
        };
        let rendered = |app: &App| {
            statusline_builtin(app, 80)
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let initial = rendered(&app);
        assert!(initial.contains("⇅ 0 › 0 › 0 tok/s"), "{initial}");
        assert!(
            !initial.contains("400"),
            "单次调用不能当会话总量：{initial}"
        );

        let request = app.begin_usage_request();
        app.apply_session_usage(
            request,
            SessionUsageResult {
                models: Vec::new(),
                total: ModelUsageSummary {
                    model: "total".into(),
                    calls: 3,
                    input_tokens: 1_200_000,
                    output_tokens: 12_345,
                    cached_input_tokens: None,
                    cache_creation_input_tokens: None,
                    reasoning_tokens: None,
                    active_ms: 60_000,
                    ttft_ms: 12_000,
                    ttft_calls: 2,
                    gen_ms: 48_000,
                },
                tool_ms: 0,
            },
        );
        let line = rendered(&app);
        // 12_345 ÷ 48s 的生成时间 ≈ 257 tok/s，首 token 等待不计入。
        assert!(
            line.contains("⇅ 1.2m › 12.35k › 257 tok/s"),
            "会话累计与单位：{line}"
        );
    }

    /// token 按 k/m/b 分档：两位小数、去掉多余的 0，不足 1000 给原数，
    /// 舍入抬到下一档时直接换单位（不写 `1000k`）。
    #[test]
    fn tokens_pick_the_largest_unit() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(9), "9");
        assert_eq!(format_tokens(150), "150");
        assert_eq!(format_tokens(999), "999", "不足 1000 不换单位");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(1_500), "1.5k");
        assert_eq!(format_tokens(12_345), "12.35k");
        assert_eq!(format_tokens(500_000), "500k");
        assert_eq!(format_tokens(999_950), "999.95k");
        assert_eq!(format_tokens(999_999), "1m", "0.999999m 舍入后直接进位到 m");
        assert_eq!(format_tokens(1_000_000), "1m");
        assert_eq!(
            format_tokens(1_200_000),
            "1.2m",
            "超过百万改用 m，不写 1200k"
        );
        assert_eq!(format_tokens(12_345_678), "12.35m");
        assert_eq!(
            format_tokens(999_999_999),
            "1b",
            "不足 1b 但舍入到 1000m 时进位"
        );
        assert_eq!(format_tokens(1_000_000_000), "1b");
        assert_eq!(format_tokens(1_234_567_890), "1.23b");
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

    fn todo_phases(
        items: Vec<(moc_protocol::methods::TodoStatus, &str)>,
    ) -> Vec<moc_protocol::methods::TodoPhase> {
        vec![moc_protocol::methods::TodoPhase {
            phase: "阶段".into(),
            items: items
                .into_iter()
                .map(|(status, task)| moc_protocol::methods::TodoItem {
                    status,
                    task: task.into(),
                    reason: None,
                })
                .collect(),
        }]
    }

    /// 胶囊贴转写区首行右端，转写区宽度不因 HUD 改变。
    #[test]
    fn todo_hud_chip_sits_in_scroll_top_right() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::TodoStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = todo_phases(vec![(TodoStatus::InProgress, "实现右上角 todo HUD")]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        let chip = app.todo_hud.chip_area;
        assert_eq!(chip.height, 1);
        assert_eq!(chip.y, app.scrollback_area.y);
        assert_eq!(chip.right(), app.scrollback_area.right());
        assert_eq!(app.scrollback_area.width, 74, "转写区宽度保持不变");
        // TestBackend 会给宽字符的尾部补空格格位，比较前先去掉空格。
        let row: String = (chip.x..chip.right())
            .filter_map(|x| terminal.backend().buffer().cell((x, chip.y)))
            .map(|cell| cell.symbol())
            .collect::<String>()
            .chars()
            .filter(|ch| *ch != ' ')
            .collect();
        assert!(row.contains("任务"), "{row:?}");
        assert!(row.contains("0/1"), "{row:?}");
        assert!(row.contains("实现右上角"), "{row:?}");
    }

    /// 无 todo、矮终端（header 消失）与过窄终端都不绘制 HUD。
    #[test]
    fn todo_hud_hides_without_space_or_todos() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::TodoStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let mut view = ScrollbackView::new();

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(app.todo_hud.chip_area.is_empty());

        app.todos = todo_phases(vec![(TodoStatus::Pending, "任务")]);
        let mut short = Terminal::new(TestBackend::new(80, 9)).expect("构造测试终端");
        short
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(app.todo_hud.chip_area.is_empty(), "header 隐藏时不叠加浮层");

        let mut narrow = Terminal::new(TestBackend::new(27, 24)).expect("构造测试终端");
        narrow
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(app.todo_hud.chip_area.is_empty(), "过窄时不绘制");
    }

    /// warning 也占首行右端：todo 胶囊自动下移一行。
    #[test]
    fn todo_hud_chip_moves_below_warning() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::TodoStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = todo_phases(vec![(TodoStatus::InProgress, "任务")]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        let base = app.todo_hud.chip_area.y;

        app.set_warning("model model → model-lite");
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert_eq!(app.todo_hud.chip_area.y, base + 1);
    }

    /// 侧栏出现时 HUD 对齐主转写区右缘，不盖住侧栏。
    #[test]
    fn todo_hud_stays_left_of_sidebar() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::{AgentInfo, AgentStatus, TodoStatus};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = todo_phases(vec![(TodoStatus::InProgress, "任务")]);
        app.subagents.insert(
            "agent".into(),
            crate::subagent::SubagentView::new(AgentInfo {
                id: "agent".into(),
                parent_id: "root".into(),
                title: "子代理".into(),
                model: "test/model".into(),
                cwd: "/tmp".into(),
                status: AgentStatus::Running,
                created_at_ms: 0,
                updated_at_ms: 0,
                last_message: None,
            }),
        );
        let mut terminal = Terminal::new(TestBackend::new(120, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        assert_eq!(app.subagent_area.width, 30);
        assert_eq!(app.todo_hud.chip_area.right(), app.scrollback_area.right());
        assert!(app.todo_hud.chip_area.right() <= app.subagent_area.x);
    }

    /// 内置 statusline 不再显示 todo；模板占位符 `{todo}` 保持兼容。
    #[test]
    fn builtin_statusline_drops_todo_but_template_keeps_it() {
        use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = vec![TodoPhase {
            phase: "阶段".into(),
            items: vec![
                TodoItem {
                    status: TodoStatus::Completed,
                    task: "一".into(),
                    reason: None,
                },
                TodoItem {
                    status: TodoStatus::Pending,
                    task: "二".into(),
                    reason: None,
                },
            ],
        }];
        let rendered = |line: Line<'static>| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let builtin = rendered(statusline_builtin(&app, 80));
        assert!(!builtin.contains("☑"), "{builtin}");
        assert!(!builtin.contains("1/2"), "{builtin}");
        let templated = rendered(statusline_from_template(&app, "{todo}"));
        assert!(templated.contains("☑ 1/2"), "{templated}");
    }

    /// 任务名超宽时只有折叠态滚动；展开卡片后停掉定时重绘。
    #[test]
    fn todo_hud_marquee_runs_only_when_collapsed() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::TodoStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let long = "很长的任务".repeat(20);
        app.todos = todo_phases(vec![(TodoStatus::InProgress, long.as_str())]);
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(app.todo_hud.marquee_active);

        app.todo_hud.pinned = true;
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(!app.todo_hud.marquee_active, "卡片展开时胶囊不滚动");
        assert!(app.todo_hud.card_visible);
    }

    /// 卡片默认贴转写区右上角；固定后使用拖动记忆的位置并夹在区内。
    #[test]
    fn todo_hud_card_defaults_top_right_and_uses_drag_origin() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::TodoStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = todo_phases(vec![(TodoStatus::InProgress, "任务")]);
        app.todo_hud.pinned = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        let card = app.todo_hud.card_area;
        assert!(app.todo_hud.card_visible);
        assert_eq!(card.right(), app.scrollback_area.right());
        assert_eq!(card.y, app.scrollback_area.y);
        assert!(card.bottom() <= app.scrollback_area.bottom());

        app.todo_hud.drag_origin = Some((5, 7));
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        let card = app.todo_hud.card_area;
        assert_eq!((card.x, card.y), (5, 7));
        assert!(card.right() <= app.scrollback_area.right());
    }

    /// 条目过多时卡片不越过转写区底边，并给出截断提示。
    #[test]
    fn todo_hud_card_caps_height_and_reports_overflow() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        use moc_protocol::methods::TodoStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.todos = todo_phases((0..30).map(|_| (TodoStatus::Pending, "条目")).collect());
        app.todo_hud.pinned = true;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        let card = app.todo_hud.card_area;
        assert!(card.height <= app.scrollback_area.height);
        assert!(card.bottom() <= app.scrollback_area.bottom());
        let mut text = String::new();
        for y in card.y..card.bottom() {
            for x in card.x..card.right() {
                if let Some(cell) = terminal.backend().buffer().cell((x, y)) {
                    text.push_str(cell.symbol());
                }
            }
        }
        let compact: String = text.chars().filter(|ch| *ch != ' ').collect();
        assert!(compact.contains("还有"), "{compact}");
    }
}

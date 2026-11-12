//! 帧绘制：布局切分 + 各区域渲染。
//!
//! 留白 → 输入框 → statusline。所有内容左右各留 [`MARGIN_X`] 格边距，行与行
//! 之间靠留白分隔；输入框随多行输入长高，其下边框兼作图例行（左＝快捷键提示，右＝模型 · 模式）。
//! 整帧先铺主题底色，滚动区由 [`ScrollbackView`] 按行缓存直写 buffer。

pub mod btw;
pub mod canvas;
pub mod chart;
pub mod diff;
pub mod graphics;
pub mod highlight;
pub mod image;
pub mod labels;
pub mod markdown;
pub mod mermaid;
pub mod modal;
pub mod overlay;
mod rows;
pub mod scrollback;
pub mod subagent;
pub mod todo_hud;
pub mod wrap;

pub(crate) mod context_content;
#[cfg(test)]
mod guards;
pub(crate) mod rewind;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Paragraph};
use std::borrow::Cow;

pub use scrollback::ScrollbackView;

use self::wrap::str_width;
use crate::app::{App, Focus, ScrollbackScrollbar};
use crate::modal::Modal;
use crate::transcript::Block as TranscriptBlock;

/// 内容区左右边距（格）：主界面与设置页共用。
pub(crate) const MARGIN_X: u16 = 3;
/// 转写块标题（bullet / 工具名 / TODO）左缩进（格）。
pub const TITLE_INDENT: usize = 1;
/// 转写块正文左缩进（格）：助手 markdown、工具正文、思考与 IRC 正文共用这一列。
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

/// 输入框按显示列硬折行，保留空白和原始字节偏移；额外留一格显示行尾光标。
fn prompt_rows(text: &str, width: u16) -> Vec<(usize, &str)> {
    let width = usize::from(width)
        .saturating_sub(2 + PROMPT_PAD + PROMPT_MARKER + 1)
        .max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut column = 0;
    for (offset, ch) in text.char_indices() {
        if ch == '\n' {
            rows.push((start, &text[start..offset]));
            start = offset + 1;
            column = 0;
            continue;
        }
        let cells = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if column > 0 && column + cells > width {
            rows.push((start, &text[start..offset]));
            start = offset;
            column = 0;
        }
        column += cells;
    }
    rows.push((start, &text[start..]));
    rows
}

fn prompt_cursor_row(rows: &[(usize, &str)], cursor: usize) -> usize {
    rows.partition_point(|(start, _)| *start <= cursor)
        .saturating_sub(1)
}

/// 输入框正文行数：按当前输入换行所需行数（1 行起，上限 [`PROMPT_MAX_ROWS`]）。
fn prompt_content_rows(app: &App, width: u16) -> usize {
    prompt_rows(app.prompt_text(), width)
        .len()
        .clamp(1, PROMPT_MAX_ROWS)
}

pub fn draw(frame: &mut Frame, app: &mut App, view: &mut ScrollbackView) {
    app.plan_body_area = Rect::default();
    app.plan_row_map.clear();
    app.key_hits.clear();
    app.legend_tip_area = Rect::default();
    app.usage_area = Rect::default();
    app.jump_bottom_area = Rect::default();
    app.header_back_area = Rect::default();
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
    let sidebar_enabled = frame_area.width >= 100 && app.subagent_sidebar_visible();
    let (scroll_area, sidebar_area) = if sidebar_enabled {
        let [main, _gap, sidebar] = Layout::horizontal([
            Constraint::Min(60),
            Constraint::Length(1),
            Constraint::Length(32),
        ])
        .areas(scroll_area);
        (main, sidebar)
    } else {
        // 侧栏不画的帧必须清掉残留命中区，否则点原位置还会聚焦代理。
        app.subagent_hits.clear();
        app.subagent_close_area = Rect::default();
        (scroll_area, Rect::default())
    };
    let scroll_area = btw::split(app, scroll_area);
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
    app.link_hits.clear();
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
        app.hover_link,
        scrollback::PaintSinks {
            hits: &mut app.mouse_hits,
            charts: &mut app.chart_hits,
            links: &mut app.link_hits,
            slots: &mut app.image_slots,
            lines: &mut app.paint_lines,
        },
    );
    // 粘性折叠头：盖住顶行的可折叠块（思考 / 工具等）把标题钉在首行，
    // 命中区一并补登——正文再长也能随时点回折叠。
    if let Some((index, title)) = view.sticky_title(app, window.skip_rows) {
        frame
            .buffer_mut()
            .set_line(scroll_area.x, scroll_area.y, title, scroll_area.width);
        app.mouse_hits
            .push((index, scroll_area.y, scroll_area.y + 1));
        // 首行已被标题覆盖，拖选复制登记的文本同步换成标题，保持所见即所得。
        if let Some((_, text)) = app
            .paint_lines
            .iter_mut()
            .find(|(row, _)| *row == scroll_area.y)
        {
            *text = title
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect();
        }
    }
    view.trim_outside(window.skip_rows, viewport);
    highlight_selection(frame, app, scroll_area);
    // 滚动条贴终端右缘：与内容边距无关，避免压住正文
    draw_scrollbar(frame, app, scroll_area, window.max_offset);
    // todo HUD 在转写之上。
    if app.modal.is_none() {
        todo_hud::draw(frame, app, scroll_area, header_rows > 0);
        draw_jump_bottom(frame, app, scroll_area);
    } else {
        app.todo_hud.on_hidden();
    }

    draw_header(frame, app, inset(header_area, MARGIN_X));
    // 折叠的提问卡占住输入框位：输入框本体（含光标）不画，
    // 细条由 draw_ask_modal 以 prompt_area 为锚补画在最后。
    if !matches!(app.modal, Some(Modal::Ask { parked: true, .. })) {
        draw_prompt(frame, app, prompt_area);
    }
    if status_rows > 0 {
        draw_statusline(frame, app, inset(status_area, MARGIN_X));
    }
    if sidebar_enabled {
        subagent::draw_sidebar(frame, app, sidebar_area);
    }

    crate::queue::draw(frame, app);
    btw::draw(frame, app);
    // 浮层最后画：补全菜单贴输入框上沿，就地引擎菜单贴模式图例，居中模态覆盖全部。
    if let Some(completion) = &mut app.completion {
        let keys = modal::draw_completion_menu(frame, completion, prompt_area);
        app.key_hits.extend(keys);
    }
    if app.modal.is_none() {
        modal::draw_engine_menu(frame, app);
        modal::draw_permission_menu(frame, app);
    } else {
        // 模态是阻塞层：不再保留就地菜单，避免两层浮层叠加。
        app.engine_menu = None;
        app.permission_menu = None;
    }
    if app.modal.is_some() {
        modal::draw_modal(frame, app);
    }
    // notice/warning 在模态之后画：模态期间触发的提示（如复制选项）不被
    // 调暗层压住，临时提示始终可读。
    draw_notice(frame, app, scroll_area);
    draw_warning(frame, app, scroll_area);
}

/// 拖选高亮：只覆盖选区内的格子；跨行时中间整行、首尾行按列收边。
fn highlight_selection(frame: &mut Frame, app: &App, area: Rect) {
    let Some(((start_col, start_row), (end_col, end_row))) =
        app.selection.map(crate::app::ordered_selection)
    else {
        return;
    };
    if area.width == 0 || area.height == 0 {
        return;
    }
    let theme = app.theme;
    let buffer = frame.buffer_mut();
    let left = area.x;
    let right = area.x.saturating_add(area.width).saturating_sub(1);
    for row in start_row..=end_row {
        if row < area.y || row >= area.y.saturating_add(area.height) {
            continue;
        }
        let (from, to) = if start_row == end_row {
            (start_col, end_col)
        } else if row == start_row {
            (start_col, right)
        } else if row == end_row {
            (left, end_col)
        } else {
            (left, right)
        };
        let mut from = from.max(left);
        let mut to = to.min(right);
        if from > to {
            continue;
        }
        // 宽字符只由首格绘制；选到续格时也要给首格铺色。
        if from > left
            && buffer
                .cell((from - 1, row))
                .is_some_and(|cell| str_width(cell.symbol()) > 1)
        {
            from -= 1;
        }
        if let Some(cell) = buffer.cell((to, row)) {
            to = to
                .saturating_add(cell_width(cell.symbol()).saturating_sub(1))
                .min(right);
        }
        for col in from..=to {
            let Some(cell) = buffer.cell_mut((col, row)) else {
                continue;
            };
            cell.set_style(Style::new().fg(theme.text_primary).bg(theme.bg_selection));
        }
    }
}

/// 一次性提示 toast：贴滚动区右下角的胶囊，TTL 由 [`App::notice_visible`] 决定。
///
/// 失败提示用错误色，其余操作反馈用中性的工具强调色：绿色只该表示「成功」。
fn draw_notice(frame: &mut Frame, app: &App, area: Rect) {
    if !app.notice_visible() || area.height == 0 {
        return;
    }
    let Some(text) = app.notice.as_deref() else {
        return;
    };
    let accent = if app.notice_error {
        app.theme.accent_error
    } else {
        app.theme.accent_tool
    };
    draw_chip(
        frame.buffer_mut(),
        app,
        area,
        area.y + area.height - 1,
        text,
        accent,
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
    let text = wrap::truncate(text.to_owned(), max_text).into_owned();
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

/// 「回到底部」浮动箭头：滚动区底缘居中、直接压在正文上不占行。
///
/// 单格实心三角 `▼`，加粗白字、无底色透出正文；命中区登记在
/// [`App::jump_bottom_area`]，按下即贴尾。只在滚离底部时出现。
fn draw_jump_bottom(frame: &mut Frame, app: &mut App, area: Rect) {
    const BODY: &str = "▼";
    if app.scroll_offset == 0 || area.is_empty() || area.width < str_width(BODY) as u16 {
        return;
    }
    let theme = app.theme;
    let x = area.x + (area.width - 1) / 2;
    let y = area.bottom() - 1;
    app.jump_bottom_area = Rect::new(x, y, 1, 1);
    let buffer = frame.buffer_mut();
    // 左邻是宽字符时尾格被盖后引导格会吞掉箭头左缘，换成空格拆开。
    if x > buffer.area.x && str_width(buffer[(x - 1, y)].symbol()) > 1 {
        buffer[(x - 1, y)].set_symbol(" ");
    }
    buffer.set_string(
        x,
        y,
        BODY,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
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
    overlay::paint_scrollbar(
        frame.buffer_mut(),
        app.scrollbar.track,
        thumb_top,
        thumb,
        theme,
    );
}

fn draw_prompt(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let border = match app.focus {
        Focus::Prompt => theme.border_active,
        Focus::Scrollback | Focus::Btw => theme.border,
    };
    frame.render_widget(
        Paragraph::new(prompt_lines(app, area)).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(border)),
        ),
        area,
    );
    let prefix = u16::try_from(1 + PROMPT_PAD + PROMPT_MARKER).expect("输入框前缀宽度不超过 u16");
    let body = Rect::new(
        area.x + prefix,
        area.y + 1,
        area.width.saturating_sub(prefix + 2),
        area.height.saturating_sub(2),
    );
    highlight_selection(frame, app, body);
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
    let rows = prompt_rows(app.prompt_text(), area.width);
    let visual_row = prompt_cursor_row(&rows, app.prompt.cursor);
    let (offset, text) = rows[visual_row];
    let column = str_width(&text[..app.prompt.cursor - offset]);
    let total_rows = rows.len();
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

pub(crate) fn prompt_cursor_from_point(app: &App, area: Rect, col: u16, row: u16) -> Option<usize> {
    if area.width < 6 || area.height < 3 {
        return None;
    }
    let text = app.prompt_text();
    let top = area.y + 1;
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    if row < top || row >= top + visible_rows as u16 {
        return None;
    }
    let left = area.x + 1 + PROMPT_PAD as u16 + PROMPT_MARKER as u16;
    let target_col = usize::from(col.saturating_sub(left));
    let visual_rows = prompt_rows(text, area.width);
    let visual_row = prompt_cursor_row(&visual_rows, app.prompt.cursor);
    let start = prompt_scroll_offset(visual_row, visual_rows.len(), visible_rows);
    let clicked_visual_row = start + usize::from(row - top);
    if let Some((start_offset, sub_text)) = visual_rows.get(clicked_visual_row) {
        Some(*start_offset + prompt_cell_range(sub_text, target_col).start)
    } else {
        Some(text.len())
    }
}

/// 显示格对应的字节范围；宽字符的两格归同一字符，尾随组合字符一并保留。
fn prompt_cell_range(text: &str, column: usize) -> std::ops::Range<usize> {
    let mut width = 0;
    let mut chars = text.char_indices().peekable();
    while let Some((offset, ch)) = chars.next() {
        width += unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if width > column {
            let mut end = offset + ch.len_utf8();
            while let Some(&(offset, ch)) = chars.peek() {
                if unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 0 {
                    break;
                }
                end = offset + ch.len_utf8();
                chars.next();
            }
            return offset..end;
        }
    }
    text.len()..text.len()
}

/// 从当前可见输入行映射回原文：软折行不插入换行，也不复制边框与提示符。
pub(crate) fn prompt_selection_text(app: &App) -> &str {
    let Some(((start_col, start_row), (end_col, end_row))) =
        app.selection.map(crate::app::ordered_selection)
    else {
        return "";
    };
    let area = app.prompt_area;
    if area.width < 6 || area.height < 3 {
        return "";
    }
    if end_row < area.y + 1 || start_row >= area.y + area.height - 1 {
        return "";
    }
    let text = app.prompt_text();
    let rows = prompt_rows(text, area.width);
    let visible = usize::from(area.height - 2);
    let start = prompt_scroll_offset(
        prompt_cursor_row(&rows, app.prompt.cursor),
        rows.len(),
        visible,
    );
    let top = area.y + 1;
    let visible = u16::try_from((rows.len() - start).min(visible)).expect("可见行数不超过输入框高度");
    let bottom = top + visible;
    if end_row < top || start_row >= bottom {
        return "";
    }
    let first_row = start_row.max(top);
    let last_row = end_row.min(bottom - 1);
    let prefix = u16::try_from(1 + PROMPT_PAD + PROMPT_MARKER).expect("输入框前缀宽度不超过 u16");
    let left = area.x + prefix;
    let (first_offset, first_text) = rows[start + usize::from(first_row - top)];
    let (last_offset, last_text) = rows[start + usize::from(last_row - top)];
    let from = first_offset
        + if first_row == start_row {
            prompt_cell_range(first_text, usize::from(start_col.saturating_sub(left))).start
        } else {
            0
        };
    let to = last_offset
        + if last_row != end_row {
            last_text.len()
        } else if end_col < left {
            0
        } else {
            prompt_cell_range(last_text, usize::from(end_col - left)).end
        };
    if from < to { &text[from..to] } else { "" }
}

/// 输入框下边框上的图例：运行时显示停止操作，否则显示快捷键键位。
///
/// 图例直接写在边框行上，因此不额外占用终端行；空间不足时先让掉左侧提示，
/// 右侧的当前模型与模式仍然保留。
fn draw_prompt_legends(frame: &mut Frame, app: &mut App, area: Rect) {
    app.engine_area = Rect::default();
    app.legend_tip_area = Rect::default();
    if area.height < 3 || area.width < 24 {
        return;
    }
    let theme = app.theme;
    let inner = usize::from(area.width).saturating_sub(2);
    let y = area.y + area.height - 1;

    // 无会话时右端只画 idle；左端仍能显示选区操作。
    let no_session = app.session.is_none();
    let model = app.model_display();
    let status = app.status_label();
    let permission = app
        .session
        .as_ref()
        .map_or("full", |session| session.permission.as_str());
    let right: Vec<(&str, Color)> = if no_session {
        vec![(labels::status::IDLE, theme.gray)]
    } else {
        vec![
            (model.as_str(), theme.gray),
            (labels::SEP, theme.border),
            (status.as_str(), theme.accent_model),
            (labels::SEP, theme.border),
            (permission, theme.accent_model),
        ]
    };
    let right_width: usize = right.iter().map(|(text, _)| str_width(text)).sum();
    if right_width + 4 > inner {
        return;
    }

    let selecting = app.selection.is_some() && app.modal.is_none();
    let copy_feedback = app.modal.is_none()
        && app
            .selection_copy_deadline()
            .is_some_and(|deadline| std::time::Instant::now() < deadline);
    let (left_key, mut left_tip, left_color) = if selecting {
        (Cow::Borrowed("Ctrl+C"), "复制 · Esc 取消", theme.gray_bright)
    } else if copy_feedback {
        (Cow::Borrowed(""), "已复制", theme.gray_bright)
    } else if app.prompt_running {
        (
            Cow::Borrowed("Esc"),
            if app.cancel_requested {
                "正在停止"
            } else {
                "停止"
            },
            if app.cancel_requested {
                theme.accent_warning
            } else {
                theme.accent_running
            },
        )
    } else {
        let help_key = app
            .preferences
            .keybindings
            .get("help")
            .and_then(|keys| keys.first())
            .map_or_else(|| "—".to_owned(), |binding| labels::key(binding));
        (Cow::Owned(help_key), "快捷键", theme.gray_dim)
    };
    let left_key = left_key.as_ref();
    let show_cancel =
        selecting && str_width(left_key) + 1 + str_width(left_tip) + right_width + 8 <= inner;
    if selecting && !show_cancel {
        left_tip = "复制";
    }
    // 左端布局：`␣key tip`（胶囊存在时接 ` · capsule`），末尾再补一格空白，
    // 与右侧的留白对称；分隔点只画在提示与胶囊之间，不留孤点。
    let tips_width = str_width(left_key) + usize::from(!left_key.is_empty()) + str_width(left_tip);
    let show_tips = tips_width + 4 + right_width + 4 <= inner;

    let buffer = frame.buffer_mut();
    let mut end_x = area.x + 3;
    if show_tips {
        buffer.set_string(area.x + 2, y, " ", Style::new());
        let mut x = end_x;
        if !left_key.is_empty() {
            buffer.set_string(x, y, left_key, Style::new().fg(theme.gray_bright));
            x += cell_width(left_key);
            buffer.set_string(x, y, " ", Style::new());
            x += 1;
        }
        buffer.set_string(x, y, left_tip, Style::new().fg(left_color));
        x += cell_width(left_tip);
        if selecting {
            let copy_width = cell_width(left_key) + 1 + cell_width("复制");
            app.key_hits.push(labels::KeyHit {
                area: Rect::new(end_x, y, copy_width, 1),
                modifiers: crossterm::event::KeyModifiers::CONTROL,
                code: crossterm::event::KeyCode::Char('c'),
            });
            if show_cancel {
                app.key_hits.push(labels::KeyHit {
                    area: Rect::new(
                        end_x + copy_width + cell_width(labels::SEP),
                        y,
                        cell_width("Esc 取消"),
                        1,
                    ),
                    modifiers: crossterm::event::KeyModifiers::NONE,
                    code: crossterm::event::KeyCode::Esc,
                });
            }
        } else if !copy_feedback {
            // 普通图例仍然可点：空闲打开帮助，运行中停止。
            app.legend_tip_area = Rect::new(end_x, y, x - end_x, 1);
        }
        end_x = x;
    }
    // workflow 胶囊：活跃 run / 待审批徽标，点击打开审批或 run 面板。
    app.workflow_capsule_area = Rect::default();
    let runs_n = app.workflow.active_runs.len();
    // 计数 = 排队审批 + 最小化审批（parked 也算待决，否则无活跃 run 时
    // 胶囊不显示、没法重开）。
    let approvals_n =
        app.workflow.queued_approvals.len() + usize::from(app.workflow.parked_approval.is_some());
    if runs_n + approvals_n > 0 {
        let capsule = if approvals_n > 0 {
            format!("⚙{runs_n}跑·{approvals_n}审")
        } else {
            format!("⚙{runs_n}跑")
        };
        let cap_cells = cell_width(&capsule);
        // 有提示时胶囊跟在分隔点之后，没有提示就直接占左端。
        let cx = if show_tips {
            end_x + cell_width(labels::SEP)
        } else {
            area.x + 3
        };
        if usize::from(cx + cap_cells + 1) + right_width + 4 <= inner + usize::from(area.x) {
            if show_tips {
                buffer.set_string(end_x, y, labels::SEP, Style::new().fg(theme.border));
            } else {
                buffer.set_string(area.x + 2, y, " ", Style::new());
            }
            buffer.set_string(cx, y, &capsule, Style::new().fg(theme.accent_warning));
            app.workflow_capsule_area = Rect::new(cx, y, cap_cells, 1);
            end_x = cx + cap_cells;
        }
    }
    if end_x > area.x + 3 {
        // 最后一个元素与边框线之间补一格空白，左右对称。
        buffer.set_string(end_x, y, " ", Style::new());
    }
    let right_cells = u16::try_from(right_width).unwrap_or(u16::MAX);
    let right_start = area.x + area.width - 2 - right_cells;
    let model_cells = cell_width(model.as_str());
    let status_start = right_start + model_cells + cell_width(labels::SEP);
    let status_cells = cell_width(status.as_str());
    let perm_start = status_start + status_cells + cell_width(labels::SEP);
    let perm_cells = cell_width(permission);
    let mut x = right_start;
    buffer.set_string(x.saturating_sub(1), y, " ", Style::new());
    for (text, color) in right {
        buffer.set_string(x, y, text, Style::new().fg(color));
        x += cell_width(text);
    }
    buffer.set_string(x, y, " ", Style::new());
    if no_session {
        app.model_tag_area = Rect::default();
        app.engine_area = Rect::default();
        app.permission_tag_area = Rect::default();
        return;
    }
    app.model_tag_area = Rect {
        x: right_start,
        y,
        width: model_cells,
        height: 1,
    };
    // 状态图例单独留命中区：鼠标点击它就地图例弹出引擎菜单，图例本身不变。
    app.engine_area = Rect {
        x: status_start,
        y,
        width: status_cells,
        height: 1,
    };
    app.permission_tag_area = Rect {
        x: perm_start,
        y,
        width: perm_cells,
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
    let rows = prompt_rows(text, area.width);
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
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .map(|(index, (_, row))| {
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
            spans.extend(chip_spans(row, theme.text_primary, theme.accent_warning));
            Line::from(spans)
        })
        .collect();
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let visual_row = prompt_cursor_row(&rows, app.prompt.cursor);
    let start = prompt_scroll_offset(visual_row, lines.len().max(1), visible);
    lines.into_iter().skip(start).take(visible).collect()
}

/// 把一行文本按 chip 占位（`[paste#`、`[image#`）切分：占位用 accent 色，
/// 其余用正文色。输入框与转写里的用户消息共用同一套标记样式。
pub(crate) fn chip_spans(row: &str, plain: Color, accent: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = row;
    while let Some(index) = rest
        .find("[paste#")
        .into_iter()
        .chain(rest.find("[image#"))
        .min()
    {
        if index > 0 {
            spans.push(Span::styled(
                rest[..index].to_owned(),
                Style::new().fg(plain),
            ));
        }
        let end = rest[index..]
            .find(']')
            .map_or(rest.len(), |end| index + end + 1);
        spans.push(Span::styled(
            rest[index..end].to_owned(),
            Style::new().fg(accent),
        ));
        rest = &rest[end..];
    }
    if !rest.is_empty() {
        spans.push(Span::styled(rest.to_owned(), Style::new().fg(plain)));
    }
    spans
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
            wrap::truncate(branch.clone(), 24).into_owned(),
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
            wrap::truncate(data.workdir.clone(), max_workdir).into_owned(),
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
            wrap::truncate(title, 24).into_owned(),
            Style::new().fg(theme.text_primary),
        ));
        let back = labels::hints(&[("esc", "返回")], usize::MAX);
        // 「Esc 返回」可点：与空输入框按 Esc 同样返回主会话。
        let used: usize = spans
            .iter()
            .map(|span| str_width(&span.content))
            .sum::<usize>()
            + 2;
        let end = (used + str_width(&back)).min(usize::from(area.width));
        if let (Ok(start), Ok(end)) = (u16::try_from(used), u16::try_from(end))
            && start < end
        {
            app.header_back_area = Rect::new(area.x + start, area.y, end - start, 1);
        }
        spans.push(Span::styled(
            format!("  {back}"),
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
            format!(
                " {} / {}",
                format_tokens(data.context_used),
                format_tokens(window)
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

fn draw_statusline(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.is_empty() {
        return;
    }
    // 内置渲染回报右侧统计段宽度：该段可点（与 `/usage` 同义），右对齐贴边。
    // 自定义模板布局未知，不登记命中区。
    let (line, usage_width) = match &app.statusline_template {
        Some(template) => (statusline_from_template(app, template), 0),
        None => statusline_builtin(app, area.width as usize),
    };
    frame.render_widget(Paragraph::new(line), area);
    if usage_width > 0 {
        let width = u16::try_from(usage_width).unwrap_or(area.width);
        app.usage_area = Rect::new(area.x + area.width.saturating_sub(width), area.y, width, 1);
    }
}

/// statusline 数据快照（内置渲染、信息条与模板共用）。
struct StatuslineData {
    model: String,
    status: String,
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
    let status = app.status_label();
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
        status,
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

/// 工具摘要里的展示路径：`cwd` 内的路径相对化（等于 cwd 显示 `.`），
/// 其余绝对路径交给 [`short_workdir`] 折叠 `$HOME`，相对路径原样。
///
/// 前缀匹配只在 `/` 边界上进行：cwd `/a/b` 不会吃掉 `/a/bc`。
pub(crate) fn display_path<'a>(path: &'a str, cwd: &str) -> std::borrow::Cow<'a, str> {
    if !cwd.is_empty() {
        if path == cwd {
            return ".".into();
        }
        if let Some(rest) = path.strip_prefix(cwd)
            && (rest.starts_with('/') || cwd.ends_with('/'))
        {
            return rest.strip_prefix('/').unwrap_or(rest).into();
        }
    }
    if path.starts_with('/') {
        return short_workdir(path).into();
    }
    path.into()
}

/// 上下文进度条：整数运算算占位（`round(used / window * slots)`），最后一格保留给分母。
///
/// 只用单宽的 `━`/`─`：两端不加全角括号，宽度恒为 `slots`，与右侧数字之间只隔一格空格。
fn context_bar(used: u64, window: u64, slots: usize) -> String {
    let slots_u64 = u64::try_from(slots).unwrap_or(1);
    let filled = used
        .saturating_mul(slots_u64)
        .saturating_add(window / 2)
        .checked_div(window)
        .map_or(0, |filled| filled.min(slots_u64));
    let filled = usize::try_from(filled).unwrap_or(slots);
    format!("{}{}", "━".repeat(filled), "─".repeat(slots - filled))
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
    if app.skills_compacting {
        return Some(Activity {
            label: "Working",
            color: app.theme.accent_running,
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

/// 窗口标题（OSC 0）：状态图标 + 项目名。
///
/// 等待人工（提问/授权卡片）时给「需人工」提示；运行中复用状态栏
/// 同一个 spinner；空闲只剩 `moc — 项目名`，没有会话时退化为 `moc`。
pub fn terminal_title(app: &App) -> String {
    let cwd = app
        .session
        .as_ref()
        .map_or(app.startup_cwd.as_str(), |session| session.cwd.as_str());
    let project = std::path::Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty());
    let base = match project {
        Some(project) => format!("moc — {project}"),
        None => "moc".to_owned(),
    };
    if app
        .modal
        .as_ref()
        .is_some_and(|modal| matches!(modal, Modal::Permission { .. } | Modal::Ask { .. }))
    {
        format!("● {base} · 需人工")
    } else if app.prompt_running {
        format!("{} {base}", spinner_frame(app))
    } else {
        base
    }
}

/// 内置渲染：左侧运行状态，右侧会话累计 token 统计（与 `/usage` 同源）。
///
/// 返回值附带右侧统计段的显示宽度：统计段可点（等价 `/usage`），由调用方登记命中区；
/// 统计为空或空间不够被裁掉时宽度为 0。
fn statusline_builtin(app: &App, width: usize) -> (Line<'static>, usize) {
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
    // 右侧统计段：in/out 都是 0 时整段（连同 cost）不显示，空闲不刷一串 0。
    let right = if data.tokens_in == 0 && data.tokens_out == 0 {
        String::new()
    } else {
        format!(
            " ⇅ {} › {} › {:.0} {}",
            format_tokens(data.tokens_in),
            format_tokens(data.tokens_out),
            data.tok_s,
            labels::RATE_UNIT,
        )
    };
    let mut right_cost = if right.is_empty() {
        String::new()
    } else {
        data.cost
            .map(|cost| format!(" › ${cost:.2}"))
            .unwrap_or_default()
    };
    let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
    let available = width.saturating_sub(used + 2);
    if str_width(&right) + str_width(&right_cost) > available {
        right_cost.clear();
    }
    let right =
        wrap::truncate(right, available.saturating_sub(str_width(&right_cost))).into_owned();
    let right_width = str_width(&right) + str_width(&right_cost);
    let gap = width.saturating_sub(used + right_width).max(1);
    spans.push(Span::styled(" ".repeat(gap), Style::new()));
    spans.push(Span::styled(right, Style::new().fg(theme.gray)));
    spans.push(Span::styled(
        right_cost,
        Style::new().fg(theme.accent_warning),
    ));
    (Line::from(spans), right_width)
}

/// 模板渲染：`{model} · {status} → {workdir}`，占位符按语义着色。
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
            "status" => Some((data.status.clone(), theme.accent_tool)),
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
            "tok_s" => Some((
                format!("{:.1} {}", data.tok_s, labels::RATE_UNIT),
                theme.accent_running,
            )),
            "prompt_elapsed" => Some((
                data.prompt_elapsed
                    .map(|elapsed| {
                        labels::elapsed(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
                    })
                    .unwrap_or_else(|| "◷ -".into()),
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

    #[test]
    fn prompt_preserves_spaces_after_deleting_arguments() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let area = Rect::new(2, 3, 30, 3);
        app.prompt.insert_text("/goal  xyz");
        for _ in 0..3 {
            app.prompt.backspace();
        }
        assert_eq!(app.prompt.text, "/goal  ");
        assert_eq!(prompt_lines(&app, area)[0].to_string(), " ❯ /goal  ");
        assert_eq!(prompt_cursor_position(&app, area), (13, 4));
        assert_eq!(prompt_cursor_from_point(&app, area, 13, 4), Some(7));
        app.prompt.insert_char('x');
        assert_eq!(prompt_cursor_position(&app, area), (14, 4));
    }

    #[test]
    fn prompt_wrap_keeps_spaces_and_cursor_mapping() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let area = Rect::new(0, 0, 14, 5);
        // 正文宽度 8 格；涵盖宽字符、连续空格、硬换行与滚动。
        app.prompt.insert_text("中文ab    cd\n123456789");
        assert_eq!(prompt_content_rows(&app, area.width), 4);
        for cursor in app
            .prompt
            .text
            .char_indices()
            .map(|(index, _)| index)
            .chain(std::iter::once(app.prompt.text.len()))
            .collect::<Vec<_>>()
        {
            app.prompt.cursor = cursor;
            let (x, y) = prompt_cursor_position(&app, area);
            assert_eq!(prompt_cursor_from_point(&app, area, x, y), Some(cursor));
        }
        app.prompt.cursor = "中文ab  ".len();
        assert_eq!(prompt_cursor_position(&app, area), (4, 1));
        assert_eq!(prompt_lines(&app, area)[0].to_string(), "     cd");
    }

    /// 窗口标题的状态选择：空闲留项目名，运行中上 spinner，等待人工置顶提示。
    #[test]
    fn terminal_title_reflects_app_state() {
        use moc_protocol::methods::SessionInfo;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.session = Some(SessionInfo {
            id: "ses_test".into(),
            title: "测试会话".into(),
            model: "test/model".into(),
            permission: moc_protocol::SessionPermission::Full,
            engine: moc_protocol::SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: false,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
            cwd: "/home/user/myproject".into(),
            created_at_ms: 0,
            updated_at_ms: 0,
            lock: None,
        });
        assert_eq!(terminal_title(&app), "moc — myproject");

        app.prompt_running = true;
        app.prompt_started_at = Some(std::time::Instant::now());
        let running = terminal_title(&app);
        assert!(running.ends_with("moc — myproject"), "{running}");
        assert!(
            running.starts_with(['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']),
            "{running}"
        );

        app.modal = Some(Modal::Permission {
            params: moc_protocol::methods::PermissionRequestParams {
                session_id: "ses_test".into(),
                prompt_id: "prm_001".into(),
                request_id: "req_001".into(),
                tool_call_id: "call_001".into(),
                tool_name: "bash".into(),
                title: "执行命令".into(),
                body: None,
                diff: None,
                options: vec![],
            },
            selected: 0,
            scroll_offset: 0,
            reply: None,
        });
        assert!(terminal_title(&app).contains("需人工"));
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

    /// 「回到底部」按钮压在滚动区底缘一行正文上（不占行），贴尾时消失。
    #[test]
    fn jump_bottom_pill_floats_over_scrollback_bottom() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        let text = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_block(TranscriptBlock::Assistant { text });
        app.scroll_offset = 8;
        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");

        let theme = Theme::global();
        let pill = app.jump_bottom_area;
        let buffer = terminal.backend().buffer();
        assert_eq!((pill.width, pill.height), (1, 1));
        assert_eq!(
            pill.bottom(),
            app.scrollback_area.bottom(),
            "箭头压在滚动区底缘、输入框上方"
        );
        let center = app.scrollback_area.x + app.scrollback_area.width / 2;
        assert!(
            pill.x <= center && center <= pill.right(),
            "单格箭头落在中间两格之一即居中"
        );
        let cell = &buffer[(pill.x, pill.y)];
        assert_eq!(cell.symbol(), "▼", "实心三角箭头");
        assert_eq!(cell.fg, theme.text_primary, "白字");
        assert!(cell.modifier.contains(Modifier::BOLD), "加粗");
        assert_eq!(cell.bg, theme.bg_base, "无底色、浮在正文上");

        app.scroll_offset = 0;
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert!(app.jump_bottom_area.is_empty(), "贴尾时胶囊消失");
    }

    /// 状态图例的命中区正好盖住引擎名；就地菜单右上对齐图例、整体贴在其上方。
    #[test]
    fn status_legend_hit_area_carries_the_menu() {
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
            engine: moc_protocol::SessionEngine::Standard,
            autonomy: moc_protocol::SessionAutonomy::Default,
            plan_active: true,
            fast_mode: false,
            engine_locked: false,
            parent_session_id: None,
            forked_from: None,
            agent_preset: None,
            delegation_depth: 0,
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
        let legend = app.engine_area;
        assert_eq!(legend.height, 1);
        assert_eq!(
            row_text(&terminal, legend),
            "standard+plan",
            "命中区正好是状态图例"
        );

        app.open_engine_menu();
        assert_eq!(
            app.engine_menu.as_ref().map(|menu| menu.selected),
            Some(0),
            "默认高亮当前引擎"
        );
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        let menu = app.engine_menu.as_ref().expect("菜单保持打开");
        assert_eq!(menu.area.right(), legend.right(), "右缘对齐状态图例");
        assert_eq!(menu.area.bottom(), legend.y, "整体贴在图例上方");
        assert_eq!(menu.entry_at(menu.area.x + 4, menu.area.y + 1), Some(0));
        // 当前引擎 standard 是菜单第一项，位于图例上方第 4 行（边框 + 3 条目）；
        // 正文从「边框 1 + 卡片内边距 CARD_PAD_X」之后开始。
        let current_y = legend.y - 4;
        assert!(
            row_text(
                &terminal,
                Rect {
                    x: menu.area.x + 2,
                    y: current_y,
                    width: 12,
                    height: 1,
                },
            )
            .starts_with("❯ standard"),
            "当前引擎行同时是菜单高亮行"
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((menu.area.x + 4, current_y))
                .expect("表头格存在")
                .fg,
            Theme::global().accent_model
        );
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((menu.area.x + 3, menu.area.y + 2))
                .expect("次行格存在")
                .fg,
            Theme::global().text_primary
        );

        // 本次提示临时问答：图例追加 `+ask`，会话引擎与 plan 叠加态不变；清除后即恢复。
        app.engine_menu = None;
        app.transient_mode = Some(moc_protocol::methods::PromptMode::Ask);
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert_eq!(row_text(&terminal, app.engine_area), "standard+plan+ask");
        assert_eq!(
            app.session
                .as_ref()
                .map(|session| { (session.engine, session.plan_active) }),
            Some((moc_protocol::SessionEngine::Standard, true)),
            "临时模式不得改写会话引擎与 plan 态"
        );
        app.transient_mode = None;
        terminal
            .draw(|frame| draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        assert_eq!(row_text(&terminal, app.engine_area), "standard+plan");
    }

    /// 告警贴滚动区右上角、notice 贴右下角；两者同屏共存且各自只占一行。
    ///
    /// 同时校验强调色：告警用 warning、普通 notice 用中性 tool 色、失败 notice 用 error 色。
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
            Theme::global().accent_tool
        );

        app.set_error_notice("clipboard image attached");
        terminal
            .draw(|frame| draw_notice(frame, &app, area))
            .expect("绘制成功");
        assert_eq!(
            terminal
                .backend()
                .buffer()
                .cell((notice_icon_x, area.y + area.height - 1))
                .expect("图标格存在")
                .fg,
            Theme::global().accent_error,
            "失败提示用错误色"
        );
    }

    /// 拖选高亮按格子收边：同一行只亮选区内的列，跨行时首尾行才铺到区域边缘。
    #[test]
    fn selection_highlight_covers_selected_columns_only() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        // 测试环境没有终端配色能力，先给主题塞两个可区分的颜色再交给 App。
        let mut custom = *Theme::global();
        custom.bg_selection = ratatui::style::Color::Rgb(10, 20, 30);
        custom.text_primary = ratatui::style::Color::Rgb(200, 200, 200);
        let theme: &'static Theme = Box::leak(Box::new(custom));

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(theme, events);
        let area = Rect::new(2, 1, 20, 5);
        let mut terminal = Terminal::new(TestBackend::new(40, 10)).expect("构造测试终端");
        let bg = |terminal: &Terminal<TestBackend>, column: u16, row: u16| {
            terminal
                .backend()
                .buffer()
                .cell((column, row))
                .expect("格存在")
                .bg
        };

        app.selection = Some(((5, 1), (10, 1)));
        terminal
            .draw(|frame| highlight_selection(frame, &app, area))
            .expect("绘制成功");
        assert_eq!(bg(&terminal, 5, 1), theme.bg_selection, "起点格子高亮");
        assert_eq!(
            bg(&terminal, 10, 1),
            theme.bg_selection,
            "终点格子包含在选区内"
        );
        assert_ne!(bg(&terminal, 4, 1), theme.bg_selection, "选区左侧不铺色");
        assert_ne!(bg(&terminal, 11, 1), theme.bg_selection, "选区右侧不铺色");
        assert_ne!(bg(&terminal, 5, 2), theme.bg_selection, "选区外的行不铺色");

        // 反向跨行拖选：首行铺到右缘、末行从左缘铺到终点，中间整行。
        app.selection = Some(((10, 3), (5, 1)));
        terminal
            .draw(|frame| highlight_selection(frame, &app, area))
            .expect("绘制成功");
        assert_ne!(bg(&terminal, 4, 1), theme.bg_selection);
        assert_eq!(bg(&terminal, area.right() - 1, 1), theme.bg_selection);
        assert_eq!(
            bg(&terminal, area.x, 2),
            theme.bg_selection,
            "中间行整行高亮"
        );
        assert_eq!(bg(&terminal, area.x, 3), theme.bg_selection);
        assert_eq!(bg(&terminal, 10, 3), theme.bg_selection);
        assert_ne!(bg(&terminal, 11, 3), theme.bg_selection);
    }
}

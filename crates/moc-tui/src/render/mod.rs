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
pub(crate) mod rewind;

use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};

pub use scrollback::ScrollbackView;

use self::wrap::str_width;
use crate::app::{App, ScrollbackScrollbar};
use crate::modal::Modal;

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

mod prompt;
use prompt::cell_width;
pub(crate) use prompt::chip_spans;
use prompt::draw_chip;
use prompt::draw_prompt;
use prompt::prompt_content_rows;
pub(crate) use prompt::prompt_cursor_from_point;
pub(crate) use prompt::prompt_cursor_position;
pub(crate) use prompt::prompt_selection_text;
mod status;
pub(crate) use status::display_path;
use status::draw_header;
use status::draw_statusline;
pub use status::format_tokens;
pub use status::terminal_title;

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
    // 通用悬停反馈：有模态时只描模态 footer 的可点单键提示（提亮 + 下划线）；
    // 无模态时再给图例/状态栏可点区铺悬停底色。指针坐标由事件层维护
    // （scrollbar_pointer），键盘输入时已清空。
    if let Some((px, py)) = app.scrollbar_pointer {
        let position = ratatui::layout::Position::new(px, py);
        for hit in &app.key_hits {
            if hit.area.contains(position) {
                frame.buffer_mut().set_style(
                    hit.area,
                    Style::new()
                        .fg(theme.text_primary)
                        .add_modifier(Modifier::UNDERLINED),
                );
            }
        }
        if app.modal.is_none() {
            for area in [
                app.legend_tip_area,
                app.model_tag_area,
                app.engine_area,
                app.permission_tag_area,
                app.usage_area,
                app.context_area,
                app.header_back_area,
                app.workflow_capsule_area,
                app.jump_bottom_area,
            ] {
                if !area.is_empty() && area.contains(position) {
                    frame
                        .buffer_mut()
                        .set_style(area, Style::new().bg(theme.bg_hover));
                }
            }
        }
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
    let interactive = matches!(app.modal, None | Some(Modal::Ask { .. }));
    if !interactive || max_offset == 0 {
        app.scrollbar_drag = None;
    }
    if max_offset == 0 || area.width < 2 || area.height < 2 {
        return;
    }
    let theme = app.theme;
    let track_height = area.height;
    let total = (max_offset as u32) + track_height as u32;
    let thumb = ((track_height as u32 * track_height as u32) / total.max(1))
        .clamp(1, track_height as u32) as u16;
    // 两列感应区完全落在右侧留白内，展开不改变正文宽度，也不覆盖提问卡。
    let right = frame.area().right();
    app.scrollbar = ScrollbackScrollbar {
        track: Rect::new(right.saturating_sub(2), area.y, 2, track_height),
        thumb,
        max_offset,
    };
    let thumb_top = app.scrollbar.thumb_top(app.scroll_offset);
    let expanded = interactive
        && (app.scrollbar_drag.is_some()
            || app
                .scrollbar_pointer
                .is_some_and(|position| app.scrollbar.track.contains(position.into())));
    let track = if expanded {
        app.scrollbar.track
    } else {
        Rect::new(right - 1, area.y, 1, track_height)
    };
    overlay::paint_scrollbar(frame.buffer_mut(), track, thumb_top, thumb, theme);
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

/// 数量分档：`(一单位多少 token, 后缀)`，按升序排列。
const TOKEN_UNITS: [(u64, &str); 3] = [(1_000, "k"), (1_000_000, "m"), (1_000_000_000, "b")];

const SPINNER_FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

struct Activity {
    label: std::borrow::Cow<'static, str>,
    color: Color,
}

#[cfg(test)]
mod tests;

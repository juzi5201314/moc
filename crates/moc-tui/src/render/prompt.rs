use super::PROMPT_MARKER;
use super::PROMPT_MAX_ROWS;
use super::PROMPT_PAD;
use super::highlight_selection;
use super::labels;
use super::wrap;
use super::wrap::str_width;
use crate::app::App;
use crate::app::Focus;
use crate::config::ActiveSubmit;
use moc_i18n::t;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Paragraph;
use std::borrow::Cow;
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

/// 输入框的引用控件映射：显示文本与双向偏移；无控件时 `text` 与原文相同。
fn prompt_ref_map(app: &App) -> crate::refs::RefMap {
    crate::refs::RefMap::new(app.prompt_text(), &crate::refs::prompt_refs(app))
}

/// 输入框按显示列硬折行，保留空白和**显示文本**的字节偏移；额外留一格显示行尾光标。
///
/// `atomic` 段（引用控件的显示范围，升序不重叠）不拆开：整段放不下当前行就先折行；
/// 段自身比行宽还长时照常逐格切，保证前进。
pub(super) fn prompt_rows<'a>(
    text: &'a str,
    width: u16,
    atomic: &[Range<usize>],
) -> Vec<(usize, &'a str)> {
    let width = usize::from(width)
        .saturating_sub(2 + PROMPT_PAD + PROMPT_MARKER + 1)
        .max(1);
    let mut rows = Vec::new();
    let mut start = 0;
    let mut column = 0;
    let mut span = 0;
    for (offset, grapheme) in text.grapheme_indices(true) {
        while span < atomic.len() && atomic[span].start < offset {
            span += 1;
        }
        if grapheme.ends_with('\n') {
            rows.push((start, &text[start..offset + grapheme.len() - 1]));
            start = offset + grapheme.len();
            column = 0;
            continue;
        }
        if column > 0
            && span < atomic.len()
            && atomic[span].start == offset
            && str_width(&text[atomic[span].clone()]) <= width
            && column + str_width(&text[atomic[span].clone()]) > width
        {
            rows.push((start, &text[start..offset]));
            start = offset;
            column = 0;
        }
        let cells = str_width(grapheme);
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

pub(super) fn prompt_cursor_row(rows: &[(usize, &str)], cursor: usize) -> usize {
    rows.partition_point(|(start, _)| *start <= cursor)
        .saturating_sub(1)
}

/// 输入框正文行数：按当前输入换行所需行数（1 行起，上限 [`PROMPT_MAX_ROWS`]）。
pub(super) fn prompt_content_rows(app: &App, width: u16) -> usize {
    let map = prompt_ref_map(app);
    prompt_rows(&map.text, width, map.display_spans())
        .len()
        .clamp(1, PROMPT_MAX_ROWS)
}

/// 右对齐的一行胶囊：抬升底色 + 左侧强调条 + 菱形图标 + 正文。
///
/// 只铺自己那块，不整行压黑底，也不盖正文；`accent` 同时用于强调条与图标。
/// 直接写 buffer 以便单测断言落位与配色。
pub(super) fn draw_chip(
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

pub(super) fn draw_prompt(frame: &mut Frame, app: &mut App, area: Rect) {
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

pub(super) fn prompt_scroll_offset(
    visual_row: usize,
    total_rows: usize,
    visible_rows: usize,
) -> usize {
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
    let map = prompt_ref_map(app);
    let rows = prompt_rows(&map.text, area.width, map.display_spans());
    let cursor = map.to_display(app.prompt.cursor);
    let visual_row = prompt_cursor_row(&rows, cursor);
    let (offset, text) = rows[visual_row];
    let column = str_width(&text[..cursor - offset]);
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
    let map = prompt_ref_map(app);
    let top = area.y + 1;
    let visible_rows = usize::from(area.height.saturating_sub(2)).max(1);
    if row < top || row >= top + visible_rows as u16 {
        return None;
    }
    let left = area.x + 1 + PROMPT_PAD as u16 + PROMPT_MARKER as u16;
    let target_col = usize::from(col.saturating_sub(left));
    let visual_rows = prompt_rows(&map.text, area.width, map.display_spans());
    let visual_row = prompt_cursor_row(&visual_rows, map.to_display(app.prompt.cursor));
    let start = prompt_scroll_offset(visual_row, visual_rows.len(), visible_rows);
    let clicked_visual_row = start + usize::from(row - top);
    if let Some((start_offset, sub_text)) = visual_rows.get(clicked_visual_row) {
        Some(map.to_buffer(*start_offset + prompt_cell_range(sub_text, target_col).start))
    } else {
        Some(app.prompt_text().len())
    }
}

/// 显示格对应的字节范围；宽字符的两格归同一字符，尾随组合字符一并保留。
pub(super) fn prompt_cell_range(text: &str, column: usize) -> std::ops::Range<usize> {
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
    let map = prompt_ref_map(app);
    let rows = prompt_rows(&map.text, area.width, map.display_spans());
    let visible = usize::from(area.height - 2);
    let start = prompt_scroll_offset(
        prompt_cursor_row(&rows, map.to_display(app.prompt.cursor)),
        rows.len(),
        visible,
    );
    let top = area.y + 1;
    let visible =
        u16::try_from((rows.len() - start).min(visible)).expect("可见行数不超过输入框高度");
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
    // 显示偏移换算回原文；控件整体入选，边界吸附由 RefMap 保证。
    let (from, to) = (map.to_buffer(from), map.to_buffer(to));
    if from < to { &text[from..to] } else { "" }
}

/// 运行中且输入框有内容时，图例改为 queue 键位及其当前动作：默认排队（Enter 按偏好补充或中断），
/// `queue_on_enter` 时为补充或中断；未绑定 queue 键时返回 `None`，仍显示停止。
pub(crate) fn queue_legend(app: &App) -> Option<(String, Cow<'static, str>)> {
    if !app.prompt_running || app.prompt.is_empty() {
        return None;
    }
    let keys = app.preferences.keybindings.get("queue")?;
    let tip = if !app.preferences.queue_on_enter {
        t!("tui.prompt.queue")
    } else if app.preferences.active_submit == ActiveSubmit::Interrupt {
        t!("tui.prompt.interrupt")
    } else {
        t!("tui.prompt.steer")
    };
    (!keys.is_empty()).then(|| (labels::keys(keys), tip))
}

/// 输入框下边框上的图例：运行时显示停止操作（输入框有内容时显示排队），否则显示快捷键键位。
///
/// 图例直接写在边框行上，因此不额外占用终端行；空间不足时先让掉左侧提示，
/// 右侧的当前模型与模式仍然保留。
pub(super) fn draw_prompt_legends(frame: &mut Frame, app: &mut App, area: Rect) {
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
    let mut model = app.model_display();
    let status = app.status_label();
    let permission = app
        .session
        .as_ref()
        .map_or("full", |session| session.permission.as_str());
    let selecting = app.selection.is_some() && app.modal.is_none();
    let copy_feedback = app.modal.is_none()
        && app
            .selection_copy_deadline()
            .is_some_and(|deadline| std::time::Instant::now() < deadline);
    let (left_key, mut left_tip, left_color) = if selecting {
        (
            Cow::Borrowed("Ctrl+C"),
            t!("tui.prompt.copy_cancel"),
            theme.gray_bright,
        )
    } else if copy_feedback {
        (
            Cow::Borrowed(""),
            t!("tui.prompt.copied"),
            theme.gray_bright,
        )
    } else if let Some((queue_key, tip)) = queue_legend(app) {
        (Cow::Owned(queue_key), tip, theme.gray_bright)
    } else if app.prompt_running {
        (
            Cow::Borrowed("Esc"),
            if app.cancel_requested {
                t!("tui.prompt.stopping")
            } else {
                t!("tui.prompt.stop")
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
        (Cow::Owned(help_key), t!("tui.prompt.keys"), theme.gray_dim)
    };
    let left_key = left_key.as_ref();
    let compact_tip = if selecting {
        t!("tui.prompt.copy")
    } else {
        left_tip.clone()
    };
    let reserved_tips =
        str_width(left_key) + usize::from(!left_key.is_empty()) + str_width(&compact_tip);
    if !no_session {
        let fixed = str_width(&status) + str_width(permission) + 2 * str_width(labels::SEP);
        let budget = inner.saturating_sub(fixed + reserved_tips + 8);
        if str_width(&model) > budget {
            model = model
                .split_whitespace()
                .map(|part| part.rsplit('/').next().unwrap_or(part))
                .collect::<Vec<_>>()
                .join(" ");
            model = wrap::truncate(model, budget).into_owned();
        }
    }
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
    let show_cancel =
        selecting && str_width(left_key) + 1 + str_width(&left_tip) + right_width + 8 <= inner;
    if selecting && !show_cancel {
        left_tip = t!("tui.prompt.copy");
    }
    // 左端布局：`␣key tip`（胶囊存在时接 ` · capsule`），末尾再补一格空白，
    // 与右侧的留白对称；分隔点只画在提示与胶囊之间，不留孤点。
    let tips_width = str_width(left_key) + usize::from(!left_key.is_empty()) + str_width(&left_tip);
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
        buffer.set_string(x, y, &left_tip, Style::new().fg(left_color));
        x += cell_width(&left_tip);
        if selecting {
            let copy_width = cell_width(left_key) + 1 + cell_width(&t!("tui.prompt.copy"));
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
                        cell_width(&t!("tui.prompt.cancel_hint")),
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
            t!(
                "tui.prompt.capsule_fmt",
                runs = runs_n,
                approvals = approvals_n
            )
            .into_owned()
        } else {
            t!("tui.prompt.capsule_runs_fmt", runs = runs_n).into_owned()
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
    if app.prompt_text().starts_with('!') && permission != "full" {
        let note = t!("tui.prompt.manual_full_access");
        let has_left = end_x > area.x + 3;
        let nx = end_x + if has_left { cell_width(labels::SEP) } else { 0 };
        let cells = cell_width(&note);
        if usize::from(nx + cells + 1) + right_width + 4 <= inner + usize::from(area.x) {
            if has_left {
                buffer.set_string(end_x, y, labels::SEP, Style::new().fg(theme.border));
            }
            buffer.set_string(nx, y, &note, Style::new().fg(theme.accent_warning));
            end_x = nx + cells;
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
pub(super) fn cell_width(text: &str) -> u16 {
    u16::try_from(str_width(text)).unwrap_or(u16::MAX)
}

/// 输入框内容行：`❯` 提示符、引用控件高亮，空输入只有提示符。
///
/// 首行 `❯ ` 起头，续行按提示符宽度悬挂对齐，与正文的缩进节奏一致。
/// 文本先经 [`prompt_ref_map`] 把有效引用替换成「图标 名称」控件再折行。
pub(super) fn prompt_lines(app: &App, area: Rect) -> Vec<Line<'static>> {
    let theme = app.theme;
    let map = prompt_ref_map(app);
    let rows = prompt_rows(&map.text, area.width, map.display_spans());
    let marker = || {
        Span::styled(
            "❯ ".to_owned(),
            Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        )
    };
    if map.text.is_empty() {
        return vec![Line::from(vec![Span::raw(" "), marker()])];
    }
    let lines: Vec<Line<'static>> = rows
        .iter()
        .enumerate()
        .map(|(index, (offset, row))| {
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
            spans.extend(ref_row_spans(&map, *offset, row, theme));
            Line::from(spans)
        })
        .collect();
    let visible = usize::from(area.height.saturating_sub(2)).max(1);
    let visual_row = prompt_cursor_row(&rows, map.to_display(app.prompt.cursor));
    let start = prompt_scroll_offset(visual_row, lines.len().max(1), visible);
    lines.into_iter().skip(start).take(visible).collect()
}

/// 一显示行按控件显示段切 span：控件画成 accent 前景 + 抬升底色的整体块，其余正文色。
fn ref_row_spans(
    map: &crate::refs::RefMap,
    offset: usize,
    row: &str,
    theme: &crate::theme::Theme,
) -> Vec<Span<'static>> {
    let end = offset + row.len();
    let mut spans = Vec::new();
    let mut at = offset;
    for disp in map.display_spans() {
        if disp.end <= offset {
            continue;
        }
        if disp.start >= end {
            break;
        }
        let (start, stop) = (disp.start.max(offset), disp.end.min(end));
        if start > at {
            spans.push(Span::styled(
                map.text[at..start].to_owned(),
                Style::new().fg(theme.text_primary),
            ));
        }
        spans.push(Span::styled(
            map.text[start..stop].to_owned(),
            Style::new().fg(theme.accent_warning).bg(theme.bg_highlight),
        ));
        at = stop;
    }
    if at < end {
        spans.push(Span::styled(
            map.text[at..end].to_owned(),
            Style::new().fg(theme.text_primary),
        ));
    }
    spans
}

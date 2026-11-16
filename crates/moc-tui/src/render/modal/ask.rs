use super::ASK_CONFIRM_KEY_MAX;
use super::ASK_DETAIL_ROWS;
use super::ASK_NAV_ITEM_MAX;
use super::ASK_PREVIEW_LEFT_MIN;
use super::ASK_PREVIEW_MIN_WIDTH;
use super::ASK_PREVIEW_SEPARATOR;
use super::ASK_QUESTION_ROWS;
use super::AskLayout;
use crate::app::App;
use crate::modal::Modal;
use crate::render::labels;
use crate::render::overlay;
use crate::render::wrap::clip_line;
use crate::render::wrap::fit_line;
use crate::render::wrap::line_width;
use crate::render::wrap::str_width;
use crate::render::wrap::truncate;
use crate::theme::Theme;
use crate::widgets::prompt::clamp_cursor;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Block;
use ratatui::widgets::Padding;
use ratatui::widgets::Paragraph;
use rust_i18n::t;

/// 提问卡片：渲染的同时把每个选项（含自定义行）的命中矩形记到 `ask_option_hits`，
/// 鼠标点选据此换算。带选项预览时放宽到 [`overlay::CARD_WIDE_MAX_WIDTH`]。
/// 折叠态（parked）不画卡片：收成细条占住输入框位，整屏留给转写区。
pub(super) fn draw_ask_modal(frame: &mut Frame, app: &mut App, area: Rect) {
    let theme = app.theme;
    if matches!(app.modal, Some(Modal::Ask { parked: true, .. })) {
        app.ask_option_hits.clear();
        app.ask_nav_hits.clear();
        app.ask_card_area = Rect::default();
        app.ask_question_area = Rect::default();
        app.ask_detail_area = Rect::default();
        draw_ask_strip(frame, app);
        return;
    }
    let Some(modal) = app.modal.as_ref() else {
        return;
    };
    let max_width = if modal.has_option_preview() {
        overlay::CARD_WIDE_MAX_WIDTH
    } else {
        overlay::CARD_MAX_WIDTH
    };
    let width = overlay::card_width(area, max_width);
    let max_rows = overlay::max_body_rows(area, true).max(1);
    let AskLayout {
        mut lines,
        footer,
        option_hits,
        nav_hits,
        question_area,
        detail_area,
        preview_scroll_max,
    } = ask_layout(theme, modal, overlay::content_width(width), max_rows);
    // 问题、空行、自定义行是提问卡的最小骨架：极矮终端连骨架都放不下时只截掉底部，
    // 落在可见区外的命中由下方 `row_y` 统一丢弃。
    lines.truncate(max_rows);
    let card = overlay::Card {
        title: modal.title(),
        body: lines,
        footer: Some(footer),
    }
    .render(frame, area, width, theme);
    app.ask_card_area = card.area;
    app.key_hits = card.keys;
    let body = card.body;
    let row_y = |row: usize| -> Option<u16> {
        let y = body.y.checked_add(u16::try_from(row).ok()?)?;
        (y < body.bottom()).then_some(y)
    };
    app.ask_option_hits = option_hits
        .into_iter()
        .filter_map(|(option, row)| {
            Some((
                option,
                Rect::new(
                    card.area.x + 1,
                    row_y(row)?,
                    card.area.width.saturating_sub(2),
                    1,
                ),
            ))
        })
        .collect();
    app.ask_nav_hits = nav_hits
        .into_iter()
        .filter_map(|(question, row, start, end)| {
            Some((
                question,
                Rect::new(
                    body.x + u16::try_from(start).ok()?,
                    row_y(row)?,
                    u16::try_from(end - start).ok()?,
                    1,
                ),
            ))
        })
        .collect();
    // 可滚文本区换算到屏幕：被卡片底边裁掉的行不在命中范围内。
    let visible = usize::from(body.height);
    let map_area = |text: Rect| -> Rect {
        let top = usize::from(text.y);
        if top >= visible {
            return Rect::default();
        }
        let bottom = (top + usize::from(text.height)).min(visible);
        Rect::new(
            body.x + text.x,
            body.y + text.y,
            text.width.min(body.width.saturating_sub(text.x)),
            u16::try_from(bottom - top).unwrap_or(u16::MAX),
        )
    };
    app.ask_question_area = question_area.map_or_default(map_area);
    app.ask_detail_area = detail_area.map_or_default(map_area);
    // 事件层只加不减：详情/确认页的滚动偏移按本帧上限写回。
    if let Some(Modal::Ask { preview_scroll, .. }) = app.modal.as_mut() {
        *preview_scroll = (*preview_scroll).min(preview_scroll_max);
    }
}

/// 折叠的提问细条：占据输入框位（输入框本轮不绘制），左边是进度与当前问，
/// 右边是返回/取消键；边框用运行色标示「有待答的提问」，正文垂直居中。
pub(super) fn draw_ask_strip(frame: &mut Frame, app: &mut App) {
    let theme = app.theme;
    let Some(Modal::Ask {
        params,
        question,
        selected,
        custom,
        ..
    }) = app.modal.as_ref()
    else {
        return;
    };
    let mut area = app.prompt_area;
    if area.height < 3 {
        // 输入框区域不可用时兜底：贴画面底部三行，卡片语义不变。
        let frame_area = frame.area();
        area = Rect::new(
            frame_area.x,
            frame_area.y + frame_area.height.saturating_sub(3),
            frame_area.width,
            3.min(frame_area.height),
        );
    }
    let confirming = *question >= params.questions.len();
    let header = if confirming {
        t!("tui.ask.confirm_answers").into_owned()
    } else {
        params
            .questions
            .get(*question)
            .and_then(|entry| entry.header.clone())
            .unwrap_or_else(|| t!("tui.ask.question").into_owned())
    };
    // 内宽 = 边框 2 + 左右内边距 2。
    let inner = usize::from(area.width.saturating_sub(4));
    let (right, right_keys) = labels::hint_layout(
        &[
            ("tab", t!("tui.hint.expand_answer")),
            ("esc", t!("tui.hint.cancel")),
        ],
        usize::MAX,
    );
    let right_width = str_width(&right);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "▸ ",
        Style::new()
            .fg(theme.accent_running)
            .add_modifier(Modifier::BOLD),
    )];
    let mut used = 2usize;
    if params.questions.len() > 1 {
        let counter = if confirming {
            // 确认页折叠时给已答进度：这正是提交前最该看到的信息。
            let answered = (0..params.questions.len())
                .filter(|&index| {
                    selected.get(index).is_some_and(|picked| !picked.is_empty())
                        || custom
                            .get(index)
                            .is_some_and(|answer| !answer.text.is_empty())
                })
                .count();
            t!(
                "tui.ask.answered_fmt",
                answered = answered,
                total = params.questions.len()
            )
            .into_owned()
        } else {
            t!(
                "tui.ask.question_fmt",
                current = question + 1,
                total = params.questions.len()
            )
            .into_owned()
        };
        used += str_width(&counter);
        spans.push(Span::styled(counter, Style::new().fg(theme.gray)));
    }
    let header = truncate(
        &header,
        inner
            .saturating_sub(used + right_width + 2)
            .max(4)
            .min(inner.saturating_sub(used)),
    )
    .into_owned();
    used += str_width(&header);
    spans.push(Span::styled(
        header,
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    ));
    let inner_rows = usize::from(area.height.saturating_sub(2));
    let top_pad = inner_rows.saturating_sub(1) / 2;
    let mut keys = Vec::new();
    if used + right_width < inner {
        // 细条右端的键位同样可点：Tab 展开作答、Esc 取消整单。
        let x = area.x + 2 + u16::try_from(inner - right_width).unwrap_or(u16::MAX);
        let y = area.y + 1 + u16::try_from(top_pad).unwrap_or(u16::MAX);
        keys.extend(right_keys.into_iter().map(|key| key.at(x, y)));
        spans.push(Span::from(" ".repeat(inner - used - right_width)));
        spans.push(Span::styled(right, Style::new().fg(theme.gray_dim)));
    }
    let mut lines = vec![Line::from(String::new()); top_pad];
    lines.push(Line::from(spans));
    overlay::paint_opaque(
        frame.buffer_mut(),
        area,
        Style::new().bg(theme.bg_panel).fg(theme.text_primary),
    );
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.accent_running))
                .style(Style::new().bg(theme.bg_panel).fg(theme.text_primary))
                .padding(Padding::horizontal(1)),
        ),
        area,
    );
    app.ask_strip_area = area;
    app.key_hits = keys;
}

/// 在最终行列表里的行号，`draw_ask_modal` 据此换算鼠标点选的屏幕矩形。
/// 选项与自定义行一律截断到单行：`Wrap` 折行会让后续行号整体漂移。
pub(super) fn ask_layout(
    theme: &Theme,
    modal: &Modal,
    width: usize,
    max_lines: usize,
) -> AskLayout {
    let Modal::Ask {
        params,
        question,
        highlight,
        selected,
        custom,
        preview_scroll,
        question_scroll,
        ..
    } = modal
    else {
        return AskLayout::default();
    };
    let mut lines = Vec::new();
    let mut nav_hits = Vec::new();
    // `preview_scroll` 的本帧上限：详情窗口或确认页各算各的，绘制后写回。
    let mut preview_scroll_max = 0usize;
    // 多问时顶部是导航条而不是计数器：`n. header` 逐项排开，当前问反白、
    // 已答带 ✓；每一项记下列范围供鼠标跳问。
    if params.questions.len() > 1 {
        let nav_row = lines.len();
        let mut spans: Vec<Span<'static>> = Vec::new();
        let mut col = 0usize;
        for (index, entry) in params.questions.iter().enumerate() {
            let separator = if index > 0 { str_width(labels::SEP) } else { 0 };
            let answered = selected.get(index).is_some_and(|picked| !picked.is_empty())
                || custom
                    .get(index)
                    .is_some_and(|answer| !answer.text.is_empty());
            let header = entry
                .header
                .clone()
                .unwrap_or_else(|| t!("tui.ask.question").into_owned());
            let label = truncate(format!("{}. {header}", index + 1), ASK_NAV_ITEM_MAX).into_owned();
            let item_width = str_width(&label) + usize::from(answered) * 2;
            // 放不下的问题收成一个 `…`；连 `…` 都放不下就不画，导航条绝不越过正文宽度。
            if col + separator + item_width > width {
                if col + 2 <= width {
                    spans.push(Span::styled(" …", Style::new().fg(theme.gray_dim)));
                    col += 2;
                }
                break;
            }
            if index > 0 {
                spans.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
                col += separator;
            }
            nav_hits.push((index, nav_row, col, col + item_width));
            if answered {
                spans.push(Span::styled("✓ ", Style::new().fg(theme.accent_success)));
            }
            let style = if index == *question {
                Style::new()
                    .fg(theme.accent_user)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else if answered {
                Style::new().fg(theme.text_secondary)
            } else {
                Style::new().fg(theme.gray)
            };
            spans.push(Span::styled(label, style));
            col += item_width;
        }
        // 导航条末尾是确认页：越过最后一问强制经过，点击或 → 直达。
        let confirm_label = t!("tui.ask.confirm");
        let confirm_width = str_width(&confirm_label);
        let separator = str_width(labels::SEP);
        if col + separator + confirm_width <= width {
            spans.push(Span::styled(labels::SEP, Style::new().fg(theme.gray_dim)));
            nav_hits.push((
                params.questions.len(),
                nav_row,
                col + separator,
                col + separator + confirm_width,
            ));
            let style = if *question == params.questions.len() {
                Style::new()
                    .fg(theme.accent_user)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_secondary)
            };
            spans.push(Span::styled(confirm_label, style));
        }
        lines.push(Line::from(spans));
    }
    let Some(current) = params.questions.get(*question) else {
        return ask_confirm_layout(theme, modal, lines, nav_hits, width, max_lines);
    };
    lines.push(Line::from(String::new()));

    // 问题正文窗口：常规字重、限高滚动——长问题铺满整卡会把选项挤出视野；
    // 溢出时末行给出滚动状态（滚轮/PgUp/PgDn 读全文），窗口区记为可滚命中。
    let question_rows = crate::render::wrap::wrap_text(&current.question, width);
    let question_overflow = question_rows.len() > ASK_QUESTION_ROWS;
    let question_start = lines.len();
    lines.extend(scroll_window(
        question_rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, Style::new().fg(theme.text_primary))))
            .collect(),
        ASK_QUESTION_ROWS,
        *question_scroll,
        width,
        theme,
    ));
    let question_area = question_overflow.then(|| {
        Rect::new(
            0,
            u16::try_from(question_start).unwrap_or(u16::MAX),
            u16::try_from(width).unwrap_or(u16::MAX),
            u16::try_from(lines.len() - question_start).unwrap_or(u16::MAX),
        )
    });
    lines.push(Line::from(String::new()));

    // 当前问任一选项带预览就走双栏（问题级判定，焦点移动不跳版）：
    // 左栏恒为单行选项列表，右栏是聚焦项的详情（描述 + 预览）。
    let two_col = current.options.iter().any(|option| {
        option
            .preview
            .as_deref()
            .is_some_and(|text| !text.trim().is_empty())
    }) && width >= ASK_PREVIEW_MIN_WIDTH;
    let focused_option = current.options.get(highlight[*question]);
    // 单栏时聚焦项描述进底部固定区：分隔线 + 限高窗口。它是次信息——
    // 预算不足保住两个可见选项 + 自定义行时，先压详情区再整个让掉。
    let detail_content: Vec<Line<'static>> = if two_col {
        Vec::new()
    } else {
        focused_option
            .map(|option| focused_detail_lines(option, width, theme))
            .unwrap_or_default()
    };
    // 固定行：自定义行 1（提示条在卡片底部另算）。
    let fixed_tail = lines.len() + 1;
    let mut detail_zone_rows = if detail_content.is_empty() {
        0
    } else {
        1 + detail_content.len().min(ASK_DETAIL_ROWS)
    };
    if detail_zone_rows > 0 && max_lines.saturating_sub(fixed_tail + detail_zone_rows) < 2 {
        detail_zone_rows = max_lines.saturating_sub(fixed_tail + 2);
        // 分隔线 + 至少一行内容才有意义，否则整区让掉。
        if detail_zone_rows < 2 {
            detail_zone_rows = 0;
        }
    }
    // 剩余行预算给选项列表窗口 + 自定义行。
    let opt_budget = max_lines
        .saturating_sub(lines.len() + 1 + detail_zone_rows)
        .max(1);

    // 选项每项恒单行（描述已挪进详情区）；超出预算时窗口跟随光标，
    // 两端以「▲/▼ 还有 N 项」提示被藏起的项数。
    let mut option_rows: Vec<Line<'static>> = Vec::with_capacity(current.options.len());
    for (index, option) in current.options.iter().enumerate() {
        let chosen = selected[*question].contains(&option.label);
        let focused = index == highlight[*question];
        let marker = if focused { "❯" } else { " " };
        // 单选用 radio、多选用 checkbox：勾选框在单选上暗示可多选，是误导。
        let check = match (current.multi, chosen) {
            (true, true) => "☑",
            (true, false) => "☐",
            (false, true) => "●",
            (false, false) => "○",
        };
        let style = if focused {
            Style::new()
                .fg(theme.accent_user)
                .bg(theme.bg_visual)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        // 前缀 " ❯ ☑ " 占 5 列，推荐后缀「（推荐）」占 8 列，都从标签预算里扣除。
        let recommended = current.recommended.contains(&(index as u32));
        let label_budget = width
            .saturating_sub(5)
            .saturating_sub(if recommended { 8 } else { 0 });
        let mut spans = vec![
            Span::styled(format!(" {marker} {check} "), style),
            Span::styled(
                truncate(option.label.as_str(), label_budget).into_owned(),
                style,
            ),
        ];
        if recommended {
            spans.push(Span::styled(
                t!("tui.ask.recommended"),
                Style::new().fg(theme.gray),
            ));
        }
        // 单栏时聚焦行补满正文宽度，选中色带右缘整齐；双栏左栏由 join_columns 补齐。
        let mut row = Line::from(spans);
        if focused && !two_col {
            let used = line_width(&row);
            row.spans
                .push(Span::styled(" ".repeat(width.saturating_sub(used)), style));
        }
        option_rows.push(row);
    }
    let total_opts = current.options.len();
    let (mut start, mut take) = (0usize, total_opts.min(opt_budget));
    if total_opts > opt_budget {
        // 先按两端都有省略指示来估算窗口；落到边缘的指示行让位给选项。
        let take_max = opt_budget.saturating_sub(2).max(1);
        start = highlight[*question].min(total_opts.saturating_sub(take_max));
        take = take_max.min(total_opts - start);
        if start == 0 {
            take = (opt_budget - 1).min(total_opts);
        }
        if start + take >= total_opts {
            take = (opt_budget - usize::from(start > 0)).min(total_opts - start);
        }
    }
    let base = lines.len();
    let mut list: Vec<Line<'static>> = Vec::with_capacity(take + 3);
    let mut option_hits: Vec<(usize, usize)> = Vec::with_capacity(take + 1);
    if start > 0 {
        list.push(scroll_edge_hint('▲', start, theme));
    }
    for (index, row) in option_rows.iter().enumerate().skip(start).take(take) {
        option_hits.push((index, base + list.len()));
        list.push(row.clone());
    }
    if start + take < total_opts {
        list.push(scroll_edge_hint('▼', total_opts - start - take, theme));
    }
    let editing_here = highlight[*question] == current.options.len();
    let answer = custom.get(*question);
    let editing = answer.is_some_and(|answer| answer.editing);
    let custom_style = if editing {
        Style::new()
            .fg(theme.accent_running)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else if editing_here {
        Style::new()
            .fg(theme.accent_user)
            .bg(theme.bg_visual)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.gray)
    };
    let custom_display = match answer {
        Some(answer) if answer.editing => {
            let cursor_pos = clamp_cursor(&answer.text, answer.cursor);
            let (before, after) = answer.text.split_at(cursor_pos);
            format!("{before}▏{after}")
        }
        Some(answer) => answer.text.clone(),
        None => String::new(),
    };
    option_hits.push((current.options.len(), base + list.len()));
    list.push(Line::from(Span::styled(
        truncate(
            t!(
                "tui.ask.custom_fmt",
                marker = if editing_here { "❯" } else { " " },
                text = custom_display.as_str()
            ),
            width,
        )
        .into_owned(),
        custom_style,
    )));

    let mut detail_area = None;
    if two_col {
        // 双栏：左栏选项列表 + 右栏聚焦项详情（描述在前、预览在后，同一滚动窗）。
        let left_width = list
            .iter()
            .map(line_width)
            .max()
            .unwrap_or(0)
            .clamp(ASK_PREVIEW_LEFT_MIN, width / 2);
        let pane_width = width.saturating_sub(left_width + str_width(ASK_PREVIEW_SEPARATOR));
        let pane_budget = opt_budget + 1;
        let pane_content = focused_option
            .map(|option| focused_detail_lines(option, pane_width, theme))
            .unwrap_or_default();
        let pane_overflow = pane_content.len() > pane_budget;
        preview_scroll_max = scroll_window_max(pane_content.len(), pane_budget);
        let pane = scroll_window(
            pane_content,
            pane_budget,
            *preview_scroll,
            pane_width,
            theme,
        );
        let pane_rows = pane.len();
        let pane_start = lines.len();
        lines.extend(join_columns(list, pane, left_width, theme));
        if pane_overflow {
            detail_area = Some(Rect::new(
                u16::try_from(left_width + str_width(ASK_PREVIEW_SEPARATOR)).unwrap_or(u16::MAX),
                u16::try_from(pane_start).unwrap_or(u16::MAX),
                u16::try_from(pane_width).unwrap_or(u16::MAX),
                u16::try_from(pane_rows).unwrap_or(u16::MAX),
            ));
        }
    } else {
        lines.extend(list);
        if detail_zone_rows > 0 {
            // 底部固定详情区：分隔线与选项列表隔开，内容限高可滚；
            // 容量取预算核定的 detail_zone_rows - 1（可能与 ASK_DETAIL_ROWS 不同）。
            let capacity = detail_zone_rows - 1;
            let detail_start = lines.len();
            lines.push(Line::from(Span::styled(
                "─".repeat(width),
                Style::new().fg(theme.border),
            )));
            let overflow = detail_content.len() > capacity;
            preview_scroll_max = scroll_window_max(detail_content.len(), capacity);
            let windowed = scroll_window(detail_content, capacity, *preview_scroll, width, theme);
            lines.extend(windowed);
            if overflow {
                detail_area = Some(Rect::new(
                    0,
                    u16::try_from(detail_start).unwrap_or(u16::MAX),
                    u16::try_from(width).unwrap_or(u16::MAX),
                    u16::try_from(lines.len() - detail_start).unwrap_or(u16::MAX),
                ));
            }
        }
    }
    // 提示条：方向键、数字键属常规交互不教，只留反直觉的键；
    // 多选最后一问 enter 从切勾变成进确认页，提示如实切换。窄屏先丢跳过/换题。
    let last = *question + 1 >= params.questions.len();
    let mut items: Vec<(&str, std::borrow::Cow<'_, str>)> = vec![("shift+x", t!("tui.hint.skip"))];
    if params.questions.len() > 1 {
        items.push(("tab", t!("tui.hint.next_question")));
    }
    items.push(("esc", t!("tui.hint.collapse_card")));
    match (current.multi, last) {
        (true, true) => items.extend([
            ("space", t!("tui.hint.check")),
            ("enter", t!("tui.hint.confirm")),
        ]),
        (true, false) => items.push(("space/enter", t!("tui.hint.check"))),
        (false, _) => items.push(("enter", t!("tui.hint.choose"))),
    }
    AskLayout {
        lines,
        footer: overlay::footer(&items, width, theme),
        option_hits,
        nav_hits,
        question_area,
        detail_area,
        preview_scroll_max,
    }
}

/// `scroll_window` 实际采用的最大滚动偏移：状态行占去一行，偏移上限要少算它。
/// 渲染写回 `scroll` 与此用同一公式，保证事件里的大值一帧内就被收紧。
pub(super) fn scroll_window_max(total: usize, capacity: usize) -> usize {
    let capacity = capacity.max(1);
    if total <= capacity {
        0
    } else {
        total - (capacity - 1).max(1)
    }
}

/// 把内容截取为最多 `capacity` 行的滚动窗口；放不下时末行换成
/// 「⋯ a-b/total 行 · 滚轮/PgUp/PgDn」状态行（各滚动区同一约定），状态行截到 `width`。
pub(super) fn scroll_window(
    rows: Vec<Line<'static>>,
    capacity: usize,
    scroll: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let total = rows.len();
    let capacity = capacity.max(1);
    if total <= capacity {
        return rows;
    }
    // 只剩一行时状态行没有意义：留给内容本身。
    let visible = (capacity - 1).max(1);
    let offset = scroll.min(scroll_window_max(total, capacity));
    let mut out: Vec<Line<'static>> = rows.into_iter().skip(offset).take(visible).collect();
    if capacity > 1 {
        let status = t!(
            "tui.ask.scroll_status_fmt",
            omitted = labels::OMITTED,
            from = offset + 1,
            to = offset + visible,
            total = total,
            sep = labels::SEP,
            hints = labels::hints(&[("pgup/pgdn", t!("tui.hint.scroll"))], usize::MAX)
        )
        .into_owned();
        out.push(Line::from(Span::styled(
            truncate(status, width).into_owned(),
            Style::new().fg(theme.gray_dim),
        )));
    }
    out
}

/// 选项列表窗口的边缘指示：上沿 `▲ 还有 N 项`、下沿 `▼ 还有 N 项`。
pub(super) fn scroll_edge_hint(arrow: char, hidden: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        t!("tui.ask.more_items_fmt", arrow = arrow, count = hidden).into_owned(),
        Style::new().fg(theme.gray_dim),
    ))
}

/// 聚焦项的详情内容：描述在前（灰字折行）、预览在后（markdown 渲染）。
/// 单栏时进底部固定区，双栏时进右栏；聚焦项两者皆无则返回空。
pub(super) fn focused_detail_lines(
    option: &moc_protocol::methods::AskOption,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut rows: Vec<Line<'static>> = Vec::new();
    if let Some(description) = option
        .description
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        rows.extend(
            crate::render::wrap::wrap_text(description, width)
                .into_iter()
                .map(|row| Line::from(Span::styled(row, Style::new().fg(theme.gray)))),
        );
    }
    if let Some(preview) = option
        .preview
        .as_deref()
        .filter(|text| !text.trim().is_empty())
    {
        let mut rendered: Vec<Line<'static>> =
            crate::render::markdown::render_markdown(preview, width, theme)
                .into_iter()
                .map(|line| clip_line(line, width))
                .collect();
        while rendered
            .last()
            .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
        {
            rendered.pop();
        }
        if !rendered.is_empty() {
            if !rows.is_empty() {
                rows.push(Line::from(String::new()));
            }
            rows.extend(rendered);
        }
    }
    rows
}

/// 确认页布局：越过最后一问强制经过，逐问一行列出全部回答，未回答的用警告色标出。
/// `preview_scroll` 复用为答案列表的滚动偏移；返回值交给 `draw_ask_modal` 照常绘制。
pub(super) fn ask_confirm_layout(
    theme: &Theme,
    modal: &Modal,
    mut lines: Vec<Line<'static>>,
    nav_hits: Vec<(usize, usize, usize, usize)>,
    width: usize,
    max_lines: usize,
) -> AskLayout {
    let Modal::Ask {
        params,
        selected,
        custom,
        preview_scroll,
        ..
    } = modal
    else {
        return AskLayout {
            lines,
            nav_hits,
            ..AskLayout::default()
        };
    };
    lines.push(Line::from(String::new()));
    lines.push(Line::from(Span::styled(
        t!("tui.ask.confirm_answers"),
        Style::new()
            .fg(theme.gray_bright)
            .add_modifier(Modifier::BOLD),
    )));
    let rows = max_lines.saturating_sub(lines.len()).max(1);
    // key 列对齐：取 header 兜底 id，统一截断到同一列宽。
    let key_width = params
        .questions
        .iter()
        .map(|entry| str_width(entry.header.as_deref().unwrap_or(&entry.id)))
        .max()
        .unwrap_or(0)
        .min(ASK_CONFIRM_KEY_MAX);
    let mut answers = Vec::with_capacity(params.questions.len());
    for (index, entry) in params.questions.iter().enumerate() {
        let key = truncate(entry.header.as_deref().unwrap_or(&entry.id), key_width).into_owned();
        let pad = " ".repeat(key_width.saturating_sub(str_width(&key)));
        let mut spans = vec![Span::styled(
            format!("{key}{pad}: "),
            Style::new().fg(theme.text_secondary),
        )];
        // 勾选集合 + 自定义文本都算回答；都没有就是未回答。
        let mut parts = selected.get(index).cloned().unwrap_or_default();
        if let Some(text) = custom
            .get(index)
            .map(|answer| answer.text.trim())
            .filter(|text| !text.is_empty())
        {
            parts.push(text.to_string());
        }
        if parts.is_empty() {
            spans.push(Span::styled(
                t!("tui.ask.unanswered"),
                Style::new()
                    .fg(theme.accent_warning)
                    .add_modifier(Modifier::BOLD),
            ));
        } else {
            spans.push(Span::styled(
                truncate(parts.join(", "), width.saturating_sub(key_width + 2)).into_owned(),
                Style::new().fg(theme.text_primary),
            ));
        }
        answers.push(Line::from(spans));
    }
    let scroll_max = answers.len().saturating_sub(rows);
    let scroll = (*preview_scroll).min(scroll_max);
    lines.extend(answers.into_iter().skip(scroll).take(rows));
    AskLayout {
        lines,
        footer: overlay::footer(
            &[
                ("shift+x", t!("tui.hint.skip")),
                ("esc", t!("tui.hint.collapse_card")),
                ("enter", t!("tui.hint.submit")),
            ],
            width,
            theme,
        ),
        nav_hits,
        preview_scroll_max: scroll_max,
        ..AskLayout::default()
    }
}

/// 并排拼接左右两栏：左栏补齐到 `left_width` 列后接竖线分隔，再接右栏。
pub(super) fn join_columns(
    left: Vec<Line<'static>>,
    right: Vec<Line<'static>>,
    left_width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut left = left.into_iter();
    let mut right = right.into_iter();
    let rows = left.len().max(right.len());
    let mut out: Vec<Line<'static>> = Vec::with_capacity(rows);
    for _ in 0..rows {
        let mut line = fit_line(
            left.next().unwrap_or_else(|| Line::from(String::new())),
            left_width,
        );
        line.spans.push(Span::styled(
            ASK_PREVIEW_SEPARATOR,
            Style::new().fg(theme.border),
        ));
        line.spans.extend(
            right
                .next()
                .unwrap_or_else(|| Line::from(String::new()))
                .spans,
        );
        out.push(line);
    }
    out
}

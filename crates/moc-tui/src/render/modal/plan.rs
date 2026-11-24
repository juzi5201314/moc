use super::PLAN_HEADER_ROWS;
use super::WrappedInput;
use crate::app::App;
use crate::modal::Modal;
use crate::modal::PlanControl;
use crate::modal::PlanEditorAction;
use crate::modal::PlanReviewState;
use crate::modal::PlanSelAction;
use crate::modal::PlanTab;
use crate::render::labels;
use crate::render::labels::KeyHit;
use crate::render::overlay;
use crate::render::overlay::Button;
use crate::render::overlay::ButtonKind;
use crate::render::overlay::ButtonState;
use crate::render::overlay::Chip;
use crate::render::overlay::Control;
use crate::render::overlay::RowAlign;
use crate::render::wrap::clip_line;
use crate::render::wrap::str_width;
use crate::render::wrap::truncate;
use crate::theme::Theme;
use moc_protocol::methods::PlanStatus;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use moc_i18n::t;

/// 计划标签正文左侧的 gutter 宽度（列）：`▎`/`+`/空白。
const GUTTER: usize = 2;

/// 一次渲染产出的计划命中几何（行号即正文行，x 为正文内相对列）。
pub(super) struct PlanHits {
    /// 标签栏条目。
    pub tabs: Vec<(PlanTab, usize, usize, usize)>,
    /// 选区条按钮。
    pub selbar: Vec<(PlanSelAction, usize, usize, usize)>,
    /// 动作区控件（芯片 + 按钮）。
    pub controls: Vec<(PlanControl, usize, usize, usize)>,
    /// 计划标签全部渲染行 → 源码区间（scroll 前的完整列表）。
    pub line_sources: Vec<Option<(usize, usize)>>,
    /// 本帧实际取用的正文起点（渲染行下标）。
    pub scroll_offset: usize,
    /// 正文实际行数（不含头尾）：body_area 高度按它登记。
    pub body_rows: usize,
}

/// 状态文字与配色。
pub(super) fn plan_status_label(theme: &Theme, status: PlanStatus) -> (&'static str, Color) {
    use labels::status;
    match status {
        PlanStatus::Draft => (status::DRAFT, theme.accent_running),
        PlanStatus::Submitted => (status::IN_REVIEW, theme.accent_success),
        PlanStatus::Revised => (status::REVISING, theme.accent_warning),
        PlanStatus::Superseded => (status::SUPERSEDED, theme.gray),
        PlanStatus::Approved => (status::APPROVED, theme.accent_success),
        PlanStatus::Rejected => (status::REJECTED, theme.accent_error),
    }
}

/// 非待审阅版本的说明行。
pub(super) fn plan_status_note(status: PlanStatus) -> std::borrow::Cow<'static, str> {
    match status {
        PlanStatus::Draft => t!("tui.plan.note_draft"),
        PlanStatus::Revised => t!("tui.plan.note_revised"),
        PlanStatus::Superseded => t!("tui.plan.note_superseded"),
        PlanStatus::Approved => t!("tui.plan.note_approved"),
        PlanStatus::Rejected => t!("tui.plan.note_rejected"),
        PlanStatus::Submitted => "".into(),
    }
}

/// 动作区按钮（显示顺序即焦点环顺序；`Submit` 仅在有草稿时出现）。
fn plan_buttons(state: &PlanReviewState) -> Vec<(PlanControl, Control)> {
    let make = |control: PlanControl, label: String, kind: ButtonKind| {
        let button_state = if state.submitting {
            ButtonState::Disabled
        } else if state.focus == control {
            ButtonState::Focused
        } else {
            ButtonState::Normal
        };
        (
            control,
            Control::Button(Button {
                label,
                kind,
                state: button_state,
            }),
        )
    };
    let secondary =
        |control: PlanControl, label: String| make(control, label, ButtonKind::Secondary);
    let mut items = vec![secondary(
        PlanControl::Resume,
        t!("tui.plan.btn_resume").into_owned(),
    )];
    if !state.drafts.is_empty() {
        items.push(secondary(
            PlanControl::Submit,
            t!("tui.plan.btn_submit_fmt", count = state.drafts.len()).into_owned(),
        ));
    }
    items.push(secondary(
        PlanControl::RunNew,
        t!("tui.plan.btn_run_new").into_owned(),
    ));
    items.push(secondary(
        PlanControl::RunCompacted,
        t!("tui.plan.btn_run_compact").into_owned(),
    ));
    items.push(make(
        PlanControl::Run,
        t!("tui.plan.btn_run").into_owned(),
        ButtonKind::Primary,
    ));
    items
}

/// 动作区芯片：会话模型 / 新会话模型 / 新会话引擎。
fn plan_chips(app: &App, state: &PlanReviewState) -> Vec<(PlanControl, Control)> {
    // 与输入框下边框图例同一个格式化函数，显示口径保持一致。
    let new_model = state
        .execution_model
        .as_deref()
        .map(crate::app::model_selector_label)
        .unwrap_or_else(|| app.model_display());
    vec![
        (
            PlanControl::Model,
            Control::Chip(Chip {
                caption: t!("tui.plan.chip_model").into_owned(),
                value: app.model_display(),
                focused: state.focus == PlanControl::Model,
            }),
        ),
        (
            PlanControl::NewModel,
            Control::Chip(Chip {
                caption: t!("tui.plan.chip_new_model").into_owned(),
                value: new_model,
                focused: state.focus == PlanControl::NewModel,
            }),
        ),
        (
            PlanControl::Engine,
            Control::Chip(Chip {
                caption: t!("tui.plan.chip_engine").into_owned(),
                value: crate::app::engine_label(state.execution_engine).to_owned(),
                focused: state.focus == PlanControl::Engine,
            }),
        ),
    ]
}

/// 动作区需要的固定行数（细分隔线 + 芯片行数 + 按钮行数）。
fn plan_action_rows(app: &App, state: &PlanReviewState, width: usize, theme: &Theme) -> usize {
    let controls: Vec<Control> = plan_buttons(state)
        .into_iter()
        .map(|(_, control)| control)
        .collect();
    let chips: Vec<Control> = plan_chips(app, state)
        .into_iter()
        .map(|(_, control)| control)
        .collect();
    1 + overlay::layout_controls(&chips, width.max(1), RowAlign::Left, 2, theme)
        .0
        .len()
        + overlay::layout_controls(&controls, width.max(1), RowAlign::Right, 2, theme)
            .0
            .len()
}

/// 正文之外的固定行数；渲染与鼠标命中都走它，两边算出的正文窗口才会一致。
///
/// 头部 [`PLAN_HEADER_ROWS`] 行；尾部非空时先空一行再排（选区条 / 动作区 /
/// 状态说明 / 错误）。操作提示在卡片底部的提示条里，不占正文行。
pub(crate) fn plan_fixed_rows(
    app: &App,
    state: &PlanReviewState,
    width: usize,
    theme: &Theme,
) -> usize {
    let mut tail = 0;
    if state.status == PlanStatus::Submitted {
        // 选区条（可批注时总是占位）+ 动作区。
        tail += 1 + plan_action_rows(app, state, width, theme);
    } else {
        tail += 1;
    }
    if state.error.is_some() || state.submitting {
        tail += 1;
    }
    PLAN_HEADER_ROWS + if tail > 0 { tail + 1 } else { 0 }
}

/// 正文可见行数。
pub(crate) fn plan_body_rows(
    app: &App,
    state: &PlanReviewState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
) -> usize {
    max_lines
        .saturating_sub(plan_fixed_rows(app, state, width, theme))
        .max(1)
}

/// 源码标签当前可见的源码行（0-based，与屏幕行一一对应）。
pub(crate) fn plan_source_window(
    app: &App,
    state: &PlanReviewState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
) -> Vec<usize> {
    let rows = plan_body_rows(app, state, width, max_lines, theme);
    let total = state.source_lines().len();
    let offset = state.scroll_offset.min(total.saturating_sub(rows));
    (offset..(offset + rows).min(total)).collect()
}

/// 标签栏：当前标签加粗高亮，其余暗色；右端 dim 显示批注计数。
fn plan_tab_bar(
    state: &PlanReviewState,
    width: usize,
    theme: &Theme,
) -> (Line<'static>, Vec<(PlanTab, usize, usize)>) {
    let tabs = [
        (PlanTab::Diff, t!("tui.plan.tab_diff")),
        (PlanTab::Plan, t!("tui.plan.tab_plan")),
        (PlanTab::Source, t!("tui.plan.tab_source")),
    ];
    let count = if state.drafts.is_empty() && state.submitted.is_empty() {
        String::new()
    } else {
        t!(
            "tui.plan.annot_count_fmt",
            drafts = state.drafts.len(),
            submitted = state.submitted.len()
        )
        .into_owned()
    };
    let mut spans = Vec::new();
    let mut hits = Vec::new();
    let mut col = 0usize;
    for (index, (tab, label)) in tabs.iter().enumerate() {
        if *tab == PlanTab::Diff && state.previous_plan_id.is_none() {
            continue;
        }
        if index > 0 && !spans.is_empty() {
            spans.push(Span::styled("│", Style::new().fg(theme.gray_dim)));
            col += 1;
        }
        let label = format!(" {} ", label);
        let label_width = str_width(&label);
        let style = if *tab == state.tab {
            Style::new()
                .fg(theme.text_primary)
                .bg(theme.bg_highlight)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        spans.push(Span::styled(label, style));
        hits.push((*tab, col, label_width));
        col += label_width;
    }
    let count_width = str_width(&count);
    if let Some(pad) = width
        .checked_sub(col)
        .and_then(|room| room.checked_sub(count_width))
    {
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(count, Style::new().fg(theme.gray_dim)));
    }
    (clip_line(Line::from(spans), width), hits)
}

/// 选区条：有选区显示 `第 a–b 行` + 按钮，否则给一句操作提示。
fn plan_selbar_line(
    state: &PlanReviewState,
    width: usize,
    theme: &Theme,
) -> (Line<'static>, Vec<(PlanSelAction, usize, usize)>) {
    let mut hits = Vec::new();
    let show_selection = match state.tab {
        PlanTab::Source => true,
        PlanTab::Plan => state.has_selection(),
        PlanTab::Diff => false,
    };
    if !show_selection {
        let hint = if state.tab == PlanTab::Diff {
            t!("tui.plan.sel_hint_diff")
        } else {
            t!("tui.plan.sel_hint_plan")
        };
        return (
            Line::from(Span::styled(
                truncate(hint.into_owned(), width).into_owned(),
                Style::new().fg(theme.gray_dim),
            )),
            hits,
        );
    }
    let (start, end) = state.selection();
    let label = format!(
        " {} ",
        t!("tui.plan.sel_lines_fmt", start = start + 1, end = end + 1)
    );
    let label_width = str_width(&label);
    let button = |label: String| {
        Control::Button(Button {
            label,
            kind: ButtonKind::Secondary,
            state: ButtonState::Normal,
        })
    };
    let mut items = vec![(
        PlanSelAction::Annotate,
        Control::Button(Button {
            label: t!("tui.plan.sel_annotate").into_owned(),
            kind: ButtonKind::Primary,
            state: ButtonState::Normal,
        }),
    )];
    if state.draft_at_cursor().is_some() {
        items.push((
            PlanSelAction::Delete,
            button(t!("tui.plan.sel_delete").into_owned()),
        ));
    }
    items.push((
        PlanSelAction::Clear,
        button(t!("tui.plan.sel_clear").into_owned()),
    ));
    let controls: Vec<Control> = items.iter().map(|(_, control)| control.clone()).collect();
    let room = width.saturating_sub(label_width).max(1);
    let (rows, slots) = overlay::layout_controls(&controls, room, RowAlign::Left, 2, theme);
    let mut spans = vec![Span::styled(label, Style::new().fg(theme.text_secondary))];
    if let Some(row) = rows.into_iter().next() {
        spans.extend(row.spans);
    }
    for ((action, _), slot) in items.into_iter().zip(slots) {
        hits.push((action, label_width + slot.col, slot.width));
    }
    (clip_line(Line::from(spans), width), hits)
}

/// 计划标签正文行：2 列 gutter + 按 `width - gutter` 排版的渲染行，
/// 选区/悬停块铺底色。
fn plan_markdown_line(
    state: &PlanReviewState,
    line: Line<'static>,
    source: Option<(usize, usize)>,
    hovered_first: Option<usize>,
    row: usize,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let drafted = source.is_some_and(|(start, end)| {
        state
            .drafts
            .iter()
            .any(|draft| draft.start_line <= end && draft.end_line >= start)
    });
    let submitted = source.is_some_and(|(start, end)| {
        state
            .submitted
            .iter()
            .any(|note| note.start_line as usize <= end && note.end_line as usize >= start)
    });
    // 不可批注时不画悬停块；拖选进行中不显示 `+`/淡底。
    let hovered = state.can_annotate() && !state.dragging && source == state.hover_block;
    let (sel_start, sel_end) = state.selection();
    let selected = state.has_selection()
        && source.is_some_and(|(start, end)| start - 1 <= sel_end && end > sel_start);
    let gutter = if drafted {
        Span::styled("▎ ", Style::new().fg(theme.accent_warning))
    } else if submitted {
        Span::styled("▎ ", Style::new().fg(theme.gray))
    } else if hovered && hovered_first == Some(row) {
        Span::styled("+ ", Style::new().fg(theme.accent_user))
    } else {
        Span::raw("  ")
    };
    let bg = if selected {
        Some(theme.bg_visual)
    } else if hovered {
        Some(theme.bg_hover)
    } else {
        None
    };
    let mut spans = Vec::with_capacity(line.spans.len() + 2);
    spans.push(gutter);
    let mut text_width = 0usize;
    for mut span in line.spans.into_iter() {
        text_width += str_width(&span.content);
        if let Some(bg) = bg {
            span.style = span.style.bg(bg);
        }
        spans.push(span);
    }
    // 补满到整行宽度，让选区/悬停色带整齐覆盖整块。
    if let Some(bg) = bg
        && let Some(pad) = width.saturating_sub(GUTTER).checked_sub(text_width)
    {
        spans.push(Span::styled(" ".repeat(pad), Style::new().bg(bg)));
    }
    Line::from(spans)
}

/// 计划审阅卡片的正文行与提示条。
pub(super) fn plan_lines(
    app: &App,
    state: &PlanReviewState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
    scroll_max: &mut usize,
) -> (Vec<Line<'static>>, overlay::Footer, PlanHits) {
    let mut hits = PlanHits {
        tabs: Vec::new(),
        selbar: Vec::new(),
        controls: Vec::new(),
        line_sources: Vec::new(),
        scroll_offset: 0,
        body_rows: 0,
    };
    let mut lines = Vec::with_capacity(max_lines);

    // ① 标题 · vN · 状态：状态先占位、标题吃剩下的宽度。
    let (status_label, status_color) = plan_status_label(theme, state.status);
    let status = format!("{}{status_label}", labels::SEP);
    lines.push(clip_line(
        Line::from(vec![
            Span::styled(
                truncate(
                    format!("{}{}v{}", state.title, labels::SEP, state.version),
                    width.saturating_sub(str_width(&status)),
                )
                .into_owned(),
                Style::new()
                    .fg(theme.text_primary)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(status, Style::new().fg(status_color)),
        ]),
        width,
    ));
    // ② 路径（+「对比 vN-1」）。
    let mut location = state.path.clone();
    if state.previous_plan_id.is_some() {
        location.push_str(&t!(
            "tui.plan.diff_ver_fmt",
            ver = state.version.saturating_sub(1)
        ));
    }
    lines.push(Line::from(Span::styled(
        truncate(&location, width).into_owned(),
        Style::new().fg(theme.gray),
    )));
    // ③ 标签栏。
    let (tab_bar, tab_hits) = plan_tab_bar(state, width, theme);
    hits.tabs = tab_hits
        .into_iter()
        .map(|(tab, col, w)| (tab, lines.len(), col, w))
        .collect();
    lines.push(tab_bar);
    debug_assert_eq!(lines.len(), PLAN_HEADER_ROWS);

    let body_start = lines.len();
    let body_rows = plan_body_rows(app, state, width, max_lines, theme);
    // 正文恒让出最右一列给滚动条（溢出时才画，未溢出只是空列）。
    let body_width = width.saturating_sub(1).max(1);
    match state.tab {
        PlanTab::Diff => {
            if let Some(error) = &state.diff_error {
                lines.push(Line::from(Span::styled(
                    truncate(
                        t!("tui.plan.diff_load_failed_fmt", err = error.as_str()),
                        width,
                    )
                    .into_owned(),
                    Style::new().fg(theme.accent_error),
                )));
            } else if let Some(diff) = &state.diff {
                let rendered =
                    crate::render::diff::render_diff(diff, body_width, theme, usize::MAX);
                if rendered.is_empty() {
                    lines.push(Line::from(Span::styled(
                        t!("tui.plan.no_text_changes"),
                        Style::new().fg(theme.gray),
                    )));
                } else {
                    *scroll_max = rendered.len().saturating_sub(body_rows);
                    lines.extend(
                        rendered
                            .into_iter()
                            .skip(state.scroll_offset.min(*scroll_max))
                            .take(body_rows),
                    );
                }
            } else {
                lines.push(Line::from(Span::styled(
                    t!("tui.plan.diff_loading"),
                    Style::new().fg(theme.accent_running),
                )));
            }
        }
        PlanTab::Plan => {
            let text_width = body_width.saturating_sub(GUTTER).max(1);
            let (rendered, sources) = crate::render::markdown::render_markdown_with_sources(
                &state.markdown,
                text_width,
                theme,
            );
            if rendered.is_empty() {
                lines.push(Line::from(String::new()));
            } else {
                // 事件层只加不减：渲染时按正文行数收紧偏移并写回。
                *scroll_max = rendered.len().saturating_sub(body_rows);
                let offset = state.scroll_offset.min(*scroll_max);
                hits.scroll_offset = offset;
                hits.line_sources = sources.clone();
                let hovered_first = state
                    .hover_block
                    .and_then(|block| sources.iter().position(|source| *source == Some(block)));
                for (row, (line, source)) in rendered
                    .into_iter()
                    .zip(sources.iter().copied())
                    .enumerate()
                    .skip(offset)
                    .take(body_rows)
                {
                    lines.push(plan_markdown_line(
                        state,
                        line,
                        source,
                        hovered_first,
                        row,
                        body_width,
                        theme,
                    ));
                }
            }
        }
        PlanTab::Source => {
            let source = state.source_lines();
            for index in plan_source_window(app, state, width, max_lines, theme) {
                lines.push(plan_source_line(state, &source, index, body_width, theme));
            }
        }
    }
    hits.body_rows = lines.len() - body_start;

    // 提示条从左丢项：引擎/模型等次要键先让位，滚动与确认/关闭恒在。
    let submitted = state.status == PlanStatus::Submitted;
    let mut items: Vec<(&str, std::borrow::Cow<'_, str>)> = Vec::with_capacity(8);
    match state.tab {
        PlanTab::Plan | PlanTab::Diff => {
            if submitted {
                items.extend([
                    ("m", t!("tui.hint.pick_model")),
                    ("n", t!("tui.hint.pick_new_model")),
                    ("e", t!("tui.hint.switch_engine")),
                ]);
            }
            if state.previous_plan_id.is_some() {
                items.push((
                    "d",
                    if state.tab == PlanTab::Diff {
                        t!("tui.hint.view_plan")
                    } else {
                        t!("tui.hint.view_diff")
                    },
                ));
            }
            if state.can_annotate() {
                items.push(("a", t!("tui.hint.annotate")));
            }
            items.push(("↑↓/pgup/pgdn", t!("tui.hint.scroll")));
            if submitted {
                items.extend([
                    ("←→/tab", t!("tui.hint.choose")),
                    ("enter", t!("tui.hint.confirm")),
                ]);
            }
            items.push(("esc", t!("tui.hint.close")));
        }
        PlanTab::Source => items.extend([
            ("ctrl+g", t!("tui.hint.suggest")),
            ("x", t!("tui.hint.delete")),
            ("v", t!("tui.hint.select_lines")),
            ("↑↓", t!("tui.hint.move")),
            ("c", t!("tui.hint.annotate")),
            ("a/esc", t!("tui.hint.back_to_read")),
            ("s", t!("tui.hint.send")),
        ]),
    }
    let footer = overlay::footer(&items, width, theme);

    let tail_start = lines.len();
    if plan_fixed_rows(app, state, width, theme) > PLAN_HEADER_ROWS {
        lines.push(Line::from(String::new()));
    }
    if state.status == PlanStatus::Submitted {
        // 选区条：可批注时总是占位，避免布局跳动。
        let (selbar, selbar_hits) = plan_selbar_line(state, width, theme);
        hits.selbar = selbar_hits
            .into_iter()
            .map(|(action, col, w)| (action, lines.len(), col, w))
            .collect();
        lines.push(selbar);
        // 动作区：细分隔线 → 芯片行 → 按钮行（放不下换行）。
        lines.push(Line::from(Span::styled(
            "─".repeat(width),
            Style::new().fg(theme.border_subtle),
        )));
        let chips = plan_chips(app, state);
        let chip_controls: Vec<Control> =
            chips.iter().map(|(_, control)| control.clone()).collect();
        let (chip_rows, chip_slots) =
            overlay::layout_controls(&chip_controls, width, RowAlign::Left, 2, theme);
        let last_row = chip_rows.len().saturating_sub(1);
        for (row_index, mut row) in chip_rows.into_iter().enumerate() {
            hits.controls.extend(
                chips
                    .iter()
                    .zip(&chip_slots)
                    .filter(|(_, slot)| slot.row == row_index)
                    .map(|((control, _), slot)| (*control, lines.len(), slot.col, slot.width)),
            );
            // 「N 条批注待提交」放最后一行芯片右侧，放得下才放。
            if row_index == last_row && !state.drafts.is_empty() {
                let pending = t!("tui.plan.pending_notes_fmt", count = state.drafts.len());
                let used = chip_slots
                    .iter()
                    .filter(|slot| slot.row == row_index)
                    .map(|slot| slot.col + slot.width)
                    .max()
                    .unwrap_or(0);
                if let Some(pad) = width.checked_sub(used + str_width(&pending)) {
                    row.spans.push(Span::raw(" ".repeat(pad)));
                    row.spans.push(Span::styled(
                        pending.into_owned(),
                        Style::new().fg(theme.gray_dim),
                    ));
                }
            }
            lines.push(row);
        }
        let buttons = plan_buttons(state);
        let controls: Vec<Control> = buttons.iter().map(|(_, control)| control.clone()).collect();
        let (rows, slots) = overlay::layout_controls(&controls, width, RowAlign::Right, 2, theme);
        for (row_index, row) in rows.into_iter().enumerate() {
            hits.controls.extend(
                buttons
                    .iter()
                    .zip(&slots)
                    .filter(|(_, slot)| slot.row == row_index)
                    .map(|((control, _), slot)| {
                        (*control, lines.len() + row_index, slot.col, slot.width)
                    }),
            );
            lines.push(row);
        }
    } else {
        lines.push(Line::from(Span::styled(
            truncate(plan_status_note(state.status).into_owned(), width).into_owned(),
            Style::new().fg(theme.gray),
        )));
    }

    if let Some(error) = &state.error {
        lines.push(Line::from(Span::styled(
            truncate(t!("tui.err.fmt", err = error.as_str()), width).into_owned(),
            Style::new().fg(theme.accent_error),
        )));
    } else if state.submitting {
        lines.push(Line::from(Span::styled(
            t!("tui.plan.submitting"),
            Style::new().fg(theme.accent_running),
        )));
    }
    debug_assert_eq!(
        lines.len() - tail_start,
        plan_fixed_rows(app, state, width, theme) - PLAN_HEADER_ROWS,
        "尾部行数必须与 plan_fixed_rows 一致，否则正文窗口与鼠标映射错位"
    );
    (lines, footer, hits)
}

/// 源码标签的一行：行号 gutter + 内容，选区铺底、光标加粗、有批注的行标 `●`。
pub(super) fn plan_source_line(
    state: &PlanReviewState,
    source: &[&str],
    index: usize,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let line_no = index + 1;
    let annotated = state.drafts.iter().any(|draft| draft.covers(line_no))
        || state.submitted.iter().any(|annotation| {
            annotation.start_line as usize <= line_no && line_no <= annotation.end_line as usize
        });
    let (sel_start, sel_end) = state.selection();
    let selected = index >= sel_start && index <= sel_end;
    let cursor = index == state.cursor;
    let base = if selected {
        Style::new().bg(theme.bg_visual)
    } else {
        Style::new()
    };
    let gutter = format!("{line_no:>4} {} ", if annotated { "●" } else { "│" });
    let gutter_width = str_width(&gutter);
    let mut text = truncate(
        source.get(index).copied().unwrap_or(""),
        width.saturating_sub(gutter_width),
    )
    .into_owned();
    // 补到整行宽度，让选区底色铺满整行而不是只盖住文字。
    let text_width = str_width(&text);
    if let Some(pad) = width.checked_sub(gutter_width + text_width) {
        text.push_str(&" ".repeat(pad));
    }
    let mut text_style = base.fg(theme.text_primary);
    if cursor {
        text_style = text_style.add_modifier(Modifier::BOLD);
    }
    Line::from(vec![
        Span::styled(
            gutter,
            base.fg(if annotated {
                theme.accent_warning
            } else {
                theme.gray_dim
            }),
        ),
        Span::styled(text, text_style),
    ])
}

/// 多行输入按宽度折行（无掩码），并给出光标所在行与列。
pub(super) fn wrap_input(
    input: &crate::widgets::prompt::PromptState,
    width: usize,
) -> WrappedInput {
    let width = width.max(1);
    let text = input.text.as_str();
    let prefix = &input.text[..input.cursor];
    let cursor_line = prefix.matches('\n').count();
    let mut lines: Vec<String> = Vec::new();
    let mut cursor_row = 0;
    let mut cursor_column = 0;
    for (index, line) in text.split('\n').enumerate() {
        if index == cursor_line {
            let head = prefix.rsplit('\n').next().unwrap_or_default();
            let wrapped = crate::render::wrap::wrap_text(head, width);
            cursor_row = lines.len() + wrapped.len().max(1) - 1;
            cursor_column = wrapped.last().map_or(0, |line| str_width(line));
        }
        let wrapped = crate::render::wrap::wrap_text(line, width);
        if wrapped.is_empty() {
            lines.push(String::new());
        } else {
            lines.extend(wrapped);
        }
    }
    WrappedInput {
        lines,
        cursor_row,
        cursor_column,
    }
}

/// 批注编辑器底部按钮行：左「插入建议」（编辑草稿时加「删除」），
/// 右「取消 / 保存」；返回按钮与各自列区间（行内相对坐标）。
fn editor_buttons(
    editor: &crate::modal::PlanAnnotationEditor,
    width: usize,
    theme: &Theme,
) -> (Line<'static>, Vec<(PlanEditorAction, usize, usize)>) {
    let make = |action: PlanEditorAction, label: String, kind: ButtonKind| {
        (
            action,
            Control::Button(Button {
                label,
                kind,
                state: ButtonState::Normal,
            }),
        )
    };
    let right = vec![
        make(
            PlanEditorAction::Cancel,
            t!("tui.plan.editor_cancel").into_owned(),
            ButtonKind::Secondary,
        ),
        make(
            PlanEditorAction::Save,
            t!("tui.plan.editor_save").into_owned(),
            ButtonKind::Primary,
        ),
    ];
    let mut left = vec![make(
        PlanEditorAction::Suggest,
        t!("tui.plan.editor_suggest").into_owned(),
        ButtonKind::Secondary,
    )];
    if editor.editing.is_some() {
        left.push(make(
            PlanEditorAction::Delete,
            t!("tui.plan.editor_delete").into_owned(),
            ButtonKind::Secondary,
        ));
    }
    let width_of = |items: &[(PlanEditorAction, Control)]| {
        items.iter().map(|(_, c)| c.width()).sum::<usize>() + 2 * items.len().saturating_sub(1)
    };
    let (left_w, right_w) = (width_of(&left), width_of(&right));
    let right_controls: Vec<Control> = right.iter().map(|(_, c)| c.clone()).collect();
    let (right_rows, right_slots) =
        overlay::layout_controls(&right_controls, width, RowAlign::Right, 2, theme);
    let right_line = right_rows.into_iter().next().unwrap_or_default();
    let mut hits: Vec<(PlanEditorAction, usize, usize)> = right
        .iter()
        .zip(&right_slots)
        .map(|((action, _), slot)| (*action, slot.col, slot.width))
        .collect();
    // 左右组能放进同一行就合并：左组贴左、右组贴右；放不下只留右侧主操作。
    if left_w + 2 + right_w <= width {
        let left_controls: Vec<Control> = left.iter().map(|(_, c)| c.clone()).collect();
        let (left_rows, left_slots) =
            overlay::layout_controls(&left_controls, width, RowAlign::Left, 2, theme);
        let left_line = left_rows.into_iter().next().unwrap_or_default();
        hits.extend(
            left.iter()
                .zip(&left_slots)
                .map(|((action, _), slot)| (*action, slot.col, slot.width)),
        );
        let pad = width - left_w - right_w;
        let mut spans = left_line.spans;
        spans.push(Span::raw(" ".repeat(pad)));
        spans.extend(right_line.spans.into_iter().skip(1));
        (Line::from(spans), hits)
    } else {
        (right_line, hits)
    }
}

/// 批注编辑器：居中覆盖在模态卡片之上，光标落在正文里；
/// 返回 footer 键位命中区与底部按钮命中区（行内坐标转屏幕由调用方补）。
pub(super) fn draw_plan_editor(
    frame: &mut Frame,
    app: &App,
    modal_area: Rect,
    area: Rect,
) -> (Vec<KeyHit>, Vec<(PlanEditorAction, Rect)>) {
    let theme = app.theme;
    let Some(Modal::Plan { state }) = &app.modal else {
        return (Vec::new(), Vec::new());
    };
    let Some(editor) = &state.editor else {
        return (Vec::new(), Vec::new());
    };
    let width = area
        .width
        .saturating_sub(8)
        .clamp(32, 76)
        .min(modal_area.width);
    let inner_width = overlay::content_width(width).max(1);
    let wrapped = wrap_input(&editor.body, inner_width);
    let capacity = overlay::max_body_rows(area, true).max(1);
    // 行预算紧张时先让掉引用，保住标签 + 至少一行正文 + 键位提示。
    let quote_rows: Vec<String> = editor
        .quote
        .lines()
        .take(2.min(capacity.saturating_sub(3)))
        .map(|line| truncate(format!("> {line}"), inner_width).into_owned())
        .collect();
    // 输入区做成输入框体感：至少 3 行、随内容增高（受可用行数约束）、整行铺底色。
    let body_space = capacity.saturating_sub(quote_rows.len() + 2).max(1);
    let input_rows = wrapped.lines.len().max(3).min(body_space);
    let start = wrapped
        .cursor_row
        .saturating_sub(input_rows.saturating_sub(1));
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(capacity);
    for row in &quote_rows {
        lines.push(Line::from(Span::styled(
            row.clone(),
            Style::new().fg(theme.gray),
        )));
    }
    lines.push(Line::from(Span::styled(
        truncate(t!("tui.plan.annotation_body_hint"), inner_width).into_owned(),
        Style::new().fg(theme.text_secondary),
    )));
    for row in 0..input_rows {
        let text = wrapped.lines.get(start + row).map_or("", String::as_str);
        let pad = inner_width.saturating_sub(str_width(text));
        lines.push(Line::from(vec![
            Span::styled(
                text.to_owned(),
                Style::new().fg(theme.text_primary).bg(theme.bg_highlight),
            ),
            Span::styled(" ".repeat(pad), Style::new().bg(theme.bg_highlight)),
        ]));
    }
    // 键位提示压底：按钮已接管点击，这里只保留键盘说明。
    lines.push(
        overlay::footer(
            &[
                ("ctrl+g", t!("tui.hint.insert_suggest")),
                ("esc", t!("tui.hint.cancel")),
                ("ctrl+s", t!("tui.hint.save")),
            ],
            inner_width,
            theme,
        )
        .line,
    );
    lines.truncate(capacity);
    let label_rows = quote_rows.len() + 1;
    let (button_line, button_hits) = editor_buttons(editor, inner_width, theme);
    let layout = overlay::Card {
        title: t!(
            "tui.plan.annotation_title_fmt",
            sep = labels::SEP,
            start = editor.start_line,
            end = editor.end_line
        )
        .into_owned(),
        body: lines,
        footer: Some(overlay::Footer {
            line: button_line,
            keys: Vec::new(),
        }),
    }
    .render(frame, area, width, theme);
    let cursor_row = wrapped.cursor_row.saturating_sub(start);
    let y = layout.body.y + u16::try_from(label_rows + cursor_row).unwrap_or(u16::MAX);
    let x = layout.body.x + u16::try_from(wrapped.cursor_column).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        x.min(layout.body.right().saturating_sub(1)),
        y.min(layout.body.bottom().saturating_sub(1)),
    ));
    // 按钮行画在 footer 槽位：正文底 + 空行后的那一行。
    let button_y = layout.area.bottom().saturating_sub(3);
    let hits = button_hits
        .into_iter()
        .map(|(action, col, w)| {
            (
                action,
                Rect::new(
                    layout.body.x + u16::try_from(col).unwrap_or(u16::MAX),
                    button_y,
                    u16::try_from(w).unwrap_or(u16::MAX),
                    1,
                ),
            )
        })
        .collect();
    (layout.keys, hits)
}

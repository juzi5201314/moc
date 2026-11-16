use super::PLAN_ACTIONS_LEN;
use super::PLAN_HEADER_ROWS;
use super::WrappedInput;
use crate::app::App;
use crate::modal::Modal;
use crate::modal::PlanPane;
use crate::modal::PlanReviewState;
use crate::render::labels;
use crate::render::labels::KeyHit;
use crate::render::overlay;
use crate::render::wrap::clip_line;
use crate::render::wrap::fit;
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
use rust_i18n::t;

/// 待审阅计划的动作列表；「在新会话执行」带上所选目标引擎。
pub(super) fn plan_action_label(state: &PlanReviewState, index: usize) -> String {
    match index {
        0 => t!("tui.plan.run_here").into_owned(),
        1 => t!("tui.plan.run_compact").into_owned(),
        2 => t!(
            "tui.plan.run_new_fmt",
            engine = crate::app::engine_label(state.execution_engine)
        )
        .into_owned(),
        _ => t!("tui.plan.resume").into_owned(),
    }
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

/// 正文之外的固定行数；渲染与鼠标命中都走它，两边算出的正文窗口才会一致。
///
/// 头部 [`PLAN_HEADER_ROWS`] 行；正文之后的尾部（动作列表 / 状态说明 / 错误）
/// 非空时先空一行再排。操作提示在卡片底部的提示条里，不占正文行。
pub(crate) fn plan_fixed_rows(state: &PlanReviewState) -> usize {
    let mut tail = 0;
    if state.pane == PlanPane::Read && state.status == PlanStatus::Submitted {
        tail += PLAN_ACTIONS_LEN;
    }
    if state.status != PlanStatus::Submitted {
        tail += 1;
    }
    if state.error.is_some() || state.submitting {
        tail += 1;
    }
    PLAN_HEADER_ROWS + if tail > 0 { tail + 1 } else { 0 }
}

/// 正文可见行数。
pub(crate) fn plan_body_rows(state: &PlanReviewState, max_lines: usize) -> usize {
    max_lines.saturating_sub(plan_fixed_rows(state)).max(1)
}

/// 批注视图当前可见的源码行（0-based，与屏幕行一一对应）。
pub(crate) fn plan_source_window(state: &PlanReviewState, max_lines: usize) -> Vec<usize> {
    let rows = plan_body_rows(state, max_lines);
    let total = state.source_lines().len();
    let offset = state.scroll_offset.min(total.saturating_sub(rows));
    (offset..(offset + rows).min(total)).collect()
}

/// 计划审阅卡片的正文行与提示条。
pub(super) fn plan_lines(
    state: &PlanReviewState,
    width: usize,
    max_lines: usize,
    theme: &Theme,
    hits: &mut Vec<(usize, usize)>,
    scroll_max: &mut usize,
) -> (Vec<Line<'static>>, overlay::Footer) {
    let mut lines = Vec::with_capacity(max_lines);

    // 状态先占位、标题吃剩下的宽度：标题再长也不会把这一行挤成两行。
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
    let mut location = state.path.clone();
    if state.previous_plan_id.is_some() {
        location.push_str(&t!(
            "tui.plan.diff_ver_fmt",
            ver = state.version.saturating_sub(1)
        ));
    }
    if !state.drafts.is_empty() || !state.submitted.is_empty() {
        location.push_str(&t!(
            "tui.plan.annot_count_fmt",
            drafts = state.drafts.len(),
            submitted = state.submitted.len()
        ));
    }
    lines.push(Line::from(Span::styled(
        truncate(&location, width).into_owned(),
        Style::new().fg(theme.gray),
    )));

    let body_rows = plan_body_rows(state, max_lines);
    match state.pane {
        PlanPane::Read => {
            if state.show_diff && state.previous_plan_id.is_some() {
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
                    let rendered = crate::render::diff::render_diff(diff, width, theme, usize::MAX);
                    if rendered.is_empty() {
                        lines.push(Line::from(Span::styled(
                            t!("tui.plan.no_text_changes"),
                            Style::new().fg(theme.gray),
                        )));
                    } else {
                        // 事件层只加不减：渲染时按正文行数收紧偏移并写回。
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
            } else {
                let rendered =
                    crate::render::markdown::render_markdown(&state.markdown, width, theme);
                if rendered.is_empty() {
                    lines.push(Line::from(String::new()));
                } else {
                    // 事件层只加不减：渲染时按正文行数收紧偏移并写回。
                    *scroll_max = rendered.len().saturating_sub(body_rows);
                    lines.extend(
                        rendered
                            .into_iter()
                            .skip(state.scroll_offset.min(*scroll_max))
                            .take(body_rows)
                            .map(|line| clip_line(line, width)),
                    );
                }
            }
        }
        PlanPane::Annotate => {
            let source = state.source_lines();
            for index in plan_source_window(state, max_lines) {
                lines.push(plan_source_line(state, &source, index, width, theme));
            }
        }
    }

    // 提示条从左丢项：引擎/模型/批注等次要键先让位，滚动与确认/关闭恒在。
    let mut items: Vec<(&str, std::borrow::Cow<'_, str>)> = Vec::with_capacity(8);
    match state.pane {
        PlanPane::Read => {
            let submitted = state.status == PlanStatus::Submitted;
            if submitted {
                items.push(("m", t!("tui.hint.switch_engine")));
            }
            items.push(("a", t!("tui.hint.annotate")));
            if state.previous_plan_id.is_some() {
                items.push((
                    "d",
                    if state.show_diff {
                        t!("tui.hint.view_plan")
                    } else {
                        t!("tui.hint.view_diff")
                    },
                ));
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
        PlanPane::Annotate => items.extend([
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
    if plan_fixed_rows(state) > PLAN_HEADER_ROWS {
        lines.push(Line::from(String::new()));
    }
    if state.pane == PlanPane::Read && state.status == PlanStatus::Submitted {
        for index in 0..PLAN_ACTIONS_LEN {
            let label = plan_action_label(state, index);
            hits.push((index, lines.len()));
            lines.push(if state.submitting {
                Line::from(Span::styled(
                    fit(&format!("  {label}"), width),
                    Style::new().fg(theme.gray_dim),
                ))
            } else {
                overlay::list_row(&label, state.selected == index, width, theme)
            });
        }
    } else if state.status != PlanStatus::Submitted {
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
        plan_fixed_rows(state) - PLAN_HEADER_ROWS,
        "尾部行数必须与 plan_fixed_rows 一致，否则正文窗口与鼠标映射错位"
    );
    (lines, footer)
}

/// 批注视图的一行源码：行号 gutter + 内容，选区铺底、光标加粗、有批注的行标 `●`。
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

/// 批注编辑器：居中覆盖在模态卡片之上，光标落在正文里；返回 footer 键位的命中区。
pub(super) fn draw_plan_editor(
    frame: &mut Frame,
    app: &App,
    modal_area: Rect,
    area: Rect,
) -> Vec<KeyHit> {
    let theme = app.theme;
    let Some(Modal::Plan { state }) = &app.modal else {
        return Vec::new();
    };
    let Some(editor) = &state.editor else {
        return Vec::new();
    };
    let width = area
        .width
        .saturating_sub(8)
        .clamp(32, 76)
        .min(modal_area.width);
    let inner_width = overlay::content_width(width).max(1);
    let wrapped = wrap_input(&editor.body, inner_width);
    let capacity = overlay::max_body_rows(area, true).max(1);
    // 行预算紧张时先让掉引用，保住标签 + 至少一行正文。
    let quote_rows: Vec<String> = editor
        .quote
        .lines()
        .take(2.min(capacity.saturating_sub(2)))
        .map(|line| truncate(format!("> {line}"), inner_width).into_owned())
        .collect();
    let body_space = capacity.saturating_sub(quote_rows.len() + 1).max(1);
    let start = wrapped
        .cursor_row
        .saturating_sub(body_space.saturating_sub(1));
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
    for row in wrapped.lines.iter().skip(start).take(body_space) {
        lines.push(Line::from(Span::styled(
            row.clone(),
            Style::new().fg(theme.text_primary),
        )));
    }
    lines.truncate(capacity);
    let label_rows = quote_rows.len() + 1;
    let layout = overlay::Card {
        title: t!(
            "tui.plan.annotation_title_fmt",
            sep = labels::SEP,
            start = editor.start_line,
            end = editor.end_line
        )
        .into_owned(),
        body: lines,
        footer: Some(overlay::footer(
            &[
                ("ctrl+g", t!("tui.hint.insert_suggest")),
                ("esc", t!("tui.hint.cancel")),
                ("ctrl+s", t!("tui.hint.save")),
            ],
            inner_width,
            theme,
        )),
    }
    .render(frame, area, width, theme);
    let cursor_row = wrapped.cursor_row.saturating_sub(start);
    let y = layout.body.y + u16::try_from(label_rows + cursor_row).unwrap_or(u16::MAX);
    let x = layout.body.x + u16::try_from(wrapped.cursor_column).unwrap_or(u16::MAX);
    frame.set_cursor_position((
        x.min(layout.body.right().saturating_sub(1)),
        y.min(layout.body.bottom().saturating_sub(1)),
    ));
    layout.keys
}

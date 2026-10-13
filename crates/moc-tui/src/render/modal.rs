//! 浮层渲染：斜杠补全菜单与模态卡片。
//!
//! 浮层画在滚动区之后（覆盖其内容）；模态居中，宽度按内容取值（帮助为双列快捷键表，
//! 普通模态上限 96 列，带选项预览的提问放宽到 120 列），高度按内容收缩。全部使用主题槽位。

use moc_protocol::Role;
use moc_protocol::methods::ContextUsageResult;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};

use crate::app::App;
use crate::modal::Modal;
use crate::render::wrap::str_width;
use crate::theme::Theme;
use crate::widgets::completion::CompletionState;
use crate::widgets::prompt::clamp_cursor;

/// 斜杠补全菜单：贴着输入框上沿，选中行反色。
pub fn draw_completion_menu(
    frame: &mut Frame,
    completion: &mut CompletionState,
    prompt_area: Rect,
) {
    let total = completion.candidates.len();
    let max_visible = 20usize.min(usize::from(prompt_area.y.saturating_sub(3)));
    let visible = total.min(max_visible);
    let rows = u16::try_from(visible).unwrap_or(u16::MAX) + u16::from(total > visible);
    let width = prompt_area.width.clamp(20, 72);
    let height = (rows + 3).min(prompt_area.y);
    if height == 0 {
        return;
    }
    let area = Rect {
        x: prompt_area.x,
        y: prompt_area.y - height,
        width,
        height,
    };
    completion.area = area;
    completion.visible_count = visible;
    completion.adjust_scroll();

    let theme = crate::theme::Theme::global();
    frame.render_widget(Clear, area);
    let content_width = usize::from(width.saturating_sub(4));
    let mut lines = Vec::with_capacity(visible + 2);
    for (rel_index, candidate) in completion
        .candidates
        .iter()
        .skip(completion.offset)
        .take(visible)
        .enumerate()
    {
        let index = completion.offset + rel_index;
        let selected = index == completion.selected;
        let prefix = if selected { "❯ " } else { "  " };
        let style = if selected {
            Style::new()
                .fg(theme.text_primary)
                .bg(theme.bg_visual)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(theme.text_primary)
        };
        lines.push(Line::from(Span::styled(
            ellipsize(
                &format!("{prefix}{:<14}{}", candidate.label, candidate.description),
                content_width,
            ),
            style,
        )));
    }
    if total > visible {
        lines.push(Line::from(Span::styled(
            format!("… 还有 {} 条", total - visible),
            Style::new().fg(theme.gray_dim),
        )));
    }
    lines.push(Line::from(Span::styled(
        "↑↓ 选择 · Tab/Enter 确认 · Esc 关闭",
        Style::new().fg(theme.gray_dim),
    )));
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active)),
        ),
        area,
    );
}

/// 就地模式菜单：右缘对齐图例里的模式名，整体贴在它上方（右键菜单那种小浮层）。
pub fn draw_mode_menu(frame: &mut Frame, app: &mut App) {
    let Some(menu) = app.mode_menu.as_ref() else {
        return;
    };
    let theme = app.theme;
    let current = app.display_mode();
    let selected = menu.selected;
    let label_width = crate::app::MODES
        .iter()
        .map(|mode| str_width(crate::app::mode_label(*mode)))
        .max()
        .unwrap_or(0);
    let frame_area = frame.area();
    let width = u16::try_from(2 + label_width + 2 + 2)
        .unwrap_or(u16::MAX)
        .min(frame_area.width);
    let height = u16::try_from(crate::app::MODES.len() + 2).unwrap_or(u16::MAX);
    let anchor = app.mode_area;
    let x = (anchor.x.saturating_add(anchor.width))
        .saturating_sub(width)
        .min(frame_area.width.saturating_sub(width))
        .max(frame_area.x);
    let area = Rect {
        x,
        y: anchor.y.saturating_sub(height).max(frame_area.y),
        width,
        height,
    };
    let lines: Vec<Line<'static>> = crate::app::MODES
        .iter()
        .enumerate()
        .map(|(index, mode)| {
            let highlighted = index == selected;
            let base = if highlighted {
                Style::new()
                    .fg(theme.text_primary)
                    .bg(theme.bg_visual)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::new().fg(theme.text_primary)
            };
            // 当前模式沿用图例里的强调色，与菜单高亮（反色）互相独立。
            let label_style = if Some(*mode) == current {
                base.fg(theme.accent_model)
            } else {
                base
            };
            Line::from(vec![
                Span::styled(if highlighted { "❯ " } else { "  " }, base),
                Span::styled(
                    format!(
                        "{:<label_width$}",
                        crate::app::mode_label(*mode),
                        label_width = label_width
                    ),
                    label_style,
                ),
            ])
        })
        .collect();
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(lines).block(
            Block::bordered()
                .border_set(ratatui::symbols::border::ROUNDED)
                .border_style(Style::new().fg(theme.border_active)),
        ),
        area,
    );
    if let Some(menu) = app.mode_menu.as_mut() {
        menu.area = area;
    }
}

/// 居中模态卡片。
pub fn draw_modal(frame: &mut Frame, app: &mut App) {
    app.plan_model_area = Rect::default();
    let Some(modal) = &app.modal else {
        return;
    };
    let area = frame.area();
    if area.width < 20 || area.height < 6 {
        return;
    }
    let theme = app.theme;
    // 保留正文轮廓但降低对比度，浮层成为唯一的视觉焦点。
    frame
        .buffer_mut()
        .set_style(area, Style::new().fg(theme.gray_dim).bg(theme.bg_dark));
    let max_content = usize::from(area.height.saturating_sub(6)).max(1);
    let (width, content_width) = if matches!(modal, Modal::Help) {
        // 帮助是双列快捷键表：按内容自然宽度取值，窄终端由 help_lines 退回单列。
        let (_, natural) = help_lines(app, usize::from(area.width).saturating_sub(6), max_content);
        let width = u16::try_from(natural + 2)
            .unwrap_or(u16::MAX)
            .min(area.width.saturating_sub(4));
        (width, usize::from(width.saturating_sub(2)))
    } else if matches!(modal, Modal::Context { .. }) {
        // 上下文占用卡片：收敛到紧凑宽度，避免宽屏下右侧过度留白且色条过长。
        let width = area
            .width
            .saturating_sub(4)
            .clamp(16, 58)
            .min(area.width.saturating_sub(2));
        (width, usize::from(width.saturating_sub(4)).max(8))
    } else if modal.has_option_preview() {
        // 提问预览：并排布局需要更宽的卡片。
        let width = if area.width < 70 {
            area.width.saturating_sub(4).max(16)
        } else {
            area.width.min(ASK_PREVIEW_MODAL_WIDTH)
        };
        (width, usize::from(width.saturating_sub(4)).max(8))
    } else {
        let width = if area.width < 70 {
            area.width.saturating_sub(4).max(16)
        } else {
            area.width.min(96)
        };
        (width, usize::from(width.saturating_sub(4)).max(8))
    };
    let lines = modal_lines(app, modal, content_width, max_content);
    let content_height = u16::try_from(lines.len().max(1)).unwrap_or(u16::MAX);
    // 2 行给上下边框，1 行留作底部内边距：内容顶格，下沿不贴边框也不空一大片。
    let height = content_height
        .saturating_add(3)
        .min(area.height.saturating_sub(2));
    let modal_area = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    frame.render_widget(Clear, modal_area);
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::bordered()
                    .border_set(ratatui::symbols::border::ROUNDED)
                    .border_style(Style::new().fg(theme.border_active))
                    .title(Span::styled(
                        modal.title(),
                        Style::new()
                            .fg(theme.text_primary)
                            .add_modifier(Modifier::BOLD),
                    )),
            )
            .wrap(Wrap { trim: false }),
        modal_area,
    );
    if let Modal::Plan { state } = modal {
        let prefix_width =
            u16::try_from(str_width("执行模型：")).expect("固定标签宽度可表示为 u16");
        let available = u16::try_from(content_width)
            .expect("正文宽度受终端 u16 宽度约束")
            .saturating_sub(prefix_width);
        let model_width = u16::try_from(str_width(&state.execution_model))
            .unwrap_or(u16::MAX)
            .min(available);
        app.plan_model_area = Rect::new(
            modal_area.x + 1 + prefix_width,
            modal_area.y + 1,
            model_width,
            1,
        );
    }
}

/// 快捷键帮助：键位列与描述列之间的间距（格）。
const HELP_KEY_GAP: usize = 2;
/// 快捷键帮助：两列之间的间距（格）。
const HELP_COLUMN_GAP: usize = 4;
/// 快捷键帮助：快捷键行左缩进（格）。
const HELP_INDENT: usize = 2;
/// 快捷键帮助：页脚（留白 + Esc 说明）占用的行数。
const HELP_FOOTER_ROWS: usize = 2;

/// 一列快捷键的（键位列宽，整列宽）。
type HelpColumn = (usize, usize);

/// 量出一列快捷键的键位列宽与整列宽。
fn measure_column(rows: &[(String, &'static str)]) -> HelpColumn {
    let key_width = rows
        .iter()
        .map(|(key, _)| str_width(key))
        .max()
        .unwrap_or(0);
    let description_width = rows
        .iter()
        .map(|(_, description)| str_width(description))
        .max()
        .unwrap_or(0);
    (key_width, key_width + HELP_KEY_GAP + description_width)
}

/// 快捷键帮助内容：默认双列（左列先排满），宽度不足时退回单列。
///
/// 键位列按该列最长绑定取齐——键位可配置，固定宽度会让 `shift/alt + enter` 这类
/// 长组合挤出描述列；返回内容的自然宽度，模态据此收窄到刚好容纳。
/// 页脚只剩一行 Esc 说明，缩进与快捷键行一致，不再贴住卡片边框。
fn help_lines(app: &App, max_width: usize, max_lines: usize) -> (Vec<Line<'static>>, usize) {
    let theme = app.theme;
    let mut rows: Vec<(String, &'static str)> = crate::config::actions()
        .iter()
        .map(|(action, description, _)| {
            let keys = app
                .preferences
                .keybindings
                .get(*action)
                .map_or_else(|| "—".to_owned(), |keys| crate::config::display_keys(keys));
            (keys, *description)
        })
        .collect();
    rows.insert(0, ("/confirm-plan".into(), "重新审阅待确认的计划"));
    // 双列按行序对半切开，两列各自收窄；退回单列时两列并作一列取并集。
    let unified = measure_column(&rows);
    let split = rows.len().div_ceil(2);
    let (left, right) = rows.split_at(split);
    let left_column = measure_column(left);
    let right_column = measure_column(right);
    let two_columns =
        HELP_INDENT + left_column.1 + HELP_COLUMN_GAP + right_column.1 <= max_width && split > 0;
    let (columns, width) = if two_columns {
        (
            [left_column, right_column],
            HELP_INDENT + left_column.1 + HELP_COLUMN_GAP + right_column.1,
        )
    } else {
        ([unified; 2], HELP_INDENT + unified.1)
    };
    let row_budget = max_lines
        .saturating_sub(HELP_FOOTER_ROWS)
        .max(1)
        .saturating_mul(if two_columns { 2 } else { 1 });
    rows.truncate(row_budget);
    let split = if two_columns {
        rows.len().div_ceil(2)
    } else {
        rows.len()
    };
    let mut lines: Vec<Line<'static>> = (0..split)
        .map(|index| {
            let mut spans = vec![Span::styled(" ".repeat(HELP_INDENT), Style::new())];
            spans.extend(help_cell(&rows[index], columns[0], theme));
            if let Some(right) = rows.get(split + index) {
                spans.push(Span::styled(" ".repeat(HELP_COLUMN_GAP), Style::new()));
                spans.extend(help_cell(right, columns[1], theme));
            }
            Line::from(spans)
        })
        .collect();
    lines.push(Line::from(String::new()));
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(HELP_INDENT), Style::new()),
        Span::styled("Esc 关闭帮助", Style::new().fg(theme.gray_dim)),
    ]));
    (lines, width)
}

/// 帮助表的一格：键位右补齐到列内 `key_width`，整格再补齐到列宽。
fn help_cell(
    row: &(String, &'static str),
    column: HelpColumn,
    theme: &crate::theme::Theme,
) -> [Span<'static>; 4] {
    let (key_width, column_width) = column;
    let (key, description) = row;
    let key_cells = str_width(key);
    let description_cells = str_width(description);
    // 键位先补齐到 key_width，整格再补齐到列宽，同一列的描述才能上下对齐。
    let pad = column_width.saturating_sub(key_width + HELP_KEY_GAP + description_cells);
    [
        Span::styled(key.clone(), Style::new().fg(theme.accent_user)),
        Span::styled(
            " ".repeat(key_width.saturating_sub(key_cells) + HELP_KEY_GAP),
            Style::new(),
        ),
        Span::styled(
            (*description).to_owned(),
            Style::new().fg(theme.text_secondary),
        ),
        Span::styled(" ".repeat(pad), Style::new()),
    ]
}
fn rewind_kind(role: Role) -> &'static str {
    match role {
        Role::User => "用户请求",
        Role::Assistant => "模型回复",
        Role::Tool => "工具结果",
        Role::System | Role::Developer => "系统消息",
    }
}

fn rewind_preview(entry: &moc_protocol::methods::TranscriptEntry) -> String {
    let text = entry.message.text();
    if !text.trim().is_empty() {
        return text.replace('\n', " ");
    }
    let calls = entry
        .message
        .tool_calls()
        .map(|call| call.name.as_str())
        .collect::<Vec<_>>();
    if !calls.is_empty() {
        return format!("调用 {}", calls.join("、"));
    }
    entry
        .message
        .tool_results()
        .map(|result| result.content.replace('\n', " "))
        .collect::<Vec<_>>()
        .join(" ")
}

/// 上下文窗：分块色标与分块名的左缩进（格）。
const CONTEXT_INDENT: usize = 2;
/// 上下文窗：分块名列宽（格）。
const CONTEXT_NAME_COL: usize = 16;
/// 上下文窗：token 列宽（格，右对齐）。
const CONTEXT_VALUE_COL: usize = 7;
/// 上下文窗：占比列宽（格，右对齐）。
const CONTEXT_PERCENT_COL: usize = 6;
/// 上下文窗：token 列与占比列之间的间距（格）。
const CONTEXT_GAP: usize = 2;

/// 占比：保留一位小数；非零但不足 0.1% 显示 `<0.1%`，不缩成 0.0%。
fn share_percent(part: u64, total: u64) -> String {
    let percent = part as f64 * 100.0 / total.max(1) as f64;
    if percent > 0.0 && percent < 0.05 {
        "<0.1%".to_owned()
    } else {
        format!("{percent:.1}%")
    }
}

/// 上下文占用明细：分块列表 + 按窗口刻度的分块色条与边界标注。
///
/// 列表每块以 `▌` 色标起头，颜色与色条上对应那段一致；色条按上下文窗口定标，
/// 因此空会话就是一整条暗色轨道，已用部分永远落在左侧。边界标注落在各分块
/// 右界，只在不重叠处显示，并优先保住「已用总量」与窗口上限两条。
fn context_lines(usage: &ContextUsageResult, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let total = usage.total_tokens;
    let window = usage.context_window;
    let mut lines = Vec::with_capacity(usage.sections.len() + 6);
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
        Span::styled(
            crate::render::tokens_k(total),
            Style::new()
                .fg(theme.text_primary)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" / {}", crate::render::tokens_k(window)),
            Style::new().fg(theme.gray),
        ),
        Span::styled(
            format!(" · {}", share_percent(total, window)),
            Style::new().fg(theme.gray),
        ),
    ]));
    lines.push(Line::from(String::new()));
    for (index, section) in usage.sections.iter().enumerate() {
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
            Span::styled("▌ ", Style::new().fg(theme.chart(index))),
            Span::styled(
                format_section_name(&section.name, CONTEXT_NAME_COL),
                Style::new().fg(theme.text_secondary),
            ),
            Span::styled(
                format!(
                    "{:>CONTEXT_VALUE_COL$}",
                    crate::render::tokens_k(section.tokens)
                ),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled(" ".repeat(CONTEXT_GAP), Style::new()),
            Span::styled(
                format!(
                    "{:>CONTEXT_PERCENT_COL$}",
                    share_percent(section.tokens, total)
                ),
                Style::new().fg(theme.gray),
            ),
        ]));
    }
    lines.push(Line::from(String::new()));
    // 分块右界：累积 token 按 `max(已用, 窗口)` 折算成格位；累积单调所以格位不回头
    let cells = width.saturating_sub(CONTEXT_INDENT).max(1);
    let scale = window.max(total).max(1);
    let cells_u64 = u64::try_from(cells).unwrap_or(u64::MAX);
    let mut marks = Vec::with_capacity(usage.sections.len());
    let mut cumulative = 0u64;
    for section in &usage.sections {
        cumulative = cumulative.saturating_add(section.tokens);
        let cell = cumulative
            .saturating_mul(cells_u64)
            .saturating_add(scale / 2)
            / scale;
        marks.push((
            cumulative,
            usize::try_from(cell).unwrap_or(cells).min(cells),
        ));
    }
    lines.push(context_bar_line(&marks, cells, theme));
    lines.push(context_scale_line(&marks, total, window, cells, theme));
    lines
}

/// 分块色条：各分块按格位铺 `━`（颜色与列表色标一致），窗口剩余部分铺暗色 `─`。
fn context_bar_line(marks: &[(u64, usize)], cells: usize, theme: &Theme) -> Line<'static> {
    let mut spans = Vec::with_capacity(marks.len() + 2);
    spans.push(Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()));
    let mut drawn = 0usize;
    for (index, &(_, boundary)) in marks.iter().enumerate() {
        if boundary > drawn {
            spans.push(Span::styled(
                "━".repeat(boundary - drawn),
                Style::new().fg(theme.chart(index)),
            ));
            drawn = boundary;
        }
    }
    if drawn < cells {
        spans.push(Span::styled(
            "─".repeat(cells - drawn),
            Style::new().fg(theme.border),
        ));
    }
    Line::from(spans)
}

/// 分块边界标注：写出各分块右界的累积用量，窗口上限贴住色条末端。
fn context_scale_line(
    marks: &[(u64, usize)],
    total: u64,
    window: u64,
    cells: usize,
    theme: &Theme,
) -> Line<'static> {
    let mut taken = vec![false; cells];
    let mut placed: Vec<(usize, String, Color)> = Vec::with_capacity(marks.len() + 1);
    if total > 0
        && let Some(&(_, boundary)) = marks.last()
    {
        push_label(
            &mut taken,
            &mut placed,
            boundary,
            crate::render::tokens_k(total),
            theme.chart(marks.len() - 1),
        );
    }
    let cap = crate::render::tokens_k(window);
    push_label(&mut taken, &mut placed, cells, cap, theme.gray_dim);
    for (index, &(cumulative, boundary)) in marks.iter().enumerate() {
        let text = crate::render::tokens_k(cumulative);
        if placed.iter().any(|(_, existing, _)| *existing == text) {
            continue;
        }
        push_label(&mut taken, &mut placed, boundary, text, theme.chart(index));
    }
    placed.sort_unstable_by_key(|(start, ..)| *start);
    let mut spans = Vec::with_capacity(placed.len() * 2 + 2);
    spans.push(Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()));
    let mut cursor = 0usize;
    for (start, text, color) in placed {
        let width = str_width(&text);
        if start > cursor {
            spans.push(Span::styled(" ".repeat(start - cursor), Style::new()));
        }
        cursor = start + width;
        spans.push(Span::styled(text, Style::new().fg(color)));
    }
    if cursor < cells {
        spans.push(Span::styled(" ".repeat(cells - cursor), Style::new()));
    }
    Line::from(spans)
}

/// 分块显示名格式化：按终端显示宽度对齐；超过列宽时截断加省略号，不足时补空格。
fn format_section_name(name: &str, target_width: usize) -> String {
    let current_width = str_width(name);
    if current_width <= target_width {
        format!("{name}{}", " ".repeat(target_width - current_width))
    } else {
        ellipsize(name, target_width)
    }
}

/// 落一个边界标注：以分块右界为锚点右对齐（使数值落位在该分块色条区间的末端）；
/// 若数值为 0k 则以起点左对齐。空间不足或与已落位的标注相碰时直接放弃。
fn push_label(
    taken: &mut [bool],
    placed: &mut Vec<(usize, String, Color)>,
    target_boundary: usize,
    text: String,
    color: Color,
) {
    let width = str_width(&text);
    let cells = taken.len();
    if width == 0 || width > cells {
        return;
    }
    let position = if target_boundary == 0 {
        0
    } else {
        target_boundary.min(cells).saturating_sub(width)
    };
    let end = (position + width).min(cells);
    let position = end.saturating_sub(width);
    let clear = !taken[position..end].iter().any(|cell| *cell)
        && (position == 0 || !taken[position - 1])
        && (end == cells || !taken[end]);
    if clear {
        taken[position..end].fill(true);
        placed.push((position, text, color));
    }
}

/// 提问预览并排布局的最小内容宽度（列）；更窄的终端把预览落到选项下方整宽渲染。
const ASK_PREVIEW_MIN_WIDTH: usize = 72;
/// 并排布局左栏（选项列表）的最小宽度（列）。
const ASK_PREVIEW_LEFT_MIN: usize = 16;
/// 并排布局两栏之间的分隔；其显示宽度即两栏的间距。
const ASK_PREVIEW_SEPARATOR: &str = " │ ";
/// 带预览的提问卡片的最大宽度（列）：比普通模态更宽，容得下并排的预览栏。
const ASK_PREVIEW_MODAL_WIDTH: u16 = 120;

/// 模态内容行（按类型分派）。
fn modal_lines(app: &App, modal: &Modal, width: usize, max_lines: usize) -> Vec<Line<'static>> {
    let theme = app.theme;
    let mut lines = match modal {
        Modal::Permission {
            params,
            selected,
            scroll_offset,
            ..
        } => {
            let mut all_content = Vec::new();
            if let Some(body) = &params.body {
                all_content.extend(body.lines().map(|row| {
                    Line::from(Span::styled(
                        format!("  {row}"),
                        Style::new().fg(theme.text_secondary),
                    ))
                }));
            }
            if let Some(diff) = &params.diff {
                all_content.extend(crate::render::diff::render_diff(diff, width, theme));
            }
            let options_rows = params.options.len() + 2;
            let max_content_rows = max_lines.saturating_sub(options_rows).max(2);
            let offset = (*scroll_offset).min(all_content.len().saturating_sub(max_content_rows));
            let mut lines: Vec<Line<'static>> = all_content
                .into_iter()
                .skip(offset)
                .take(max_content_rows)
                .collect();
            if lines.is_empty() {
                lines.push(Line::from(String::new()));
            }
            lines.push(Line::from(String::new()));
            for (index, option) in params.options.iter().enumerate() {
                let marker = if index == *selected { "❯ " } else { "  " };
                let style = if index == *selected {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    format!("{marker}{} {}", index + 1, option.label),
                    style,
                )));
            }
            lines
        }
        Modal::Ask {
            params,
            question,
            highlight,
            selected,
            custom,
            preview_scroll,
            ..
        } => {
            let mut lines = Vec::new();
            if params.questions.len() > 1 {
                lines.push(Line::from(Span::styled(
                    format!("问题 {}/{}", question + 1, params.questions.len()),
                    Style::new().fg(theme.gray),
                )));
            }
            let Some(current) = params.questions.get(*question) else {
                return lines;
            };
            lines.push(Line::from(String::new()));

            // 选项 + 自定义输入行；带预览时整体作为并排布局的左栏。
            let mut body: Vec<Line<'static>> = Vec::with_capacity(current.options.len() + 1);
            for (index, option) in current.options.iter().enumerate() {
                let chosen = selected[*question].contains(&option.label);
                let marker = if index == highlight[*question] {
                    "❯"
                } else {
                    " "
                };
                let check = if chosen { "☑" } else { "☐" };
                let style = if index == highlight[*question] {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                body.push(Line::from(Span::styled(
                    format!(" {marker} {check} {}", option.label),
                    style,
                )));
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
            body.push(Line::from(Span::styled(
                format!(
                    " {} ✎ 自定义：{custom_display}",
                    if editing_here { "❯" } else { " " }
                ),
                custom_style,
            )));

            // 预览只跟随聚焦项：自定义行/无预览项不进入预览布局。
            let preview = current
                .options
                .get(highlight[*question])
                .and_then(|option| option.preview.as_deref())
                .filter(|text| !text.trim().is_empty());
            // 顶部固定行 + 底部提示行之外的余量都留给预览。
            let rows = max_lines.saturating_sub(lines.len() + 1).max(1);
            match preview {
                Some(source) if width >= ASK_PREVIEW_MIN_WIDTH => {
                    // 并排布局：左栏选项列表、右栏聚焦项预览（同 Claude Code 的提问卡片）。
                    let left_width = body
                        .iter()
                        .map(line_width)
                        .max()
                        .unwrap_or(0)
                        .clamp(ASK_PREVIEW_LEFT_MIN, width / 2);
                    let preview_width =
                        width.saturating_sub(left_width + str_width(ASK_PREVIEW_SEPARATOR));
                    let pane =
                        ask_preview_lines(source, preview_width, rows, *preview_scroll, theme);
                    lines.extend(join_columns(body, pane, left_width, theme));
                }
                Some(source) => {
                    // 窄终端：预览整宽落到选项下方，同样围绕聚焦项。
                    let preview_rows = rows.saturating_sub(body.len() + 2).max(1);
                    lines.extend(body);
                    lines.push(Line::from(String::new()));
                    lines.push(Line::from(Span::styled(
                        "预览",
                        Style::new().fg(theme.gray_dim),
                    )));
                    lines.extend(ask_preview_lines(
                        source,
                        width,
                        preview_rows,
                        *preview_scroll,
                        theme,
                    ));
                }
                None => lines.extend(body),
            }
            lines.push(Line::from(Span::styled(
                format!(
                    "space 切换 · enter 选择{} · tab 下一问/Shift+Tab 上一问 · esc 取消",
                    if current.multi { "（多选）" } else { "" }
                ),
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Model {
            models,
            current,
            picker,
            ..
        } => {
            let filtered = crate::modal::filtered_models(models, &picker.query);
            let visible = max_lines.saturating_sub(5).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            let mut lines = vec![
                Line::from(Span::styled(
                    format!(
                        "当前：{}",
                        ellipsize(crate::app::model_selector_label(current), width)
                    ),
                    Style::new().fg(theme.gray),
                )),
                Line::from(Span::styled(
                    format!(
                        "搜索：{}",
                        if picker.query.is_empty() {
                            "_"
                        } else {
                            &picker.query
                        }
                    ),
                    Style::new().fg(theme.accent_model),
                )),
                Line::from(String::new()),
            ];
            if picker.loading {
                lines.push(Line::from(Span::styled(
                    "正在切换模型…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 重试 · Esc 关闭",
                    Style::new().fg(theme.gray_dim),
                )));
            }
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "没有匹配的模型",
                    Style::new().fg(theme.gray),
                )));
            } else {
                for (index, (_, model)) in filtered.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let selected = actual == picker.selected;
                    let style = if selected {
                        Style::new()
                            .fg(theme.accent_user)
                            .bg(theme.bg_visual)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::new().fg(theme.text_primary)
                    };
                    let context = model
                        .context_window
                        .map(|window| format!(" · Ctx {}", crate::render::tokens_k(window)));
                    let row_text = ellipsize(
                        &format!(
                            "{marker}{}{}",
                            model.id,
                            context.as_deref().unwrap_or_default()
                        ),
                        width,
                    );
                    let row = row_text.strip_prefix(marker).unwrap_or(&row_text);
                    let (model_text, context_text) = context
                        .as_deref()
                        .filter(|context| row.ends_with(context))
                        .map_or((row, None), |context| {
                            (row.strip_suffix(context).unwrap_or(row), Some(context))
                        });
                    let mut spans = vec![Span::styled(marker, style)];
                    if let Some((provider, model_name)) = model_text.split_once('/') {
                        let provider_style = if selected {
                            Style::new().fg(theme.text_secondary).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        };
                        spans.push(Span::styled(format!("{provider}/"), provider_style));
                        spans.push(Span::styled(model_name.to_owned(), style));
                    } else {
                        spans.push(Span::styled(model_text.to_owned(), style));
                    }
                    if let Some(context) = context_text {
                        let context_style = if selected {
                            Style::new().fg(theme.accent_model).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.accent_model)
                        };
                        spans.push(Span::styled(context.to_owned(), context_style));
                    }
                    lines.push(Line::from(spans));
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ 移动 · PgUp/PgDn 翻页 · 输入搜索 · Enter 确认 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::SwitchModel {
            selector,
            overflow,
            option,
            loading,
            error,
            ..
        } => {
            let window = crate::render::tokens_k(overflow.context_window);
            let leading = if overflow.tokens > overflow.context_window {
                format!(
                    "当前上下文 {} 超过 {selector} 的窗口 {window}",
                    crate::render::tokens_k(overflow.tokens)
                )
            } else {
                format!(
                    "当前上下文 {} 超过 {selector} 的压缩触发线 {}（窗口 {window}）",
                    crate::render::tokens_k(overflow.tokens),
                    crate::render::tokens_k(overflow.threshold)
                )
            };
            let mut lines = vec![
                Line::from(Span::styled(leading, Style::new().fg(theme.text_primary))),
                Line::from(Span::styled(
                    format!(
                        "确认后先切换到 {selector}，再由它压缩上下文（压缩后不超过 {}）",
                        crate::render::tokens_k(overflow.target_tokens)
                    ),
                    Style::new().fg(theme.text_secondary),
                )),
                Line::from(String::new()),
            ];
            if *loading {
                lines.push(Line::from(Span::styled(
                    "正在切换并压缩…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
            }
            for (index, text) in ["切换并压缩", "取消"].iter().enumerate() {
                let marker = if index == *option { "❯ " } else { "  " };
                let style = if index == *option {
                    Style::new()
                        .fg(theme.accent_user)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(format!("{marker}{text}"), style)));
            }
            lines.push(Line::from(Span::styled(
                if error.is_some() {
                    "enter 重试 · esc 取消"
                } else {
                    "↑↓ 选择 · enter 确认 · esc 取消"
                },
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Memories {
            root_uri,
            entries,
            picker,
        } => {
            let query = picker.query.to_lowercase();
            let filtered: Vec<_> = entries
                .iter()
                .filter(|entry| entry.name.to_lowercase().contains(&query))
                .collect();
            let visible = max_lines.saturating_sub(5).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            let mut lines = vec![Line::from(Span::styled(
                ellipsize(root_uri, width),
                Style::new().fg(theme.accent_model),
            ))];
            if picker.loading {
                lines.push(Line::from(Span::styled(
                    "正在加载记忆…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
            } else if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "没有记忆条目",
                    Style::new().fg(theme.gray),
                )));
            } else {
                for (index, entry) in filtered.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let kind = if entry.is_dir { "▸" } else { "·" };
                    lines.push(Line::from(Span::styled(
                        ellipsize(
                            &format!("{marker}{kind} {}  {}", entry.name, entry.abstract_text),
                            width,
                        ),
                        if actual == picker.selected {
                            Style::new()
                                .fg(theme.accent_user)
                                .bg(theme.bg_visual)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        },
                    )));
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ 移动 · Enter 进入目录 · Esc 返回",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::MemoryEditor {
            uri, editor, dirty, ..
        } => {
            let mut lines = vec![Line::from(Span::styled(
                ellipsize(uri, width),
                Style::new().fg(theme.accent_model),
            ))];
            lines.push(Line::from(Span::styled(
                if *dirty {
                    "已修改 · Ctrl+S 保存"
                } else {
                    "Ctrl+S 保存 · Esc 返回"
                },
                Style::new().fg(theme.gray_dim),
            )));
            lines.extend(
                editor
                    .text
                    .lines()
                    .take(max_lines.saturating_sub(3))
                    .map(|line| {
                        Line::from(Span::styled(
                            line.to_owned(),
                            Style::new().fg(theme.text_secondary),
                        ))
                    }),
            );
            lines
        }
        Modal::Rewind { state } => {
            let mut lines = vec![Line::from(Span::styled(
                "↑↓ 选择当前用户消息内的条目 · ←→ 切换用户消息",
                Style::new().fg(theme.gray_dim),
            ))];
            if state.loading && state.prompts.is_empty() {
                lines.push(Line::from(Span::styled(
                    "正在加载消息历史…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &state.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 重试 · Esc 关闭",
                    Style::new().fg(theme.gray_dim),
                )));
            } else if state.prompts.is_empty() {
                lines.push(Line::from(Span::styled(
                    "当前会话没有可回退的消息",
                    Style::new().fg(theme.gray),
                )));
            } else {
                let prompt = &state.prompts[state.selected_prompt];
                lines.push(Line::from(Span::styled(
                    format!(
                        "用户消息 {}/{} · 当前条目 {}/{}",
                        state.selected_prompt + 1,
                        state.prompts.len(),
                        state.selected_item + 1,
                        prompt.len()
                    ),
                    Style::new().fg(theme.accent_model),
                )));
                let visible = max_lines.saturating_sub(5).max(1);
                let offset = state
                    .selected_item
                    .saturating_sub(visible.saturating_sub(1));
                for (index, entry) in prompt.iter().skip(offset).take(visible).enumerate() {
                    let actual = offset + index;
                    let selected = actual == state.selected_item;
                    let marker = if selected { "❯ " } else { "  " };
                    let text = format!(
                        "{marker}{}  {}",
                        rewind_kind(entry.message.role),
                        rewind_preview(entry)
                    );
                    lines.push(Line::from(Span::styled(
                        ellipsize(&text, width),
                        if selected {
                            Style::new()
                                .fg(theme.accent_user)
                                .bg(theme.bg_visual)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        },
                    )));
                }
                lines.push(Line::from(Span::styled(
                    "Enter 回退到此处（用户消息会恢复到输入框） · Esc 取消",
                    Style::new().fg(theme.gray_dim),
                )));
            }
            lines
        }
        Modal::Sessions { sessions, picker } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query);
            let visible = max_lines.saturating_sub(4).max(1);
            let offset = picker.offset.min(filtered.len().saturating_sub(visible));
            let mut lines = vec![Line::from(Span::styled(
                format!(
                    "搜索：{}",
                    if picker.query.is_empty() {
                        "_"
                    } else {
                        &picker.query
                    }
                ),
                Style::new().fg(theme.accent_model),
            ))];
            if picker.loading && sessions.is_empty() {
                lines.push(Line::from(Span::styled(
                    "正在加载会话…",
                    Style::new().fg(theme.accent_running),
                )));
            } else if let Some(error) = &picker.error {
                lines.push(Line::from(Span::styled(
                    format!("错误：{error}"),
                    Style::new().fg(theme.accent_error),
                )));
                lines.push(Line::from(Span::styled(
                    "Enter 重试 · Esc 关闭",
                    Style::new().fg(theme.gray_dim),
                )));
            } else if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "没有匹配的会话",
                    Style::new().fg(theme.gray),
                )));
            } else {
                for (index, (_, session)) in filtered.iter().skip(offset).take(visible).enumerate()
                {
                    let actual = offset + index;
                    let marker = if actual == picker.selected {
                        "❯ "
                    } else {
                        "  "
                    };
                    let text = format!(
                        "{marker}{}  {}",
                        session.title,
                        ellipsize(&session.cwd, width / 2),
                    );
                    lines.push(Line::from(Span::styled(
                        ellipsize(&text, width),
                        if actual == picker.selected {
                            Style::new()
                                .fg(theme.accent_user)
                                .bg(theme.bg_visual)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::new().fg(theme.text_secondary)
                        },
                    )));
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ 移动 · PgUp/PgDn 翻页 · 输入搜索 · Enter 恢复 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Plan { state } => {
            let prefix = "执行模型：";
            let mut lines = vec![Line::from(vec![
                Span::styled(prefix, Style::new().fg(theme.text_secondary)),
                Span::styled(
                    ellipsize(
                        &state.execution_model,
                        width.saturating_sub(str_width(prefix)),
                    ),
                    Style::new()
                        .fg(theme.accent_model)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ])];
            lines.extend(
                crate::render::wrap::wrap_text(&state.path, width)
                    .into_iter()
                    .map(|text| Line::from(Span::styled(text, Style::new().fg(theme.gray)))),
            );
            let labels = [
                "执行（当前会话）",
                "压缩当前会话后执行",
                "在新会话执行",
                "继续计划",
            ];
            let feedback = usize::from(state.error.is_some() || state.submitting);
            let content_rows = max_lines.saturating_sub(lines.len() + labels.len() + 1 + feedback);
            let all_lines = crate::render::markdown::render_markdown(&state.markdown, width, theme);
            let total = all_lines.len();
            let offset = state.scroll_offset.min(total.saturating_sub(content_rows));
            lines.extend(all_lines.into_iter().skip(offset).take(content_rows));
            lines.push(Line::from(Span::styled(
                ellipsize(
                    "↑↓/PgUp/PgDn 滚动 · ←→/Tab 选择 · Enter 确认 · Ctrl+P 选模型",
                    width,
                ),
                Style::new().fg(theme.gray_dim),
            )));
            for (index, label) in labels.iter().enumerate() {
                let style = if state.submitting {
                    Style::new().fg(theme.gray_dim)
                } else if state.selected == index {
                    Style::new()
                        .fg(theme.accent_success)
                        .bg(theme.bg_visual)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_secondary)
                };
                lines.push(Line::from(Span::styled(
                    ellipsize(
                        &format!(
                            "{} {label}",
                            if state.selected == index { "❯" } else { " " }
                        ),
                        width,
                    ),
                    style,
                )));
            }
            if let Some(error) = &state.error {
                lines.push(Line::from(Span::styled(
                    ellipsize(&format!("错误：{error}"), width),
                    Style::new().fg(theme.accent_error),
                )));
            } else if state.submitting {
                lines.push(Line::from(Span::styled(
                    "正在提交…",
                    Style::new().fg(theme.accent_running),
                )));
            }
            lines
        }
        Modal::Context { usage } => {
            let mut lines = match usage {
                Some(usage) => context_lines(usage, width, theme),
                None => vec![Line::from(vec![
                    Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
                    Span::styled("正在计算上下文…", Style::new().fg(theme.accent_running)),
                ])],
            };
            lines.push(Line::from(String::new()));
            lines.push(Line::from(vec![
                Span::styled(" ".repeat(CONTEXT_INDENT), Style::new()),
                Span::styled("Esc 关闭", Style::new().fg(theme.gray_dim)),
            ]));
            lines
        }
        Modal::Subagents { agents, picker } => {
            let query = picker.query.to_lowercase();
            let filtered: Vec<_> = agents
                .iter()
                .filter(|agent| {
                    agent.title.to_lowercase().contains(&query) || agent.id.contains(&query)
                })
                .collect();
            let mut lines = Vec::with_capacity(filtered.len() + 2);
            if filtered.is_empty() {
                lines.push(Line::from(Span::styled(
                    "当前会话还没有子代理",
                    Style::new().fg(theme.text_secondary),
                )));
                lines.push(Line::from(Span::styled(
                    "可使用 /agent <task> 创建",
                    Style::new().fg(theme.gray_dim),
                )));
            } else {
                for (index, agent) in filtered.iter().enumerate() {
                    let selected = index == picker.selected;
                    lines.push(Line::from(Span::styled(
                        format!(
                            "{}{} · {} · {}",
                            if selected { "❯ " } else { "  " },
                            agent.title,
                            agent.status as u8,
                            agent.model
                        ),
                        if selected {
                            Style::new().fg(theme.text_primary).bg(theme.bg_visual)
                        } else {
                            Style::new().fg(theme.text_primary)
                        },
                    )));
                    if let Some(message) = &agent.last_message {
                        lines.push(Line::from(Span::styled(
                            format!("    {}", ellipsize(message, width.saturating_sub(4))),
                            Style::new().fg(theme.gray_dim),
                        )));
                    }
                }
            }
            lines.push(Line::from(Span::styled(
                "↑↓ 选择 · Enter 查看 · x 停止 · Esc 关闭",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::SubagentDetail {
            agent_id, editor, ..
        } => {
            let mut lines = Vec::new();
            if let Some(view) = app.subagents.get(agent_id) {
                lines.push(Line::from(Span::styled(
                    format!(
                        "{} · {} · {}",
                        view.info.title,
                        view.status_label(),
                        view.info.model
                    ),
                    Style::new().fg(theme.text_secondary),
                )));
                lines.push(Line::from(String::new()));
                for entry in view.blocks.iter().take(max_lines.saturating_sub(4)) {
                    let text = match &entry.block {
                        crate::transcript::Block::User { text }
                        | crate::transcript::Block::Assistant { text }
                        | crate::transcript::Block::Thinking { text, .. }
                        | crate::transcript::Block::Note { text, .. } => text.clone(),
                        crate::transcript::Block::Tool(tool) => format!(
                            "{}: {}",
                            tool.name,
                            tool.result.as_deref().unwrap_or("运行中")
                        ),
                        crate::transcript::Block::Todo { .. } => "Todo 已更新".into(),
                    };
                    lines.push(Line::from(ellipsize(&text, width)));
                }
            } else {
                lines.push(Line::from(Span::styled(
                    "子代理不存在或已切换父会话",
                    Style::new().fg(theme.accent_error),
                )));
            }
            lines.push(Line::from(String::new()));
            lines.push(Line::from(Span::styled(
                if editor.text.is_empty() {
                    "输入消息后 Enter 发送，Alt+Enter 换行".to_owned()
                } else {
                    editor.text.clone()
                },
                Style::new().fg(theme.text_primary),
            )));
            lines.push(Line::from(Span::styled(
                "Enter 发送/steer · Ctrl+S 停止 · Esc 返回",
                Style::new().fg(theme.gray_dim),
            )));
            lines
        }
        Modal::Help => help_lines(app, width, max_lines).0,
    };
    lines.truncate(max_lines.max(1));
    lines
}

fn ellipsize(text: &str, width: usize) -> String {
    if crate::render::wrap::str_width(text) <= width {
        return text.to_owned();
    }
    crate::render::scrollback::truncate_to_width(text.to_owned(), width.max(1))
}

/// 一行的显示宽度（列，CJK 双宽）。
fn line_width(line: &Line<'static>) -> usize {
    line.spans
        .iter()
        .map(|span| str_width(span.content.as_ref()))
        .sum()
}

/// 截断一行到 `width` 列，超出部分以 `…` 收尾。
fn clip_line(line: Line<'static>, width: usize) -> Line<'static> {
    if line_width(&line) <= width {
        return line;
    }
    let budget = width.saturating_sub(1);
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(line.spans.len() + 1);
    let mut used = 0usize;
    for span in line.spans {
        let mut kept = String::new();
        let mut whole = true;
        for ch in span.content.chars() {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_width > budget {
                whole = false;
                break;
            }
            kept.push(ch);
            used += ch_width;
        }
        if !kept.is_empty() {
            spans.push(Span::styled(kept, span.style));
        }
        if !whole {
            spans.push(Span::styled("…", span.style));
            break;
        }
    }
    Line::from(spans).style(line.style)
}

/// 补齐一行到 `width` 列，超出则截断：并排布局两栏互不越界的基础。
fn fit_line(line: Line<'static>, width: usize) -> Line<'static> {
    let used = line_width(&line);
    if used > width {
        return clip_line(line, width);
    }
    if used == width {
        return line;
    }
    let mut spans = line.spans;
    spans.push(Span::raw(" ".repeat(width - used)));
    Line::from(spans).style(line.style)
}

/// 并排拼接左右两栏：左栏补齐到 `left_width` 列后接竖线分隔，再接右栏。
fn join_columns(
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

/// 预览窗内容：按窗宽渲染 markdown 并裁剪到窗口内，截取滚动窗口；
/// 内容高于窗口时末行显示滚动位置与按键提示。
fn ask_preview_lines(
    source: &str,
    width: usize,
    rows: usize,
    scroll: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let width = width.max(8);
    let mut rendered: Vec<Line<'static>> =
        crate::render::markdown::render_markdown(source, width, theme)
            .into_iter()
            .map(|line| clip_line(line, width))
            .collect();
    while rendered
        .last()
        .is_some_and(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
    {
        rendered.pop();
    }
    let rows = rows.max(1);
    let total = rendered.len();
    if total <= rows {
        return rendered;
    }
    let visible = rows - 1;
    let offset = scroll.min(total.saturating_sub(visible));
    let mut out: Vec<Line<'static>> = rendered.into_iter().skip(offset).take(visible).collect();
    out.push(Line::from(Span::styled(
        ellipsize(
            &format!(
                "… {}-{}/{total} 行 · PgUp/PgDn 滚动",
                offset + 1,
                offset + visible
            ),
            width,
        ),
        Style::new().fg(theme.gray_dim),
    )));
    out
}

#[cfg(test)]
mod tests {
    use super::modal_lines;
    use crate::{
        app::App,
        modal::{Modal, PickerState},
        theme::Theme,
    };
    use moc_protocol::methods::{
        AskOption, AskParams, AskQuestion, ContextUsageResult, ContextUsageSection, ModelInfo,
    };
    use ratatui::style::Color;
    use ratatui::text::Line;
    use tokio::sync::mpsc::unbounded_channel;

    #[test]
    fn model_picker_separates_provider_and_context_colors() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let theme = app.theme;
        let modal = Modal::Model {
            models: vec![ModelInfo {
                id: "p/model".into(),
                display_name: "Model".into(),
                thinking_levels: None,
                context_window: Some(500_000),
                max_output_tokens: None,
                pricing: None,
            }],
            current: "p/model".into(),
            picker: PickerState::default(),
            target: crate::modal::ModelTarget::Session,
        };

        let lines = modal_lines(&app, &modal, 80, 20);
        let row = &lines[3];
        let text = row
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();

        assert_eq!(text, "❯ p/model · Ctx 500k");
        assert_eq!(row.spans[1].style.fg, Some(theme.text_secondary));
        assert_eq!(row.spans[2].style.fg, Some(theme.accent_user));
        assert_eq!(row.spans[3].style.fg, Some(theme.accent_model));
        assert!(!text.contains("上下文"));
    }

    /// 上下文窗：分块名用英文、数值统一为 k，色标与色条同色，色条按窗口定标。
    #[test]
    fn context_modal_colors_sections_and_scales_bar_to_window() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let theme = app.theme;
        let modal = Modal::Context {
            usage: Some(ContextUsageResult {
                context_window: 500_000,
                total_tokens: 250_000,
                sections: vec![
                    ContextUsageSection {
                        name: "system".into(),
                        tokens: 100_000,
                    },
                    ContextUsageSection {
                        name: "tools".into(),
                        tokens: 50_000,
                    },
                    ContextUsageSection {
                        name: "messages".into(),
                        tokens: 100_000,
                    },
                ],
            }),
        };

        let lines = modal_lines(&app, &modal, 80, 24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "  250k / 500k · 50.0%");
        assert_eq!(texts[2], "  ▌ system             100k   40.0%");
        assert_eq!(texts[3], "  ▌ tools               50k   20.0%");
        assert_eq!(texts[4], "  ▌ messages           100k   40.0%");
        for (index, row) in [&lines[2], &lines[3], &lines[4]].iter().enumerate() {
            assert_eq!(
                row.spans[1].style.fg,
                Some(theme.chart(index)),
                "第 {index} 块色标应与色条同色"
            );
        }

        // 已用 250k 落在一半格位（39/78），剩余 39 格是空窗口
        let segments: Vec<(usize, Option<Color>)> = lines[6]
            .spans
            .iter()
            .skip(1)
            .map(|span| (span.content.chars().count(), span.style.fg))
            .collect();
        assert_eq!(
            segments,
            vec![
                (16, Some(theme.chart(0))),
                (7, Some(theme.chart(1))),
                (16, Some(theme.chart(2))),
                (39, Some(theme.border)),
            ]
        );

        // 刻度行：分块右界的累积用量 + 右端窗口上限，行宽与色条一致
        let labels: Vec<(String, Option<Color>)> = lines[7]
            .spans
            .iter()
            .filter(|span| !span.content.trim().is_empty())
            .map(|span| (span.content.to_string(), span.style.fg))
            .collect();
        assert_eq!(
            labels,
            vec![
                ("100k".to_owned(), Some(theme.chart(0))),
                ("150k".to_owned(), Some(theme.chart(1))),
                ("250k".to_owned(), Some(theme.chart(2))),
                ("500k".to_owned(), Some(theme.gray_dim)),
            ]
        );
        assert_eq!(line_width(&texts[7]), line_width(&texts[6]));
    }

    /// 上下文窗：空会话是一整条空窗口，分块与合计都显示 0k。
    #[test]
    fn context_modal_shows_empty_window_for_fresh_session() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = Modal::Context {
            usage: Some(ContextUsageResult {
                context_window: 500_000,
                total_tokens: 0,
                sections: vec![
                    ContextUsageSection {
                        name: "system".into(),
                        tokens: 0,
                    },
                    ContextUsageSection {
                        name: "messages".into(),
                        tokens: 0,
                    },
                ],
            }),
        };

        let lines = modal_lines(&app, &modal, 60, 24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        assert_eq!(texts[0], "  0k / 500k · 0.0%");
        assert_eq!(texts[2], "  ▌ system               0k    0.0%");
        assert_eq!(texts[3], "  ▌ messages             0k    0.0%");
        assert!(
            texts[5].chars().all(|ch| ch == ' ' || ch == '─'),
            "空会话的色条整条都是空窗口：{:?}",
            texts[5]
        );
        assert!(texts[6].starts_with("  0k"), "{:?}", texts[6]);
        assert!(texts[6].ends_with("500k"), "{:?}", texts[6]);
    }

    /// 上下文窗：分块名称过长或包含宽字符时保持列宽对齐，Esc 行具有缩进。
    #[test]
    fn context_modal_handles_wide_names_and_indents_footer() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = Modal::Context {
            usage: Some(ContextUsageResult {
                context_window: 100_000,
                total_tokens: 50_000,
                sections: vec![
                    ContextUsageSection {
                        name: "very_long_custom_section_name_overflow".into(),
                        tokens: 30_000,
                    },
                    ContextUsageSection {
                        name: "中文分块名称".into(),
                        tokens: 20_000,
                    },
                ],
            }),
        };

        let lines = modal_lines(&app, &modal, 60, 24);
        let texts: Vec<String> = lines.iter().map(line_text).collect();
        // 验证各分块行总宽度一致，列对齐未被顶破
        assert_eq!(line_width(&texts[2]), line_width(&texts[3]));
        // 验证底部的 Esc 说明具备 2 格左缩进
        let last_line = texts.last().expect("footer line");
        assert!(last_line.starts_with("  Esc 关闭"), "{:?}", last_line);
    }

    /// 提问预览：宽内容宽度并排（左选项、右预览），窄内容宽度落到选项下方；
    /// 预览内容跟随聚焦项。
    #[test]
    fn ask_preview_joins_columns_when_wide_and_stacks_when_narrow() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let modal = ask_modal(0, 0);

        // 并排：聚焦项与其预览同行，分隔线左缘对齐。
        let wide: Vec<String> = modal_lines(&app, &modal, 100, 24)
            .iter()
            .map(line_text)
            .collect();
        let row = wide
            .iter()
            .find(|text| text.contains("❯ ☐ 上中下"))
            .expect("选项行");
        assert!(row.contains("+--+"), "聚焦项预览应与选项同行：{row:?}");
        let offsets: Vec<usize> = wide
            .iter()
            .filter_map(|text| text.split_once(" │ "))
            .map(|(left, _)| crate::render::wrap::str_width(left))
            .collect();
        assert!(offsets.len() >= 3, "选项与预览应共行：{wide:?}");
        assert!(
            offsets.iter().all(|offset| *offset == offsets[0]),
            "左栏宽度应逐行一致：{offsets:?}"
        );
        assert!(
            wide.iter()
                .all(|text| crate::render::wrap::str_width(text) <= 100),
            "并排行不得超出内容宽度：{wide:?}"
        );

        // 窄内容宽度：预览整宽落到选项下方，带标题行。
        let narrow: Vec<String> = modal_lines(&app, &modal, 60, 24)
            .iter()
            .map(line_text)
            .collect();
        assert!(!narrow.iter().any(|text| text.contains(" │ ")));
        let label = narrow
            .iter()
            .position(|text| text == "预览")
            .expect("窄布局的预览标题");
        assert!(
            narrow[label..].iter().any(|text| text.contains("+--+")),
            "预览应渲染在标题行之后：{narrow:?}"
        );

        // 聚焦到无预览的选项：不进入预览布局。
        let plain: Vec<String> = modal_lines(&app, &ask_modal(1, 0), 100, 24)
            .iter()
            .map(line_text)
            .collect();
        assert!(!plain.iter().any(|text| text.contains("+--+")));
        assert!(!plain.iter().any(|text| text == "预览"));
    }

    /// 提问预览：内容高于窗口时按滚动偏移开窗，末行给出位置与按键，
    /// 偏移越界夹到尾部。
    #[test]
    fn ask_preview_scrolls_and_reports_position() {
        let (events, _receiver) = unbounded_channel();
        let app = App::new(Theme::global(), events);
        let mut modal = ask_modal(0, 0);
        let rows = 18;
        let code = (1..=30)
            .map(|line| format!("L{line:02}"))
            .collect::<Vec<_>>()
            .join("\n");
        if let Modal::Ask { params, .. } = &mut modal {
            params.questions[0].options[0].preview = Some(format!("```\n{code}\n```"));
        }

        let texts = |modal: &Modal| -> Vec<String> {
            modal_lines(&app, modal, 60, rows)
                .iter()
                .map(line_text)
                .collect()
        };
        let head = texts(&modal);
        assert!(head.iter().any(|text| text.contains("L01")), "{head:?}");
        let status = head
            .iter()
            .position(|text| text.contains("PgUp/PgDn 滚动"))
            .unwrap_or_else(|| panic!("超窗预览应给滚动提示：{head:?}"));
        assert_eq!(status, head.len() - 2, "滚动提示应是预览窗末行：{head:?}");

        if let Modal::Ask { preview_scroll, .. } = &mut modal {
            *preview_scroll = 8;
        }
        let scrolled = texts(&modal);
        assert!(
            !scrolled.iter().any(|text| text.contains("L01")),
            "滚过首行后不应再显示 L01：{scrolled:?}"
        );
        assert!(
            scrolled.iter().any(|text| text.contains("L09")),
            "{scrolled:?}"
        );

        if let Modal::Ask { preview_scroll, .. } = &mut modal {
            *preview_scroll = usize::MAX;
        }
        let tail = texts(&modal);
        assert!(tail.iter().any(|text| text.contains("L30")), "{tail:?}");
        assert!(
            tail.iter()
                .all(|text| !text.contains("L00") && !text.contains("L31")),
            "{tail:?}"
        );
    }

    /// 卡片级渲染：带预览的提问在真实终端缓冲里并排出现，卡片高度容得下
    /// 选项、预览与底部按键行（不会因换行把提示挤出边框）。
    #[test]
    fn ask_preview_draws_side_by_side_in_terminal_buffer() {
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = unbounded_channel();
        let mut app = App::new(Theme::global(), events);
        app.modal = Some(ask_modal(0, 0));
        let (width, height) = (120u16, 30u16);
        let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
        terminal
            .draw(|frame| super::draw_modal(frame, &mut app))
            .expect("绘制成功");

        let rows: Vec<String> = (0..height)
            .map(|row| {
                (0..width)
                    .filter_map(|x| terminal.backend().buffer().cell((x, row)))
                    .map(|cell| cell.symbol())
                    .collect::<String>()
            })
            .collect();
        // TestBackend 给宽字符的尾部补空格格位：比较时先去掉所有空格。
        let compact: Vec<String> = rows.iter().map(|text| text.replace(' ', "")).collect();
        let option_row = compact
            .iter()
            .find(|text| text.contains("❯☐上中下"))
            .unwrap_or_else(|| {
                let shown: Vec<&String> =
                    rows.iter().filter(|text| !text.trim().is_empty()).collect();
                panic!("选项行应出现在卡片内：{shown:#?}")
            });
        assert!(
            option_row.contains("+--+"),
            "聚焦项预览应与选项同行：{option_row:?}"
        );
        assert!(
            compact.iter().any(|text| text.contains("esc取消")),
            "底部按键行应留在卡片内：{compact:#?}"
        );
    }

    /// 单问、单选的提问模态；`highlight` 指向聚焦选项。
    fn ask_modal(highlight: usize, preview_scroll: usize) -> Modal {
        Modal::Ask {
            params: AskParams {
                session_id: "ses_1".into(),
                prompt_id: "prompt_1".into(),
                request_id: "ask_1".into(),
                questions: vec![AskQuestion {
                    id: "layout".into(),
                    question: "选哪个布局？".into(),
                    header: None,
                    multi: false,
                    recommended: None,
                    options: vec![
                        AskOption {
                            label: "上中下".into(),
                            description: None,
                            preview: Some("```\n+--+\n|A |\n+--+\n```".into()),
                        },
                        AskOption {
                            label: "左右".into(),
                            description: None,
                            preview: None,
                        },
                    ],
                }],
            },
            question: 0,
            highlight: vec![highlight],
            selected: vec![Vec::new()],
            custom: vec![crate::modal::CustomAnswer::default()],
            preview_scroll,
            reply: None,
        }
    }

    /// 一行渲染后的文本。
    fn line_text(line: &Line<'static>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// 一行渲染后的显示宽度（格）。
    fn line_width(text: &str) -> usize {
        crate::render::wrap::str_width(text)
    }
}

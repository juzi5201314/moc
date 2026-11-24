use moc_i18n::t;

use super::super::PROSE_INDENT;
use super::super::TITLE_INDENT;
use super::super::labels;
use super::super::markdown::RenderViews;
use super::super::markdown::render_markdown;
use super::super::markdown::render_markdown_with;
use super::super::wrap::clip_line;
use super::super::wrap::str_width;
use super::super::wrap::wrap_text;
use super::AGENT_MESSAGE_PREVIEW_ROWS;
use super::BRIEF_THINKING_ROWS;
use super::EntryLines;
use super::LocalTable;
use super::agent_status_label;
use super::layout::prose_width;
use super::layout::wrapped_text_rows;
use super::outcome_word;
use super::text::hang;
use super::text::plain_text;
use crate::app::App;
use crate::theme::Theme;
use crate::transcript::DetailLevel;
use crate::transcript::Entry;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

/// 用户消息：与输入框同款的圆角边框定格进转写，边框与 `❯` 提示符取
/// `accent_user`；末行右侧放本地时间，放不下则在框下另起一行。
pub(super) fn user_message(
    text: &str,
    at_ms: Option<u64>,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let border = Style::new().fg(theme.accent_user);
    // 框从块标记列起：`│ ` + `❯ ` + 正文 + ` │`，正文列宽 = 框宽 − 6。
    let box_width = width.saturating_sub(TITLE_INDENT);
    let rows = wrap_text(text, box_width.saturating_sub(6).max(1));
    let last = rows.len().saturating_sub(1);
    let mut clock_pending = at_ms.and_then(format_clock);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 4);
    let frame = |open: &str, close: &str| {
        Line::from(vec![
            Span::raw(" ".repeat(TITLE_INDENT)),
            Span::styled(
                format!("{open}{}{close}", "─".repeat(box_width.saturating_sub(2))),
                border,
            ),
        ])
    };
    lines.push(frame("╭", "╮"));
    for (index, row) in rows.into_iter().enumerate() {
        let mut spans = vec![
            Span::raw(" ".repeat(TITLE_INDENT)),
            Span::styled("│ ", border),
        ];
        spans.push(if index == 0 {
            Span::styled(
                "❯ ",
                Style::new()
                    .fg(theme.accent_user)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw("  ")
        });
        // 正文列右侧的余量：`│` 前的全部空位；时间戳要留至少两格间距加一格贴框。
        let rest = box_width.saturating_sub(5 + str_width(&row));
        spans.extend(crate::render::chip_spans(
            &row,
            theme.text_primary,
            theme.accent_warning,
        ));
        if index == last
            && let Some(clock) = clock_pending.take_if(|clock| str_width(clock) + 3 <= rest)
        {
            spans.push(Span::raw(" ".repeat(rest - str_width(&clock) - 1)));
            spans.push(Span::styled(clock, Style::new().fg(theme.gray_dim)));
            spans.push(Span::raw(" "));
        } else {
            spans.push(Span::raw(" ".repeat(rest.max(1))));
        }
        spans.push(Span::styled("│", border));
        lines.push(Line::from(spans));
    }
    lines.push(frame("╰", "╯"));
    if let Some(clock) = clock_pending {
        lines.push(Line::from(vec![
            Span::raw(" ".repeat(width.saturating_sub(str_width(&clock)))),
            Span::styled(clock, Style::new().fg(theme.gray_dim)),
        ]));
    }
    lines.push(Line::default());
    lines
}

/// 助手回复：正文按 [`PROSE_INDENT`] 悬挂缩进，尾注行给耗时与本地时间。
///
/// 正文交给带表格视图态的渲染器：已切换成图表的表格会变成占位行，其余表格
/// 下方挂入口按钮；返回的落位信息（局部行号）供命中表与位图槽位登记。
pub(super) fn assistant_message(
    text: &str,
    entry: &Entry,
    app: &App,
    width: usize,
    spawn_images: bool,
) -> EntryLines {
    let theme = app.theme;
    assistant_layout(
        text,
        width,
        theme,
        RenderViews {
            charts: &entry.charts,
            graphics: app.graphics,
            streaming: app.prompt_running && entry.elapsed_ms.is_none(),
            images: Some(&app.images),
            cwd: Some(std::path::Path::new(&app.startup_cwd)),
            spawn_images,
            links_expanded: entry.links_expanded,
        },
        prompt_footer(entry, width, theme),
    )
}

pub(super) fn assistant_layout(
    text: &str,
    width: usize,
    theme: &Theme,
    views: RenderViews<'_>,
    footer: Option<Line<'static>>,
) -> EntryLines {
    if text.len() > 256 * 1024 {
        let rendered = super::super::markdown::render_markdown_pages(
            text,
            prose_width(width).max(8),
            theme,
            views,
            true,
            false,
        );
        let height = rendered.lines.len() + usize::from(footer.is_some()) + 1;
        let mut tail = Vec::new();
        if let Some(footer) = footer {
            tail.push(footer);
        }
        tail.push(Line::default());
        // Footer 存在独立的最终页中，避免修改已经插入表格入口的正文坐标。
        return EntryLines {
            lines: tail,
            paged: Some(rendered.lines),
            height,
            images: rendered.images,
            links_button: rendered.links_button,
            tables: rendered
                .tables
                .into_iter()
                .map(|slot| LocalTable {
                    table: slot.table,
                    button: slot.button,
                    chart: slot.chart,
                })
                .collect(),
        };
    }
    let rendered = render_markdown_with(text, prose_width(width).max(8), theme, views);
    let mut lines: Vec<Line<'static>> = rendered
        .lines
        .into_iter()
        .map(|line| hang(line, PROSE_INDENT, usize::MAX))
        .collect();
    if let Some(footer) = footer {
        lines.push(footer);
    }
    lines.push(Line::from(String::new()));
    EntryLines {
        height: lines.len(),
        paged: None,
        lines,
        images: rendered.images,
        links_button: rendered.links_button,
        tables: rendered
            .tables
            .into_iter()
            .map(|slot| LocalTable {
                table: slot.table,
                button: slot.button,
                chart: slot.chart,
            })
            .collect(),
    }
}

/// 助手回复尾注：左侧耗时、右侧本地时间；提示未结束（或耗时未知）时不占行。
pub(super) fn prompt_footer(entry: &Entry, width: usize, theme: &Theme) -> Option<Line<'static>> {
    let text = labels::elapsed(entry.elapsed_ms?);
    let clock = entry.at_ms.and_then(format_clock);
    let used = PROSE_INDENT + str_width(&text);
    let mut spans = vec![
        Span::raw(" ".repeat(PROSE_INDENT)),
        Span::styled(text, Style::new().fg(theme.gray_dim)),
    ];
    if let Some(clock) = clock {
        let gap = width.saturating_sub(used + str_width(&clock)).max(2);
        spans.push(Span::raw(" ".repeat(gap)));
        spans.push(Span::styled(clock, Style::new().fg(theme.gray_dim)));
    }
    Some(clip_line(Line::from(spans), width))
}

/// 思维链：标题行 `◆ Thought · 1.2s · 35 tok/s`；简略档预览正文末尾若干行
/// （跳过段间空行，超出预算补一行省略提示）；完整档显示全部正文（低对比度引文带）。
///
/// 耗时与速率只在本地测到数据时出现：历史恢复的块没有这两项，
/// 不拿 `0ms` 充数，否则会与「瞬间想完」混淆。
pub(super) fn thinking_block(
    text: &str,
    reasoning_tokens: Option<u64>,
    duration_ms: Option<u64>,
    app: &App,
    width: usize,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let theme = app.theme;
    let dim = Style::new().fg(theme.gray_dim);
    let mut spans = vec![
        Span::raw(" ".repeat(TITLE_INDENT)),
        Span::styled("◆ ", Style::new().fg(theme.accent_thinking)),
        Span::styled(
            labels::title::THOUGHT,
            Style::new()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::BOLD),
        ),
    ];
    if let Some(ms) = duration_ms {
        spans.push(Span::styled(
            format!("{}{}", labels::SEP, labels::duration(ms)),
            dim,
        ));
        if let Some(tokens) = reasoning_tokens
            && let Some(rate) = thinking_tokens_per_second(tokens, ms)
        {
            spans.push(Span::styled(
                format!("{}{rate} {}", labels::SEP, labels::RATE_UNIT),
                dim,
            ));
        }
    }
    if level == DetailLevel::Hidden {
        let binding = app
            .preferences
            .keybindings
            .get("thinking")
            .and_then(|keys| keys.first())
            .map_or("ctrl+t", String::as_str);
        spans.push(Span::styled(
            format!(
                "  {}",
                labels::hints(&[(binding, t!("tui.hint.expand_row"))], usize::MAX)
            ),
            dim,
        ));
    }
    let mut lines = vec![clip_line(Line::from(spans), width)];
    match level {
        DetailLevel::Hidden => {}
        DetailLevel::Full => lines.extend(thinking_rows(
            render_markdown(text, prose_width(width).max(8), theme),
            theme,
        )),
        DetailLevel::Brief => {
            let mut rows = render_markdown(text, prose_width(width).max(8), theme);
            // 段间空行不占预览预算
            rows.retain(|line| line.width() > 0 && !plain_text(line).trim().is_empty());
            let omitted = rows.len().saturating_sub(BRIEF_THINKING_ROWS);
            rows.drain(..omitted);
            if omitted > 0 {
                rows.push(Line::from(Span::styled(labels::more_lines(omitted), dim)));
            }
            lines.extend(thinking_rows(rows, theme));
        }
    }
    lines.push(Line::default());
    lines
}

/// 思考速率：思考 token 数 / 思考耗时，四舍五入到整 token/s。
/// 耗时为 0 表示没测到有效时长，此时不给速率。
pub(super) fn thinking_tokens_per_second(tokens: u64, duration_ms: u64) -> Option<u64> {
    (duration_ms > 0)
        .then(|| tokens.saturating_mul(1_000).saturating_add(duration_ms / 2) / duration_ms)
}

/// 思考正文的引文带行：`▏ ` 落在块标记列，正文因此落在正文列；文字一律低对比度。
pub(super) fn thinking_rows(rows: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|line| {
            let mut spans = vec![
                Span::raw(" ".repeat(TITLE_INDENT)),
                Span::styled("▏ ", Style::new().fg(theme.accent_thinking)),
            ];
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style.fg(theme.gray))),
            );
            Line::from(spans)
        })
        .collect()
}

/// 接收侧 IRC 卡片：隐藏档只留标题行；简略档预览正文若干行；完整档全文。
pub(super) fn agent_message_block(
    meta: &moc_protocol::methods::AgentMessageMeta,
    body: &str,
    width: usize,
    theme: &Theme,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let (word, color) = outcome_label(meta.delivery, theme);
    agent_card(
        Line::from(vec![
            Span::raw(" ".repeat(TITLE_INDENT)),
            Span::styled("◆ ", Style::new().fg(theme.accent_tool)),
            Span::styled(
                format!("{} ", labels::title::IRC),
                Style::new()
                    .fg(theme.accent_tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled("◀ ", Style::new().fg(theme.accent_tool)),
            Span::styled(meta.from_title.clone(), Style::new().fg(theme.text_primary)),
            Span::styled(format!("{}{word}", labels::SEP), Style::new().fg(color)),
        ]),
        body,
        width,
        theme,
        level,
    )
}

/// 结果标题明确区分来源与终态；报告正文沿用 IRC 的折叠与折行规则。
pub(super) fn agent_result_block(
    meta: &moc_protocol::methods::AgentResultMeta,
    body: &str,
    width: usize,
    theme: &Theme,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let color = match meta.status {
        moc_protocol::methods::AgentStatus::Failed => theme.accent_error,
        moc_protocol::methods::AgentStatus::Stopped => theme.gray_bright,
        _ => theme.accent_tool,
    };
    agent_card(
        Line::from(vec![
            Span::raw(" ".repeat(TITLE_INDENT)),
            Span::styled("◆ ", Style::new().fg(color)),
            Span::styled(
                t!("tui.subagent.result").into_owned(),
                Style::new()
                    .fg(theme.accent_tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(meta.title.clone(), Style::new().fg(theme.text_primary)),
            Span::styled(
                format!("{}{}", labels::SEP, agent_status_label(meta.status)),
                Style::new().fg(color),
            ),
        ]),
        body,
        width,
        theme,
        level,
    )
}

/// 任务通知卡片：标题带状态与耗时尾注，正文是送达模型的原文。
pub(super) fn job_notice_block(
    job: &moc_protocol::methods::JobInfo,
    body: &str,
    width: usize,
    theme: &Theme,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    use moc_protocol::methods::JobStatus;
    let status = if job.stalled {
        t!("tui.job.stalled").into_owned()
    } else {
        match job.status {
            JobStatus::Exited => job.exit_code.map_or_else(
                || t!("tui.job.finished").into_owned(),
                |code| t!("tui.job.exit_fmt", code = code).into_owned(),
            ),
            JobStatus::Killed => t!("tui.job.killed").into_owned(),
            JobStatus::TimedOut => t!("tui.job.timed_out").into_owned(),
            JobStatus::Running | JobStatus::Killing => t!("tui.job.running").into_owned(),
        }
    };
    let color = if job.stalled || matches!(job.status, JobStatus::Running | JobStatus::Killing) {
        theme.accent_warning
    } else if job.status == JobStatus::Exited && job.exit_code == Some(0) {
        theme.accent_success
    } else {
        theme.accent_error
    };
    let duration = job
        .duration_ms
        .map(|ms| format!("{}{}", labels::SEP, labels::duration(ms)))
        .unwrap_or_default();
    agent_card(
        Line::from(vec![
            Span::raw(" ".repeat(TITLE_INDENT)),
            Span::styled("◆ ", Style::new().fg(color)),
            Span::styled(
                t!("tui.job.notice").into_owned(),
                Style::new()
                    .fg(theme.accent_tool)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(job.job_id.clone(), Style::new().fg(theme.text_primary)),
            Span::styled(
                format!("{}{status}{duration}", labels::SEP),
                Style::new().fg(color),
            ),
        ]),
        body,
        width,
        theme,
        level,
    )
}

pub(super) fn agent_card(
    header: Line<'static>,
    body: &str,
    width: usize,
    theme: &Theme,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let mut lines = vec![clip_line(header, width)];
    let indent = " ".repeat(PROSE_INDENT);
    let rows = wrapped_text_rows(body, prose_width(width));
    let total = rows.len();
    let shown = match level {
        DetailLevel::Hidden => 0,
        DetailLevel::Brief => total.min(AGENT_MESSAGE_PREVIEW_ROWS),
        DetailLevel::Full => total,
    };
    for row in rows.into_iter().take(shown) {
        lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(row, Style::new().fg(theme.text_primary)),
        ]));
    }
    if shown < total && level != DetailLevel::Hidden {
        lines.push(Line::from(vec![
            Span::raw(indent),
            Span::styled(
                labels::more_lines(total - shown),
                Style::new().fg(theme.gray_dim),
            ),
        ]));
    }
    lines.push(Line::default());
    lines
}

/// goal 续跑轮卡片：标题给 `goal` 与轮次，正文同代理消息卡片排版。
pub(super) fn goal_round_block(
    meta: &moc_protocol::methods::GoalRoundMeta,
    body: &str,
    width: usize,
    theme: &Theme,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let mut lines = vec![clip_line(
        Line::from(vec![
            Span::raw(" ".repeat(TITLE_INDENT)),
            Span::styled("◇ ", Style::new().fg(theme.accent_running)),
            Span::styled(
                t!("tui.subagent.goal").into_owned(),
                Style::new()
                    .fg(theme.accent_running)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                t!(
                    "tui.subagent.round_fmt",
                    round = meta.round,
                    sep = labels::SEP
                )
                .into_owned(),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled(
                t!("tui.subagent.auto").into_owned(),
                Style::new().fg(theme.gray_bright),
            ),
        ]),
        width,
    )];
    let indent = " ".repeat(PROSE_INDENT);
    let rows = wrapped_text_rows(body, prose_width(width));
    let total = rows.len();
    let shown = match level {
        DetailLevel::Hidden => 0,
        DetailLevel::Brief => total.min(AGENT_MESSAGE_PREVIEW_ROWS),
        DetailLevel::Full => total,
    };
    for row in rows.into_iter().take(shown) {
        lines.push(Line::from(vec![
            Span::raw(indent.clone()),
            Span::styled(row, Style::new().fg(theme.text_primary)),
        ]));
    }
    if shown < total && level != DetailLevel::Hidden {
        lines.push(Line::from(vec![
            Span::raw(indent),
            Span::styled(
                labels::more_lines(total - shown),
                Style::new().fg(theme.gray_dim),
            ),
        ]));
    }
    lines.push(Line::default());
    lines
}

/// 系统提示：错误 `✘`、警告 `▲`、普通 `»` 落在块标记列，正文落在正文列。
pub(super) fn note_block(
    text: &str,
    tone: crate::transcript::NoteTone,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use crate::transcript::NoteTone;
    let (marker, color) = match tone {
        NoteTone::Error => ("✘ ", theme.accent_error),
        NoteTone::Warn => ("▲ ", theme.accent_warning),
        NoteTone::Info => ("» ", theme.gray_bright),
    };
    let mut lines: Vec<Line<'static>> = wrap_text(text, prose_width(width))
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(vec![
                Span::raw(" ".repeat(TITLE_INDENT)),
                if index == 0 {
                    Span::styled(marker, Style::new().fg(color))
                } else {
                    Span::raw("  ")
                },
                Span::styled(row, Style::new().fg(color)),
            ])
        })
        .collect();
    lines.push(Line::default());
    lines
}

/// UNIX 毫秒 → 本地 `HH:MM`；超出可表示范围时返回 `None`。
pub(super) fn format_clock(at_ms: u64) -> Option<String> {
    use chrono::TimeZone;
    let millis = i64::try_from(at_ms).ok()?;
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|at| at.format("%H:%M").to_string())
}

/// 单条投递结果的措辞与颜色。
pub(super) fn outcome_label(
    outcome: moc_protocol::methods::AgentMessageOutcome,
    theme: &Theme,
) -> (&'static str, ratatui::style::Color) {
    use moc_protocol::methods::AgentMessageOutcome;
    let color = match outcome {
        AgentMessageOutcome::Injected => theme.accent_running,
        AgentMessageOutcome::Woken => theme.accent_tool,
        AgentMessageOutcome::Failed => theme.accent_error,
    };
    (outcome_word(outcome), color)
}

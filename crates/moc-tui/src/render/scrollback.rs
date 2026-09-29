//! 滚动区渲染管线：块 → 行缓存 → 视口绘制。
//!
//! 缓存 key = `(块内容版本, 视图版本, 宽度)`：流式增量只让活动块失效，
//! 全局折叠开关让受影响块失效，resize 全量重建。绘制按行直写 buffer，
//! 视口之外的历史完全不进渲染器。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use serde_json::Value;

use super::diff;
use super::markdown::render_markdown;
use super::wrap::{str_width, wrap_text};
use super::{PROSE_INDENT, TITLE_INDENT};
use crate::app::App;
use crate::theme::Theme;
use crate::transcript::{Block, Entry, ToolCallBlock};

/// 工具结果在展开态下最多显示的行数（超出折叠为提示）。
const TOOL_RESULT_TAIL_ROWS: usize = 14;

/// 滚动区渲染状态（与 `App::blocks` 长度保持同步）。
#[derive(Default)]
pub struct ScrollbackView {
    width: u16,
    cache: Vec<CachedEntry>,
}

struct CachedEntry {
    key: (u64, bool, u64),
    height: usize,
    lines: Vec<Line<'static>>,
    rendered: bool,
}

impl ScrollbackView {
    pub fn new() -> Self {
        Self::default()
    }

    /// 同步失效块；非视口块的文本行会在绘制后释放，只保留高度索引。
    pub fn sync(&mut self, app: &App, width: u16) -> usize {
        if self.width != width {
            self.width = width;
            self.cache.clear();
        }
        for (index, entry) in app.blocks.iter().enumerate() {
            let key = (entry.generation, entry.folded, app.view_gen);
            if self
                .cache
                .get(index)
                .is_some_and(|cached| cached.key == key)
            {
                continue;
            }
            let lines = render_entry(entry, app, width, index == 0);
            let height = lines.len();
            let cached = CachedEntry {
                key,
                height,
                lines,
                rendered: true,
            };
            if index < self.cache.len() {
                self.cache[index] = cached;
            } else {
                self.cache.push(cached);
            }
        }
        self.cache.truncate(app.blocks.len());
        self.cache.iter().map(|cached| cached.height).sum()
    }

    /// 在绘制后释放完全离开视口的行，只保留高度与缓存键。
    pub fn trim_outside(&mut self, skip_rows: usize, viewport: usize) {
        let end = skip_rows.saturating_add(viewport);
        let mut offset = 0usize;
        for cached in &mut self.cache {
            let next = offset.saturating_add(cached.height);
            if next <= skip_rows || offset >= end {
                cached.lines.clear();
                cached.rendered = false;
            }
            offset = next;
        }
    }

    /// 为当前视口懒加载被回收的块。
    pub fn ensure_visible(&mut self, app: &App, skip_rows: usize, viewport: usize) {
        let end = skip_rows.saturating_add(viewport);
        let mut offset = 0usize;
        for (index, cached) in self.cache.iter_mut().enumerate() {
            let next = offset.saturating_add(cached.height);
            if !cached.rendered && offset < end && next > skip_rows {
                let lines = render_entry(&app.blocks[index], app, self.width, index == 0);
                cached.height = lines.len();
                cached.lines = lines;
                cached.rendered = true;
            }
            offset = next;
        }
    }

    /// 从第 `skip_rows` 行开始把内容画进视口，并登记命中表与行文本。
    pub fn paint(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        skip_rows: usize,
        collect_lines: bool,
        hits: &mut Vec<(usize, u16, u16)>,
        lines: &mut Vec<(u16, String)>,
    ) {
        let mut offset = skip_rows;
        let mut y = area.y;
        let bottom = area.y.saturating_add(area.height);
        for (index, cached) in self.cache.iter().enumerate() {
            if offset >= cached.height {
                offset -= cached.height;
                continue;
            }
            let has_title = offset == 0;
            let row_start = y;
            for line in &cached.lines[offset..] {
                if y >= bottom {
                    break;
                }
                buffer.set_line(area.x, y, line, area.width);
                if collect_lines {
                    let text = line.spans.iter().fold(String::new(), |mut text, span| {
                        text.push_str(span.content.as_ref());
                        text
                    });
                    lines.push((y, text));
                }
                y += 1;
            }
            if has_title {
                hits.push((index, row_start, y));
            }
            offset = 0;
            if y >= bottom {
                return;
            }
        }
    }
}

/// 单块渲染。`first` 表示它是转写首块（不额外加前导留白）。
/// 折叠状态由全局开关与块类型共同决定。
fn render_entry(entry: &Entry, app: &App, width: u16, first: bool) -> Vec<Line<'static>> {
    let width = usize::from(width);
    let theme = app.theme;
    match &entry.block {
        Block::User { text } => user_message(text, entry.at_ms, first, width, theme),
        Block::Assistant { text } => assistant_message(text, entry, width, theme),
        Block::Thinking { text } => thinking_block(text, entry, app, width),
        Block::Tool(tool) => render_tool(tool, app, width, entry.folded),
        Block::Todo { phases } => render_todo(phases, app, width),
        Block::Note { text, error } => note_block(text, *error, width, theme),
    }
}

/// 用户消息：整幅圆角边框面板，末行右侧给本地时间。
///
/// 面板左右各留 1 格内边距、上下一行边框，背景与转写区一致；长文本吃满面板宽度换行；
/// 末行放不下时间戳时单独追加一行，绝不与正文重叠。
fn user_message(
    text: &str,
    at_ms: Option<u64>,
    first: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut pending_clock = at_ms.and_then(format_clock);
    let inner = width.saturating_sub(2);
    let text_width = inner.saturating_sub(2).max(8);
    let border_style = Style::new().fg(theme.border).bg(theme.bg_base);
    let fill = Style::new().bg(theme.bg_base);
    let text_style = Style::new().fg(theme.accent_user).bg(theme.bg_base);

    let rows = wrap_text(text, text_width);
    let last_row = rows.len().saturating_sub(1);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 3);
    if !first {
        lines.push(Line::from(String::new()));
    }
    lines.push(Line::from(Span::styled(
        format!("╭{}╮", "─".repeat(inner)),
        border_style,
    )));
    for (index, row) in rows.into_iter().enumerate() {
        let free = inner.saturating_sub(2 + str_width(&row));
        let mut spans = vec![
            Span::styled("│".to_owned(), border_style),
            Span::styled(" ".to_owned(), fill),
            Span::styled(row, text_style),
        ];
        if index == last_row
            && let Some(clock) = &pending_clock
        {
            let clock_width = str_width(clock);
            if free >= clock_width + 2 {
                spans.push(Span::styled(" ".repeat(free - clock_width), fill));
                spans.push(Span::styled(
                    clock.clone(),
                    Style::new().fg(theme.gray).bg(theme.bg_highlight),
                ));
                pending_clock = None;
            }
        }
        spans.push(Span::styled(" ".to_owned(), fill));
        spans.push(Span::styled("│".to_owned(), border_style));
        lines.push(pad_line(spans, fill, width));
    }
    if let Some(clock) = pending_clock {
        let clock_width = str_width(&clock);
        let free = inner.saturating_sub(2);
        lines.push(pad_line(
            vec![
                Span::styled("│".to_owned(), border_style),
                Span::styled(" ".to_owned(), fill),
                Span::styled(" ".repeat(free.saturating_sub(clock_width)), fill),
                Span::styled(clock, Style::new().fg(theme.gray).bg(theme.bg_highlight)),
                Span::styled(" ".to_owned(), fill),
                Span::styled("│".to_owned(), border_style),
            ],
            fill,
            width,
        ));
    }
    lines.push(Line::from(Span::styled(
        format!("╰{}╯", "─".repeat(inner)),
        border_style,
    )));
    lines.push(Line::from(String::new()));
    lines
}

/// 助手回复：正文按 [`PROSE_INDENT`] 悬挂缩进，尾注行给耗时与本地时间。
fn assistant_message(text: &str, entry: &Entry, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> =
        render_markdown(text, width.saturating_sub(PROSE_INDENT).max(8), theme)
            .into_iter()
            .map(indent_prose)
            .collect();
    if let Some(footer) = turn_footer(entry, width, theme) {
        lines.push(footer);
    }
    lines.push(Line::from(String::new()));
    lines
}

/// 助手回复尾注：左侧 `耗时`、右侧本地时间；轮次未结束（或耗时未知）时不占行。
fn turn_footer(entry: &Entry, width: usize, theme: &Theme) -> Option<Line<'static>> {
    let elapsed = format_duration(entry.elapsed_ms?);
    let clock = entry.at_ms.and_then(format_clock);
    let text = format!("耗时 {elapsed}");
    let used = PROSE_INDENT + str_width(&text);
    let mut spans = vec![
        Span::styled(" ".repeat(PROSE_INDENT), Style::new()),
        Span::styled(text, Style::new().fg(theme.gray_dim)),
    ];
    if let Some(clock) = clock {
        let gap = width.saturating_sub(used + str_width(&clock)).max(2);
        spans.push(Span::styled(" ".repeat(gap), Style::new()));
        spans.push(Span::styled(clock, Style::new().fg(theme.gray_dim)));
    }
    Some(Line::from(spans))
}

/// 思维链：折叠时单行标题；展开时引文带（左竖线 + 抬升底色 + 斜体）。
fn thinking_block(text: &str, entry: &Entry, app: &App, width: usize) -> Vec<Line<'static>> {
    let theme = app.theme;
    let expanded = app.show_thinking && !entry.folded;
    let mut spans = vec![
        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
        Span::styled("◆ ".to_owned(), Style::new().fg(theme.accent_thinking)),
        Span::styled(
            "思考内容".to_owned(),
            Style::new()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(" · {} 字符", text.chars().count()),
            Style::new().fg(theme.gray_dim),
        ),
    ];
    if !expanded {
        let key = app
            .preferences
            .keybindings
            .get("thinking")
            .and_then(|keys| keys.first())
            .map_or("ctrl+t", String::as_str);
        spans.push(Span::styled(
            format!("  {key} 展开"),
            Style::new().fg(theme.gray_dim),
        ));
    }
    let mut lines = vec![Line::from(spans)];
    if expanded {
        let band = Style::new().bg(theme.bg_hover);
        for line in render_markdown(text, width.saturating_sub(PROSE_INDENT + 1).max(8), theme) {
            let mut spans = vec![Span::styled(
                "▏ ".to_owned(),
                Style::new().fg(theme.accent_thinking).bg(theme.bg_hover),
            )];
            spans.extend(line.spans.into_iter().map(|span| {
                Span::styled(
                    span.content.into_owned(),
                    span.style
                        .fg(theme.accent_thinking)
                        .bg(theme.bg_hover)
                        .add_modifier(Modifier::ITALIC),
                )
            }));
            lines.push(pad_line(spans, band, width));
        }
    }
    lines.push(Line::from(String::new()));
    lines
}

/// 系统提示：错误 `✘`、普通 `»`，缩进与块标题一致。
fn note_block(text: &str, error: bool, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (marker, color) = if error {
        ("✘ ", theme.accent_error)
    } else {
        ("» ", theme.gray_bright)
    };
    let mut lines: Vec<Line<'static>> = wrap_text(text, width.saturating_sub(TITLE_INDENT + 2))
        .into_iter()
        .enumerate()
        .map(|(index, row)| {
            Line::from(vec![
                Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
                if index == 0 {
                    Span::styled(marker.to_owned(), Style::new().fg(color))
                } else {
                    Span::styled("  ".to_owned(), Style::new())
                },
                Span::styled(row, Style::new().fg(color)),
            ])
        })
        .collect();
    lines.push(Line::from(String::new()));
    lines
}

/// 用 `fill` 补满到 `width`：所有行都自带底色，面板/引文带才成形。
fn pad_line(spans: Vec<Span<'static>>, fill: Style, width: usize) -> Line<'static> {
    let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
    let mut spans = spans;
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), fill));
    }
    Line::from(spans)
}

/// 正文统一左缩进（助手回复、工具内容共用同一列）。
fn indent_prose(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(" ".repeat(PROSE_INDENT), Style::new())];
    spans.extend(line.spans);
    Line::from(spans)
}

/// UNIX 毫秒 → 本地 `HH:MM`；超出可表示范围时返回 `None`。
fn format_clock(at_ms: u64) -> Option<String> {
    use chrono::TimeZone;
    let millis = i64::try_from(at_ms).ok()?;
    chrono::Local
        .timestamp_millis_opt(millis)
        .single()
        .map(|at| at.format("%H:%M").to_string())
}

/// 耗时展示：毫秒级给 `ms`，不足一分钟给一位小数，再长给 `Xm Ys`。
fn format_duration(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms} ms")
    } else if ms < 60_000 {
        format!("{}.{}s", ms / 1000, (ms % 1000) / 100)
    } else {
        format!("{}m {}s", ms / 60_000, (ms % 60_000) / 1000)
    }
}

/// 工具块：标题行 + 展开内容（按工具名分派）。
fn render_tool(tool: &ToolCallBlock, app: &App, width: usize, folded: bool) -> Vec<Line<'static>> {
    let theme = app.theme;
    let mut lines = Vec::new();
    let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);

    // 标题行：bullet + 工具名 + 参数摘要
    let (bullet, bullet_color) = match (tool.running, tool.status) {
        (true, _) => ("▸", theme.accent_running),
        (_, Some(moc_protocol::methods::ToolStatus::Completed)) => ("◆", theme.accent_tool),
        (_, Some(_)) => ("✘", theme.accent_error),
        (_, None) => ("◆", theme.accent_tool),
    };
    let summary = tool_summary(tool, &args);
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
        Span::styled(format!("{bullet} "), Style::new().fg(bullet_color)),
        Span::styled(
            tool.name.clone(),
            Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {summary}"), Style::new().fg(theme.text_secondary)),
    ]));

    if app.tools_collapsed || folded {
        return lines;
    }

    let inner_width = width.saturating_sub(4);
    let is_error = tool
        .status
        .is_some_and(|status| status != moc_protocol::methods::ToolStatus::Completed);

    if let Some(diff) = &tool.diff {
        // edit / write：diff 全文渲染
        lines.extend(
            diff::render_diff(diff, inner_width, theme)
                .into_iter()
                .map(|line| indent_line(line)),
        );
    } else if tool.name == "bash" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        for (index, row) in wrap_text(command, inner_width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(Span::styled(
                format!("{}{}", if index == 0 { "  $ " } else { "    " }, row),
                Style::new().fg(theme.command),
            )));
        }
        if let Some(result) = &tool.result {
            let rows = clamp_rows(result, inner_width - 2);
            lines.extend(rows.into_iter().map(|text| {
                Line::from(Span::styled(
                    format!("  {text}"),
                    Style::new().fg(if is_error {
                        theme.diff_delete_fg
                    } else {
                        theme.text_secondary
                    }),
                ))
            }));
        }
    } else if tool.name == "read" || tool.name == "find" {
        lines.extend(file_list_rows(&args, inner_width, theme));
    } else if tool.name == "grep" {
        let pattern = args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default();
        lines.push(Line::from(Span::styled(
            format!("  ⌕ {pattern}"),
            Style::new().fg(theme.path),
        )));
        if let Some(result) = &tool.result {
            let rows = clamp_rows(result, inner_width - 2);
            lines.extend(rows.into_iter().map(|text| {
                Line::from(Span::styled(
                    format!("  {text}"),
                    Style::new().fg(theme.text_secondary),
                ))
            }));
        }
    } else if let Some(result) = &tool.result {
        let rows = clamp_rows(result, inner_width);
        lines.extend(rows.into_iter().map(|text| {
            Line::from(Span::styled(
                format!("  {text}"),
                Style::new().fg(theme.text_secondary),
            ))
        }));
    }

    // 尾注：耗时 / 失败原因
    if let Some(duration) = tool.duration_ms {
        lines.push(Line::from(Span::styled(
            format!("  ⏱ {}", format_duration(duration)),
            Style::new().fg(theme.gray_dim),
        )));
    }
    lines.push(Line::from(String::new()));
    lines
}

/// 标题行的参数摘要。
fn tool_summary(tool: &ToolCallBlock, args: &Value) -> String {
    match tool.name.as_str() {
        "edit" | "write" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
            let counts = tool
                .diff
                .as_ref()
                .map(|diff| {
                    let stats = diff::stats(diff);
                    format!(" [+{}/-{}]", stats.insertions, stats.deletions)
                })
                .unwrap_or_default();
            format!("{path}{counts}")
        }
        "bash" => args
            .get("command")
            .and_then(Value::as_str)
            .and_then(|command| command.lines().next())
            .unwrap_or_default()
            .to_owned(),
        "read" => args
            .get("path")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "grep" => args
            .get("pattern")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "find" => args
            .get("glob")
            .and_then(Value::as_array)
            .map(|globs| {
                globs
                    .iter()
                    .filter_map(Value::as_str)
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default(),
        "todo" => "更新任务清单".to_owned(),
        "ask" => args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "job" | "submit_plan" => String::new(),
        _ => {
            // 未知工具：参数截断展示
            let text = tool.arguments.replace('\n', " ");
            text.chars().take(60).collect()
        }
    }
}

/// read / find 的文件列表树。
fn file_list_rows(args: &Value, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut push = |text: String| {
        rows.push(Line::from(Span::styled(
            format!("  ├─ {text}"),
            Style::new().fg(theme.path),
        )));
    };
    if let Some(path) = args.get("path").and_then(Value::as_str) {
        let offset = args.get("offset").and_then(Value::as_u64);
        let limit = args.get("limit").and_then(Value::as_u64);
        let detail = match (offset, limit) {
            (Some(offset), Some(limit)) => format!("{path}:{offset}-{}", offset + limit),
            (Some(offset), None) => format!("{path}:{offset}-"),
            _ => path.to_owned(),
        };
        push(truncate_to_width(detail, width.saturating_sub(6)));
    }
    if let Some(globs) = args.get("glob").and_then(Value::as_array) {
        for glob in globs.iter().filter_map(Value::as_str) {
            push(truncate_to_width(glob.to_owned(), width.saturating_sub(6)));
        }
    }
    rows
}

/// 结果文本行数截断：头部 + 省略提示。
fn clamp_rows(result: &str, width: usize) -> Vec<String> {
    let width = width.max(8);
    let mut rows: Vec<String> = Vec::new();
    let mut total_rows = 0usize;
    for line in result.lines() {
        total_rows += 1;
        if rows.len() < TOOL_RESULT_TAIL_ROWS {
            rows.extend(wrap_text(line, width));
        }
    }
    if total_rows > TOOL_RESULT_TAIL_ROWS {
        rows.push(format!("⋯ 还有 {} 行", total_rows - TOOL_RESULT_TAIL_ROWS));
    }
    rows
}

/// 截断到显示宽度上限，末尾补省略号。
pub(crate) fn truncate_to_width(text: String, max: usize) -> String {
    if str_width(&text) <= max {
        return text;
    }
    let mut out = String::new();
    let mut used = 0usize;
    for ch in text.chars() {
        let width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if used + width + 1 > max {
            out.push('…');
            return out;
        }
        out.push(ch);
        used += width;
    }
    out
}

/// 整体右移两格（diff 与工具内容的内边距）。
fn indent_line(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled("  ".to_owned(), Style::new())];
    spans.extend(line.spans);
    Line::from(spans)
}

/// Todo 树（omp 风格）：阶段行带完成进度，条目按状态给符号与颜色。
fn render_todo(
    phases: &[moc_protocol::methods::TodoPhase],
    app: &App,
    width: usize,
) -> Vec<Line<'static>> {
    use moc_protocol::methods::TodoStatus;

    let theme = app.theme;
    let mut lines = vec![Line::from(vec![
        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
        Span::styled(
            "◆ 任务".to_owned(),
            Style::new()
                .fg(theme.accent_user)
                .add_modifier(Modifier::BOLD),
        ),
    ])];
    let phase_count = phases.len();
    for (phase_index, phase) in phases.iter().enumerate() {
        let is_last_phase = phase_index + 1 == phase_count;
        let branch = if is_last_phase { "└─ " } else { "├─ " };
        let done = phase
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();
        let total = phase.items.len();
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
            Span::styled(branch.to_owned(), Style::new().fg(theme.border)),
            Span::styled(phase.phase.clone(), Style::new().fg(theme.text_secondary)),
            Span::styled(
                format!(" · {done}/{total}"),
                Style::new().fg(if done == total && total > 0 {
                    theme.accent_success
                } else {
                    theme.gray_dim
                }),
            ),
        ]));
        let item_branch = if is_last_phase { "    " } else { "│   " };
        for item in &phase.items {
            let (marker, color) = match item.status {
                TodoStatus::Completed => ("☑ ", theme.accent_success),
                TodoStatus::InProgress => ("▶ ", theme.accent_running),
                TodoStatus::Pending => ("☐ ", theme.gray),
                TodoStatus::Blocked => ("⚠ ", theme.accent_warning),
                TodoStatus::Abandoned => ("✗ ", theme.gray_dim),
            };
            let mut style = Style::new().fg(color);
            if item.status == TodoStatus::Completed {
                style = style.add_modifier(Modifier::CROSSED_OUT);
            }
            let indent = format!("{item_branch}  ");
            let budget = width.saturating_sub(TITLE_INDENT + indent.len() + 4).max(8);
            for (index, row) in wrap_text(&item.task, budget).into_iter().enumerate() {
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
                        Span::styled(item_branch.to_owned(), Style::new().fg(theme.border)),
                        Span::styled(marker.to_owned(), Style::new().fg(color)),
                        Span::styled(row, style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
                        Span::styled(item_branch.to_owned(), Style::new().fg(theme.border)),
                        Span::styled("    ".to_owned(), Style::new()),
                        Span::styled(row, style),
                    ]));
                }
            }
        }
    }
    lines.push(Line::from(String::new()));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorLevel;

    /// 用户面板：时间戳优先与末行同行，放不下时另起一行，且所有行都不超出面板宽度。
    #[test]
    fn user_panel_keeps_clock_inside_panel_width() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        // 固定 UNIX 毫秒 ⇒ 固定的本地 `HH:MM`
        let at_ms = 1_700_000_000_000;
        let clock = format_clock(at_ms).expect("演示时间戳应可格式化");

        // 短消息：时间戳与正文同行
        let short = user_message("hi", Some(at_ms), true, 40, &theme);
        assert_eq!(short.len(), 4, "面板应为 上边框/正文/下边框 + 尾随留白");
        assert!(short[1].spans.iter().any(|span| span.content == clock));

        // 正文吃满整行：时间戳另起一行，任何行都不超宽
        let narrow = user_message("一二三四五六七八九十", Some(at_ms), true, 24, &theme);
        assert_eq!(narrow.len(), 5, "时间戳应独占一行");
        for line in &narrow {
            assert!(line.width() <= 24, "行宽 {} 超出面板", line.width());
        }
        let clock_rows = narrow
            .iter()
            .filter(|line| line.spans.iter().any(|span| span.content == clock))
            .count();
        assert_eq!(clock_rows, 1, "时间戳应恰好出现一次");
    }
}

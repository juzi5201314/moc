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
use crate::app::App;
use crate::theme::Theme;
use crate::transcript::{Block, Entry, ToolCallBlock};

/// 工具结果在展开态下最多显示的行数（超出折叠为提示）。
const TOOL_RESULT_TAIL_ROWS: usize = 14;

/// 滚动区渲染状态（与 `App::blocks` 长度保持同步）。
pub struct ScrollbackView {
    width: u16,
    cache: Vec<CachedEntry>,
}

struct CachedEntry {
    key: (u64, bool, u64),
    lines: Vec<Line<'static>>,
}

impl Default for ScrollbackView {
    fn default() -> Self {
        Self {
            width: 0,
            cache: Vec::new(),
        }
    }
}

impl ScrollbackView {
    pub fn new() -> Self {
        Self::default()
    }

    /// 同步缓存（按需重渲染失效的块），返回总行数。
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
            let lines = render_entry(entry, app, width);
            let cached = CachedEntry { key, lines };
            if index < self.cache.len() {
                self.cache[index] = cached;
            } else {
                self.cache.push(cached);
            }
        }
        self.cache.truncate(app.blocks.len());
        self.cache.iter().map(|cached| cached.lines.len()).sum()
    }

    /// 从第 `skip_rows` 行开始把内容画进视口，并登记命中表与行文本。
    pub fn paint(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        skip_rows: usize,
        hits: &mut Vec<(usize, u16, u16)>,
        lines: &mut Vec<(u16, String)>,
    ) {
        let mut offset = skip_rows;
        let mut y = area.y;
        let bottom = area.y.saturating_add(area.height);
        for (index, cached) in self.cache.iter().enumerate() {
            if offset >= cached.lines.len() {
                offset -= cached.lines.len();
                continue;
            }
            let row_start = y;
            for line in &cached.lines[offset..] {
                if y >= bottom {
                    break;
                }
                buffer.set_line(area.x, y, line, area.width);
                lines.push((
                    y,
                    line.spans
                        .iter()
                        .map(|span| span.content.to_string())
                        .collect(),
                ));
                y += 1;
            }
            hits.push((index, row_start, y));
            offset = 0;
            if y >= bottom {
                return;
            }
        }
    }
}

/// 单块渲染。折叠状态由全局开关与块类型共同决定。
fn render_entry(entry: &Entry, app: &App, width: u16) -> Vec<Line<'static>> {
    let width = width as usize;
    let theme = app.theme;
    match &entry.block {
        Block::User { text } => {
            let mut lines = Vec::new();
            let rows = wrap_text(text, width.saturating_sub(2));
            for (index, row) in rows.into_iter().enumerate() {
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(
                            "❯ ".to_owned(),
                            Style::new()
                                .fg(theme.accent_user)
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(row, Style::new().fg(theme.accent_user)),
                    ]));
                } else {
                    lines.push(Line::from(Span::styled(
                        format!("  {row}"),
                        Style::new().fg(theme.accent_user),
                    )));
                }
            }
            lines.push(Line::from(String::new()));
            lines
        }
        Block::Assistant { text } => {
            let mut lines = render_markdown(text, width.saturating_sub(2), theme);
            lines.push(Line::from(String::new()));
            lines
        }
        Block::Thinking { text } => {
            if !app.show_thinking || entry.folded {
                return vec![Line::from(Span::styled(
                    format!(
                        "◆ Thinking（{} 字符，{}）",
                        text.chars().count(),
                        if entry.folded {
                            "点击/ctrl+t 展开"
                        } else {
                            "ctrl+t 展开"
                        }
                    ),
                    Style::new().fg(theme.accent_thinking),
                ))];
            }
            let mut lines = render_markdown(text, width.saturating_sub(6), theme)
                .into_iter()
                .map(|line| {
                    let mut spans = vec![Span::styled(
                        "    ".to_owned(),
                        Style::new().fg(theme.accent_thinking),
                    )];
                    spans.extend(line.spans.into_iter().map(|span| {
                        Span::styled(
                            span.content.into_owned(),
                            span.style
                                .fg(theme.accent_thinking)
                                .add_modifier(Modifier::ITALIC),
                        )
                    }));
                    Line::from(spans)
                })
                .collect::<Vec<_>>();
            lines.insert(
                0,
                Line::from(Span::styled(
                    "◆ Thinking".to_owned(),
                    Style::new()
                        .fg(theme.accent_thinking)
                        .add_modifier(Modifier::BOLD),
                )),
            );
            lines.push(Line::from(String::new()));
            lines
        }
        Block::Tool(tool) => render_tool(tool, app, width, entry.folded),
        Block::Todo { phases } => render_todo(phases, app, width),
        Block::Note { text, error } => {
            let color = if *error {
                theme.accent_error
            } else {
                theme.gray_bright
            };
            vec![
                Line::from(Span::styled(
                    wrap_text(text, width)
                        .into_iter()
                        .next()
                        .unwrap_or_default(),
                    Style::new().fg(color),
                )),
                Line::from(String::new()),
            ]
        }
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
            format!("  ⏱ {duration} ms"),
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

fn truncate_to_width(text: String, max: usize) -> String {
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
    let mut lines = vec![Line::from(Span::styled(
        "◆ TODO".to_owned(),
        Style::new()
            .fg(theme.accent_user)
            .add_modifier(Modifier::BOLD),
    ))];
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
            let budget = width.saturating_sub(indent.len() + 4).max(8);
            for (index, row) in wrap_text(&item.task, budget).into_iter().enumerate() {
                if index == 0 {
                    lines.push(Line::from(vec![
                        Span::styled(item_branch.to_owned(), Style::new().fg(theme.border)),
                        Span::styled(marker.to_owned(), Style::new().fg(color)),
                        Span::styled(row, style),
                    ]));
                } else {
                    lines.push(Line::from(vec![
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

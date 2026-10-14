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

/// 接收侧 IRC 卡片折叠时保留的正文行数。
const AGENT_MESSAGE_PREVIEW_ROWS: usize = 3;

/// 滚动区渲染状态（与 `App::blocks` 长度保持同步）。
#[derive(Default)]
pub struct ScrollbackView {
    width: u16,
    cache: Vec<CachedEntry>,
    /// 每帧重算的块内容代次（批次成员取同批最大值），与 `App::blocks` 对齐。
    generations: Vec<u64>,
}

struct CachedEntry {
    key: (u64, bool, u64),
    height: usize,
    lines: Vec<Line<'static>>,
    rendered: bool,
}

/// 绘制窗口：从内容顶部跳过的行数、距尾部的滚动量与最大滚动量。
///
/// 懒渲染会修正块高，因此这两个量连同视口在 [`ScrollbackView::ensure_visible`]
/// 里按**渲染后**的高度重算，绘制与滚动条读的是同一份窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RowWindow {
    /// 绘制起点：从内容顶部跳过的行数。
    pub skip_rows: usize,
    /// 夹紧到新总高后的滚动量（距尾部行数）。
    pub scroll_offset: usize,
    /// 最大滚动量（总高 − 视口）。
    pub max_offset: usize,
}

impl ScrollbackView {
    pub fn new() -> Self {
        Self::default()
    }

    /// 让缓存与 `app.blocks` 对齐（懒块只留估算高度），高度累计由 [`RowWindow`] 负责。
    pub fn sync(&mut self, app: &App, width: u16) {
        const EAGER_BLOCKS: usize = 64;
        if self.width != width {
            self.width = width;
            self.cache.clear();
        }
        let eager_start = app.blocks.len().saturating_sub(EAGER_BLOCKS);
        fill_block_generations(&app.blocks, &mut self.generations);
        for (index, entry) in app.blocks.iter().enumerate() {
            let key = (
                self.generations.get(index).copied().unwrap_or(0),
                entry.folded,
                app.view_gen,
            );
            if self
                .cache
                .get(index)
                .is_some_and(|cached| cached.key == key)
            {
                continue;
            }
            let (lines, height, rendered) = if index >= eager_start {
                let lines = render_entry(index, entry, app, width, index == 0);
                let height = lines.len();
                (lines, height, true)
            } else {
                (
                    Vec::new(),
                    estimate_entry_height_at(app, index, width),
                    false,
                )
            };
            let cached = CachedEntry {
                key,
                height,
                lines,
                rendered,
            };
            if index < self.cache.len() {
                self.cache[index] = cached;
            } else {
                self.cache.push(cached);
            }
        }
        self.cache.truncate(app.blocks.len());
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

    /// 为当前视口懒加载被回收的块，并用渲染后的高度重算行窗口。
    ///
    /// 渲染与绘制必须共用同一份高度累计：估算高度在视口内被修正会同时改变总高与
    /// 起点，若绘制仍按旧累计直接切片，就会切进只存了估算高度的空块。渲染会改写
    /// 块高进而改写起点，故迭代到「窗口内没有可渲染的块」为止；每轮至少渲染一块，
    /// 迭代次数以缓存长度为界。
    pub fn ensure_visible(
        &mut self,
        app: &App,
        viewport: usize,
        scroll_offset: usize,
    ) -> RowWindow {
        let mut window = self.row_window(viewport, scroll_offset);
        for _ in 0..=self.cache.len() {
            let mut rendered = false;
            let end = window.skip_rows.saturating_add(viewport);
            let mut offset = 0usize;
            for (index, cached) in self.cache.iter_mut().enumerate() {
                let next = offset.saturating_add(cached.height);
                if !cached.rendered && offset < end && next > window.skip_rows {
                    let lines = render_entry(index, &app.blocks[index], app, self.width, index == 0);
                    cached.height = lines.len();
                    cached.lines = lines;
                    cached.rendered = true;
                    rendered = true;
                }
                offset = offset.saturating_add(cached.height);
            }
            if !rendered {
                break;
            }
            window = self.row_window(viewport, scroll_offset);
        }
        window
    }

    /// 按当前高度累计把「距尾部的滚动量」翻译成行窗口。
    fn row_window(&self, viewport: usize, scroll_offset: usize) -> RowWindow {
        let max_offset = self.total_height().saturating_sub(viewport);
        let scroll_offset = scroll_offset.min(max_offset);
        RowWindow {
            skip_rows: max_offset - scroll_offset,
            scroll_offset,
            max_offset,
        }
    }

    /// 当前缓存高度累计出的内容总高。
    fn total_height(&self) -> usize {
        self.cache.iter().map(|cached| cached.height).sum()
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

/// 计算块内容代次：批次成员的代次归并到同批最大值。
///
/// 逐块现算要对每个批次成员全表扫描一次（O(块数²)/帧）；这里一遍建表、一遍回填，
/// 结果与逐块计算一致，`generations` 复用调用方的缓冲避免每帧分配。
fn fill_block_generations(blocks: &[Entry], generations: &mut Vec<u64>) {
    use std::collections::HashMap;

    generations.clear();
    generations.extend(blocks.iter().map(|entry| entry.generation));
    let mut batch_max: HashMap<(&str, &str), u64> = HashMap::new();
    for entry in blocks {
        let Some((batch_id, name)) = tool_batch_key(entry) else {
            continue;
        };
        let slot = batch_max.entry((batch_id, name)).or_insert(entry.generation);
        *slot = (*slot).max(entry.generation);
    }
    for (index, entry) in blocks.iter().enumerate() {
        if let Some((batch_id, name)) = tool_batch_key(entry)
            && let Some(generation) = batch_max.get(&(batch_id, name))
        {
            generations[index] = *generation;
        }
    }
}

fn tool_batch_key(entry: &Entry) -> Option<(&str, &str)> {
    match &entry.block {
        Block::Tool(tool) if tool.parent_id.is_none() => {
            Some((tool.batch_id.as_deref()?, tool.name.as_str()))
        }
        _ => None,
    }
}

fn estimate_entry_height_at(app: &App, index: usize, width: u16) -> usize {
    let Some(entry) = app.blocks.get(index) else {
        return 0;
    };
    let Some((batch_id, name)) = tool_batch_key(entry) else {
        return estimate_entry_height(entry, app, width);
    };
    if batch_group_start(app, index, batch_id, name) != index {
        return 0;
    }
    let tools = app
        .blocks
        .iter()
        .filter(|candidate| is_same_batch_tool(candidate, batch_id, name))
        .count();
    estimate_tool_batch_height(tools, index == 0)
}

/// 批次渲染的行数：标题 + 每个工具的摘要行 + 完成统计 + 尾随留白。
///
/// 批次只在组首块渲染，其余成员高度为零；批次忽略展开内容，故不按成员逐个估算。
fn estimate_tool_batch_height(tools: usize, first: bool) -> usize {
    tools
        .saturating_add(3)
        .saturating_add(usize::from(!first))
}

fn is_same_batch_tool(entry: &Entry, batch_id: &str, name: &str) -> bool {
    tool_batch_key(entry).is_some_and(|(candidate_batch, candidate_name)| {
        candidate_batch == batch_id && candidate_name == name
    })
}

fn batch_group_start(app: &App, index: usize, batch_id: &str, name: &str) -> usize {
    app.blocks[..=index]
        .iter()
        .position(|entry| is_same_batch_tool(entry, batch_id, name))
        .expect("当前工具必须存在于批次中")
}

/// 懒块的估算高度：与各渲染分支同结构（标题行 + 内容行 + 尾注 + 尾随留白）。
///
/// 工具结果的截断规则、折叠态与展开态都与渲染共用同一套判定；正文按显示宽度折行
/// 近似 markdown 的落行数。估算与渲染差一个数量级会让滚动范围、滚动条与懒渲染的
/// 可见区间同时失真。
fn estimate_entry_height(entry: &Entry, app: &App, width: u16) -> usize {
    let width = usize::from(width).max(8);
    match &entry.block {
        Block::User { text } | Block::Assistant { text } | Block::Note { text, .. } => {
            wrapped_rows(text, width).saturating_add(2)
        }
        Block::Thinking { text, .. } => {
            if app.show_thinking && !entry.folded {
                wrapped_rows(text, width.saturating_sub(PROSE_INDENT + 2)).saturating_add(2)
            } else {
                2
            }
        }
        Block::Tool(tool) => estimate_tool_height(tool, app, entry.folded, width),
        Block::AgentMessage { body, .. } => {
            // 与 `agent_message_block` 同源：折叠时是预览行数 + 省略行。
            let rows = wrapped_rows(body, width.saturating_sub(TITLE_INDENT + 2));
            let shown = if entry.folded {
                rows.min(AGENT_MESSAGE_PREVIEW_ROWS)
            } else {
                rows
            };
            shown + usize::from(shown < rows) + 2
        }
        Block::Todo { phases } => estimate_todo_height(phases, width),
    }
}

/// 多行文本按显示宽度折行：先按换行分行，再逐行折行（[`wrap_text`] 只折单行）。
fn wrapped_text_rows(text: &str, width: usize) -> Vec<String> {
    text.lines()
        .flat_map(|line| wrap_text(line, width))
        .collect()
}

/// 文本按显示宽度的折行行数（空文本也算一行）。
fn wrapped_rows(text: &str, width: usize) -> usize {
    let width = width.max(8);
    text.lines()
        .map(|line| (str_width(line).saturating_add(width - 1) / width).max(1))
        .sum::<usize>()
        .max(1)
}

/// 工具块高度：折叠态只有标题行；展开态按 [`render_tool`] 的分支规则累计。
fn estimate_tool_height(tool: &ToolCallBlock, app: &App, folded: bool, width: usize) -> usize {
    if app.tools_collapsed || folded {
        return 1;
    }
    let inner_width = width.saturating_sub(4);
    let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
    let mut rows = 1;
    if let Some(diff) = &tool.diff {
        rows += diff::row_count(diff);
    } else if tool.name == "bash" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        rows += wrap_text(command, inner_width.saturating_sub(2)).len();
        rows += tool.result.as_deref().map_or(0, |result| {
            clamped_rows(result, inner_width.saturating_sub(2), false).1
        });
    } else if tool.name == "read" || tool.name == "find" {
        rows += file_list_row_count(&args);
    } else if tool.name == "grep" {
        rows += 1;
        rows += tool.result.as_deref().map_or(0, |result| {
            clamped_rows(result, inner_width.saturating_sub(2), false).1
        });
    } else if tool.name == "message_agent" {
        let message = args
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        rows += clamped_rows(message, inner_width.saturating_sub(2), false).1;
        rows += message_receipts(tool).map_or(0, |receipts| {
            let expanded = receipts.len() > 1
                || receipts.iter().any(|receipt| {
                    receipt.outcome == moc_protocol::methods::AgentMessageOutcome::Failed
                });
            if expanded { receipts.len() } else { 0 }
        });
    } else if tool.name == "wait_agent" {
        rows += match &tool.progress {
            Some(progress) => progress.len(),
            None => wait_result(tool).map_or(0, |result| result.agents.len()),
        };
    } else {
        rows += tool
            .result
            .as_deref()
            .map_or(0, |result| clamped_rows(result, inner_width, false).1);
    }
    rows += usize::from(tool.duration_ms.is_some());
    rows + 1
}

/// Todo 树高度：标题行 + 每阶段一行 + 每条目折行 + 尾随留白。
fn estimate_todo_height(phases: &[moc_protocol::methods::TodoPhase], width: usize) -> usize {
    let phase_count = phases.len();
    let mut rows = 2; // 标题行 + 尾随留白
    for (index, phase) in phases.iter().enumerate() {
        rows += 1;
        // 与 render_todo 的条目缩进同源：末阶段用空白分支，其余用竖线分支
        let branch = if index + 1 == phase_count {
            "    "
        } else {
            "│   "
        };
        let budget = width.saturating_sub(TITLE_INDENT + branch.len() + 2 + 4).max(8);
        rows += phase
            .items
            .iter()
            .map(|item| wrap_text(&item.task, budget).len())
            .sum::<usize>();
    }
    rows
}

/// read / find 文件列表的行数（每条一行，超宽由渲染层省略）。
fn file_list_row_count(args: &Value) -> usize {
    let path = usize::from(args.get("path").and_then(Value::as_str).is_some());
    let globs = args
        .get("glob")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    path.saturating_add(globs)
}

/// 单块渲染。`first` 表示它是转写首块（不额外加前导留白）。
/// 折叠状态由全局开关与块类型共同决定。
fn render_entry(
    index: usize,
    entry: &Entry,
    app: &App,
    width: u16,
    first: bool,
) -> Vec<Line<'static>> {
    if let Some((batch_id, name)) = tool_batch_key(entry) {
        if batch_group_start(app, index, batch_id, name) != index {
            return Vec::new();
        }
        let tools: Vec<_> = app
            .blocks
            .iter()
            .filter_map(|candidate| match &candidate.block {
                Block::Tool(tool) if is_same_batch_tool(candidate, batch_id, name) => Some(tool),
                _ => None,
            })
            .collect();
        return render_tool_batch(app, &tools, usize::from(width), first);
    }
    render_entry_single(entry, app, usize::from(width), first)
}

fn render_entry_single(entry: &Entry, app: &App, width: usize, first: bool) -> Vec<Line<'static>> {
    let theme = app.theme;
    match &entry.block {
        Block::User { text } => user_message(text, entry.at_ms, first, width, theme),
        Block::Assistant { text } => assistant_message(text, entry, width, theme),
        Block::Thinking {
            text,
            reasoning_tokens,
            duration_ms,
        } => thinking_block(text, *reasoning_tokens, *duration_ms, entry, app, width),
        Block::Tool(tool) => render_tool(tool, app, width, entry.folded),
        Block::AgentMessage { meta, body } => {
            agent_message_block(meta, body, width, theme, entry.folded)
        }
        Block::Todo { phases } => render_todo(phases, app, width),
        Block::Note { text, error } => note_block(text, *error, width, theme),
    }
}
/// 同一模型响应中的同名工具合并为一个树形批次。
fn render_tool_batch(
    app: &App,
    tools: &[&ToolCallBlock],
    _width: usize,
    first: bool,
) -> Vec<Line<'static>> {
    let theme = app.theme;
    let name = tools.first().map_or("tool", |tool| tool.name.as_str());
    let completed = tools.iter().filter(|tool| !tool.running).count();
    let mut lines = Vec::new();
    if !first {
        lines.push(Line::from(String::new()));
    }
    lines.push(Line::from(vec![
        Span::styled("• ", Style::new().fg(theme.gray_bright)),
        Span::styled(name.to_owned(), Style::new().fg(theme.accent_tool)),
        Span::styled(
            format!(" ({})", tools.len()),
            Style::new().fg(theme.gray_bright),
        ),
    ]));
    for (offset, tool) in tools.iter().enumerate() {
        let branch = if offset + 1 == tools.len() {
            "└─"
        } else {
            "├─"
        };
        let args = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        lines.push(Line::from(vec![
            Span::raw(format!("  {branch} ")),
            Span::styled(tool_summary(tool, &args), Style::new().fg(theme.path)),
        ]));
    }
    lines.push(Line::from(Span::styled(
        format!("  {completed}/{} 完成", tools.len()),
        Style::new().fg(theme.gray_dim),
    )));
    lines.push(Line::from(String::new()));
    lines
}

/// 用户消息：`>` 前缀、正文与右侧本地时间，不使用额外面板边框。
fn user_message(
    text: &str,
    at_ms: Option<u64>,
    first: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !first {
        lines.push(Line::from(String::new()));
    }

    let prefix = "> ";
    let prefix_width = str_width(prefix);
    let text_width = width.saturating_sub(prefix_width).max(8);
    let rows = wrap_text(text, text_width);
    let row_count = rows.len();
    let clock = at_ms.and_then(format_clock);
    let mut clock_pending = clock.as_ref();

    for (index, row) in rows.into_iter().enumerate() {
        let row_width = str_width(&row);
        let mut spans = Vec::new();
        if index == 0 {
            spans.push(Span::styled(
                prefix.to_owned(),
                Style::new().fg(theme.gray_bright),
            ));
        } else {
            spans.push(Span::styled(" ".repeat(prefix_width), Style::new()));
        }
        spans.push(Span::styled(row, Style::new().fg(theme.text_primary)));

        if index + 1 == row_count
            && let Some(clock) = clock_pending
            && width.saturating_sub(prefix_width + row_width + str_width(clock)) >= 2
        {
            let gap = width - prefix_width - row_width - str_width(clock);
            spans.push(Span::styled(" ".repeat(gap), Style::new()));
            spans.push(Span::styled(clock.clone(), Style::new().fg(theme.gray_dim)));
            clock_pending = None;
        }
        lines.push(Line::from(spans));
    }

    if let Some(clock) = clock_pending {
        let clock_width = str_width(clock);
        lines.push(Line::from(vec![
            Span::styled(" ".repeat(width.saturating_sub(clock_width)), Style::new()),
            Span::styled(clock.clone(), Style::new().fg(theme.gray_dim)),
        ]));
    }

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
    if let Some(footer) = prompt_footer(entry, width, theme) {
        lines.push(footer);
    }
    lines.push(Line::from(String::new()));
    lines
}

/// 助手回复尾注：左侧 `耗时`、右侧本地时间；提示未结束（或耗时未知）时不占行。
fn prompt_footer(entry: &Entry, width: usize, theme: &Theme) -> Option<Line<'static>> {
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

/// 思维链：折叠时显示 `◆ Thought for X.Xs (Yt)`；展开时显示低对比度引文带。
fn thinking_block(
    text: &str,
    reasoning_tokens: Option<u64>,
    duration_ms: Option<u64>,
    entry: &Entry,
    app: &App,
    width: usize,
) -> Vec<Line<'static>> {
    let theme = app.theme;
    let expanded = app.show_thinking && !entry.folded;
    let duration = duration_ms.map_or_else(|| "0.0s".to_owned(), format_duration);
    let mut spans = vec![
        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
        Span::styled("◆ ".to_owned(), Style::new().fg(theme.accent_thinking)),
        Span::styled(
            "思考耗时".to_owned(),
            Style::new()
                .fg(theme.gray_bright)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {duration}"), Style::new().fg(theme.gray_dim)),
    ];
    if let Some(tokens) = reasoning_tokens {
        spans.push(Span::styled(
            format!(" ({tokens}t)"),
            Style::new().fg(theme.gray_dim),
        ));
    }
    spans.push(Span::styled("  ".to_owned(), Style::new()));
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
        for line in render_markdown(text, width.saturating_sub(PROSE_INDENT + 2).max(8), theme) {
            let mut spans = vec![
                Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
                Span::styled("▏ ".to_owned(), Style::new().fg(theme.accent_thinking)),
            ];
            spans.extend(
                line.spans
                    .into_iter()
                    .map(|span| Span::styled(span.content.into_owned(), span.style.fg(theme.gray))),
            );
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(String::new()));
    lines
}

/// 接收侧 IRC 卡片：标题行 + 正文预览（折叠时截断到预览行数）。
fn agent_message_block(
    meta: &moc_protocol::methods::AgentMessageMeta,
    body: &str,
    width: usize,
    theme: &Theme,
    folded: bool,
) -> Vec<Line<'static>> {
    let (word, color) = outcome_label(meta.delivery, theme);
    let mut lines = vec![Line::from(vec![
        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
        Span::styled("◆ ".to_owned(), Style::new().fg(theme.accent_tool)),
        Span::styled(
            "IRC ".to_owned(),
            Style::new()
                .fg(theme.accent_tool)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled("◀ ".to_owned(), Style::new().fg(theme.accent_tool)),
        Span::styled(meta.from_title.clone(), Style::new().fg(theme.text_primary)),
        Span::styled(format!(" · {word}"), Style::new().fg(color)),
    ])];
    let indent = " ".repeat(TITLE_INDENT + 2);
    let rows = wrapped_text_rows(body, width.saturating_sub(TITLE_INDENT + 2));
    let total = rows.len();
    let shown = if folded {
        total.min(AGENT_MESSAGE_PREVIEW_ROWS)
    } else {
        total
    };
    for row in rows.into_iter().take(shown) {
        lines.push(Line::from(vec![
            Span::styled(indent.clone(), Style::new()),
            Span::styled(row, Style::new().fg(theme.text_primary)),
        ]));
    }
    if shown < total {
        lines.push(Line::from(vec![
            Span::styled(indent, Style::new()),
            Span::styled(
                format!("⋯ 还有 {} 行", total - shown),
                Style::new().fg(theme.gray_dim),
            ),
        ]));
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

/// 代理状态的中文标签；TUI 各处共用一套措辞。
pub(crate) fn agent_status_label(status: moc_protocol::methods::AgentStatus) -> &'static str {
    use moc_protocol::methods::AgentStatus;
    match status {
        AgentStatus::Queued => "排队中",
        AgentStatus::Running => "运行中",
        AgentStatus::Completed => "已完成",
        AgentStatus::Stopped => "已停止",
        AgentStatus::Failed => "失败",
        AgentStatus::Idle => "空闲",
    }
}

/// 单条投递结果的措辞。
pub(crate) fn outcome_word(outcome: moc_protocol::methods::AgentMessageOutcome) -> &'static str {
    use moc_protocol::methods::AgentMessageOutcome;
    match outcome {
        AgentMessageOutcome::Injected => "已注入",
        AgentMessageOutcome::Woken => "已唤醒",
        AgentMessageOutcome::Failed => "失败",
    }
}

/// 单条投递结果的措辞与颜色。
fn outcome_label(
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

fn tool_depth(app: &App, tool_id: &str) -> usize {
    let mut depth = 0;
    let mut current = Some(tool_id);
    while let Some(id) = current {
        if depth >= 32 {
            break;
        }
        current = app.blocks.iter().find_map(|entry| match &entry.block {
            Block::Tool(tool) if tool.id == id => tool.parent_id.as_deref(),
            _ => None,
        });
        depth += usize::from(current.is_some());
    }
    depth
}

fn indent_tool_lines(mut lines: Vec<Line<'static>>, depth: usize) -> Vec<Line<'static>> {
    if depth == 0 {
        return lines;
    }
    let prefix = Span::raw(" ".repeat(depth * 2));
    for line in &mut lines {
        line.spans.insert(0, prefix.clone());
    }
    lines
}

/// 工具块：标题行 + 展开内容（按工具名分派）。
fn render_tool(tool: &ToolCallBlock, app: &App, width: usize, folded: bool) -> Vec<Line<'static>> {
    let theme = app.theme;
    let depth = tool_depth(app, &tool.id);
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
    let name = match tool.name.as_str() {
        // 代理间消息沿用 `!! bash` 的自定义标题先例。
        "message_agent" => "IRC",
        _ => match args.get("exclude_from_context").and_then(Value::as_bool) {
            Some(true) => "!! bash · 不进入上下文",
            Some(false) => "! bash",
            None => &tool.name,
        },
    };
    lines.push(Line::from(vec![
        Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
        Span::styled(format!("{bullet} "), Style::new().fg(bullet_color)),
        Span::styled(
            name.to_owned(),
            Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
        ),
        Span::styled(format!(" {summary}"), Style::new().fg(theme.text_secondary)),
    ]));

    if app.tools_collapsed || folded {
        return indent_tool_lines(lines, depth);
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
    } else if tool.name == "message_agent" {
        lines.extend(agent_message_tool_rows(tool, &args, inner_width, theme));
    } else if tool.name == "wait_agent" {
        lines.extend(wait_agent_tool_rows(tool, inner_width, theme));
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
    indent_tool_lines(lines, depth)
}

/// 解析 `message_agent` 的投递回执。
fn message_receipts(tool: &ToolCallBlock) -> Option<Vec<moc_protocol::methods::AgentMessageReceipt>> {
    let result = tool.result.as_deref()?;
    serde_json::from_str::<moc_protocol::methods::AgentMessageResult>(result)
        .ok()
        .map(|result| result.receipts)
}

/// 解析 `wait_agent` 的等待结果。
fn wait_result(tool: &ToolCallBlock) -> Option<moc_protocol::methods::AgentWaitResult> {
    let result = tool.result.as_deref()?;
    serde_json::from_str::<moc_protocol::methods::AgentWaitResult>(result).ok()
}

/// 发送侧 IRC 卡片正文：消息预览 + 收件人明细。
///
/// 明细只在「广播或有失败」时逐行展开：单目标成功时标题行的回执词已经说清结果，
/// 再列一遍只是重复。
fn agent_message_tool_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    let message = args.get("message").and_then(Value::as_str).unwrap_or_default();
    for row in clamp_rows(message, width.saturating_sub(2)) {
        lines.push(Line::from(Span::styled(
            format!("  {row}"),
            Style::new().fg(theme.text_primary),
        )));
    }
    let Some(receipts) = message_receipts(tool) else {
        return lines;
    };
    let expanded = receipts.len() > 1
        || receipts
            .iter()
            .any(|receipt| receipt.outcome == moc_protocol::methods::AgentMessageOutcome::Failed);
    if !expanded {
        return lines;
    }
    for receipt in receipts {
        let (word, color) = outcome_label(receipt.outcome, theme);
        let mut spans = vec![
            Span::styled("  ".to_owned(), Style::new()),
            Span::styled(format!("{} ", receipt.title), Style::new().fg(theme.text_secondary)),
            Span::styled(word.to_owned(), Style::new().fg(color)),
        ];
        if let Some(error) = receipt.error {
            spans.push(Span::styled(format!("：{error}"), Style::new().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// 等待卡片的行：优先实时进度，工具结束后回落到结果快照。
fn wait_agent_tool_rows(tool: &ToolCallBlock, _width: usize, theme: &Theme) -> Vec<Line<'static>> {
    use moc_protocol::methods::AgentStatus;
    let rows: Vec<(String, AgentStatus, u64)> = match &tool.progress {
        Some(progress) => progress
            .iter()
            .map(|row| (row.title.clone(), row.status, row.elapsed_ms))
            .collect(),
        None => wait_result(tool).map_or_else(Vec::new, |result| {
            result
                .agents
                .into_iter()
                .map(|entry| (entry.title, entry.status, 0))
                .collect()
        }),
    };
    if rows.is_empty() {
        return Vec::new();
    }
    let mut lines = Vec::with_capacity(rows.len());
    for (title, status, elapsed_ms) in rows {
        let (glyph, color) = match status {
            AgentStatus::Queued | AgentStatus::Running => ("▸", theme.accent_running),
            AgentStatus::Completed => ("◆", theme.accent_tool),
            AgentStatus::Stopped => ("◼", theme.gray_dim),
            AgentStatus::Failed => ("✘", theme.accent_error),
            AgentStatus::Idle => ("○", theme.gray_dim),
        };
        let mut spans = vec![
            Span::styled(format!("  {glyph} "), Style::new().fg(color)),
            Span::styled(title, Style::new().fg(theme.text_primary)),
            Span::styled(
                format!(" · {}", agent_status_label(status)),
                Style::new().fg(color),
            ),
        ];
        if elapsed_ms > 0 {
            spans.push(Span::styled(
                format!(" · {}", format_duration(elapsed_ms)),
                Style::new().fg(theme.gray_dim),
            ));
        }
        lines.push(Line::from(spans));
    }
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
        "message_agent" => {
            use moc_protocol::methods::AgentMessageOutcome;
            let target = args.get("target").and_then(Value::as_str).unwrap_or_default();
            let mut summary = format!("▶ {target}");
            if let Some(receipts) = message_receipts(tool) {
                if target == "all" {
                    summary.push_str(" · 广播");
                }
                let failed = receipts
                    .iter()
                    .filter(|receipt| receipt.outcome == AgentMessageOutcome::Failed)
                    .count();
                match receipts.as_slice() {
                    [receipt] => {
                        summary.push_str(&format!(" · {}", outcome_word(receipt.outcome)))
                    }
                    receipts => summary.push_str(&format!(" · {} 已送达", receipts.len() - failed)),
                }
                if failed > 0 {
                    summary.push_str(&format!(" · {failed} 失败"));
                }
            }
            summary
        }
        "wait_agent" => {
            let mut summary = match args.get("agent_ids").and_then(Value::as_array) {
                Some(ids) if !ids.is_empty() => format!("等待 {} 个代理", ids.len()),
                _ => "等待全部运行中代理".to_owned(),
            };
            if let Some(result) = wait_result(tool) {
                if result.messages_pending {
                    summary.push_str(" · 有新消息");
                }
                if result.timed_out {
                    summary.push_str(" · 超时");
                }
            }
            summary
        }
        "ask" => args
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        "job" | "write_plan" | "exit_plan" => String::new(),
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

/// 结果文本的展示行（头部 [`TOOL_RESULT_TAIL_ROWS`] 行 + 省略提示）。
fn clamp_rows(result: &str, width: usize) -> Vec<String> {
    clamped_rows(result, width, true).0
}

/// 结果文本的展示行与行数，估算与渲染共用同一条截断规则。
///
/// `collect_rows` 为假时只数行数、不保留行文本：懒渲染的高度估算必须与真实渲染
/// 得出同一个数字，否则滚动范围与可见区间都会失真。
fn clamped_rows(result: &str, width: usize, collect_rows: bool) -> (Vec<String>, usize) {
    let width = width.max(8);
    let mut rows: Vec<String> = Vec::new();
    let mut count = 0usize;
    let mut total_lines = 0usize;
    for line in result.lines() {
        total_lines += 1;
        if count < TOOL_RESULT_TAIL_ROWS {
            for row in wrap_text(line, width) {
                count += 1;
                if collect_rows {
                    rows.push(row);
                }
            }
        }
    }
    if total_lines > TOOL_RESULT_TAIL_ROWS {
        count += 1;
        if collect_rows {
            rows.push(format!("⋯ 还有 {} 行", total_lines - TOOL_RESULT_TAIL_ROWS));
        }
    }
    (rows, count)
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

    /// 用户消息：使用 `>` 前缀，短消息时间戳右对齐，窄宽度时另起一行。
    #[test]
    fn user_message_aligns_clock_without_panel_borders() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let at_ms = 1_700_000_000_000;
        let clock = format_clock(at_ms).expect("演示时间戳应可格式化");

        let short = user_message("hi", Some(at_ms), true, 40, &theme);
        assert_eq!(short.len(), 2, "短消息应只有正文和尾随留白");
        assert!(short[0].spans.iter().any(|span| span.content == "> "));
        assert!(short[0].spans.iter().any(|span| span.content == clock));
        assert!(!short[0].spans.iter().any(|span| span.content.contains('╭')));

        let narrow = user_message("一二三四五六七八九十", Some(at_ms), true, 24, &theme);
        assert_eq!(narrow.len(), 3, "时间戳应在正文后另起一行");
        for line in &narrow {
            assert!(line.width() <= 24, "行宽 {} 超出消息区域", line.width());
        }
        let clock_rows = narrow
            .iter()
            .filter(|line| line.spans.iter().any(|span| span.content == clock))
            .count();
        assert_eq!(clock_rows, 1, "时间戳应恰好出现一次");
    }
    #[test]
    fn groups_same_name_tools_from_one_batch() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.start_batched_tool_block("call_a".into(), "read".into(), "batch".into());
        app.start_batched_tool_block("call_b".into(), "read".into(), "batch".into());
        app.tool_entry_mut("call_a")
            .expect("第一个工具块")
            .push_args(r#"{"path":"a.rs"}"#);
        app.tool_entry_mut("call_b")
            .expect("第二个工具块")
            .push_args(r#"{"path":"b.rs"}"#);

        let lines = render_entry(0, &app.blocks[0], &app, 80, true);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("read (2)"));
        assert!(text.contains("a.rs"));
        assert!(text.contains("b.rs"));
        assert_eq!(render_entry(1, &app.blocks[1], &app, 80, false).len(), 0);
    }

    /// C6：懒渲染与绘制共用同一份渲染后高度累计。
    ///
    /// 估算高度只影响滚动范围，随时会被真实渲染修正（工具结果按全文估高、渲染却
    /// 截断到 [`TOOL_RESULT_TAIL_ROWS`] 行）。若绘制仍按渲染前的累计取起点，向上滚进
    /// 懒区时就会切进只存估算高度的空块。用例同时钉住契约：视口覆盖的块必须已渲染。
    #[test]
    fn lazy_paint_never_slices_unrendered_blocks() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let result = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        // 70 个工具块：只有末尾 64 块进 eager 渲染，前面几块只留估算高度。
        for index in 0..70 {
            let id = format!("call_{index}");
            app.start_tool_block(id.clone(), "bash".into());
            app.tool_entry_mut(&id)
                .expect("工具块存在")
                .push_args(r#"{"command":"run"}"#);
            app.finish_tool_block(
                id.clone(),
                ToolStatus::Completed,
                ToolResult {
                    tool_call_id: id,
                    content: result.clone(),
                    is_error: false,
                    images: Vec::new(),
                },
                Some(1),
                None,
            );
        }

        let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("首帧绘制成功");
        let viewport = usize::from(app.scrollback_area.height);
        assert!(viewport > 0, "测试终端应有转写区");

        // 向上滚进懒区：绘制起点落在首个懒块的估算高度范围内。
        let estimate = view.cache[0].height;
        app.scroll_offset = app.scroll_max_offset.saturating_sub(estimate / 2);
        app.selection = Some(((0, 0), (0, 0)));
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("懒区绘制不得越界切片");

        let skip_rows = app.scroll_max_offset - app.scroll_offset;
        let end = skip_rows.saturating_add(viewport);
        let mut offset = 0usize;
        let mut covered = 0usize;
        for cached in &view.cache {
            let next = offset.saturating_add(cached.height);
            if next > skip_rows && offset < end {
                assert!(cached.rendered, "视口覆盖的块必须已渲染");
                assert_eq!(cached.lines.len(), cached.height, "渲染高度必须等于行数");
                covered += 1;
            }
            offset = next;
        }
        assert!(covered > 0, "视口应落在至少一个块上");
    }

    /// M25：懒块的估算高度与渲染同规则。
    ///
    /// 估算按全文计高（工具结果 2000 行即 2000 行、折叠态也按展开算）时，滚动范围、
    /// 滚动条与懒渲染的可见区间都失真一个数量级。
    #[test]
    fn estimates_follow_render_rules() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus, ToolStatus};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let long_result = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");

        app.start_tool_block("call_bash".into(), "bash".into());
        app.start_batched_tool_block("call_a".into(), "read".into(), "batch".into());
        app.start_batched_tool_block("call_b".into(), "read".into(), "batch".into());
        app.tool_entry_mut("call_bash")
            .expect("工具块")
            .push_args(r#"{"command":"print lines"}"#);
        app.finish_tool_block(
            "call_bash".into(),
            ToolStatus::Completed,
            ToolResult {
                tool_call_id: "call_bash".into(),
                content: long_result,
                is_error: false,
                images: Vec::new(),
            },
            Some(42),
            None,
        );
        app.push_block(Block::Todo {
            phases: vec![TodoPhase {
                phase: "阶段".into(),
                items: vec![TodoItem {
                    status: TodoStatus::Pending,
                    task: "很长的任务描述".repeat(12),
                    reason: None,
                }],
            }],
        });

        let rendered = |app: &App, index: usize| {
            render_entry(index, &app.blocks[index], app, width, index == 0).len()
        };
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, width),
                rendered(&app, index),
                "第 {index} 块的估算应与渲染一致"
            );
        }

        let bash = estimate_entry_height_at(&app, 0, width);
        assert!(bash < 30, "200 行结果不得按全文估算：{bash}");

        // 折叠态：全局折叠与单块折叠都只剩标题行
        app.tools_collapsed = true;
        assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
        assert_eq!(estimate_entry_height_at(&app, 0, width), 1);
        app.tools_collapsed = false;
        app.blocks[0].toggle_fold();
        assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
    }

    /// 小2：批次代次一遍算清——每个成员都取同批最大代次，非批次块保留自身代次。
    ///
    /// 逐块现算时每个批次成员都要全表扫描一次（O(块数²)/帧）；归并结果必须与它一致，
    /// 否则批次里任一成员变化都可能漏掉刷新。
    #[test]
    fn batch_members_share_one_generation_scan() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.start_batched_tool_block("call_a".into(), "read".into(), "batch".into());
        app.start_batched_tool_block("call_b".into(), "read".into(), "batch".into());
        app.start_batched_tool_block("call_c".into(), "read".into(), "batch".into());
        app.push_note("独立块".into(), false);
        // 只有第二个成员继续变化：同批成员的代次都应是这一批的最大值。
        app.tool_entry_mut("call_b")
            .expect("批次成员")
            .touch();
        let latest = app.blocks[1].generation;
        assert!(latest > app.blocks[0].generation, "成员各自推进");

        let mut generations = Vec::new();
        fill_block_generations(&app.blocks, &mut generations);
        assert_eq!(generations.len(), app.blocks.len());
        assert_eq!(
            &generations[..3],
            &[latest, latest, latest],
            "同批成员共用最大代次"
        );
        assert_eq!(
            generations[3],
            app.blocks[3].generation,
            "非批次块保留自身代次"
        );

        // 缓存 key 用的就是这份代次
        let mut view = ScrollbackView::new();
        view.sync(&app, 80);
        assert_eq!(view.cache[0].key, (latest, app.blocks[0].folded, app.view_gen));
        assert_eq!(view.cache[1].key.0, latest);
    }

    /// 接收侧 IRC 卡片：标题措辞、折叠预览与估高同源。
    #[test]
    fn agent_message_card_renders_irc_header_and_preview() {
        use moc_protocol::methods::{AgentMessageMeta, AgentMessageOutcome};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 60u16;
        let body = (0..6)
            .map(|index| format!("第 {index} 行消息"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_block(Block::AgentMessage {
            meta: AgentMessageMeta {
                from_id: "ses_child".into(),
                from_title: "探查者".into(),
                delivery: AgentMessageOutcome::Injected,
            },
            body,
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.blocks[0].toggle_fold();
        let folded = render_entry(0, &app.blocks[0], &app, width, true);
        let text = text_of(&folded);
        assert!(text.contains("IRC ◀ 探查者"), "标题必须标明来源：{text}");
        assert!(text.contains("已注入"), "标题必须标出投递结果：{text}");
        assert!(text.contains("⋯ 还有 3 行"), "折叠只留预览行：{text}");
        assert!(text.contains("第 0 行消息") && text.contains("第 2 行消息"));
        assert!(!text.contains("第 3 行消息"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            folded.len(),
            "折叠态估算与渲染同源"
        );

        app.blocks[0].toggle_fold();
        let expanded = render_entry(0, &app.blocks[0], &app, width, true);
        let text = text_of(&expanded);
        assert!(text.contains("第 5 行消息"));
        assert!(!text.contains("还有"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            expanded.len(),
            "展开态估算与渲染同源"
        );
    }

    /// 发送侧 IRC 卡片：广播回执逐行展开，摘要给出总量。
    #[test]
    fn message_agent_card_lists_receipt_rows() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 60u16;
        app.start_tool_block("call_msg".into(), "message_agent".into());
        app.tool_entry_mut("call_msg")
            .expect("工具块")
            .push_args(r#"{"target":"all","message":"请复查一下改动"}"#);
        app.finish_tool_block(
            "call_msg".into(),
            ToolStatus::Completed,
            ToolResult {
                tool_call_id: "call_msg".into(),
                content: serde_json::json!({
                    "receipts": [
                        {"agent_id": "ses_a", "title": "甲", "outcome": "injected"},
                        {"agent_id": "ses_b", "title": "乙", "outcome": "failed", "error": "收件箱已满"}
                    ]
                })
                .to_string(),
                is_error: true,
                images: Vec::new(),
            },
            Some(3),
            None,
        );
        let lines = render_entry(0, &app.blocks[0], &app, width, true);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("IRC"), "工具卡标题必须显示 IRC：{text}");
        assert!(text.contains("▶ all"));
        assert!(text.contains("广播"));
        assert!(text.contains("1 已送达") && text.contains("1 失败"));
        assert!(text.contains("请复查一下改动"), "正文必须带消息预览：{text}");
        assert!(text.contains("甲 已注入"));
        assert!(text.contains("乙 失败：收件箱已满"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    /// 等待卡片：运行中显示进度行与耗时，结束后改用结果快照。
    #[test]
    fn wait_agent_renders_progress_rows() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::{AgentStatus, ToolProgressRow, ToolStatus};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 60u16;
        app.start_tool_block("call_wait".into(), "wait_agent".into());
        app.tool_entry_mut("call_wait")
            .expect("工具块")
            .push_args(r#"{"agent_ids":["ses_a","ses_b"]}"#);
        app.set_tool_progress(
            "call_wait",
            vec![
                ToolProgressRow {
                    agent_id: "ses_a".into(),
                    title: "甲".into(),
                    status: AgentStatus::Running,
                    elapsed_ms: 3_200,
                },
                ToolProgressRow {
                    agent_id: "ses_b".into(),
                    title: "乙".into(),
                    status: AgentStatus::Completed,
                    elapsed_ms: 0,
                },
            ],
        );
        let lines = render_entry(0, &app.blocks[0], &app, width, true);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("等待 2 个代理"));
        assert!(text.contains("▸ 甲 · 运行中 · 3.2s"), "运行行带耗时：{text}");
        assert!(text.contains("◆ 乙 · 已完成"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            lines.len(),
            "进度态估算与渲染同源"
        );

        app.finish_tool_block(
            "call_wait".into(),
            ToolStatus::Completed,
            ToolResult {
                tool_call_id: "call_wait".into(),
                content: serde_json::json!({
                    "agents": [
                        {"id": "ses_a", "title": "甲", "status": "completed"},
                        {"id": "ses_b", "title": "乙", "status": "running"}
                    ],
                    "settled": true,
                    "messages_pending": false,
                    "timed_out": false
                })
                .to_string(),
                is_error: false,
                images: Vec::new(),
            },
            Some(1_100),
            None,
        );
        let lines = render_entry(0, &app.blocks[0], &app, width, true);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("◆ 甲 · 已完成"), "结果行回落到快照：{text}");
        assert!(text.contains("▸ 乙 · 运行中"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            lines.len(),
            "结果态估算与渲染同源"
        );
    }
}

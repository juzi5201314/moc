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
use crate::transcript::{Block, DetailLevel, Entry, ToolCallBlock};

/// 简略档的思考预览行数（取正文末尾若干行）。
pub(crate) const BRIEF_THINKING_ROWS: usize = 4;

/// 简略档的工具正文行数上限（超出补一行省略提示）。
const BRIEF_BODY_ROWS: usize = 6;

/// 完整档的正文行数上限：等同于「不截断」（core 侧输出预算与
/// [`diff::MAX_RENDERED_ROWS`] 已先行收窄），只用来把不限变成有限的算术上界，
/// 避免预算参与加法溢出。
const FULL_BODY_ROWS: usize = 4001;

// 完整档的正文上限必须覆盖 diff 自身的渲染上限，否则 diff 会在估算与渲染里
// 走不同的收窄口径。
const _: () = assert!(FULL_BODY_ROWS >= diff::MAX_RENDERED_ROWS);

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
    key: (u64, u64),
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
                    let lines =
                        render_entry(index, &app.blocks[index], app, self.width, index == 0);
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
        let slot = batch_max
            .entry((batch_id, name))
            .or_insert(entry.generation);
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
    let level = entry.level(app.block_level(&entry.block));
    let Some((batch_id, name)) = tool_batch_key(entry) else {
        return estimate_entry_height(entry, level, width);
    };
    if batch_group_start(app, index, batch_id, name) != index {
        return 0;
    }
    let tools: Vec<&ToolCallBlock> = app
        .blocks
        .iter()
        .filter_map(|candidate| match &candidate.block {
            Block::Tool(tool) if is_same_batch_tool(candidate, batch_id, name) => Some(tool),
            _ => None,
        })
        .collect();
    estimate_tool_batch_height(&tools, index == 0, level, usize::from(width))
}

/// 批次渲染的行数：标题 + 每个工具的（摘要行 + 正文行）+ 完成统计 + 尾随留白。
///
/// 批次只在组首块渲染，其余成员高度为零。
fn estimate_tool_batch_height(
    tools: &[&ToolCallBlock],
    first: bool,
    level: DetailLevel,
    width: usize,
) -> usize {
    let mut rows = tools.len() + 3 + usize::from(!first);
    if level == DetailLevel::Hidden {
        return rows;
    }
    let inner_width = width.saturating_sub(BATCH_BODY_INDENT);
    let limit = brief_limit(level);
    for tool in tools {
        if !tool.has_body() {
            continue;
        }
        let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        rows += tool_body_rows(tool, &args, inner_width, limit);
    }
    rows
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
fn estimate_entry_height(entry: &Entry, level: DetailLevel, width: u16) -> usize {
    let width = usize::from(width).max(8);
    match &entry.block {
        Block::User { text } | Block::Assistant { text } | Block::Note { text, .. } => {
            wrapped_rows(text, width).saturating_add(2)
        }
        Block::Thinking { text, .. } => {
            let rows = wrapped_rows(text, width.saturating_sub(PROSE_INDENT + 2));
            match level {
                DetailLevel::Hidden => 2,
                DetailLevel::Brief => rows.min(BRIEF_THINKING_ROWS).saturating_add(2),
                DetailLevel::Full => rows.saturating_add(2),
            }
        }
        Block::Tool(tool) => estimate_tool_height(tool, level, width),
        Block::AgentMessage { body, .. } => {
            // 与 `agent_message_block` 同源：简略档是预览行数 + 省略行，隐藏档只剩标题行。
            let rows = wrapped_rows(body, width.saturating_sub(TITLE_INDENT + 2));
            let (shown, omitted) = match level {
                DetailLevel::Hidden => (0, false),
                DetailLevel::Brief => (
                    rows.min(AGENT_MESSAGE_PREVIEW_ROWS),
                    rows > AGENT_MESSAGE_PREVIEW_ROWS,
                ),
                DetailLevel::Full => (rows, false),
            };
            shown + usize::from(omitted) + 2
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

/// `message_agent` 的正文行数：消息预览行 + 展开时的逐条回执行，
/// 与 [`message_agent_tool_rows`] 消耗同一份预算。
fn message_agent_rows_count(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    limit: usize,
) -> (usize, usize) {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let clamped = clamp_rows(message, width.saturating_sub(2), limit, BodyEdge::Head);
    let mut rows = clamped.shown;
    let mut dropped = clamped.dropped;
    let Some(receipts) = message_receipts(tool) else {
        return (rows, dropped);
    };
    let expanded = receipts.len() > 1
        || receipts
            .iter()
            .any(|receipt| receipt.outcome == moc_protocol::methods::AgentMessageOutcome::Failed);
    if expanded {
        for _ in receipts {
            if rows >= limit {
                dropped += 1;
            } else {
                rows += 1;
            }
        }
    }
    (rows, dropped)
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
        let budget = width
            .saturating_sub(TITLE_INDENT + branch.len() + 2 + 4)
            .max(8);
        rows += phase
            .items
            .iter()
            .map(|item| wrap_text(&item.task, budget).len())
            .sum::<usize>();
    }
    rows
}

/// 工具块高度：无正文工具有一行标题；有正文工具是
/// 标题 +（隐藏档无正文）+（耗时尾注）+ 尾随留白，与 [`render_tool`] 同规则。
fn estimate_tool_height(tool: &ToolCallBlock, level: DetailLevel, width: usize) -> usize {
    if !tool.has_body() {
        return 1;
    }
    let body = if level == DetailLevel::Hidden {
        0
    } else {
        let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        tool_body_rows(tool, &args, width, brief_limit(level))
    };
    body.saturating_add(1)
        .saturating_add(usize::from(tool.duration_ms.is_some()))
        .saturating_add(1)
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
    let level = entry.level(app.block_level(&entry.block));
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
        return render_tool_batch(app, &tools, usize::from(width), first, level);
    }
    render_entry_single(entry, app, usize::from(width), first, level)
}

fn render_entry_single(
    entry: &Entry,
    app: &App,
    width: usize,
    first: bool,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let theme = app.theme;
    match &entry.block {
        Block::User { text } => user_message(text, entry.at_ms, first, width, theme),
        Block::Assistant { text } => assistant_message(text, entry, width, theme),
        Block::Thinking {
            text,
            reasoning_tokens,
            duration_ms,
        } => thinking_block(text, *reasoning_tokens, *duration_ms, app, width, level),
        Block::Tool(tool) => render_tool(tool, app, width, level),
        Block::AgentMessage { meta, body } => agent_message_block(meta, body, width, theme, level),
        Block::Todo { phases } => render_todo(phases, app, width),
        Block::Note { text, error } => note_block(text, *error, width, theme),
    }
}
/// 批次成员正文相对批次标题行的缩进（`├─ ` 分支宽度）。
const BATCH_BODY_INDENT: usize = TITLE_INDENT + 4;

/// 同一模型响应中的同名工具合并为一个树形批次。
///
/// 档位取组首块的层档位（组内其余成员不单独渲染标题行，不参与档位判定）；
/// `Hidden` 时只留摘要行。
fn render_tool_batch(
    app: &App,
    tools: &[&ToolCallBlock],
    width: usize,
    first: bool,
    level: DetailLevel,
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
    let limit = brief_limit(level);
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
        if level == DetailLevel::Hidden {
            continue;
        }
        let indent = Span::raw(" ".repeat(BATCH_BODY_INDENT));
        for line in tool_body(
            tool,
            &args,
            app,
            width.saturating_sub(BATCH_BODY_INDENT),
            limit,
        ) {
            let mut spans = vec![indent.clone()];
            spans.extend(line.spans);
            lines.push(Line::from(spans));
        }
    }
    lines.push(Line::from(Span::styled(
        format!("  {completed}/{} 完成", tools.len()),
        Style::new().fg(theme.gray_dim),
    )));
    lines.push(Line::from(String::new()));
    lines
}

/// 该档位的正文行上限（隐藏档没有正文）。
fn brief_limit(level: DetailLevel) -> usize {
    match level {
        DetailLevel::Hidden => 0,
        DetailLevel::Brief => BRIEF_BODY_ROWS,
        DetailLevel::Full => FULL_BODY_ROWS,
    }
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

/// 思维链：隐藏档只留 `◆ 思考耗时 X.Xs (Yt)`；简略档预览正文末尾若干行；
/// 完整档显示全部正文（低对比度引文带）。
fn thinking_block(
    text: &str,
    reasoning_tokens: Option<u64>,
    duration_ms: Option<u64>,
    app: &App,
    width: usize,
    level: DetailLevel,
) -> Vec<Line<'static>> {
    let theme = app.theme;
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
    if level == DetailLevel::Hidden {
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
    if level != DetailLevel::Hidden {
        let mut rows = thinking_rows(text, width, theme);
        if level == DetailLevel::Brief && rows.len() > BRIEF_THINKING_ROWS {
            rows.drain(..rows.len() - BRIEF_THINKING_ROWS);
        }
        lines.extend(rows);
    }
    lines.push(Line::from(String::new()));
    lines
}

/// 思考正文的引文带行（markdown 渲染后逐行加低对比度前缀）。
fn thinking_rows(text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    render_markdown(text, width.saturating_sub(PROSE_INDENT + 2).max(8), theme)
        .into_iter()
        .map(|line| {
            let mut spans = vec![
                Span::styled(" ".repeat(TITLE_INDENT), Style::new()),
                Span::styled("▏ ".to_owned(), Style::new().fg(theme.accent_thinking)),
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
fn agent_message_block(
    meta: &moc_protocol::methods::AgentMessageMeta,
    body: &str,
    width: usize,
    theme: &Theme,
    level: DetailLevel,
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
    let shown = match level {
        DetailLevel::Hidden => 0,
        DetailLevel::Brief => total.min(AGENT_MESSAGE_PREVIEW_ROWS),
        DetailLevel::Full => total,
    };
    for row in rows.into_iter().take(shown) {
        lines.push(Line::from(vec![
            Span::styled(indent.clone(), Style::new()),
            Span::styled(row, Style::new().fg(theme.text_primary)),
        ]));
    }
    if shown < total && level != DetailLevel::Hidden {
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

/// 工具块：标题行 + 正文（隐藏档或无正文工具只留标题行）。
fn render_tool(
    tool: &ToolCallBlock,
    app: &App,
    width: usize,
    level: DetailLevel,
) -> Vec<Line<'static>> {
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

    if !tool.has_body() {
        return indent_tool_lines(lines, depth);
    }
    if level != DetailLevel::Hidden {
        lines.extend(tool_body(tool, &args, app, width, brief_limit(level)));
    }
    // 尾注：耗时；尾随留白（失败原因由正文承载）
    if let Some(duration) = tool.duration_ms {
        lines.push(Line::from(Span::styled(
            format!("  ⏱ {}", format_duration(duration)),
            Style::new().fg(theme.gray_dim),
        )));
    }
    lines.push(Line::from(String::new()));
    indent_tool_lines(lines, depth)
}

/// 工具正文行（不含标题、耗时尾注与留白，无缩进）。
///
/// `limit` 是**内容行**上限，由 [`brief_limit`] 按档位给出；被略去的内容折算成
/// 一行 `⋯ 还有 N 行` 追在最后（`N` 按各分支的原生度量：结果文本按源行、
/// diff 按 diff 行）。`bash`／`grep` 的结果取**尾部**（最新输出），命令行走首行。
fn tool_body(
    tool: &ToolCallBlock,
    args: &Value,
    app: &App,
    width: usize,
    limit: usize,
) -> Vec<Line<'static>> {
    let theme = app.theme;
    let inner_width = width.saturating_sub(4);
    let is_error = tool
        .status
        .is_some_and(|status| status != moc_protocol::methods::ToolStatus::Completed);
    let mut lines = Vec::new();
    let mut dropped = 0usize;

    if let Some(diff) = &tool.diff {
        let rendered = diff::rendered_row_count(diff, limit);
        let mut body = diff::render_diff(diff, inner_width, theme, limit);
        dropped += diff::row_count(diff).saturating_sub(rendered);
        lines.extend(body.drain(..).map(indent_line));
    } else if tool.name == "bash" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (shown_command, command_dropped) = head_lines(command, limit);
        for (index, row) in wrap_text(&shown_command, inner_width.saturating_sub(2))
            .into_iter()
            .enumerate()
        {
            lines.push(Line::from(Span::styled(
                format!("{}{}", if index == 0 { "  $ " } else { "    " }, row),
                Style::new().fg(theme.command),
            )));
        }
        dropped += command_dropped;
        if let Some(result) = &tool.result {
            let budget = limit.saturating_sub(lines.len());
            let rows = clamp_rows(
                result,
                inner_width.saturating_sub(2),
                budget,
                BodyEdge::Tail,
            );
            dropped += rows.dropped;
            lines.extend(rows.rows.into_iter().map(|text| {
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
    } else if tool.name == "grep" {
        if let Some(result) = &tool.result {
            let rows = clamp_rows(result, inner_width.saturating_sub(2), limit, BodyEdge::Tail);
            dropped += rows.dropped;
            lines.extend(rows.rows.into_iter().map(|text| {
                Line::from(Span::styled(
                    format!("  {text}"),
                    Style::new().fg(theme.text_secondary),
                ))
            }));
        }
    } else if tool.name == "message_agent" {
        let (rows, omitted) = message_agent_tool_rows(tool, args, inner_width, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "wait_agent" {
        let (rows, omitted) = wait_agent_tool_rows(tool, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if let Some(result) = &tool.result {
        let rows = clamp_rows(result, inner_width, limit, BodyEdge::Head);
        dropped += rows.dropped;
        lines.extend(rows.rows.into_iter().map(|text| {
            Line::from(Span::styled(
                format!("  {text}"),
                Style::new().fg(theme.text_secondary),
            ))
        }));
    }

    // 省略提示统一由此处追加；diff 分支由 `render_diff` 自己产出（只有它知道
    // diff 的总行数），因此不叠加第二行。
    if dropped > 0 && tool.diff.is_none() {
        lines.push(Line::from(Span::styled(
            format!("  ⋯ 还有 {dropped} 行"),
            Style::new().fg(theme.gray_dim),
        )));
    }
    lines
}

/// 工具正文的展示行数：与 [`tool_body`] 同规则（含省略提示行），懒渲染估高用。
fn tool_body_rows(tool: &ToolCallBlock, args: &Value, width: usize, limit: usize) -> usize {
    let inner_width = width.saturating_sub(4);
    let mut rows = 0usize;
    let mut dropped = 0usize;

    if let Some(diff) = &tool.diff {
        let rendered = diff::rendered_row_count(diff, limit);
        rows += rendered;
        dropped += diff::row_count(diff).saturating_sub(rendered);
    } else if tool.name == "bash" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let (shown_command, command_dropped) = head_lines(command, limit);
        rows += wrap_text(&shown_command, inner_width.saturating_sub(2)).len();
        dropped += command_dropped;
        if let Some(result) = &tool.result {
            let clamped = clamp_rows(
                result,
                inner_width.saturating_sub(2),
                limit.saturating_sub(rows),
                BodyEdge::Tail,
            );
            rows = rows.saturating_add(clamped.shown);
            dropped = dropped.saturating_add(clamped.dropped);
        }
    } else if tool.name == "grep" {
        if let Some(result) = &tool.result {
            let clamped = clamp_rows(result, inner_width.saturating_sub(2), limit, BodyEdge::Tail);
            rows += clamped.shown;
            dropped += clamped.dropped;
        }
    } else if tool.name == "message_agent" {
        let (count, omitted) = message_agent_rows_count(tool, args, inner_width, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "wait_agent" {
        let (count, omitted) = wait_agent_rows_count(tool, limit);
        rows += count;
        dropped += omitted;
    } else if let Some(result) = &tool.result {
        let clamped = clamp_rows(result, inner_width, limit, BodyEdge::Head);
        rows += clamped.shown;
        dropped += clamped.dropped;
    }

    rows + usize::from(dropped > 0)
}

/// 解析 `message_agent` 的投递回执。
fn message_receipts(
    tool: &ToolCallBlock,
) -> Option<Vec<moc_protocol::methods::AgentMessageReceipt>> {
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

/// 发送侧 IRC 卡片正文：消息预览 + 收件人明细，返回（展示行、被略去的源行数）。
///
/// 明细只在「广播或有失败」时逐行展开：单目标成功时标题行的回执词已经说清结果，
/// 再列一遍只是重复。
fn message_agent_tool_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    theme: &Theme,
    limit: usize,
) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    let mut dropped = 0usize;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let clamped = clamp_rows(message, width.saturating_sub(2), limit, BodyEdge::Head);
    dropped += clamped.dropped;
    for row in clamped.rows {
        lines.push(Line::from(Span::styled(
            format!("  {row}"),
            Style::new().fg(theme.text_primary),
        )));
    }
    let Some(receipts) = message_receipts(tool) else {
        return (lines, dropped);
    };
    let expanded = receipts.len() > 1
        || receipts
            .iter()
            .any(|receipt| receipt.outcome == moc_protocol::methods::AgentMessageOutcome::Failed);
    if !expanded {
        return (lines, dropped);
    }
    for receipt in receipts {
        if lines.len() >= limit {
            dropped += 1;
            continue;
        }
        let (word, color) = outcome_label(receipt.outcome, theme);
        let mut spans = vec![
            Span::styled("  ".to_owned(), Style::new()),
            Span::styled(
                format!("{} ", receipt.title),
                Style::new().fg(theme.text_secondary),
            ),
            Span::styled(word.to_owned(), Style::new().fg(color)),
        ];
        if let Some(error) = receipt.error {
            spans.push(Span::styled(format!("：{error}"), Style::new().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    (lines, dropped)
}

/// 等待卡片的行：优先实时进度，工具结束后回落到结果快照，
/// 返回（展示行、被略去的成员数）。
fn wait_agent_tool_rows(
    tool: &ToolCallBlock,
    theme: &Theme,
    limit: usize,
) -> (Vec<Line<'static>>, usize) {
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
        return (Vec::new(), 0);
    }
    let dropped = rows.len().saturating_sub(limit);
    let mut lines = Vec::with_capacity(rows.len().min(limit));
    for (title, status, elapsed_ms) in rows.into_iter().take(limit) {
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
    (lines, dropped)
}

/// 等待卡片的行数：与 [`wait_agent_tool_rows`] 同规则（只数行、不构造行），
/// 返回（展示行数、被略去的成员数）。
fn wait_agent_rows_count(tool: &ToolCallBlock, limit: usize) -> (usize, usize) {
    let total = match &tool.progress {
        Some(progress) => progress.len(),
        None => wait_result(tool).map_or(0, |result| result.agents.len()),
    };
    (total.min(limit), total.saturating_sub(limit))
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
        "read" => {
            let path = args.get("path").and_then(Value::as_str).unwrap_or_default();
            let offset = args.get("offset").and_then(Value::as_u64);
            let limit = args.get("limit").and_then(Value::as_u64);
            let range = match (offset, limit) {
                (Some(offset), Some(limit)) => format!(":{offset}-{}", offset + limit),
                (Some(offset), None) => format!(":{offset}-"),
                _ => String::new(),
            };
            let globs = args
                .get("glob")
                .and_then(Value::as_array)
                .map(|globs| {
                    globs
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            let globs = if globs.is_empty() {
                String::new()
            } else {
                format!(" {globs}")
            };
            format!("{path}{range}{globs}")
        }
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
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default();
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
                    [receipt] => summary.push_str(&format!(" · {}", outcome_word(receipt.outcome))),
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

/// 截断时保留文本的哪一端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyEdge {
    /// 保留开头（命令预览等）。
    Head,
    /// 保留结尾（bash／grep 结果的事件日志语义）。
    Tail,
}

/// 结果文本的折行结果：展示行、展示行数与被略去的源行数。
struct TextRows {
    rows: Vec<String>,
    shown: usize,
    dropped: usize,
}

/// 结果文本的折行（每源行按宽度折行，`limit` 为展示行上限）。
///
/// [`BodyEdge::Head`] 保留前段、[`BodyEdge::Tail`] 保留后段（bash／grep 的
/// 输出只看最新几行）；渲染与懒渲染估算共用同一条截断规则，两条路径必须
/// 得出同一个行数。
fn clamp_rows(result: &str, width: usize, limit: usize, edge: BodyEdge) -> TextRows {
    let width = width.max(8);
    let lines: Vec<&str> = result.lines().collect();
    match edge {
        BodyEdge::Head => {
            let mut rows: Vec<String> = Vec::new();
            let mut shown = 0usize;
            let mut dropped = 0usize;
            for line in &lines {
                if shown >= limit {
                    dropped += 1;
                    continue;
                }
                for row in wrap_text(line, width) {
                    if shown >= limit {
                        break;
                    }
                    shown += 1;
                    rows.push(row);
                }
            }
            TextRows {
                rows,
                shown,
                dropped,
            }
        }
        BodyEdge::Tail => {
            // 所有源行按宽度折行后取末尾若干**显示行**（bash／grep 只看最新输出）。
            let mut all: Vec<String> = Vec::with_capacity(lines.len());
            let mut widths: Vec<usize> = Vec::with_capacity(lines.len());
            for line in &lines {
                let rows = wrap_text(line, width);
                widths.push(rows.len());
                all.extend(rows);
            }
            let show_from = all.len().saturating_sub(limit);
            let mut dropped = 0usize;
            let mut before = 0usize;
            for rows in widths {
                if before >= show_from {
                    break;
                }
                before += rows;
                dropped += 1;
            }
            let shown = all.len().saturating_sub(show_from);
            debug_assert!(shown <= limit, "tail 切片不得超过展示预算");
            TextRows {
                rows: all.into_iter().skip(show_from).collect(),
                shown,
                dropped,
            }
        }
    }
}

/// 文本按源行数截断（不折行）：展示的文本与被略去的源行数。
fn head_lines(text: &str, limit: usize) -> (String, usize) {
    let mut shown = String::new();
    let mut dropped = 0usize;
    for (index, line) in text.split('\n').enumerate() {
        if index < limit {
            if index > 0 {
                shown.push('\n');
            }
            shown.push_str(line);
        } else {
            dropped += 1;
        }
    }
    (shown, dropped)
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

        // 三档下估算与渲染都必须同源（隐藏只剩标题行、简略是预览、完整是全文）。
        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            for index in 0..app.blocks.len() {
                assert_eq!(
                    estimate_entry_height_at(&app, index, width),
                    rendered(&app, index),
                    "{level:?} 档第 {index} 块的估算应与渲染一致"
                );
            }
        }
        app.tool_detail = DetailLevel::Hidden;
        assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            3,
            "隐藏档只剩标题、耗时与留白"
        );
        app.tool_detail = DetailLevel::Brief;
        assert!(
            estimate_entry_height_at(&app, 0, width) < 30,
            "简略档不得按全文估算"
        );
        app.tool_detail = DetailLevel::Full;
        assert_eq!(estimate_entry_height_at(&app, 0, width), rendered(&app, 0));
        app.blocks[0].toggle_level(app.tool_detail);
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
        app.tool_entry_mut("call_b").expect("批次成员").touch();
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
            generations[3], app.blocks[3].generation,
            "非批次块保留自身代次"
        );

        // 缓存 key 用的就是这份代次
        let mut view = ScrollbackView::new();
        view.sync(&app, 80);
        assert_eq!(view.cache[0].key, (latest, app.view_gen));
        assert_eq!(view.cache[1].key.0, latest);
    }

    /// 思考块三档：隐藏只剩标题、简略取末尾若干行、完整全文。
    #[test]
    fn thinking_levels_render_head_and_tail() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 60u16;
        let text = (0..12)
            .map(|index| format!("思考第 {index} 行"))
            .collect::<Vec<_>>()
            .join("\n");
        app.push_block(Block::Thinking {
            text,
            reasoning_tokens: Some(7),
            duration_ms: Some(1_500),
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.thinking_detail = DetailLevel::Hidden;
        let hidden = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(hidden.len(), 2, "隐藏档 = 标题行 + 留白");
        assert!(text_of(&hidden).contains("ctrl+t 展开"));
        assert!(!text_of(&hidden).contains("思考第"));

        app.thinking_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(brief.len(), 2 + BRIEF_THINKING_ROWS, "简略档 = 末尾 4 行");
        let brief_text = text_of(&brief);
        assert!(
            !brief_text.contains("思考第 0 行"),
            "简略档不含首行：{brief_text}"
        );
        assert_eq!(brief_text.matches("思考第").count(), BRIEF_THINKING_ROWS);

        app.thinking_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(full.len(), 2 + 12, "完整档 = 全部正文");
        assert!(text_of(&full).contains("思考第 0 行"));
    }

    /// 工具正文的简略预算：6 行内容 + 一行省略提示；完整档不截断。
    #[test]
    fn tool_brief_preview_marks_omission() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let result = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.start_tool_block("call_bash".into(), "bash".into());
        app.tool_entry_mut("call_bash")
            .expect("工具块")
            .push_args(r#"{"command":"run"}"#);
        app.finish_tool_block(
            "call_bash".into(),
            ToolStatus::Completed,
            ToolResult {
                tool_call_id: "call_bash".into(),
                content: result,
                is_error: false,
                images: Vec::new(),
            },
            Some(10),
            None,
        );
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width, true);
        // 标题 + 命令行 1 + 结果尾 5 + 省略提示 1 + 耗时 1 + 留白 1
        assert_eq!(brief.len(), 10, "简略档行数固定");
        let brief_text = text_of(&brief);
        assert!(
            brief_text.contains("⋯ 还有 195 行"),
            "略去行数按源行计：{brief_text}"
        );
        assert!(
            brief_text.contains("line-195") && brief_text.contains("line-199"),
            "bash 只留尾部最新几行：{brief_text}"
        );
        assert!(
            !brief_text.contains("line-194") && !brief_text.contains("line-0"),
            "更早的输出被省略：{brief_text}"
        );
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            brief.len(),
            "简略档估算与渲染同源"
        );

        app.tool_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(
            full.len(),
            200 + 4,
            "完整档 = 命令行 + 全部 200 行 + 耗时 + 留白"
        );
        assert!(!text_of(&full).contains("还有"), "完整档不出现省略提示");
        assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());

        app.tool_detail = DetailLevel::Hidden;
        let hidden = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(hidden.len(), 3, "隐藏档 = 标题 + 耗时 + 留白");
        assert!(!text_of(&hidden).contains("line-"), "隐藏档无正文");
        assert_eq!(estimate_entry_height_at(&app, 0, width), hidden.len());
    }

    /// read 这类「回显参数」工具在任何档位都只占一行，且摘要带行号范围。
    #[test]
    fn read_tool_stays_one_line_at_every_level() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let result = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.start_tool_block("call_read".into(), "read".into());
        app.tool_entry_mut("call_read")
            .expect("工具块")
            .push_args(r#"{"path":"crates/moc-tui/src/app.rs","offset":1,"limit":200}"#);
        app.finish_tool_block(
            "call_read".into(),
            ToolStatus::Completed,
            ToolResult {
                tool_call_id: "call_read".into(),
                content: result.clone(),
                is_error: false,
                images: Vec::new(),
            },
            Some(5),
            None,
        );

        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            let lines = render_entry(0, &app.blocks[0], &app, width, true);
            assert_eq!(lines.len(), 1, "{level:?} 档 read 只占一行");
            let text = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(
                text.contains("crates/moc-tui/src/app.rs:1-201"),
                "摘要带行号范围：{text}"
            );
            assert_eq!(estimate_entry_height_at(&app, 0, width), 1);
        }

        // 失败时正文是错误原因，仍可展开。
        app.finish_tool_block(
            "call_read".into(),
            ToolStatus::Error,
            ToolResult {
                tool_call_id: "call_read".into(),
                content: "读取失败：权限不足".into(),
                is_error: true,
                images: Vec::new(),
            },
            Some(5),
            None,
        );
        app.tool_detail = DetailLevel::Brief;
        let lines = render_entry(0, &app.blocks[0], &app, width, true);
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            text.contains("读取失败：权限不足"),
            "失败原因必须可见：{text}"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    /// diff 简略档停在 6 行并补省略提示；完整档给出行数。
    #[test]
    fn brief_diff_stops_at_six_rows() {
        use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let hunk = |start: u32, count: usize| DiffHunk {
            old_start: start,
            new_start: start,
            lines: (0..count)
                .map(|offset| DiffLine {
                    kind: DiffLineKind::Insert,
                    old_no: None,
                    new_no: Some(start + u32::try_from(offset).expect("测试行号不会溢出 u32")),
                    text: format!(
                        "新增第 {} 行",
                        start + u32::try_from(offset).expect("测试行号不会溢出 u32")
                    ),
                })
                .collect(),
        };
        let diff = ToolDiff {
            files: vec![ToolFileDiff {
                path: "src/lib.rs".into(),
                hunks: vec![hunk(1, 3), hunk(20, 3), hunk(40, 1)],
            }],
            truncated: false,
        };
        let total = crate::render::diff::row_count(&diff);
        assert_eq!(total, 9, "3+3+1 行内容 + 2 条 hunk 分隔");
        app.start_tool_block("call_edit".into(), "edit".into());
        app.tool_entry_mut("call_edit")
            .expect("工具块")
            .push_args(r#"{"path":"src/lib.rs"}"#);
        app.finish_tool_block(
            "call_edit".into(),
            moc_protocol::methods::ToolStatus::Completed,
            moc_protocol::ToolResult {
                tool_call_id: "call_edit".into(),
                content: String::new(),
                is_error: false,
                images: Vec::new(),
            },
            Some(1),
            Some(diff),
        );
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width, true);
        // 标题 + 6 行 diff + 省略提示 + 耗时 + 留白
        assert_eq!(brief.len(), 10, "简略档 diff 限 6 行");
        assert_eq!(
            text_of(&brief).matches("还有").count(),
            1,
            "省略提示只出现一次"
        );
        assert!(
            text_of(&brief).contains("⋯ 还有 3 行"),
            "{}",
            text_of(&brief)
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), brief.len());
        app.tool_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(
            full.len(),
            total + 3,
            "完整档 = 全部 diff 行 + 耗时 + 留白 + 标题"
        );
        assert!(!text_of(&full).contains("还有"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());
    }

    /// 接收侧 IRC 卡片：标题措辞、默认简略预览、点击后完整／隐藏与估高同源。
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
        let axis = || DetailLevel::Brief;

        // 默认档位（简略）：预览 3 行 + 省略提示。
        let preview = render_entry(0, &app.blocks[0], &app, width, true);
        let text = text_of(&preview);
        assert!(text.contains("IRC ◀ 探查者"), "标题必须标明来源：{text}");
        assert!(text.contains("已注入"), "标题必须标出投递结果：{text}");
        assert!(text.contains("⋯ 还有 3 行"), "简略档只留预览行：{text}");
        assert!(text.contains("第 0 行消息") && text.contains("第 2 行消息"));
        assert!(!text.contains("第 3 行消息"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            preview.len(),
            "简略档估算与渲染同源"
        );

        // 点一次到完整：全文，无省略提示。
        app.blocks[0].toggle_level(axis());
        let expanded = render_entry(0, &app.blocks[0], &app, width, true);
        let text = text_of(&expanded);
        assert!(text.contains("第 5 行消息"));
        assert!(!text.contains("还有"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            expanded.len(),
            "完整档估算与渲染同源"
        );

        // 再点一次：回到当前层档位（简略），不留覆盖。
        app.blocks[0].toggle_level(axis());
        assert_eq!(
            app.blocks[0].level_override, None,
            "目标与层档位相同时不留覆盖"
        );
        let back = render_entry(0, &app.blocks[0], &app, width, true);
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            back.len(),
            "回落简略档后估算与渲染同源"
        );

        // 隐藏档（IRC 卡片的层档位是简略，只能由覆盖或调用方给定）：只剩标题行。
        let hidden = render_entry_single(
            &app.blocks[0],
            &app,
            usize::from(width),
            true,
            DetailLevel::Hidden,
        );
        assert_eq!(hidden.len(), 2, "隐藏档只剩标题行与尾随留白");
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
        assert!(
            text.contains("请复查一下改动"),
            "正文必须带消息预览：{text}"
        );
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
        assert!(
            text.contains("▸ 甲 · 运行中 · 3.2s"),
            "运行行带耗时：{text}"
        );
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

    #[test]
    fn tmp_repro_empty_bash_command() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        app.tool_detail = DetailLevel::Full;
        let result = "\npub fn filtered_entries<'a>(entries: &'a [ModelEntry], query: &str) -> Vec<(usize, &'a ModelEntry)> {\n    let query = query.to_lowercase();\n    entries\n        .iter()\n        .enumerate()\n        .filter(|(_, entry)| {\n            query.is_empty()\n                || entry.label.to_lowercase().contains(&query)\n                || entry.display_name.to_lowercase().contains(&query)\n\n        .collect()\n}\n\npub fn filtered_sessions<'a>(\n";
        app.start_tool_block("call_0_186028a48b407df4".into(), "bash".into());
        app.tool_entry_mut("call_0_186028a48b407df4")
            .expect("工具块")
            .push_args(r#"{"command":"cd /home/soeur/project/moc && sed -n 760,774p crates/moc-tui/src/modal.rs"}"#);
        app.finish_tool_block(
            "call_0_186028a48b407df4".into(),
            ToolStatus::Completed,
            ToolResult {
                tool_call_id: "call_0_186028a48b407df4".into(),
                content: result.into(),
                is_error: false,
                images: Vec::new(),
            },
            Some(208),
            None,
        );
        let lines = render_entry(0, &app.blocks[0], &app, width, true);
        for line in &lines {
            let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
            eprintln!("[{text}]");
        }
    }
}

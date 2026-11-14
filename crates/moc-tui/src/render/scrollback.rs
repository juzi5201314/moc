//! 滚动区渲染管线：块 → 行缓存 → 视口绘制。
//!
//! 缓存 key = `(块内容版本, 视图版本, 宽度)`：流式增量只让活动块失效，
//! 全局折叠开关让受影响块失效，resize 全量重建。绘制按行直写 buffer，
//! 视口之外的历史完全不进渲染器。

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::Line;

use super::PROSE_INDENT;
use super::image::ImagePlacement;
use super::markdown::{ButtonSpan, ChartPlacement, RenderViews};
use super::{diff, labels};
use crate::app::{App, ChartHit, ImageSlot, SlotSource};
use crate::theme::Theme;
use crate::transcript::Block;
#[cfg(test)]
use crate::transcript::Entry;

/// 简略档的思考预览行数（取正文末尾若干行）。
pub(crate) const BRIEF_THINKING_ROWS: usize = 4;

/// 简略档的工具正文行数上限（超出补一行省略提示）。
const BRIEF_BODY_ROWS: usize = 6;

/// 简略档 bash 结果保留的末尾输出行数（命令行独占一行，不计入这里）。
const BASH_BRIEF_OUTPUT_ROWS: usize = 4;

/// 完整档的正文行数上限：等同于「不截断」（core 侧输出预算与
/// [`diff::MAX_RENDERED_ROWS`] 已先行收窄），只用来把不限变成有限的算术上界，
/// 避免预算参与加法溢出。
const FULL_BODY_ROWS: usize = 4001;

// 完整档的正文上限必须覆盖 diff 自身的渲染上限，否则 diff 会在估算与渲染里
// 走不同的收窄口径。
const _: () = assert!(FULL_BODY_ROWS >= diff::MAX_RENDERED_ROWS);

/// 接收侧 IRC 与子代理结果卡片折叠时保留的正文行数。
const AGENT_MESSAGE_PREVIEW_ROWS: usize = 3;

/// 滚动区渲染状态（与 `App::blocks` 长度保持同步）。
#[derive(Default)]
pub struct ScrollbackView {
    width: u16,
    cache: Vec<CachedEntry>,
    /// 每帧重算的块内容代次（批次成员共享输入序列的版本），与 `App::blocks` 对齐。
    generations: Vec<u64>,
    batches: BatchIndex,
    tree: TreeIndex,
    deferred: bool,
    background_layout: bool,
    pending_layout: Option<PendingLayout>,
    live_layout_block: Option<usize>,
}

struct PendingLayout {
    index: usize,
    key: (u64, u64),
    epoch: u64,
    width: u16,
    result: tokio::sync::oneshot::Receiver<Result<LayoutResult, tokio::task::JoinError>>,
}

struct LayoutResult {
    rendered: EntryLines,
    cost: std::time::Duration,
}

#[derive(Default)]
struct BatchIndex {
    /// 块下标和批次编号均为密集索引；非批次块没有编号。
    by_block: Vec<Option<usize>>,
    groups: Vec<BatchGroup>,
}

#[derive(Default)]
struct BatchGroup {
    members: Vec<usize>,
    /// 与 members 同序的上次布局输入；索引密集、有序比较，不使用可能碰撞的摘要。
    previous: Vec<(usize, u64, u64)>,
    generation: u64,
}

/// 批次归属键：顶层调用按协议批次 + 工具名分组；嵌套调用按「同一父调用下
/// 连续同名的子调用段」分组，段首块下标作键（嵌套块忽略 `batch_id`）。
#[derive(PartialEq, Eq, Hash)]
enum BatchKey<'a> {
    Batch(&'a str, &'a str),
    Run(usize),
}

mod index;
mod notices;
use index::block_layout;

/// 嵌套工具块（`parent_id` 为 `Some` 的 PTC 子调用）的树形状。
///
/// 渲染时决定连线前缀；同时进缓存有效性判断——新子块到达会让上一个兄弟的
/// `more` 翻转，父调用结束会让最后一个子块的 `more` 落下。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TreeShape {
    /// 嵌套深度（1 = 顶层调用的直接子调用），上限 [`TreeIndex::MAX_DEPTH`]。
    depth: u8,
    /// 祖先层位掩码：第 k 位 = 第 k+1 层祖先自己的 `more` 标志（最低位是最外层）。
    rails: u32,
    /// 本层是否还有后续：后面出现同一 `parent_id` 的子块，或父调用仍在运行。
    more: bool,
}

/// 块的树形版式：嵌套连线形状 + 块尾是否留一行空行。
///
/// `gap` 的规则：有子调用的父块与树中间的子调用都不留空行，只有整棵树
/// 最后一个后代（父调用已结束、后面没有同层兄弟、祖先层也没有后续）才带
/// 一行空行——这行空行不带任何连线前缀。顶层块恒为空行分隔。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TreeLayout {
    shape: Option<TreeShape>,
    gap: bool,
    /// 父结果改变会影响子调用通知去重，必须使子块的高度和正文缓存一起失效。
    notice_parent_generation: u64,
}

/// 嵌套工具块的树形状索引，与 `App::blocks` 对齐；非嵌套块 `shape` 为 `None`。
///
/// 与 [`BatchIndex`] 同样每帧 O(n) 重建：逆序一遍算出「后面还有同 parent 的
/// 子块」与「后面已有子块」，正序一遍沿父链继承深度、把祖先标志压进 `rails`
/// 位掩码，渲染路径不必再向后扫块列表。
#[derive(Default)]
struct TreeIndex {
    layouts: Vec<TreeLayout>,
}

struct CachedEntry {
    key: (u64, u64),
    layout_epoch: u64,
    /// 树形版式（[`TreeIndex`]）；连线形状或尾随空行变了都必须重渲染，即使内容代次没变。
    layout: TreeLayout,
    height: usize,
    lines: Vec<Line<'static>>,
    paged: Option<super::rows::RowBuffer>,
    footer: Vec<Line<'static>>,
    line_start: usize,
    rendered: bool,
    rendered_at: std::time::Instant,
    render_cost: std::time::Duration,
    /// 表格入口与图表占位（行号是块内局部行）。
    tables: Vec<LocalTable>,
    /// 参考链接折叠入口（行号是块内局部行）。
    links_button: Option<ButtonSpan>,
    /// 通用图片落位（行号是块内局部行）。
    images: Vec<ImagePlacement>,
}

/// 一张顶层表格在块内的落位（局部行号，尚未加屏幕原点与正文缩进）。
#[derive(Debug, Clone, Copy)]
struct LocalTable {
    table: usize,
    button: Option<ButtonSpan>,
    chart: Option<ChartPlacement>,
}

/// 单块的渲染结果：行 + 表格落位。
///
/// 行与落位必须同源产出：命中区与位图槽位都靠局部行号换算屏幕坐标，
/// 任何一处单独变化都会让点击落到错误的位置。
struct EntryLines {
    lines: Vec<Line<'static>>,
    paged: Option<super::rows::RowBuffer>,
    height: usize,
    tables: Vec<LocalTable>,
    /// 参考链接折叠入口（块内局部行号）。
    links_button: Option<ButtonSpan>,
    images: Vec<ImagePlacement>,
}

impl EntryLines {
    /// 无表格、无图片的块：只有行。
    fn plain(lines: Vec<Line<'static>>) -> Self {
        Self {
            height: lines.len(),
            lines,
            paged: None,
            tables: Vec::new(),
            links_button: None,
            images: Vec::new(),
        }
    }
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

    pub(crate) fn background() -> Self {
        Self {
            background_layout: true,
            ..Self::default()
        }
    }

    fn collect_layout(&mut self, app: &App) {
        use tokio::sync::oneshot::error::TryRecvError;
        let Some(pending) = &mut self.pending_layout else {
            return;
        };
        let result = match pending.result.try_recv() {
            Ok(result) => result.expect("后台 markdown 布局失败"),
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Closed) => panic!("后台布局未发送结果"),
        };
        let pending = self.pending_layout.take().expect("已取得布局结果");
        let Some(entry) = app.blocks.get(pending.index) else {
            return;
        };
        if pending.width != self.width
            || pending.key.1 != app.view_gen
            || pending.epoch != entry.layout_epoch
            || !app.prompt_running
            || entry.elapsed_ms.is_some()
            || !matches!(entry.block, Block::Assistant { .. })
        {
            return;
        }
        let Some(cached) = self.cache.get_mut(pending.index) else {
            return;
        };
        if cached.key.0 >= pending.key.0 || entry.generation < pending.key.0 {
            return;
        }
        // 同一追加族可显示已完成的中间帧；不能等流停止才第一次显示结果。
        let (lines, footer) = if result.rendered.paged.is_some() {
            (Vec::new(), result.rendered.lines)
        } else {
            (result.rendered.lines, Vec::new())
        };
        *cached = CachedEntry {
            key: pending.key,
            layout_epoch: pending.epoch,
            layout: block_layout(&self.batches, &self.tree, pending.index),
            height: result.rendered.height,
            lines,
            footer,
            paged: result.rendered.paged,
            line_start: 0,
            tables: result.rendered.tables,
            links_button: result.rendered.links_button,
            images: result.rendered.images,
            rendered: true,
            rendered_at: std::time::Instant::now(),
            render_cost: result.cost,
        };
    }

    fn start_layout(&mut self, app: &App, index: usize, key: (u64, u64)) {
        debug_assert!(self.pending_layout.is_none(), "每个视图至多一份布局快照");
        let entry = &app.blocks[index];
        let Block::Assistant { text } = &entry.block else {
            unreachable!("只调度助手正文");
        };
        let text = text.clone();
        let charts = entry.charts.clone();
        let links_expanded = entry.links_expanded;
        let graphics = app.graphics;
        let images = app.images.clone();
        let cwd = std::path::PathBuf::from(&app.startup_cwd);
        let theme = app.theme;
        let width = self.width;
        let epoch = entry.layout_epoch;
        let events = app.events.clone();
        let (send, result) = tokio::sync::oneshot::channel();
        let job = tokio::task::spawn_blocking(move || {
            let started = std::time::Instant::now();
            let rendered = assistant_layout(
                &text,
                usize::from(width),
                theme,
                RenderViews {
                    charts: &charts,
                    graphics,
                    streaming: true,
                    images: Some(&images),
                    cwd: Some(&cwd),
                    spawn_images: true,
                    links_expanded,
                },
                None,
            );
            LayoutResult {
                rendered,
                cost: started.elapsed(),
            }
        });
        tokio::spawn(async move {
            let _ = send.send(job.await);
            let _ = events.send(crate::protocol::AppEvent::LayoutReady);
        });
        self.pending_layout = Some(PendingLayout {
            index,
            key,
            epoch,
            width,
            result,
        });
    }

    /// 让缓存与 `app.blocks` 对齐（懒块只留估算高度），高度累计由 [`RowWindow`] 负责。
    pub fn sync(&mut self, app: &App, width: u16) {
        const EAGER_BLOCKS: usize = 64;
        self.deferred = false;
        if self.width != width {
            self.width = width;
            self.cache.clear();
        }
        self.collect_layout(app);
        self.live_layout_block = self.background_layout.then(|| app.blocks.len().checked_sub(1))
            .flatten().filter(|&index| app.prompt_running && app.blocks[index].elapsed_ms.is_none()
                && matches!(&app.blocks[index].block, Block::Assistant { text } if text.len() > 64 * 1024));
        let eager_start = app.blocks.len().saturating_sub(EAGER_BLOCKS);
        self.batches.sync(&app.blocks, &mut self.generations);
        self.tree.sync(&app.blocks);
        for (index, entry) in app.blocks.iter().enumerate() {
            let key = (
                self.generations.get(index).copied().unwrap_or(0),
                app.view_gen,
            );
            let layout = block_layout(&self.batches, &self.tree, index);
            if self.cache.get(index).is_some_and(|cached| {
                cached.key == key
                    && cached.layout_epoch == entry.layout_epoch
                    && cached.layout == layout
            }) {
                continue;
            }
            if self.live_layout_block == Some(index)
                && self.cache.get(index).is_some_and(|cached| {
                    cached.rendered
                        && cached.key.1 == key.1
                        && cached.layout_epoch == entry.layout_epoch
                })
            {
                if self.pending_layout.is_none() {
                    let cached = &self.cache[index];
                    let wait = (cached.render_cost * 4).clamp(
                        std::time::Duration::from_millis(16),
                        std::time::Duration::from_millis(250),
                    );
                    if cached.rendered_at.elapsed() >= wait {
                        self.start_layout(app, index, key);
                    } else {
                        self.deferred = true;
                    }
                }
                continue;
            }
            if app.prompt_running
                && entry.elapsed_ms.is_none()
                && matches!(&entry.block, Block::Assistant { text } if text.len() > 64 * 1024)
                && self.cache.get(index).is_some_and(|cached| {
                    cached.rendered
                        && cached.key.1 == key.1
                        && cached.layout_epoch == entry.layout_epoch
                        && cached.rendered_at.elapsed()
                            < (cached.render_cost * 4).clamp(
                                std::time::Duration::from_millis(16),
                                std::time::Duration::from_millis(250),
                            )
                })
            {
                self.deferred = true;
                continue;
            }
            let render_started = std::time::Instant::now();
            let (lines, footer, paged, tables, links_button, images, height, rendered) = if index
                >= eager_start
            {
                let rendered =
                    render_entry_with(index, entry, app, width, true, &self.batches, &self.tree);
                let (lines, footer) = if rendered.paged.is_some() {
                    (Vec::new(), rendered.lines)
                } else {
                    (rendered.lines, Vec::new())
                };
                (
                    lines,
                    footer,
                    rendered.paged,
                    rendered.tables,
                    rendered.links_button,
                    rendered.images,
                    rendered.height,
                    true,
                )
            } else {
                (
                    Vec::new(),
                    Vec::new(),
                    None,
                    Vec::new(),
                    None,
                    Vec::new(),
                    estimate_entry_height_indexed(app, index, width, &self.batches, &self.tree),
                    false,
                )
            };
            let cached = CachedEntry {
                key,
                layout_epoch: entry.layout_epoch,
                layout,
                height,
                lines,
                footer,
                paged,
                line_start: 0,
                rendered,
                rendered_at: std::time::Instant::now(),
                render_cost: render_started.elapsed(),
                tables,
                links_button,
                images,
            };
            if index < self.cache.len() {
                self.cache[index] = cached;
            } else {
                self.cache.push(cached);
            }
        }
        self.cache.truncate(app.blocks.len());
    }

    pub fn has_deferred_render(&self) -> bool {
        self.deferred
    }

    /// 释放屏外解码行；压缩排版按 8 MiB 预算保留，超额时淘汰屏外块。
    pub fn trim_outside(&mut self, skip_rows: usize, viewport: usize) {
        self.trim_with_budget(skip_rows, viewport, 8 * 1024 * 1024);
    }

    fn trim_with_budget(&mut self, skip_rows: usize, viewport: usize, budget: usize) {
        let mut bytes: usize = self
            .cache
            .iter()
            .filter_map(|cached| cached.paged.as_ref())
            .map(super::rows::RowBuffer::stored_bytes)
            .sum();
        let end = skip_rows.saturating_add(viewport);
        let mut offset = 0usize;
        for (index, cached) in self.cache.iter_mut().enumerate() {
            let next = offset.saturating_add(cached.height);
            if Some(index) != self.live_layout_block && (next <= skip_rows || offset >= end) {
                cached.lines = Vec::new();
                if bytes > budget
                    && let Some(pages) = cached.paged.take()
                {
                    bytes = bytes.saturating_sub(pages.stored_bytes());
                    cached.footer = Vec::new();
                }
                if cached.paged.is_none() {
                    cached.tables = Vec::new();
                    cached.links_button = None;
                    cached.images = Vec::new();
                    cached.rendered = false;
                }
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
            let mut rendered_count = 0usize;
            let end = window.skip_rows.saturating_add(viewport);
            let mut offset = 0usize;
            for (index, cached) in self.cache.iter_mut().enumerate() {
                let next = offset.saturating_add(cached.height);
                if !cached.rendered && offset < end && next > window.skip_rows {
                    let started = std::time::Instant::now();
                    let rendered = render_entry_with(
                        index,
                        &app.blocks[index],
                        app,
                        self.width,
                        true,
                        &self.batches,
                        &self.tree,
                    );
                    cached.render_cost = started.elapsed();
                    cached.rendered_at = std::time::Instant::now();
                    cached.height = rendered.height;
                    if rendered.paged.is_some() {
                        cached.footer = rendered.lines;
                        cached.lines.clear();
                    } else {
                        cached.lines = rendered.lines;
                    }
                    cached.paged = rendered.paged;
                    cached.tables = rendered.tables;
                    cached.images = rendered.images;
                    cached.rendered = true;
                    rendered_count += 1;
                }
                offset = offset.saturating_add(cached.height);
            }
            if rendered_count == 0 {
                break;
            }
            window = self.row_window(viewport, scroll_offset);
        }
        let mut offset = 0usize;
        for cached in &mut self.cache {
            if let Some(pages) = &cached.paged {
                let end = window.skip_rows.saturating_add(viewport);
                if offset < end && offset.saturating_add(cached.height) > window.skip_rows {
                    let from = window.skip_rows.saturating_sub(offset);
                    let to = end.saturating_sub(offset).min(cached.height);
                    if from < cached.line_start || to > cached.line_start + cached.lines.len() {
                        let start = from.saturating_sub(128);
                        let end = to.saturating_add(128).min(cached.height);
                        cached.lines = pages
                            .window(start.min(pages.len())..end.min(pages.len()))
                            .into_iter()
                            .map(|line| hang(line, PROSE_INDENT, usize::MAX))
                            .collect();
                        for row in start.max(pages.len())..end {
                            cached.lines.push(cached.footer[row - pages.len()].clone());
                        }
                        cached.line_start = start;
                    }
                }
            }
            offset = offset.saturating_add(cached.height);
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

    /// 粘性折叠头：标题被滚出视口而正文仍盖住顶行的可折叠块，返回其标题行。
    ///
    /// 顶层绘制把这行叠在转写区首行并补登命中，折叠入口不随滚动丢失。
    pub fn sticky_title(&self, app: &App, skip_rows: usize) -> Option<(usize, &Line<'static>)> {
        let mut offset = 0usize;
        for (index, cached) in self.cache.iter().enumerate() {
            let end = offset.saturating_add(cached.height);
            if end <= skip_rows {
                offset = end;
                continue;
            }
            // 盖住顶行的第一块：只有块头在 skip_rows 之前才算「被滚出」，
            // 标题恰好在顶的行自然绘制、不钉。
            if offset < skip_rows
                && cached.line_start == 0
                && app
                    .blocks
                    .get(index)
                    .is_some_and(|entry| entry.block.foldable())
            {
                return cached.lines.first().map(|line| (index, line));
            }
            return None;
        }
        None
    }

    /// 从第 `skip_rows` 行开始把内容画进视口，并登记命中表、位图槽位与行文本。
    ///
    /// 位图槽位只在**完整落在视口内**时登记：图像按格坐标一次性贴上，半截
    /// 图像会盖住顶部信息条或输入框；半截可见时宁可不贴，占位行留白。
    #[expect(
        clippy::too_many_arguments,
        reason = "绘制上下文（buffer/区域/起点/行收集/主题/悬停/登记目标）一次传完，拆结构体反而每次多一次装箱"
    )]
    pub fn paint(
        &self,
        buffer: &mut Buffer,
        area: Rect,
        skip_rows: usize,
        collect_lines: bool,
        theme: &Theme,
        hover: Option<(usize, usize)>,
        links_hover: Option<usize>,
        sinks: PaintSinks<'_>,
    ) {
        let PaintSinks {
            hits,
            charts,
            links,
            slots,
            lines,
        } = sinks;
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
            for line in cached
                .lines
                .iter()
                .skip(offset.saturating_sub(cached.line_start))
            {
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
            let origin = row_start;
            for table in &cached.tables {
                if let Some(button) = table.button
                    && button.row >= offset
                {
                    let row = origin
                        .saturating_add(u16::try_from(button.row - offset).unwrap_or(u16::MAX));
                    if row < bottom {
                        if hover == Some((index, table.table)) {
                            buffer.set_style(
                                Rect::new(area.x, row, area.width, 1),
                                Style::new().bg(theme.bg_hover),
                            );
                        }
                        charts.push(ChartHit {
                            entry: index,
                            table: table.table,
                            row,
                            x: area.x + PROSE_INDENT as u16 + button.x,
                            w: button.w,
                        });
                    }
                }
                if let Some(chart) = table.chart
                    && chart.row >= offset
                {
                    let row = origin
                        .saturating_add(u16::try_from(chart.row - offset).unwrap_or(u16::MAX));
                    let end = row.saturating_add(chart.rows);
                    if row >= area.y && end <= bottom {
                        let from = chart.row;
                        let to = from + usize::from(chart.rows);
                        slots.push(ImageSlot {
                            entry: index,
                            source: SlotSource::Chart { table: table.table },
                            x: area.x + PROSE_INDENT as u16,
                            y: row,
                            cols: chart.cols,
                            rows: chart.rows,
                            px: chart.px,
                            hash: line_hash(
                                &cached.lines[from - cached.line_start..to - cached.line_start],
                                0,
                            ),
                            placement_id: placement_id(index, 0, table.table),
                        });
                    }
                }
            }
            // 参考链接的折叠入口：与表格入口一样只在按钮文字区间命中
            if let Some(button) = cached.links_button
                && button.row >= offset
            {
                let row =
                    origin.saturating_add(u16::try_from(button.row - offset).unwrap_or(u16::MAX));
                if row < bottom {
                    if links_hover == Some(index) {
                        buffer.set_style(
                            Rect::new(area.x, row, area.width, 1),
                            Style::new().bg(theme.bg_hover),
                        );
                    }
                    links.push(crate::app::LinkHit {
                        entry: index,
                        row,
                        x: area.x + PROSE_INDENT as u16 + button.x,
                        w: button.w,
                    });
                }
            }
            // 通用图片：同一张图可以在一屏里出现多次，放置 ID 按块内序号区分
            for (ordinal, image) in cached.images.iter().enumerate() {
                if image.row < offset {
                    continue;
                }
                let row =
                    origin.saturating_add(u16::try_from(image.row - offset).unwrap_or(u16::MAX));
                let end = row.saturating_add(image.rows);
                if row < area.y || end > bottom {
                    continue;
                }
                if image.x.saturating_add(image.cols) > area.width {
                    continue;
                }
                let digest = image.key.digest();
                let from = image.row;
                let to = from + usize::from(image.rows);
                let Some(lines) = cached.lines.get(
                    from.saturating_sub(cached.line_start)..to.saturating_sub(cached.line_start),
                ) else {
                    continue;
                };
                slots.push(ImageSlot {
                    entry: index,
                    source: SlotSource::Image(image.key.clone()),
                    x: area.x.saturating_add(image.x),
                    y: row,
                    cols: image.cols,
                    rows: image.rows,
                    px: image.px,
                    hash: line_hash(lines, digest),
                    placement_id: placement_id(index, 1, ordinal),
                });
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

/// 绘制时的登记目标：标题命中、表格入口、链接入口、位图槽位与行文本。
///
/// 打包成结构体传参：五个 `&mut Vec` 平铺在签名里既难读，也容易传错顺序。
pub struct PaintSinks<'a> {
    /// 块标题行命中（点击折叠）。
    pub hits: &'a mut Vec<(usize, u16, u16)>,
    /// 表格入口命中（点击切换图表）。
    pub charts: &'a mut Vec<ChartHit>,
    /// 参考链接折叠入口命中（点击展开 / 收起）。
    pub links: &'a mut Vec<crate::app::LinkHit>,
    /// 本帧要贴的位图。
    pub slots: &'a mut Vec<ImageSlot>,
    /// 行文本快照（拖选复制）。
    pub lines: &'a mut Vec<(u16, String)>,
}

/// 一段行的内容指纹：位图只在指纹变化时重发。
///
/// `salt` 用来把「内容寻址的图片键」混进来——同一段空白占位行可能对应两张
/// 不同的图（键不同），只比文本会漏发。
fn line_hash(lines: &[Line<'static>], salt: u64) -> u64 {
    use std::hash::{Hash, Hasher};

    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    salt.hash(&mut hasher);
    for line in lines {
        for span in &line.spans {
            span.content.hash(&mut hasher);
        }
    }
    hasher.finish()
}

/// kitty 放置 ID：`(块, 域, 序号)` 的稳定散列，取低 24 位并保证非零。
///
/// 同一个块的同一个位置每帧算出同一个 ID，位置或内容变化时由 `emit` 比对
/// 指纹重发；同一张图贴多处则靠序号分开。
fn placement_id(entry: usize, domain: u8, ordinal: usize) -> u32 {
    let mut hasher = xxhash_rust::xxh3::Xxh3::new();
    hasher.update(&u64::try_from(entry).unwrap_or(u64::MAX).to_le_bytes());
    hasher.update(&[domain]);
    hasher.update(&u64::try_from(ordinal).unwrap_or(u64::MAX).to_le_bytes());
    // 低 24 位：kitty 的 placement id 只需进程内唯一，24 位足够避开碰撞
    let low = u32::try_from(hasher.digest() & 0x00ff_ffff).unwrap_or(0);
    low | 1
}

/// 计算块内容代次：批次成员共享同一输入序列的布局版本。
///
/// 一遍建表、一遍回填，任何成员改变都使全批失效；不再逐块全表扫描。
#[cfg(test)]
fn fill_block_generations(blocks: &[Entry], generations: &mut Vec<u64>) {
    BatchIndex::default().sync(blocks, generations);
}

#[cfg(test)]
fn estimate_entry_height_at(app: &App, index: usize, width: u16) -> usize {
    let mut batches = BatchIndex::default();
    batches.sync(&app.blocks, &mut Vec::new());
    let mut tree = TreeIndex::default();
    tree.sync(&app.blocks);
    estimate_entry_height_indexed(app, index, width, &batches, &tree)
}

mod messages;
use messages::assistant_layout;
mod agent_tools;
mod layout;
mod structured_tools;
mod tool_content;
mod tool_summary;
use layout::estimate_entry_height_indexed;
use layout::render_entry_with;
mod text;
use text::hang;

/// 单块渲染。折叠状态由全局开关与块类型共同决定。
///
/// 留白规则只有一条：块自带一行尾随留白，从不加前导留白；工具块按
/// [`TreeLayout::gap`] 走——有子调用的父块与树中间的子调用都不留白。
#[cfg(test)]
fn render_entry(index: usize, entry: &Entry, app: &App, width: u16) -> EntryLines {
    let mut batches = BatchIndex::default();
    batches.sync(&app.blocks, &mut Vec::new());
    let mut tree = TreeIndex::default();
    tree.sync(&app.blocks);
    render_entry_with(index, entry, app, width, true, &batches, &tree)
}

/// 代理状态词；TUI 各处共用一套措辞（[`labels::status`]）。
pub(crate) fn agent_status_label(status: moc_protocol::methods::AgentStatus) -> &'static str {
    use moc_protocol::methods::AgentStatus;
    match status {
        AgentStatus::Queued => labels::status::QUEUED,
        AgentStatus::Running => labels::status::RUNNING,
        AgentStatus::Completed => labels::status::DONE,
        AgentStatus::Stopped => labels::status::STOPPED,
        AgentStatus::Failed => labels::status::FAILED,
        AgentStatus::Idle => labels::status::IDLE,
    }
}

/// 单条投递结果的措辞。
pub(crate) fn outcome_word(outcome: moc_protocol::methods::AgentMessageOutcome) -> &'static str {
    use moc_protocol::methods::AgentMessageOutcome;
    match outcome {
        AgentMessageOutcome::Injected => labels::status::INJECTED,
        AgentMessageOutcome::Woken => labels::status::WOKEN,
        AgentMessageOutcome::Failed => labels::status::FAILED,
    }
}

/// ask 转写卡 key 列宽上限（格）：对齐答案正文，超长 header/id 截断。
const ASK_TRANSCRIPT_KEY_MAX: usize = 16;

/// 截断时保留文本的哪一端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BodyEdge {
    /// 保留开头（命令预览等）。
    Head,
    /// 保留结尾（bash 输出只看最新几行）。
    Tail,
}

/// 结果文本的折行结果：展示行、展示行数与被略去的源行数。
struct TextRows {
    rows: Vec<String>,
    shown: usize,
    dropped: usize,
}

/// Todo 状态符号与样式：转写树与 HUD 卡片共用。
///
/// 返回 `(符号, 符号样式, 任务文本样式)`；Completed 只给文本加删除线，
/// 保留符号本身可读。
pub(crate) fn todo_status_marker(
    status: moc_protocol::methods::TodoStatus,
    theme: &Theme,
) -> (&'static str, Style, Style) {
    use moc_protocol::methods::TodoStatus;
    let (glyph, color) = match status {
        TodoStatus::Completed => ("☑ ", theme.accent_success),
        TodoStatus::InProgress => ("▶ ", theme.accent_running),
        TodoStatus::Pending => ("☐ ", theme.gray),
        TodoStatus::Blocked => ("⚠ ", theme.accent_warning),
        TodoStatus::Abandoned => ("✗ ", theme.gray_dim),
    };
    let marker = Style::new().fg(color);
    let text = if status == TodoStatus::Completed {
        marker.add_modifier(Modifier::CROSSED_OUT)
    } else {
        marker
    };
    (glyph, marker, text)
}

#[cfg(test)]
mod tests;

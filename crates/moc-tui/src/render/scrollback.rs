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

use super::image::{ImagePlacement, ImageState, layout as image_layout};
use super::markdown::{
    ButtonSpan, ChartPlacement, RenderViews, render_markdown, render_markdown_with,
};
use super::wrap::{clip_line, str_width, wrap_text};
use super::{PROSE_INDENT, TITLE_INDENT};
use super::{diff, labels};
use crate::app::{App, ChartHit, ImageSlot, SlotSource};
use crate::theme::Theme;
use crate::transcript::{
    Block, DetailLevel, Entry, MAX_TOOL_IMAGES, ToolBody, ToolCallBlock, tool_body_policy,
};

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

/// 接收侧 IRC 卡片折叠时保留的正文行数。
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
    /// 嵌套段组（同一父调用下连续同名的子调用）；单成员的段不算批次。
    nested: bool,
}

/// 批次归属键：顶层调用按协议批次 + 工具名分组；嵌套调用按「同一父调用下
/// 连续同名的子调用段」分组，段首块下标作键（嵌套块忽略 `batch_id`）。
#[derive(PartialEq, Eq, Hash)]
enum BatchKey<'a> {
    Batch(&'a str, &'a str),
    Run(usize),
}

impl BatchIndex {
    fn sync(&mut self, blocks: &[Entry], generations: &mut Vec<u64>) {
        // 全部 parent_id：自己也是父的嵌套块不能并入同名段（子块会插在段中间）。
        let mut parents = std::collections::HashSet::new();
        for entry in blocks {
            if let Block::Tool(tool) = &entry.block
                && let Some(parent) = tool.parent_id.as_deref()
            {
                parents.insert(parent);
            }
        }
        let mut ids = std::collections::HashMap::new();
        let mut count = 0;
        self.by_block.clear();
        generations.clear();
        for group in &mut self.groups {
            group.members.clear();
        }
        // 上一块的嵌套身份（parent, name, id）与本段首块的下标。
        let mut previous: Option<(&str, &str, &str)> = None;
        let mut run_start = 0usize;
        for (index, entry) in blocks.iter().enumerate() {
            generations.push(entry.generation);
            let key = match &entry.block {
                Block::Tool(tool) => {
                    if let Some(parent) = tool.parent_id.as_deref() {
                        let merges = previous.is_some_and(|(p, name, id)| {
                            p == parent
                                && name == tool.name
                                && !parents.contains(tool.id.as_str())
                                && !parents.contains(id)
                        });
                        if !merges {
                            run_start = index;
                        }
                        previous = Some((parent, tool.name.as_str(), tool.id.as_str()));
                        Some(BatchKey::Run(run_start))
                    } else {
                        previous = None;
                        tool.batch_id
                            .as_deref()
                            .map(|batch| BatchKey::Batch(batch, tool.name.as_str()))
                    }
                }
                _ => {
                    previous = None;
                    None
                }
            };
            let group = key.map(|key| {
                let nested = matches!(key, BatchKey::Run(_));
                *ids.entry(key).or_insert_with(|| {
                    let id = count;
                    count += 1;
                    if self.groups.len() == id {
                        self.groups.push(BatchGroup::default());
                    }
                    self.groups[id].nested = nested;
                    id
                })
            });
            self.by_block.push(group);
            if let Some(group) = group {
                let group = &mut self.groups[group];
                group.members.push(index);
            }
        }
        self.groups.truncate(count);
        for group in &mut self.groups {
            // 单成员的嵌套段退回单块渲染，不参与批次。
            if group.nested && group.members.len() == 1 {
                self.by_block[group.members[0]] = None;
                continue;
            }
            let inputs = || {
                group
                    .members
                    .iter()
                    .map(|&index| (index, blocks[index].generation, blocks[index].layout_epoch))
            };
            if group.previous.iter().copied().ne(inputs()) {
                group.generation = crate::transcript::next_layout_epoch();
                group.previous.clear();
                group.previous.extend(inputs());
            }
            debug_assert_eq!(group.previous.len(), group.members.len());
            for &index in &group.members {
                generations[index] = group.generation;
            }
        }
    }

    fn tools<'a>(&self, index: usize, app: &'a App) -> Option<Vec<&'a ToolCallBlock>> {
        let group = &self.groups[self.by_block[index]?];
        debug_assert!(!group.members.is_empty());
        if group.members[0] != index {
            return Some(Vec::new());
        }
        Some(
            group
                .members
                .iter()
                .map(|&member| {
                    debug_assert!(member < app.blocks.len());
                    let Block::Tool(tool) = &app.blocks[member].block else {
                        unreachable!("批次索引只保存工具块");
                    };
                    tool.as_ref()
                })
                .collect(),
        )
    }

    /// 块的版式代表：批次组的全部成员都以最后一个成员的树形状为准
    /// （整组的 `more` 与 `gap` 由它代表）。
    fn layout_owner(&self, index: usize) -> usize {
        self.by_block
            .get(index)
            .copied()
            .flatten()
            .and_then(|group| self.groups[group].members.last().copied())
            .unwrap_or(index)
    }
}

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

impl TreeIndex {
    const MAX_DEPTH: u8 = 32;
    const EMPTY: TreeLayout = TreeLayout {
        shape: None,
        gap: true,
    };

    fn sync(&mut self, blocks: &[Entry]) {
        // 调用 id → 块下标：父链解析与父块 running 读取共用。
        let mut index_of = std::collections::HashMap::new();
        for (index, entry) in blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                index_of.insert(tool.id.as_str(), index);
            }
        }
        // 逆序：「已在后面出现过子块的 parent_id」集合，标出后面还有兄弟的子块；
        // 同时记下每个块后面是否已有子块（父块不留尾随空行）。
        let mut seen = std::collections::HashSet::new();
        let mut later_sibling = vec![false; blocks.len()];
        let mut has_children = vec![false; blocks.len()];
        for (index, entry) in blocks.iter().enumerate().rev() {
            let Block::Tool(tool) = &entry.block else {
                continue;
            };
            has_children[index] = seen.contains(tool.id.as_str());
            let Some(parent) = tool.parent_id.as_deref() else {
                continue;
            };
            later_sibling[index] = seen.contains(parent);
            seen.insert(parent);
        }
        // 正序：深度沿父链 +1，祖先的 `more` 标志压进位掩码；
        // 父块缺席（或排在后面）时按深度 1 处理。
        self.layouts.clear();
        self.layouts.resize(blocks.len(), Self::EMPTY);
        for (index, entry) in blocks.iter().enumerate() {
            let Block::Tool(tool) = &entry.block else {
                continue;
            };
            let shape = tool.parent_id.as_deref().map(|parent_id| {
                let parent = index_of.get(parent_id).copied().filter(|&p| p < index);
                let (depth, rails) =
                    parent
                        .and_then(|p| self.layouts[p].shape)
                        .map_or((1, 0), |shape| {
                            let depth = shape.depth.saturating_add(1).min(Self::MAX_DEPTH);
                            let rails = shape.rails | u32::from(shape.more) << (depth - 2);
                            (depth, rails)
                        });
                let parent_running = parent
                    .is_some_and(|p| matches!(&blocks[p].block, Block::Tool(tool) if tool.running));
                TreeShape {
                    depth,
                    rails,
                    more: later_sibling[index] || parent_running,
                }
            });
            let gap =
                !has_children[index] && shape.is_none_or(|shape| !shape.more && shape.rails == 0);
            self.layouts[index] = TreeLayout { shape, gap };
        }
    }

    fn layout(&self, index: usize) -> TreeLayout {
        self.layouts.get(index).copied().unwrap_or(Self::EMPTY)
    }
}

/// 块用于版式判定的树布局：批次组取最后一个成员的形状，其余块取自身。
fn block_layout(batches: &BatchIndex, tree: &TreeIndex, index: usize) -> TreeLayout {
    tree.layout(batches.layout_owner(index))
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
            let (lines, footer, paged, tables, images, height, rendered) = if index >= eager_start {
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
        sinks: PaintSinks<'_>,
    ) {
        let PaintSinks {
            hits,
            charts,
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

/// 绘制时的登记目标：标题命中、表格入口、位图槽位与行文本。
///
/// 打包成结构体传参：四个 `&mut Vec` 平铺在签名里既难读，也容易传错顺序。
pub struct PaintSinks<'a> {
    /// 块标题行命中（点击折叠）。
    pub hits: &'a mut Vec<(usize, u16, u16)>,
    /// 表格入口命中（点击切换图表）。
    pub charts: &'a mut Vec<ChartHit>,
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

fn estimate_entry_height_indexed(
    app: &App,
    index: usize,
    width: u16,
    batches: &BatchIndex,
    tree: &TreeIndex,
) -> usize {
    let Some(entry) = app.blocks.get(index) else {
        return 0;
    };
    // 含表格 / 图片 / mermaid 的正文不能用折行近似：表格列宽收缩、入口按钮、
    // 图表占位行、图片占位行和字符图的行数都是渲染期才算出来的，估错会让滚动
    // 范围与懒渲染的可见区间一起失真。
    if let Block::Assistant { text } = &entry.block
        && (may_contain_table(text) || may_contain_image(text) || may_contain_mermaid(text))
    {
        return render_entry_with(index, entry, app, width, false, batches, tree).height;
    }
    let level = entry.level(app.block_level(&entry.block));
    let layout = block_layout(batches, tree, index);
    let Some(tools) = batches.tools(index, app) else {
        return estimate_entry_height(entry, app, level, width, layout);
    };
    if tools.is_empty() {
        return 0;
    }
    estimate_tool_batch_height(app, &tools, level, usize::from(width), layout)
}

/// 成组渲染的行数：组头 + 每个成员的（摘要行 + 正文行）+ 尾随留白（树尾才有）。
///
/// 组只在组首块渲染，其余成员高度为零；`layout` 是最后一个成员的树布局。
fn estimate_tool_batch_height(
    app: &App,
    tools: &[&ToolCallBlock],
    level: DetailLevel,
    width: usize,
    layout: TreeLayout,
) -> usize {
    // 成员都画在组头下一层：可用宽度固定为 width − 3×成员深度 − TITLE_INDENT。
    let member_depth = usize::from(member_shape(layout.shape, false).depth);
    let member_width = width.saturating_sub(3 * member_depth);
    let mut rows = 1 + usize::from(layout.gap);
    for tool in tools {
        rows += 1;
        if !tool.has_body() {
            continue;
        }
        let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        let tail = batch_member_tail(tool, app.theme);
        let budget = batch_summary_budget(&tail, member_width);
        let (_, in_title) = title_summary(tool, &args, budget, level, app.theme, app.cwd());
        rows += tool_body_rows(
            tool,
            &args,
            app,
            member_width.saturating_sub(TITLE_INDENT),
            level,
            in_title,
        );
    }
    rows
}

/// 懒块的估算高度：与各渲染分支同结构（标题行 + 内容行 + 尾注 + 尾随留白）。
///
/// 工具结果的截断规则、折叠态与展开态都与渲染共用同一套判定；正文按显示宽度折行
/// 近似 markdown 的落行数。估算与渲染差一个数量级会让滚动范围、滚动条与懒渲染的
/// 可见区间同时失真。
fn estimate_entry_height(
    entry: &Entry,
    app: &App,
    level: DetailLevel,
    width: u16,
    layout: TreeLayout,
) -> usize {
    let width = usize::from(width).max(8);
    match &entry.block {
        // 纯文本折行很便宜：直接复用渲染，估高与渲染恒等。
        Block::User { text } => user_message(text, entry.at_ms, width, app.theme).len(),
        Block::Note { text, error } => note_block(text, *error, width, app.theme).len(),
        Block::Assistant { text } => wrapped_rows(text, prose_width(width))
            .saturating_add(usize::from(entry.elapsed_ms.is_some()))
            .saturating_add(1),
        Block::Thinking { text, .. } => {
            let width = prose_width(width);
            match level {
                DetailLevel::Hidden => 2,
                // 简略档跳过段间空行，超出预算再补一行省略提示
                DetailLevel::Brief => {
                    let rows: usize = text
                        .lines()
                        .filter(|line| !line.trim().is_empty())
                        .map(|line| wrapped_rows(line, width))
                        .sum();
                    rows.min(BRIEF_THINKING_ROWS) + usize::from(rows > BRIEF_THINKING_ROWS) + 2
                }
                DetailLevel::Full => wrapped_rows(text, width).saturating_add(2),
            }
        }
        Block::Tool(tool) => estimate_tool_height(tool, app, level, width, layout),
        Block::AgentMessage { body, .. } | Block::GoalRound { body, .. } => {
            // 与 `agent_message_block` 同源：简略档是预览行数 + 省略行，隐藏档只剩标题行。
            let rows = wrapped_text_rows(body, prose_width(width)).len();
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
    }
}

/// 块正文的可用宽度：去掉 [`PROSE_INDENT`]，右侧不再另留边距（滚动区已留 `MARGIN_X`）。
///
/// 所有块的正文（用户、助手、工具输出、IRC、Note、Todo 条目）都按这个宽度折行/截断，
/// 左缘与右缘因此只有一套网格。
fn prose_width(width: usize) -> usize {
    width.saturating_sub(PROSE_INDENT)
}

/// 正文里是否可能出现 GFM 表格：分隔行（只有 `|`、`-`、`:`、空白且含 `-`）。
///
/// 只做廉价预筛，宁可多算（走一次真实渲染），不能漏判——漏判会破坏
/// 「估算高度 == 渲染高度」这条被测试钉住的不变量。
fn may_contain_table(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim();
        trimmed.contains('-')
            && trimmed
                .chars()
                .all(|ch| matches!(ch, '|' | '-' | ':' | ' ' | '\t'))
    })
}

/// 正文里是否可能出现内联图片：`![](…)` 或 SVG 代码块。
///
/// 与 [`may_contain_table`] 同理：只做廉价预筛，宁可多渲染一次。
/// SVG 的判定必须与 [`crate::render::markdown`] 里实际出图的条件一致——
/// 围栏支持 ` ``` ` 与 `~~~`、info 前可有空格、大小写不敏感；只要这里漏判，
/// 懒块的估高就会用折行近似顶替真实占位行，滚动范围随之失真。
fn may_contain_image(text: &str) -> bool {
    text.lines().any(|line| {
        if line.contains("![") {
            return true;
        }
        let trimmed = line.trim_start();
        let fence_len = trimmed
            .chars()
            .take_while(|ch| *ch == '`' || *ch == '~')
            .count();
        fence_len >= 3 && crate::render::markdown::is_svg_fence_info(&trimmed[fence_len..])
    })
}

/// 正文里是否可能出现 mermaid 围栏。口径与 [`may_contain_image`] 相同：漏判会让
/// 字符图的行数被折行近似顶替。
fn may_contain_mermaid(text: &str) -> bool {
    text.lines().any(|line| {
        let trimmed = line.trim_start();
        let fence_len = trimmed
            .chars()
            .take_while(|ch| *ch == '`' || *ch == '~')
            .count();
        fence_len >= 3 && crate::render::markdown::is_mermaid_fence_info(&trimmed[fence_len..])
    })
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
    let clamped = clamp_rows(message, width, limit, BodyEdge::Head);
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

/// 工具块高度：标题 + 正文行 + 尾随留白（树尾才有），与 [`render_tool`] 同规则。
fn estimate_tool_height(
    tool: &ToolCallBlock,
    app: &App,
    level: DetailLevel,
    width: usize,
    layout: TreeLayout,
) -> usize {
    let gap = usize::from(layout.gap);
    if !shows_tool_body(tool, level) {
        return 1 + gap;
    }
    let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
    let nested = layout.shape.is_some();
    let depth = layout.shape.map_or(0, |shape| usize::from(shape.depth));
    let width = width.saturating_sub(3 * depth);
    let budget = tool_summary_budget(tool_title(tool, &args), tool, app.theme, width, nested);
    let (_, in_title) = title_summary(tool, &args, budget, level, app.theme, app.cwd());
    let indent = if nested { TITLE_INDENT } else { PROSE_INDENT };
    let body = tool_body_rows(
        tool,
        &args,
        app,
        width.saturating_sub(indent),
        level,
        in_title,
    );
    body + 1 + gap
}

/// 正文是否需要渲染；固定摘要独立于全局档位与手动展开状态。
fn shows_tool_body(tool: &ToolCallBlock, level: DetailLevel) -> bool {
    let policy = tool_body_policy(&tool.name);
    tool.has_body()
        && !(policy == ToolBody::FullOnly && !tool_is_error(tool) && level != DetailLevel::Full)
        && !(policy == ToolBody::Todo && level == DetailLevel::Hidden)
}

/// 工具是否以失败终态结束；运行中（`status: None`）不算失败。
fn tool_is_error(tool: &ToolCallBlock) -> bool {
    tool.status
        .is_some_and(|status| status != moc_protocol::methods::ToolStatus::Completed)
}

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

/// `spawn_images = false` 用于估高：只查缓存，绝不触发视口外图片解码。
fn render_entry_with(
    index: usize,
    entry: &Entry,
    app: &App,
    width: u16,
    spawn_images: bool,
    batches: &BatchIndex,
    tree: &TreeIndex,
) -> EntryLines {
    // 估高路径统一按「至少 8 列」计算宽度；渲染必须用同一口径，否则超窄
    // 终端下图片版式（可容纳列数 → 行数）在两侧会算出不同的块高。
    let width = width.max(8);
    let level = entry.level(app.block_level(&entry.block));
    let layout = block_layout(batches, tree, index);
    if let Some(tools) = batches.tools(index, app) {
        if tools.is_empty() {
            return EntryLines::plain(Vec::new());
        }
        return render_tool_batch(app, &tools, usize::from(width), level, spawn_images, layout);
    }
    render_entry_single(entry, app, usize::from(width), level, spawn_images, layout)
}

fn render_entry_single(
    entry: &Entry,
    app: &App,
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    layout: TreeLayout,
) -> EntryLines {
    let theme = app.theme;
    match &entry.block {
        Block::User { text } => EntryLines::plain(user_message(text, entry.at_ms, width, theme)),
        Block::Assistant { text } => assistant_message(text, entry, app, width, spawn_images),
        Block::Thinking {
            text,
            reasoning_tokens,
            duration_ms,
        } => EntryLines::plain(thinking_block(
            text,
            *reasoning_tokens,
            *duration_ms,
            app,
            width,
            level,
        )),
        Block::Tool(tool) => render_tool(tool, app, width, level, spawn_images, layout),
        Block::AgentMessage { meta, body } => {
            EntryLines::plain(agent_message_block(meta, body, width, theme, level))
        }
        Block::GoalRound { meta, body } => {
            EntryLines::plain(goal_round_block(meta, body, width, theme, level))
        }
        Block::Note { text, error } => EntryLines::plain(note_block(text, *error, width, theme)),
    }
}

/// 成员行尾的状态列：失败 `✘` 与耗时（todo 是完成进度）；无正文的记账工具（skill 等）不报。
fn batch_member_tail(tool: &ToolCallBlock, theme: &Theme) -> Vec<Span<'static>> {
    let mut tail = Vec::new();
    if tool_is_error(tool) {
        tail.push(Span::styled("✘", Style::new().fg(theme.accent_error)));
    }
    if tool.has_body() {
        let spans = tool_title_tail(tool, theme);
        if !tail.is_empty() && !spans.is_empty() {
            tail.push(Span::raw(" "));
        }
        tail.extend(spans);
    }
    tail
}

/// 成员摘要的列宽预算：让出行首缩进与行尾状态列（状态列前至少两格间距）。
///
/// `width` 是成员行在连线前缀之内的可用宽度。
fn batch_summary_budget(tail: &[Span<'_>], width: usize) -> usize {
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let reserved = if tail_width > 0 { tail_width + 2 } else { 0 };
    width.saturating_sub(TITLE_INDENT + reserved)
}

/// 嵌套连线的行首前缀：一个空格 + 祖先层各 3 格（`│  `／`   `）+ 本层连接符
/// （首行 `├─`／`└─`，续行 `│ `／`  `）；行尾不留空格，空行由调用方 `trim_end`。
fn tree_prefix(shape: TreeShape, first: bool) -> String {
    let mut prefix = String::from(" ");
    for level in 0..usize::from(shape.depth).saturating_sub(1) {
        prefix.push_str(if shape.rails & (1 << level) != 0 {
            "│  "
        } else {
            "   "
        });
    }
    prefix.push_str(match (first, shape.more) {
        (true, true) => "├─",
        (true, false) => "└─",
        (false, true) => "│ ",
        (false, false) => "  ",
    });
    prefix
}

/// 给一段行加树形连线前缀：首行连接符、续行竖线；空行只补 trim_end 后的竖线段。
fn apply_tree_prefix(lines: &mut [Line<'static>], shape: TreeShape, rail: Style) {
    for (index, line) in lines.iter_mut().enumerate() {
        let prefix = tree_prefix(shape, index == 0);
        if line.spans.is_empty() {
            let prefix = prefix.trim_end();
            if !prefix.is_empty() {
                line.spans.push(Span::styled(prefix.to_owned(), rail));
            }
        } else {
            line.spans.insert(0, Span::styled(prefix, rail));
        }
    }
}

/// 批次成员的树形状：组头的下一层，`rails` 继承组头自己的 `more`，
/// 成员的 `more` 只看是否末位成员。
fn member_shape(head: Option<TreeShape>, last: bool) -> TreeShape {
    match head {
        Some(head) => TreeShape {
            depth: head.depth.saturating_add(1).min(TreeIndex::MAX_DEPTH),
            rails: head.rails | u32::from(head.more) << (head.depth - 1),
            more: !last,
        },
        None => TreeShape {
            depth: 1,
            rails: 0,
            more: !last,
        },
    }
}

/// 同名工具的成组渲染：顶层批次与嵌套连续同名子调用共用这棵树。
///
/// 组头 = bullet（仅顶层）+ 工具名 + ` (N)`；有失败或运行中的成员时右端补
/// `{done}/{N} done` 统计（失败、运行中另行点明）。成员画在组头下一层：
/// `{连线前缀}` + 摘要 + 右端状态列，正文悬挂一格与摘要同列。
/// `layout` 取组内最后一个成员的树布局（它的 `more` 与 `gap` 才代表整组）。
fn render_tool_batch(
    app: &App,
    tools: &[&ToolCallBlock],
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    layout: TreeLayout,
) -> EntryLines {
    use moc_protocol::methods::ToolStatus;

    let theme = app.theme;
    let head = layout.shape;
    let name = tools.first().map_or("tool", |tool| tool.name.as_str());
    let head_width = width.saturating_sub(3 * head.map_or(0, |shape| usize::from(shape.depth)));
    let count = |pick: fn(&ToolCallBlock) -> bool| tools.iter().filter(|tool| pick(tool)).count();
    let done = count(|tool| tool.status == Some(ToolStatus::Completed));
    let failed = count(tool_is_error);
    let running = count(|tool| tool.running);
    let (bullet, bullet_color) = if running > 0 {
        ("▸", theme.accent_running)
    } else if failed == tools.len() {
        ("✘", theme.accent_error)
    } else {
        ("◆", theme.accent_tool)
    };
    let mut title = vec![Span::raw(" ".repeat(TITLE_INDENT))];
    if head.is_none() {
        title.push(Span::styled(
            format!("{bullet} "),
            Style::new().fg(bullet_color),
        ));
    }
    title.push(Span::styled(
        name.to_owned(),
        Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
    ));
    title.push(Span::styled(
        format!(" ({})", tools.len()),
        Style::new().fg(theme.gray_bright),
    ));
    // 组尾统计挪进组头右端：只在有失败或运行中时才占列。
    if failed > 0 || running > 0 {
        let mut tail = vec![Span::styled(
            format!("{done}/{} {}", tools.len(), labels::status::DONE),
            Style::new().fg(theme.gray_dim),
        )];
        if failed > 0 {
            tail.push(Span::styled(
                format!("{}{failed} {}", labels::SEP, labels::status::FAILED),
                Style::new().fg(theme.accent_error),
            ));
        }
        if running > 0 {
            tail.push(Span::styled(
                format!("{}{running} {}", labels::SEP, labels::status::RUNNING),
                Style::new().fg(theme.accent_running),
            ));
        }
        let tail_width: usize = tail.iter().map(Span::width).sum();
        let used: usize = title.iter().map(Span::width).sum();
        let gap = head_width.saturating_sub(used + tail_width).max(2);
        title.push(Span::raw(" ".repeat(gap)));
        title.extend(tail);
    }
    let mut lines = vec![clip_line(Line::from(title), head_width)];
    let rail = tree_rail_style(theme);
    if let Some(head) = head {
        // 嵌套组的组头按最后一棒的树形状接线（├─ 表示后面还有兄弟或父调用还在跑）。
        lines[0]
            .spans
            .insert(0, Span::styled(tree_prefix(head, true), rail));
    }
    let mut images = Vec::new();
    let last = tools.len() - 1;
    for (offset, tool) in tools.iter().enumerate() {
        let shape = member_shape(head, offset == last);
        let depth = usize::from(shape.depth);
        let width = width.saturating_sub(3 * depth);
        let args = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);
        let tail = batch_member_tail(tool, theme);
        let (summary, in_title) = title_summary(
            tool,
            &args,
            batch_summary_budget(&tail, width),
            level,
            theme,
            app.cwd(),
        );
        let tail_width: usize = tail.iter().map(Span::width).sum();
        let mut spans = vec![Span::raw(" ".repeat(TITLE_INDENT))];
        spans.extend(summary);
        if tail_width > 0 {
            let used: usize = spans.iter().map(Span::width).sum();
            let gap = width.saturating_sub(used + tail_width).max(2);
            spans.push(Span::raw(" ".repeat(gap)));
            spans.extend(tail);
        }
        let mut member_lines = vec![clip_line(Line::from(spans), width)];
        let mut placements = Vec::new();
        if tool.has_body() {
            let body_width = width.saturating_sub(TITLE_INDENT);
            let (body, body_placements) =
                tool_body(tool, &args, app, body_width, level, spawn_images, in_title);
            let base = member_lines.len();
            member_lines.extend(
                body.into_iter()
                    .map(|line| hang(line, TITLE_INDENT, body_width)),
            );
            placements = body_placements;
            for placement in &mut placements {
                placement.row += base;
                placement.x = placement
                    .x
                    .saturating_add(u16::try_from(TITLE_INDENT + 3 * depth).unwrap_or(u16::MAX));
            }
        }
        apply_tree_prefix(&mut member_lines, shape, rail);
        lines.extend(member_lines);
        images.extend(placements);
    }
    // 树尾才补一行纯空行（无连线前缀）。
    if layout.gap {
        lines.push(Line::default());
    }
    EntryLines {
        height: lines.len(),
        paged: None,
        lines,
        tables: Vec::new(),
        images,
    }
}

/// 普通正文的展示行上限；隐藏档为 0（计数型工具不进正文，数量在标题尾列）。
fn content_limit(level: DetailLevel) -> usize {
    match level {
        DetailLevel::Hidden => 0,
        DetailLevel::Brief => BRIEF_BODY_ROWS,
        DetailLevel::Full => FULL_BODY_ROWS,
    }
}

/// 用户消息：`>` 落在块标记列，正文落在正文列，右侧本地时间；不使用面板边框。
fn user_message(text: &str, at_ms: Option<u64>, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let rows = wrap_text(text, prose_width(width));
    let last = rows.len().saturating_sub(1);
    let mut clock_pending = at_ms.and_then(format_clock);
    let mut lines: Vec<Line<'static>> = Vec::with_capacity(rows.len() + 2);
    for (index, row) in rows.into_iter().enumerate() {
        let mut spans = vec![Span::raw(" ".repeat(TITLE_INDENT))];
        spans.push(if index == 0 {
            Span::styled("> ", Style::new().fg(theme.gray_bright))
        } else {
            Span::raw("  ")
        });
        let used = PROSE_INDENT + str_width(&row);
        spans.extend(crate::render::chip_spans(
            &row,
            theme.text_primary,
            theme.accent_warning,
        ));
        if index == last
            && let Some(clock) =
                clock_pending.take_if(|clock| width.saturating_sub(used + str_width(clock)) >= 2)
        {
            spans.push(Span::raw(" ".repeat(width - used - str_width(&clock))));
            spans.push(Span::styled(clock, Style::new().fg(theme.gray_dim)));
        }
        lines.push(Line::from(spans));
    }
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
fn assistant_message(
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
        },
        prompt_footer(entry, width, theme),
    )
}

fn assistant_layout(
    text: &str,
    width: usize,
    theme: &Theme,
    views: RenderViews<'_>,
    footer: Option<Line<'static>>,
) -> EntryLines {
    if text.len() > 256 * 1024 {
        let rendered = super::markdown::render_markdown_pages(
            text,
            prose_width(width).max(8),
            theme,
            views,
            true,
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
fn prompt_footer(entry: &Entry, width: usize, theme: &Theme) -> Option<Line<'static>> {
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
fn thinking_block(
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
            format!("  {}", labels::hints(&[(binding, "展开")], usize::MAX)),
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
fn thinking_tokens_per_second(tokens: u64, duration_ms: u64) -> Option<u64> {
    (duration_ms > 0)
        .then(|| tokens.saturating_mul(1_000).saturating_add(duration_ms / 2) / duration_ms)
}

/// 思考正文的引文带行：`▏ ` 落在块标记列，正文因此落在正文列；文字一律低对比度。
fn thinking_rows(rows: Vec<Line<'static>>, theme: &Theme) -> Vec<Line<'static>> {
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

/// 一行的纯文本（不含样式）。
fn plain_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
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
    let mut lines = vec![clip_line(
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

/// goal 续跑轮卡片：标题给 `goal` 与轮次，正文同代理消息卡片排版。
fn goal_round_block(
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
                "goal ".to_owned(),
                Style::new()
                    .fg(theme.accent_running)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(
                format!("round {}{}", meta.round, labels::SEP),
                Style::new().fg(theme.text_primary),
            ),
            Span::styled("auto".to_owned(), Style::new().fg(theme.gray_bright)),
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

/// 系统提示：错误 `✘`、普通 `»` 落在块标记列，正文落在正文列。
fn note_block(text: &str, error: bool, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let (marker, color) = if error {
        ("✘ ", theme.accent_error)
    } else {
        ("» ", theme.gray_bright)
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

/// 正文行右移 `indent` 列，并截到 `width` 列（超宽以 `…` 收尾）：块正文落位的唯一出口。
///
/// 空行保持为空，块尾留白因此不会变成一串空格。
fn hang(line: Line<'static>, indent: usize, width: usize) -> Line<'static> {
    let mut line = clip_line(line, width);
    if indent > 0 && line.width() > 0 {
        line.spans.insert(0, Span::raw(" ".repeat(indent)));
    }
    line
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

/// 树形连线的统一颜色：批次树、嵌套连线、todo 树、JSON 树共用。
///
/// `theme.border` 在 #141414 底上几乎看不见，统一用高一档的 `gray_dim`。
fn tree_rail_style(theme: &Theme) -> Style {
    Style::new().fg(theme.gray_dim)
}

/// 工具标题名：自定义标题（`IRC`、`Sidekick`、`! bash`）统一在此决定。
fn tool_title<'a>(tool: &'a ToolCallBlock, args: &Value) -> &'a str {
    match tool.name.as_str() {
        "message_agent" => labels::title::IRC,
        "sidekick" => labels::title::SIDEKICK,
        _ => match args.get("exclude_from_context").and_then(Value::as_bool) {
            Some(true) => "!! bash · 不进入上下文",
            Some(false) => "! bash",
            None => &tool.name,
        },
    }
}

/// 标题行右端列：已完成调用的耗时（todo 是完成进度）；运行中没有尾部。
///
/// todo 的 `done/total` 里 done 计「已关闭」（completed + abandoned），
/// 全部关闭时给成功色。
fn tool_title_tail(tool: &ToolCallBlock, theme: &Theme) -> Vec<Span<'static>> {
    if tool.name == "todo" {
        let Some(result) = tool.result.as_deref() else {
            return Vec::new();
        };
        let phases = moc_protocol::methods::parse_todos(result);
        let mut done = 0usize;
        let mut total = 0usize;
        for item in phases.iter().flat_map(|phase| &phase.items) {
            total += 1;
            done += usize::from(matches!(
                item.status,
                moc_protocol::methods::TodoStatus::Completed
                    | moc_protocol::methods::TodoStatus::Abandoned
            ));
        }
        if total == 0 {
            return Vec::new();
        }
        return vec![Span::styled(
            format!("{done}/{total}"),
            Style::new().fg(if done == total {
                theme.accent_success
            } else {
                theme.gray_dim
            }),
        )];
    }
    let mut tail: Vec<Span<'static>> = Vec::new();
    // grep／find 的计数进尾列：非完整档没有正文，计数不能在转写里消失。
    if !tool_is_error(tool)
        && let Some(result) = tool.result.as_deref()
        && let Some(text) = tool_count_text(tool, result)
    {
        tail.push(Span::styled(text, Style::new().fg(theme.gray_dim)));
    }
    if let Some(ms) = tool.duration_ms {
        if !tail.is_empty() {
            tail.push(Span::styled(
                labels::SEP.to_owned(),
                Style::new().fg(theme.gray_dim),
            ));
        }
        tail.push(Span::styled(
            labels::duration(ms),
            Style::new().fg(theme.gray_dim),
        ));
    }
    tail
}

/// 单卡标题行留给参数摘要的列宽：让出标记、bullet（子调用无 bullet）、
/// 工具名、一格间距与右端尾列。
fn tool_summary_budget(
    name: &str,
    tool: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    nested: bool,
) -> usize {
    let tail = if nested {
        batch_member_tail(tool, theme)
    } else {
        tool_title_tail(tool, theme)
    };
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let reserved = if tail_width > 0 { tail_width + 2 } else { 0 };
    width.saturating_sub(TITLE_INDENT + usize::from(!nested) * 2 + str_width(name) + 1 + reserved)
}

/// 标题摘要（已按 `budget` 截断的分段着色 span），以及 bash 命令是否已整条放进标题。
///
/// bash 命令只出现一次：没有描述时命令默认进标题（单行截断），正文不再重复 `$` 行；
/// 只有完整档且命令放不下（多行或超宽）时才留空标题、交给正文的 `$` 行完整折行展示。
/// 有描述时标题给描述，命令同样交给正文。
fn title_summary(
    tool: &ToolCallBlock,
    args: &Value,
    budget: usize,
    level: DetailLevel,
    theme: &Theme,
    cwd: &str,
) -> (Vec<Span<'static>>, bool) {
    if tool.name == "bash" && ui_description(args).is_none() {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let fits = !command.contains('\n') && str_width(command) <= budget;
        if fits || level != DetailLevel::Full {
            let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
            let line = clip_line(
                Line::from(Span::styled(flat, Style::new().fg(theme.command))),
                budget,
            );
            return (line.spans, true);
        }
        return (Vec::new(), false);
    }
    (
        clip_line(Line::from(summary_spans(tool, args, theme, cwd)), budget).spans,
        false,
    )
}

/// 标题摘要的分段着色：路径、行号范围、pattern、改动计数各给各的色；
/// 路径一律过 [`super::display_path`] 相对化。
///
/// 产出未截断的 span 序列；截断由调用方用 [`clip_line`] 统一完成。
fn summary_spans(
    tool: &ToolCallBlock,
    args: &Value,
    theme: &Theme,
    cwd: &str,
) -> Vec<Span<'static>> {
    let styled =
        |text: String, color: ratatui::style::Color| Span::styled(text, Style::new().fg(color));
    let arg_str = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let path_str = |key: &str| super::display_path(&arg_str(key), cwd).into_owned();
    let glob_str = |key: &str| {
        args.get(key)
            .and_then(Value::as_array)
            .map(|globs| {
                globs
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|glob| super::display_path(glob, cwd).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    };
    match tool.name.as_str() {
        "edit" | "write" => {
            let mut spans = vec![styled(path_str("path"), theme.path)];
            if let Some(diff) = &tool.diff {
                let stats = diff::stats(diff);
                // 空 diff 不渲染 +0/-0；截断时计数只是已交付的部分，
                // 用 … 标明，避免把部分值当成完整统计。
                if stats.insertions + stats.deletions > 0 {
                    spans.push(styled(
                        format!(" +{}", stats.insertions),
                        theme.accent_success,
                    ));
                    spans.push(styled(format!(" -{}", stats.deletions), theme.accent_error));
                    if diff.truncated {
                        spans.push(styled(" …".to_owned(), theme.gray_dim));
                    }
                }
            }
            spans
        }
        // 描述加粗：bash／run_code 的摘要不是参数值而是模型写的一句话，
        // 加粗后才不会和后面的 path／pattern 参数混成一列。
        "bash" | "run_code" => vec![Span::styled(
            ui_description(args).unwrap_or_default().to_owned(),
            Style::new()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD),
        )],
        "read" => {
            let offset = args.get("offset").and_then(Value::as_u64);
            let limit = args.get("limit").and_then(Value::as_u64);
            let range = match (offset, limit) {
                (Some(offset), Some(limit)) => format!(":{offset}-{}", offset + limit),
                (Some(offset), None) => format!(":{offset}-"),
                _ => String::new(),
            };
            let globs = glob_str("glob");
            let globs = if globs.is_empty() {
                String::new()
            } else {
                format!(" {globs}")
            };
            vec![
                styled(path_str("path"), theme.path),
                styled(range, theme.gray),
                styled(globs, theme.gray),
            ]
        }
        "grep" => vec![styled(arg_str("pattern"), theme.text_secondary)],
        "find" => vec![styled(glob_str("glob"), theme.path)],
        "web_fetch" => vec![styled(
            args.get("urls")
                .and_then(Value::as_array)
                .map_or_default(|urls| {
                    urls.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" · ")
                }),
            theme.path,
        )],
        "spawn_agent" => vec![styled(arg_str("title"), theme.text_secondary)],
        "yield" => vec![styled("交付报告".to_owned(), theme.text_secondary)],
        "skill" => vec![styled(arg_str("name"), theme.text_secondary)],
        "clm_context" => vec![styled(arg_str("op"), theme.text_secondary)],
        "stop_agent" => vec![styled(arg_str("agent_id"), theme.text_secondary)],
        "todo" => todo_summary_spans(args, theme),
        "TodoReminder" => vec![styled(
            "隐式提醒：同步待办进度".to_owned(),
            theme.text_secondary,
        )],
        "job" | "plan" | "exit_plan" => Vec::new(),
        "message_agent" | "sidekick" | "wait_agent" | "ask" => {
            vec![styled(tool_summary(tool, args), theme.text_secondary)]
        }
        _ => {
            // 未知工具（MCP 等）：参数截断展示
            let text = tool.arguments.replace('\n', " ");
            vec![styled(text.chars().take(60).collect(), theme.gray)]
        }
    }
}

/// todo 摘要：op + 目标——start/done/drop/block/unblock/rm 跟 task 或 phase 文本，
/// init 跟 `N tasks`，append 跟 `phase · N items`；op 缺失时与 core 的
/// `infer_op` 同规则推断（非空 list → init，带 phase 的非空 items → append，否则 init）。
fn todo_summary_spans(args: &Value, theme: &Theme) -> Vec<Span<'static>> {
    let styled =
        |text: String, color: ratatui::style::Color| Span::styled(text, Style::new().fg(color));
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let list = args.get("list").and_then(Value::as_array);
            let items = args.get("items").and_then(Value::as_array);
            let phase = args.get("phase").and_then(Value::as_str);
            if list.is_some_and(|list| !list.is_empty()) {
                "init"
            } else if items.is_some_and(|items| !items.is_empty())
                && phase.is_some_and(|phase| !phase.is_empty())
            {
                "append"
            } else {
                "init"
            }
            .to_owned()
        });
    let mut spans = vec![styled(op.clone(), theme.text_secondary)];
    let arg_str = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    match op.as_str() {
        "init" => {
            let count = args
                .get("list")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|phase| {
                            phase
                                .get("items")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len)
                        })
                        .sum()
                })
                .or_else(|| args.get("items").and_then(Value::as_array).map(Vec::len));
            if let Some(count) = count {
                spans.push(styled(format!(" {count} tasks"), theme.gray));
            }
        }
        "append" => {
            let phase = arg_str("phase");
            if !phase.is_empty() {
                spans.push(styled(format!(" {phase}"), theme.text_primary));
            }
            if let Some(items) = args.get("items").and_then(Value::as_array) {
                spans.push(styled(format!(" · {} items", items.len()), theme.gray));
            }
        }
        "start" | "done" | "drop" | "block" | "unblock" | "rm" => {
            let target = {
                let task = arg_str("task");
                if task.is_empty() {
                    arg_str("phase")
                } else {
                    task
                }
            };
            if !target.is_empty() {
                spans.push(styled(format!(" {target}"), theme.text_primary));
            }
        }
        _ => {}
    }
    spans
}

/// 工具块：标题行 + 正文 + 尾随留白（树尾才有）。
///
/// 标题落在块标记列，耗时在标题行右端（todo 是 `done/total` 进度），正文落在正文列；
/// 收起态（无正文、read 非完整档）只留标题行。嵌套调用（PTC）按 [`TreeShape`] 加
/// 连线前缀：子调用不画 bullet（连接符本身就是标记），正文与工具名首字对齐。
fn render_tool(
    tool: &ToolCallBlock,
    app: &App,
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    layout: TreeLayout,
) -> EntryLines {
    let theme = app.theme;
    let nested = layout.shape.is_some();
    let depth = layout.shape.map_or(0, |shape| usize::from(shape.depth));
    let width = width.saturating_sub(3 * depth);
    let args: Value = serde_json::from_str(&tool.arguments).unwrap_or(Value::Null);

    let (bullet, bullet_color) = match (tool.running, tool.status) {
        (true, _) => ("▸", theme.accent_running),
        (_, Some(moc_protocol::methods::ToolStatus::Completed)) => ("◆", theme.accent_tool),
        (_, Some(_)) => ("✘", theme.accent_error),
        (_, None) => ("◆", theme.accent_tool),
    };
    let name = tool_title(tool, &args);
    // 子调用没有 bullet，失败标记挪到尾列的 ✘（与批次成员同一套尾列）。
    let tail = if nested {
        batch_member_tail(tool, theme)
    } else {
        tool_title_tail(tool, theme)
    };
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let (summary, in_title) = title_summary(
        tool,
        &args,
        tool_summary_budget(name, tool, theme, width, nested),
        level,
        theme,
        app.cwd(),
    );
    let mut title = vec![Span::raw(" ".repeat(TITLE_INDENT))];
    if !nested {
        title.push(Span::styled(
            format!("{bullet} "),
            Style::new().fg(bullet_color),
        ));
    }
    title.push(Span::styled(
        name.to_owned(),
        Style::new().fg(bullet_color).add_modifier(Modifier::BOLD),
    ));
    if !summary.is_empty() {
        title.push(Span::raw(" "));
        title.extend(summary);
    }
    // 后台任务标记（job_id 只在任务于后台存活期间挂着）
    if let Some(job_id) = &tool.job_id {
        title.push(Span::styled(
            format!(" · 后台 {job_id}"),
            Style::new().fg(theme.gray_dim),
        ));
    }
    if tail_width > 0 {
        let used: usize = title.iter().map(Span::width).sum();
        let gap = width.saturating_sub(used + tail_width).max(2);
        title.push(Span::raw(" ".repeat(gap)));
        title.extend(tail);
    }
    let mut lines = vec![clip_line(Line::from(title), width)];
    let mut placements = Vec::new();
    if shows_tool_body(tool, level) {
        // 子调用正文与工具名首字对齐；顶层留在正文列。
        let indent = if nested { TITLE_INDENT } else { PROSE_INDENT };
        let body_width = width.saturating_sub(indent);
        let (body, body_placements) =
            tool_body(tool, &args, app, body_width, level, spawn_images, in_title);
        let base = lines.len();
        lines.extend(body.into_iter().map(|line| hang(line, indent, body_width)));
        placements = body_placements;
        for placement in &mut placements {
            placement.row += base;
            placement.x = placement
                .x
                .saturating_add(u16::try_from(indent).unwrap_or(u16::MAX));
        }
    }
    if let Some(shape) = layout.shape {
        // 嵌套调用画连线：一个空格 + 祖先层各 3 格 + 本层 2 格；
        // 子块自己行首的缩进空格就是连接符后面那一格。
        apply_tree_prefix(&mut lines, shape, tree_rail_style(theme));
        for placement in &mut placements {
            placement.x = placement
                .x
                .saturating_add(u16::try_from(3 * depth).unwrap_or(u16::MAX));
        }
    }
    // 树尾才补一行纯空行（无连线前缀）；树中间与父块不留。
    if layout.gap {
        lines.push(Line::default());
    }
    EntryLines {
        height: lines.len(),
        paged: None,
        lines,
        tables: Vec::new(),
        images: placements,
    }
}

/// 固定摘要只渲染元信息、任务／报告开头和截断警告，不显示原始正文。
fn fixed_tool_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut preview = |label: &str, text: &str| {
        rows.push(Line::from(Span::styled(
            label.to_owned(),
            Style::new()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD),
        )));
        let (head, omitted) = head_lines(text, BRIEF_BODY_ROWS);
        let rendered = render_markdown(&head, width.max(8), theme);
        let mut content = rendered
            .into_iter()
            .filter(|line| !plain_text(line).trim().is_empty());
        rows.extend(content.by_ref().take(AGENT_MESSAGE_PREVIEW_ROWS));
        if omitted > 0 || content.next().is_some() {
            rows.push(Line::from(Span::styled(
                "⋯ 仅显示开头",
                Style::new().fg(theme.gray_dim),
            )));
        }
    };
    if tool.name == "spawn_agent" {
        if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
            for task in tasks {
                preview(
                    task.get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("子代理任务"),
                    task.get("task").and_then(Value::as_str).unwrap_or_default(),
                );
            }
        } else {
            preview(
                "子代理任务",
                args.get("task").and_then(Value::as_str).unwrap_or_default(),
            );
        }
    } else if tool.name == "yield" {
        preview(
            "交付报告",
            args.get("report")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
    }
    if let Some(result) = tool.result.as_deref() {
        if tool.name == "web_fetch"
            && let Some(json) = result
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("Web fetch summary: "))
            && let Ok(Value::Array(pages)) = serde_json::from_str::<Value>(json)
        {
            for page in pages {
                let url = page.get("url").and_then(Value::as_str).unwrap_or_default();
                let title = page.get("title").and_then(Value::as_str).unwrap_or(url);
                let text = if let Some(error) = page.get("error").and_then(Value::as_str) {
                    format!("抓取失败：{url} — {error}")
                } else {
                    format!(
                        "{title} · 约 {} tokens{}",
                        page.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                        if page.get("truncated").and_then(Value::as_bool) == Some(true) {
                            " · 警告：内容已截断"
                        } else {
                            ""
                        }
                    )
                };
                let color = if page.get("error").is_some()
                    || page.get("truncated").and_then(Value::as_bool) == Some(true)
                {
                    theme.accent_warning
                } else {
                    theme.text_secondary
                };
                rows.extend(
                    wrap_text(&text, width)
                        .into_iter()
                        .map(|row| Line::from(Span::styled(row, Style::new().fg(color)))),
                );
            }
        }
        for notice in crate::transcript::tool_notices(result) {
            rows.extend(
                wrap_text(notice, width).into_iter().map(|row| {
                    Line::from(Span::styled(row, Style::new().fg(theme.accent_warning)))
                }),
            );
        }
    }
    rows
}

/// 工具正文行（不含标题与留白，不带缩进，宽度不超过 `width`）。
///
/// 固定摘要不受档位影响；其余内容按 [`content_limit`] 截断，被略去的内容折算成一行
/// 省略提示（`bash` 结果取尾部最新输出，其他文本结果取开头）。
/// `command_in_title` 为真时 bash 命令已在标题里，正文不再重复 `$` 行。
fn tool_body(
    tool: &ToolCallBlock,
    args: &Value,
    app: &App,
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    command_in_title: bool,
) -> (Vec<Line<'static>>, Vec<ImagePlacement>) {
    let theme = app.theme;
    let is_error = tool_is_error(tool);
    let policy = tool_body_policy(&tool.name);
    let level = if is_error
        && level == DetailLevel::Hidden
        && matches!(
            policy,
            ToolBody::None | ToolBody::Summary | ToolBody::AgentPreview
        ) {
        DetailLevel::Brief
    } else {
        level
    };

    if !is_error
        && matches!(
            policy,
            ToolBody::None | ToolBody::Summary | ToolBody::AgentPreview
        )
    {
        let mut lines = fixed_tool_rows(tool, args, width, theme);
        let placements = tool_images(tool, app, width, spawn_images, &mut lines);
        return (lines, placements);
    }
    // run_code 的隐藏、简略档没有正文。
    if policy == ToolBody::FullOnly && level != DetailLevel::Full && !is_error {
        return (Vec::new(), Vec::new());
    }
    // 其余工具在隐藏档没有正文（计数型已在上面返回）。
    if level == DetailLevel::Hidden {
        return (Vec::new(), Vec::new());
    }

    let limit = content_limit(level);
    let mut lines = Vec::new();
    let mut dropped = 0usize;
    let text_row = |text: String, color| Line::from(Span::styled(text, Style::new().fg(color)));

    if let Some(diff) = &tool.diff {
        let rendered = diff::rendered_row_count(diff, limit);
        lines = diff::render_diff(diff, width, theme, limit);
        dropped += diff::row_count(diff).saturating_sub(rendered);
    } else if tool.name == "bash" {
        if !command_in_title {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if level == DetailLevel::Full {
                let (shown_command, command_dropped) = head_lines(command, limit);
                for (index, row) in wrap_text(&shown_command, width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let lead = if index == 0 { "$ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(lead, Style::new().fg(theme.gray_dim)),
                        Span::styled(row, Style::new().fg(theme.command)),
                    ]));
                }
                dropped += command_dropped;
            } else {
                let command = single_line_command(command, width.saturating_sub(2));
                lines.push(Line::from(vec![
                    Span::styled("$ ", Style::new().fg(theme.gray_dim)),
                    Span::styled(command, Style::new().fg(theme.command)),
                ]));
            }
        }
        // 运行中的前台任务：显示实时输出而不是完整结果
        if tool.running && tool.live_output.is_some() {
            if let Some(live_out) = &tool.live_output {
                let budget = BASH_BRIEF_OUTPUT_ROWS;
                let rows = clamp_rows(live_out, width, budget, BodyEdge::Tail);
                let color = theme.gray_bright;
                lines.extend(rows.rows.into_iter().map(|text| text_row(text, color)));
            }
        } else if let Some(result) = &tool.result {
            let budget = if level == DetailLevel::Full {
                limit.saturating_sub(lines.len())
            } else {
                BASH_BRIEF_OUTPUT_ROWS
            };
            let rows = clamp_rows(result, width, budget, BodyEdge::Tail);
            dropped += rows.dropped;
            let color = if is_error {
                theme.diff_delete_fg
            } else {
                theme.gray_bright
            };
            lines.extend(rows.rows.into_iter().map(|text| text_row(text, color)));
        }
    } else if tool.name == "message_agent" {
        let (rows, omitted) = message_agent_tool_rows(tool, args, width, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "wait_agent" {
        let (rows, omitted) = wait_agent_tool_rows(tool, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "sidekick" && !is_error {
        let (rows, omitted) = sidekick_tool_rows(tool, width, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "ask" && !is_error {
        // 每问恒占一行：key（header 兜底 id）降档对齐、答案单行截断，
        // 「未回答」用确认页同款警告标记；行数上限走 content_limit，超出的计 dropped。
        let pairs = ask_answer_pairs(tool, args);
        let key_width = pairs
            .iter()
            .map(|(key, ..)| str_width(key))
            .max()
            .unwrap_or(0)
            .min(ASK_TRANSCRIPT_KEY_MAX);
        let answer_width = width.saturating_sub(key_width + 2);
        for (index, (key, text, unanswered)) in pairs.iter().enumerate() {
            if index >= limit {
                dropped += 1;
                continue;
            }
            let mut spans = Vec::with_capacity(2);
            if !key.is_empty() {
                let key = crate::render::wrap::truncate(key.clone(), key_width).into_owned();
                let pad = " ".repeat(key_width.saturating_sub(str_width(&key)));
                spans.push(Span::styled(
                    format!("{key}{pad}: "),
                    Style::new().fg(theme.text_secondary),
                ));
            }
            spans.push(Span::styled(
                crate::render::wrap::truncate(text.clone(), answer_width).into_owned(),
                if *unanswered {
                    Style::new()
                        .fg(theme.accent_warning)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_primary)
                },
            ));
            lines.push(Line::from(spans));
        }
    } else if tool.name == "run_code" && !is_error {
        let (rows, omitted) = run_code_rows(tool, args, width, limit, theme);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "todo" && !is_error {
        let phases = tool
            .result
            .as_deref()
            .map(moc_protocol::methods::parse_todos)
            .unwrap_or_default();
        lines.extend(todo_tree_rows(&phases, width, level, theme));
    } else if let Some(result) = &tool.result {
        // grep 与通用文本结果：取开头；失败输出给错误色
        let rows = clamp_rows(result, width, limit, BodyEdge::Head);
        dropped += rows.dropped;
        let color = if is_error {
            theme.diff_delete_fg
        } else {
            theme.gray_bright
        };
        lines.extend(rows.rows.into_iter().map(|text| text_row(text, color)));
    }

    // 省略提示统一由此处追加；diff 分支由 `render_diff` 自己产出（只有它知道
    // diff 的总行数），因此不叠加第二行。
    if dropped > 0 && tool.diff.is_none() {
        lines.push(text_row(labels::more_lines(dropped), theme.gray_dim));
    }
    let placements = tool_images(tool, app, width, spawn_images, &mut lines);
    (lines, placements)
}

/// grep／find 在隐藏、简略档的计数行文本；其它工具返回 `None`。
fn tool_count_text(tool: &ToolCallBlock, result: &str) -> Option<String> {
    match tool.name.as_str() {
        "grep" => {
            let (matches, files) = grep_counts(result);
            Some(format!(
                "{} · {}",
                format_count(matches, "match", "matches"),
                format_count(files, "file", "files")
            ))
        }
        "find" => Some(format_count(find_file_count(result), "file", "files")),
        _ => None,
    }
}

/// 数量标签：单数用 `1 file`，其余用 `N files`。
fn format_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// grep 结果里的匹配行与文件数。
///
/// 行格式由 core 固定，每个文件是两种形态之一：单行文件写成 `路径:行号:文本`
/// （匹配）或 `路径-行号-文本`（上下文）；多行文件写成 `路径` 标题行加若干
/// 「两个空格 + 行号 + 分隔符 + 文本」的行。缩进行只从行首解析，正文里的
/// `12:34:56` 不会参与判断；`[` 开头的提示行跳过。
fn grep_counts(result: &str) -> (usize, usize) {
    let mut matches = 0usize;
    let mut files = std::collections::BTreeSet::new();
    let mut current: Option<&str> = None;
    for line in result.lines() {
        if line.starts_with('[') {
            continue;
        }
        if let Some(marker) = grouped_row_marker(line) {
            if marker == b':' {
                matches += 1;
                if let Some(path) = current {
                    files.insert(path);
                }
            }
            continue;
        }
        if let Some((path, marker)) = grep_row_prefix(line) {
            current = Some(path);
            if marker == b':' {
                matches += 1;
                files.insert(path);
            }
            continue;
        }
        current = Some(line);
    }
    (matches, files.len())
}

/// 分组结果行的「行号 + 分隔符」前缀：`  12: 文本` 与 `  12- 文本`。
///
/// 只解析行首，避免把正文里的 `12:34:56` 当成行号前缀。返回分隔符。
fn grouped_row_marker(line: &str) -> Option<u8> {
    let rest = line.strip_prefix("  ")?;
    let digits = rest.find(|ch: char| !ch.is_ascii_digit())?;
    if digits == 0 {
        return None;
    }
    let marker = *rest.as_bytes().get(digits)?;
    matches!(marker, b':' | b'-').then_some(marker)
}

/// 找 grep 结果行的「路径 + 分隔符 + 行号 + 分隔符」前缀。
fn grep_row_prefix(line: &str) -> Option<(&str, u8)> {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(*byte, b':' | b'-') {
            continue;
        }
        let mut end = index + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > index + 1 && bytes.get(end) == Some(byte) {
            return Some((&line[..index], *byte));
        }
    }
    None
}

/// find 结果里的路径数；跳过空行与 core 追加的提示行。
fn find_file_count(result: &str) -> usize {
    result
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty()
                && line != "No files found matching pattern"
                && !line.starts_with("[limit ")
                && !line.starts_with("[Scan incomplete")
        })
        .count()
}

/// 把命令折成单行预览：换行与连续空白折叠成一个空格，再按宽度截断。
fn single_line_command(command: &str, width: usize) -> String {
    let flattened = command.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::render::wrap::truncate(flattened, width).into_owned()
}

/// 把工具结果里的图片追加到正文行后，返回它们的落位。
///
/// 行数与 [`tool_body_rows`] 走同一份判定：状态（解码中 / 就绪 / 失败）决定占
/// 几行，两侧必须完全一致，否则懒渲染的块高会与真实渲染错位。
fn tool_images(
    tool: &ToolCallBlock,
    app: &App,
    width: usize,
    spawn_images: bool,
    lines: &mut Vec<Line<'static>>,
) -> Vec<ImagePlacement> {
    let theme = app.theme;
    let mut placements = Vec::new();
    if tool.images.is_empty() {
        return placements;
    }
    let max_cols = u16::try_from(width).unwrap_or(u16::MAX);
    let shown = tool.images.len().min(MAX_TOOL_IMAGES);
    let note = |text: String| Line::from(Span::styled(text, Style::new().fg(theme.gray_dim)));
    for image in &tool.images[..shown] {
        let Some(graphics) = app.graphics else {
            lines.push(note(format!("[图片 {}]", image.media_type)));
            continue;
        };
        let state = if spawn_images {
            match image.spec_factory() {
                Some(make) => app.images.state(&image.key, make),
                // 超出显示上限的图在落地时已丢弃载荷：不该走到这里
                None => ImageState::Failed("图片载荷已丢弃".into()),
            }
        } else {
            match app.images.peek(&image.key) {
                Some(state) => state,
                None => ImageState::Pending,
            }
        };
        match state {
            ImageState::Ready(prepared) => {
                let Some(layout) = image_layout(prepared.size(), graphics.cell, max_cols) else {
                    lines.push(note(format!("[图片 {}]", image.media_type)));
                    continue;
                };
                let row = lines.len();
                for _ in 0..layout.rows {
                    lines.push(Line::from(" ".repeat(width)));
                }
                placements.push(ImagePlacement {
                    row,
                    // 正文行不带内部前缀，落位列由调用方随正文一起平移
                    x: 0,
                    cols: layout.cols,
                    rows: layout.rows,
                    px: layout.px,
                    key: image.key.clone(),
                });
            }
            ImageState::Pending => lines.push(note("图片解码中…".to_owned())),
            ImageState::Failed(reason) => lines.push(note(format!("图片无法显示：{reason}"))),
        }
    }
    if tool.images.len() > shown {
        lines.push(note(format!(
            "{} 还有 {} 张图片",
            labels::OMITTED,
            tool.images.len() - shown
        )));
    }
    placements
}

/// 工具正文的展示行数：与 [`tool_body`] 同规则（含省略提示行），懒渲染估高用。
fn tool_body_rows(
    tool: &ToolCallBlock,
    args: &Value,
    app: &App,
    width: usize,
    level: DetailLevel,
    command_in_title: bool,
) -> usize {
    let is_error = tool_is_error(tool);
    let policy = tool_body_policy(&tool.name);
    let level = if is_error
        && level == DetailLevel::Hidden
        && matches!(
            policy,
            ToolBody::None | ToolBody::Summary | ToolBody::AgentPreview
        ) {
        DetailLevel::Brief
    } else {
        level
    };

    if !is_error
        && matches!(
            policy,
            ToolBody::None | ToolBody::Summary | ToolBody::AgentPreview
        )
    {
        let mut rows = fixed_tool_rows(tool, args, width, app.theme);
        tool_images(tool, app, width, false, &mut rows);
        return rows.len();
    }
    if policy == ToolBody::FullOnly && level != DetailLevel::Full && !is_error {
        return 0;
    }
    if level == DetailLevel::Hidden {
        return 0;
    }

    let limit = content_limit(level);
    let mut rows = 0usize;
    let mut dropped = 0usize;

    if let Some(diff) = &tool.diff {
        let rendered = diff::rendered_row_count(diff, limit);
        rows += rendered;
        dropped += diff::row_count(diff).saturating_sub(rendered);
    } else if tool.name == "bash" {
        if !command_in_title {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if level == DetailLevel::Full {
                let (shown_command, command_dropped) = head_lines(command, limit);
                rows += wrap_text(&shown_command, width.saturating_sub(2)).len();
                dropped += command_dropped;
            } else {
                rows += 1;
            }
        }
        if let Some(result) = &tool.result {
            let budget = if level == DetailLevel::Full {
                limit.saturating_sub(rows)
            } else {
                BASH_BRIEF_OUTPUT_ROWS
            };
            let clamped = clamp_rows(result, width, budget, BodyEdge::Tail);
            rows = rows.saturating_add(clamped.shown);
            dropped = dropped.saturating_add(clamped.dropped);
        }
    } else if tool.name == "message_agent" {
        let (count, omitted) = message_agent_rows_count(tool, args, width, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "wait_agent" {
        let (count, omitted) = wait_agent_rows_count(tool, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "sidekick" && !is_error {
        let (count, omitted) = sidekick_rows_count(tool, width, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "ask" && !is_error {
        // 每问恒占一行（与 [`tool_body`] 同规则）：行数上限 = 答案条数。
        let count = ask_answer_pairs(tool, args).len();
        rows += count.min(limit);
        dropped += count.saturating_sub(limit);
    } else if tool.name == "run_code" && !is_error {
        // 完整档 run_code 直接复用行构造函数取行数，估高与渲染同源。
        let (constructed, omitted) = run_code_rows(tool, args, width, limit, app.theme);
        rows += constructed.len();
        dropped += omitted;
    } else if tool.name == "todo" && !is_error {
        let phases = tool
            .result
            .as_deref()
            .map(moc_protocol::methods::parse_todos)
            .unwrap_or_default();
        rows += todo_tree_rows(&phases, width, level, app.theme).len();
    } else if let Some(result) = &tool.result {
        let clamped = clamp_rows(result, width, limit, BodyEdge::Head);
        rows += clamped.shown;
        dropped += clamped.dropped;
    }

    // 图片行与会话状态相关（解码中 / 就绪 / 失败），只能复用渲染判定
    let mut probe = Vec::new();
    tool_images(tool, app, width, false, &mut probe);
    rows += probe.len();
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
    let clamped = clamp_rows(message, width, limit, BodyEdge::Head);
    dropped += clamped.dropped;
    for row in clamped.rows {
        lines.push(Line::from(Span::styled(
            row,
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
    let rows: Vec<(String, AgentStatus, u64, Option<String>)> = match &tool.progress {
        Some(progress) => progress
            .iter()
            .map(|row| {
                (
                    row.title.clone(),
                    row.status,
                    row.elapsed_ms,
                    row.activity.clone(),
                )
            })
            .collect(),
        None => wait_result(tool).map_or_else(Vec::new, |result| {
            result
                .agents
                .into_iter()
                .map(|entry| (entry.title, entry.status, 0, None))
                .collect()
        }),
    };
    if rows.is_empty() {
        return (Vec::new(), 0);
    }
    let dropped = rows.len().saturating_sub(limit);
    let mut lines = Vec::with_capacity(rows.len().min(limit));
    for (title, status, elapsed_ms, activity) in rows.into_iter().take(limit) {
        let (glyph, color) = match status {
            AgentStatus::Queued | AgentStatus::Running => ("▸", theme.accent_running),
            AgentStatus::Completed => ("◆", theme.accent_tool),
            AgentStatus::Stopped => ("◼", theme.gray_dim),
            AgentStatus::Failed => ("✘", theme.accent_error),
            AgentStatus::Idle => ("○", theme.gray_dim),
        };
        let mut spans = vec![
            Span::styled(format!("{glyph} "), Style::new().fg(color)),
            Span::styled(title, Style::new().fg(theme.text_primary)),
            Span::styled(
                format!("{}{}", labels::SEP, agent_status_label(status)),
                Style::new().fg(color),
            ),
        ];
        if elapsed_ms > 0 {
            spans.push(Span::styled(
                format!("{}{}", labels::SEP, labels::duration(elapsed_ms)),
                Style::new().fg(theme.gray_dim),
            ));
        }
        if let Some(activity) = activity
            && !activity.is_empty()
        {
            spans.push(Span::styled(
                format!("{}{activity}", labels::SEP),
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

/// `sidekick` 卡片正文：运行中沿用 `wait_agent` 的进度行；落定后解析 JSON
/// 结果——状态行（glyph + 交付方式 + 状态）、可选 note 与报告正文；
/// 非 JSON 正文（错误文本等）退回通用截断渲染。
fn sidekick_tool_rows(
    tool: &ToolCallBlock,
    width: usize,
    theme: &Theme,
    limit: usize,
) -> (Vec<Line<'static>>, usize) {
    let Some(result) = tool.result.as_deref() else {
        return wait_agent_tool_rows(tool, theme, limit);
    };
    let Ok(json) = serde_json::from_str::<Value>(result) else {
        let clamped = clamp_rows(result, width, limit, BodyEdge::Head);
        let lines = clamped
            .rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, Style::new().fg(theme.text_secondary))))
            .collect();
        return (lines, clamped.dropped);
    };
    let mut lines = Vec::new();
    let mut dropped = 0usize;
    let status = json
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (glyph, color) = match status {
        "completed" => ("◆", theme.accent_tool),
        "failed" => ("✘", theme.accent_error),
        "stopped" => ("◼", theme.gray_dim),
        _ => ("▸", theme.accent_running),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(labels::title::SIDEKICK, Style::new().fg(theme.text_primary)),
    ];
    if let Some(delivery) = json.get("delivery").and_then(Value::as_str) {
        spans.push(Span::styled(
            format!("{}{delivery}", labels::SEP),
            Style::new().fg(theme.gray_dim),
        ));
    }
    if !status.is_empty() {
        spans.push(Span::styled(
            format!("{}{status}", labels::SEP),
            Style::new().fg(color),
        ));
    }
    if let Some(model) = json.get("model").and_then(Value::as_str)
        && !model.is_empty()
    {
        spans.push(Span::styled(
            format!("{}{model}", labels::SEP),
            Style::new().fg(theme.gray_dim),
        ));
    }
    lines.push(Line::from(spans));
    // 结构化结果：状态摘要行 + 每条 open_issue 一行。
    if let Some(result_obj) = json.get("result").and_then(Value::as_object) {
        lines.push(Line::from(Span::styled(
            sidekick_result_summary(result_obj),
            Style::new().fg(theme.text_secondary),
        )));
        if let Some(issues) = result_obj.get("open_issues").and_then(Value::as_array) {
            let budget = limit.saturating_sub(lines.len());
            for issue in issues.iter().take(budget) {
                // 每条恒占一行：与 `sidekick_rows_count` 的计数保持一致。
                let text = crate::render::wrap::truncate(
                    issue.as_str().unwrap_or_default(),
                    width.saturating_sub(2),
                );
                lines.push(Line::from(Span::styled(
                    format!("! {text}"),
                    Style::new().fg(theme.accent_running),
                )));
            }
            dropped += issues.len().saturating_sub(budget);
        }
    }
    if let Some(note) = json.get("note").and_then(Value::as_str)
        && !note.is_empty()
    {
        lines.push(Line::from(Span::styled(
            note.to_owned(),
            Style::new().fg(theme.gray_dim),
        )));
    }
    if let Some(report) = json.get("report").and_then(Value::as_str)
        && !report.is_empty()
    {
        let budget = limit.saturating_sub(lines.len()).max(1);
        let clamped = clamp_rows(report, width, budget, BodyEdge::Head);
        dropped += clamped.dropped;
        for row in clamped.rows {
            lines.push(Line::from(Span::styled(
                row,
                Style::new().fg(theme.text_primary),
            )));
        }
    }
    (lines, dropped)
}

/// `sidekick` 卡片行数：与 [`sidekick_tool_rows`] 同规则。
fn sidekick_rows_count(tool: &ToolCallBlock, width: usize, limit: usize) -> (usize, usize) {
    let Some(result) = tool.result.as_deref() else {
        return wait_agent_rows_count(tool, limit);
    };
    let Ok(json) = serde_json::from_str::<Value>(result) else {
        let clamped = clamp_rows(result, width, limit, BodyEdge::Head);
        return (clamped.shown, clamped.dropped);
    };
    let mut rows = 1 + usize::from(
        json.get("note")
            .and_then(Value::as_str)
            .is_some_and(|note| !note.is_empty()),
    );
    let mut dropped = 0;
    // 结构化结果的摘要行与 open_issue 行同样占用行数预算。
    if let Some(result_obj) = json.get("result").and_then(Value::as_object) {
        rows += 1;
        let issues = result_obj
            .get("open_issues")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let shown = issues.min(limit.saturating_sub(rows));
        rows += shown;
        dropped += issues.saturating_sub(shown);
    }
    if let Some(report) = json.get("report").and_then(Value::as_str)
        && !report.is_empty()
    {
        let budget = limit.saturating_sub(rows).max(1);
        let clamped = clamp_rows(report, width, budget, BodyEdge::Head);
        rows += clamped.shown;
        dropped += clamped.dropped;
    }
    (rows, dropped)
}

/// `result` 对象的一行摘要：`结果 <status> · 改动 N 个文件 · 验证 P 通过 F 失败 U 未跑`，
/// 为 0 的部分省略。
fn sidekick_result_summary(result: &serde_json::Map<String, Value>) -> String {
    let mut parts = vec![format!(
        "结果 {}",
        result
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
    )];
    let changes = result.get("changes").and_then(Value::as_array);
    if let Some(changes) = changes
        && !changes.is_empty()
    {
        parts.push(format!("改动 {} 个文件", changes.len()));
    }
    if let Some(verification) = result.get("verification").and_then(Value::as_array)
        && !verification.is_empty()
    {
        let count = |outcome: &str| {
            verification
                .iter()
                .filter(|item| item.get("outcome").and_then(Value::as_str) == Some(outcome))
                .count()
        };
        let (passed, failed, not_run) = (count("passed"), count("failed"), count("not_run"));
        let mut word = String::from("验证");
        if passed > 0 {
            word.push_str(&format!(" {passed} 通过"));
        }
        if failed > 0 {
            word.push_str(&format!(" {failed} 失败"));
        }
        if not_run > 0 {
            word.push_str(&format!(" {not_run} 未跑"));
        }
        parts.push(word);
    }
    parts.join(" · ")
}

/// ask 转写卡 key 列宽上限（格）：对齐答案正文，超长 header/id 截断。
const ASK_TRANSCRIPT_KEY_MAX: usize = 16;

/// ask 结果解析成 `(key, 答案, 未回答)` 三元组，渲染与估高共用。
///
/// core 输出是给模型看的纯文本——多问是 `User answers:` 头加 `id: 答案`
/// 逐行，单问只有裸答案；取消与超时是整段哨兵文本。这里按已知 id 前缀
/// 逐行认领新条，认领不到的行并进上一条答案（自定义答案含换行时不丢
/// 内容），key 取 header 兜底 id。
fn ask_answer_pairs(tool: &ToolCallBlock, args: &Value) -> Vec<(String, String, bool)> {
    const UNANSWERED: &str = "The user did not answer.";
    const TIMEOUT: &str =
        "The user did not answer within the time limit; continue without an answer.";
    let Some(result) = tool.result.as_deref() else {
        return Vec::new();
    };
    let result = result.trim_end();
    if result == TIMEOUT {
        return vec![(String::new(), "超时未回答".to_owned(), true)];
    }
    if result == UNANSWERED {
        return vec![(String::new(), "未回答".to_owned(), true)];
    }
    let questions: Vec<(String, String)> = args
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .map(|question| {
                    let id = question
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let key = question
                        .get("header")
                        .and_then(Value::as_str)
                        .filter(|header| !header.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| id.clone());
                    (id, key)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for line in result
        .lines()
        .skip(usize::from(result.starts_with("User answers:")))
    {
        if let Some((id, _)) = questions
            .iter()
            .find(|(id, _)| line.starts_with(&format!("{id}: ")))
        {
            pairs.push((id.clone(), line[id.len() + 2..].to_owned()));
        } else if let Some((_, answer)) = pairs.last_mut() {
            answer.push(' ');
            answer.push_str(line.trim());
        } else if questions.len() == 1 {
            // 单问输出是裸答案，没有 `id: ` 前缀，整段收下。
            pairs.push((questions[0].0.clone(), line.to_owned()));
        }
        // 既非已知 id 也无上一条可并的行直接丢。
    }
    pairs
        .into_iter()
        .enumerate()
        .map(|(index, (id, answer))| {
            let key = questions
                .iter()
                .find(|(qid, _)| *qid == id)
                .map(|(_, key)| key.clone())
                .or_else(|| questions.get(index).map(|(_, key)| key.clone()))
                .unwrap_or_else(|| "回答".to_owned());
            let unanswered = answer == UNANSWERED;
            (
                key,
                if unanswered {
                    "未回答".to_owned()
                } else {
                    answer
                },
                unanswered,
            )
        })
        .collect()
}

/// 模型给的 UI 描述：只取首个非空行，空串视为未提供。
fn ui_description(args: &Value) -> Option<&str> {
    let text = args.get("description").and_then(Value::as_str)?;
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// 标题行的参数摘要：只覆盖产出单段文本的工具；分段着色的工具走 [`summary_spans`]。
fn tool_summary(tool: &ToolCallBlock, args: &Value) -> String {
    match tool.name.as_str() {
        "message_agent" => {
            use moc_protocol::methods::AgentMessageOutcome;
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut summary = format!("▶ {target}");
            if let Some(receipts) = message_receipts(tool) {
                if target == "all" {
                    summary.push_str(&format!("{}{}", labels::SEP, labels::status::BROADCAST));
                }
                let failed = receipts
                    .iter()
                    .filter(|receipt| receipt.outcome == AgentMessageOutcome::Failed)
                    .count();
                match receipts.as_slice() {
                    [receipt] => {
                        summary.push_str(&format!(
                            "{}{}",
                            labels::SEP,
                            outcome_word(receipt.outcome)
                        ));
                    }
                    receipts => summary.push_str(&format!(
                        "{}{} {}",
                        labels::SEP,
                        receipts.len() - failed,
                        labels::status::DELIVERED
                    )),
                }
                if failed > 0 {
                    summary.push_str(&format!(
                        "{}{failed} {}",
                        labels::SEP,
                        labels::status::FAILED
                    ));
                }
            }
            summary
        }
        "sidekick" => {
            // 摘要只占一行：handoff 首行；mode 与后台执行（block=false）附注。
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("keep");
            let mut summary = args
                .get("message")
                .and_then(Value::as_str)
                .and_then(|text| text.lines().next())
                .unwrap_or_default()
                .to_owned();
            if mode == "retry" {
                summary = "重试".to_owned();
            }
            if mode == "compact" {
                summary.push_str(" · 压缩后");
            } else if mode == "fresh" {
                summary.push_str(" · 新会话");
            }
            if args.get("block").and_then(Value::as_bool) == Some(false) {
                summary.push_str(" · 后台");
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
            .get("questions")
            .and_then(Value::as_array)
            .map(|questions| {
                // 摘要只占一行：问题正文可含换行，取首行避免控制字符在标题里粘连；
                // 多问时点明总量，后面的题不藏在首问后面。
                let first = questions
                    .first()
                    .and_then(|question| question.get("question"))
                    .and_then(Value::as_str)
                    .and_then(|text| text.lines().next())
                    .unwrap_or_default();
                if questions.len() > 1 {
                    format!("共 {} 问 · {}", questions.len(), first)
                } else {
                    first.to_owned()
                }
            })
            .unwrap_or_default(),
        _ => String::new(),
    }
}

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

/// 结果文本的折行（每源行按宽度折行，`limit` 为展示行上限）。
///
/// [`BodyEdge::Head`] 保留前段、[`BodyEdge::Tail`] 保留后段（bash 的输出
/// 只看最新几行）；渲染与懒渲染估算共用同一条截断规则，两条路径必须
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

/// 简略档 todo 树的正文行数上限（超出补一行省略提示）。
const TODO_BRIEF_ROWS: usize = 10;

/// todo 调用结果的快照树：结果文本解析出的阶段列表，正文列宽度下的树形行。
///
/// 单阶段省略阶段标题、任务直接作树叶（`├─ ☑ 任务`）；多阶段沿用阶段树
/// （`├─ 阶段名 · d/t`，任务挂在 `│  ` 下）。简略档折叠：全部关闭的阶段只留
/// 标题行，未关闭阶段从第一个未关闭任务的前一项开始显示，更早的已关闭任务
/// 折成一行 `⋯ N done`；总行数超 [`TODO_BRIEF_ROWS`] 补 [`labels::more_lines`]。
fn todo_tree_rows(
    phases: &[moc_protocol::methods::TodoPhase],
    width: usize,
    level: DetailLevel,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use moc_protocol::methods::TodoStatus;

    let rail_style = tree_rail_style(theme);
    let brief = level != DetailLevel::Full;
    let single = phases.len() == 1;
    // 任务文本列：连接符/竖线 3 格 + 状态符号 2 格
    let text_width = width.saturating_sub(5).max(1);
    let closed = |item: &moc_protocol::methods::TodoItem| {
        matches!(item.status, TodoStatus::Completed | TodoStatus::Abandoned)
    };
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (phase_index, phase) in phases.iter().enumerate() {
        let last_phase = phase_index + 1 == phases.len();
        let all_closed = !phase.items.is_empty() && phase.items.iter().all(|item| closed(item));
        let collapse_phase = brief && all_closed;
        let show_header = !single || collapse_phase;
        if show_header {
            let done = phase
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .count();
            let total = phase.items.len();
            let count = format!("{}{done}/{total}", labels::SEP);
            let name_budget = width.saturating_sub(3 + str_width(&count));
            rows.push(clip_line(
                Line::from(vec![
                    Span::styled(if last_phase { "└─ " } else { "├─ " }, rail_style),
                    Span::styled(
                        crate::render::wrap::truncate(phase.phase.as_str(), name_budget)
                            .into_owned(),
                        Style::new().fg(theme.text_secondary),
                    ),
                    Span::styled(
                        count,
                        Style::new().fg(if done == total && total > 0 {
                            theme.accent_success
                        } else {
                            theme.gray_dim
                        }),
                    ),
                ]),
                width,
            ));
            if collapse_phase {
                continue;
            }
        }
        if phase.items.is_empty() {
            continue;
        }
        // 展示窗口：简略档保留一个已完成任务作上下文，更早的已关闭任务折叠。
        let first_open = phase.items.iter().position(|item| !closed(item));
        let from = match (brief, first_open) {
            (true, Some(open)) => open.saturating_sub(1),
            _ => 0,
        };
        let collapsed = from;
        let shown = &phase.items[from..];
        // 树叶前缀：单阶段任务直接挂连接符，多阶段挂在阶段竖线下。
        let leaf_count = shown.len() + usize::from(collapsed > 0);
        let phase_rail = if last_phase { "   " } else { "│  " };
        if collapsed > 0 {
            // `⋯ N done` 与任务行的状态符号同列。
            let prefix = if single {
                "├─ ".to_owned()
            } else {
                phase_rail.to_owned()
            };
            rows.push(Line::from(vec![
                Span::styled(prefix, rail_style),
                Span::styled(
                    format!("⋯ {collapsed} done"),
                    Style::new().fg(theme.gray_dim),
                ),
            ]));
        }
        for (item_index, item) in shown.iter().enumerate() {
            let last_leaf = usize::from(collapsed > 0) + item_index + 1 == leaf_count;
            let (marker, marker_style, text_style) = todo_status_marker(item.status, theme);
            let wrapped = wrap_text(&item.task, text_width);
            let last_row = wrapped.len() - 1;
            for (row_index, row_text) in wrapped.into_iter().enumerate() {
                let mut spans = if single {
                    vec![Span::styled(
                        if row_index == 0 {
                            if last_leaf { "└─ " } else { "├─ " }
                        } else if last_leaf {
                            "   "
                        } else {
                            "│  "
                        },
                        rail_style,
                    )]
                } else {
                    vec![Span::styled(phase_rail, rail_style)]
                };
                spans.push(if row_index == 0 {
                    Span::styled(marker, marker_style)
                } else {
                    Span::raw("  ")
                });
                spans.push(Span::styled(row_text, text_style));
                if row_index == last_row
                    && let Some(reason) = &item.reason
                {
                    spans.push(Span::styled(
                        format!(" — {reason}"),
                        Style::new().fg(theme.gray),
                    ));
                }
                rows.push(clip_line(Line::from(spans), width));
            }
        }
    }
    if brief && rows.len() > TODO_BRIEF_ROWS {
        let dropped = rows.len() - TODO_BRIEF_ROWS;
        rows.truncate(TODO_BRIEF_ROWS);
        rows.push(Line::from(Span::styled(
            labels::more_lines(dropped),
            Style::new().fg(theme.gray_dim),
        )));
    }
    rows
}

/// run_code 完整档正文：高亮脚本（行号槽 `{n:>w} │ `）+ `result` 段。
///
/// `result` 能解析成 JSON object/array 就画 JSON 树，标量并进标签行
/// （`result: 42`），不是 JSON 按普通文本（Head 截断）；任何档位都不把
/// 原始返回值 JSON 直接倒出来。返回（展示行、被略去的行数）。
fn run_code_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    limit: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut dropped = 0usize;
    let push = |lines: &mut Vec<Line<'static>>, line: Line<'static>, dropped: &mut usize| {
        if lines.len() >= limit {
            *dropped += 1;
        } else {
            lines.push(line);
        }
    };
    let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
    let highlighted = super::highlight::highlight("javascript", code);
    let digits = highlighted.lines.len().max(1).to_string().len();
    let gutter = digits + 3;
    let text_width = width.saturating_sub(gutter).max(8);
    let rail = tree_rail_style(theme);
    for (index, spans) in highlighted.lines.iter().enumerate() {
        let styled: Vec<Span<'static>> = spans
            .iter()
            .map(|part| {
                Span::styled(
                    part.text.clone(),
                    Style::new().fg(part
                        .fg
                        .map(super::highlight::rgb_to_color)
                        .unwrap_or(theme.text_secondary)),
                )
            })
            .collect();
        for (row_index, row) in super::wrap::wrap_spans(&styled, text_width)
            .into_iter()
            .enumerate()
        {
            let mut line = Vec::with_capacity(row.spans.len() + 2);
            line.push(Span::styled(
                if row_index == 0 {
                    format!("{:>w$} ", index + 1, w = digits)
                } else {
                    " ".repeat(digits + 1)
                },
                Style::new().fg(theme.gray_dim),
            ));
            line.push(Span::styled("│ ", rail));
            line.extend(row.spans);
            push(&mut lines, Line::from(line), &mut dropped);
        }
    }
    if let Some(result) = tool.result.as_deref() {
        match serde_json::from_str::<Value>(result) {
            Ok(value) if value.is_object() || value.is_array() => {
                push(
                    &mut lines,
                    Line::from(Span::styled("result", Style::new().fg(theme.gray))),
                    &mut dropped,
                );
                for row in json_member_rows(&value, &[], 1, width, theme) {
                    push(&mut lines, row, &mut dropped);
                }
            }
            Ok(value) => {
                let mut spans = vec![Span::styled(
                    "result: ".to_owned(),
                    Style::new().fg(theme.gray),
                )];
                spans.extend(json_scalar_spans(&value, width.saturating_sub(8), theme));
                push(
                    &mut lines,
                    clip_line(Line::from(spans), width),
                    &mut dropped,
                );
            }
            Err(_) => {
                let budget = limit.saturating_sub(lines.len());
                let clamped = clamp_rows(result, width, budget, BodyEdge::Head);
                dropped += clamped.dropped;
                for row in clamped.rows {
                    lines.push(Line::from(Span::styled(
                        row,
                        Style::new().fg(theme.gray_bright),
                    )));
                }
            }
        }
    }
    (lines, dropped)
}

/// JSON 成员行：`├─ key: 值`；容器成员独占一行、子成员挂在竖线下。
///
/// 深度超过 6 的容器折成 `{N keys}`／`[N items]`；空容器写 `{}`／`[]`。
fn json_member_rows(
    value: &Value,
    rails: &[bool],
    depth: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let members: Vec<(String, &Value)> = match value {
        Value::Object(map) => map.iter().map(|(key, v)| (key.clone(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, v)| (format!("[{index}]"), v))
            .collect(),
        _ => Vec::new(),
    };
    let mut rows = Vec::new();
    for (index, (key, member)) in members.iter().enumerate() {
        let last = index + 1 == members.len();
        let mut prefix = String::new();
        for rail in rails {
            prefix.push_str(if *rail { "│  " } else { "   " });
        }
        prefix.push_str(if last { "└─ " } else { "├─ " });
        let content_width = width.saturating_sub(str_width(&prefix)).max(8);
        let mut spans = vec![
            Span::styled(prefix, tree_rail_style(theme)),
            Span::styled(key.clone(), Style::new().fg(theme.accent_user)),
        ];
        let children = match member {
            Value::Object(map) if !map.is_empty() && depth < 6 => Some(json_member_rows(
                member,
                &[&*rails, &[!last]].concat(),
                depth + 1,
                width,
                theme,
            )),
            Value::Array(items) if !items.is_empty() && depth < 6 => Some(json_member_rows(
                member,
                &[&*rails, &[!last]].concat(),
                depth + 1,
                width,
                theme,
            )),
            _ => None,
        };
        if let Some(children) = children {
            rows.push(clip_line(Line::from(spans), width));
            rows.extend(children);
            continue;
        }
        spans.push(Span::styled(": ".to_owned(), Style::new().fg(theme.gray)));
        match member {
            Value::Object(map) => spans.push(Span::styled(
                if map.is_empty() {
                    "{}".to_owned()
                } else {
                    format!("{{{} keys}}", map.len())
                },
                Style::new().fg(theme.gray),
            )),
            Value::Array(items) => spans.push(Span::styled(
                if items.is_empty() {
                    "[]".to_owned()
                } else {
                    format!("[{} items]", items.len())
                },
                Style::new().fg(theme.gray),
            )),
            // 标量预算要扣掉 `key: ` 的宽度，否则整行超长时后缀会被 clip_line 吃掉。
            scalar => spans.extend(json_scalar_spans(
                scalar,
                content_width.saturating_sub(str_width(key) + 2),
                theme,
            )),
        }
        rows.push(clip_line(Line::from(spans), width));
    }
    rows
}

/// JSON 标量的着色 span：字符串 `"首行…"`（多行追加 ` (N lines)`）、
/// 数字 `accent_running`、bool/null `accent_assistant`。
fn json_scalar_spans(value: &Value, budget: usize, theme: &Theme) -> Vec<Span<'static>> {
    match value {
        Value::String(text) => {
            let first = text.lines().next().unwrap_or_default();
            let count = text.lines().count();
            // ` (N lines)` 后缀要计入预算：先扣掉它再给首行截断，
            // 否则后缀会被行尾的 clip_line 吃掉。
            let suffix = if count > 1 {
                format!(" ({count} lines)")
            } else {
                String::new()
            };
            let shown = crate::render::wrap::truncate(
                first.to_owned(),
                budget
                    .saturating_sub(2)
                    .saturating_sub(str_width(&suffix))
                    .max(1),
            );
            let mut spans = vec![Span::styled(
                format!("\"{shown}\""),
                Style::new().fg(theme.accent_success),
            )];
            if !suffix.is_empty() {
                spans.push(Span::styled(suffix, Style::new().fg(theme.gray)));
            }
            spans
        }
        Value::Number(number) => vec![Span::styled(
            number.to_string(),
            Style::new().fg(theme.accent_running),
        )],
        Value::Bool(flag) => vec![Span::styled(
            flag.to_string(),
            Style::new().fg(theme.accent_assistant),
        )],
        Value::Null => vec![Span::styled(
            "null".to_owned(),
            Style::new().fg(theme.accent_assistant),
        )],
        _ => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::ColorLevel;
    use crate::transcript::{ToolCallFinish, ToolCallStart};

    /// 回归：图片预筛必须与渲染期真正出图的条件一致。
    ///
    /// 漏判不会导致画错，而是让懒块用折行近似顶替真实的占位行——块高差十几行，
    /// 滚动范围与滚动条一起失真。围栏的四种写法都得出图。
    #[test]
    fn image_prefilter_matches_the_renderer() {
        for fence in ["```svg", "```SVG", "~~~svg", "``` svg", "~~~~svg"] {
            let source = format!("{fence}\n<svg width=\"10\" height=\"10\"/>\n```\n");
            assert!(may_contain_image(&source), "{fence} 应被预筛命中");
        }
        assert!(
            !may_contain_image("```rust\nfn main() {}\n```"),
            "普通代码块不该命中"
        );
        assert!(may_contain_image("前文\n\n![](./a.png)\n"), "markdown 图片");
        assert!(!may_contain_image("纯文本\n没有图也没有代码块"), "纯文本");
    }

    #[test]
    fn mermaid_prefilter_matches_the_renderer() {
        for fence in ["```mermaid", "```Mermaid", "~~~mermaid", "``` mermaid"] {
            let source = format!("{fence}\nflowchart TD\n  A-->B\n```\n");
            assert!(may_contain_mermaid(&source), "{fence} 应被预筛命中");
        }
        assert!(
            !may_contain_mermaid("```rust\nfn main() {}\n```"),
            "普通代码块不该命中"
        );
        assert!(
            !may_contain_mermaid("```svg\n<svg/>\n```"),
            "SVG 围栏走图片预筛"
        );
    }

    /// 用户消息：使用 `>` 前缀，短消息时间戳右对齐，窄宽度时另起一行。
    #[test]
    fn user_message_aligns_clock_without_panel_borders() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let at_ms = 1_700_000_000_000;
        let clock = format_clock(at_ms).expect("演示时间戳应可格式化");

        let short = user_message("hi", Some(at_ms), 40, &theme);
        assert_eq!(short.len(), 2, "短消息应只有正文和尾随留白");
        assert!(short[0].spans.iter().any(|span| span.content == "> "));
        assert!(short[0].spans.iter().any(|span| span.content == clock));
        assert!(!short[0].spans.iter().any(|span| span.content.contains('╭')));

        let narrow = user_message("一二三四五六七八九十", Some(at_ms), 24, &theme);
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
    fn paged_message_paints_identically_after_scroll_and_resize() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let text = "中文 mixed paragraph words /path/file，标点。 ".repeat(6000);
        assert!(text.len() > 256 * 1024);
        app.push_block(Block::Assistant { text: text.clone() });
        let mut view = ScrollbackView::new();
        for width in [41, 83] {
            let mut full: Vec<_> = render_markdown_with(
                &text,
                prose_width(width).max(8),
                app.theme,
                RenderViews::default(),
            )
            .lines
            .into_iter()
            .map(|line| hang(line, PROSE_INDENT, usize::MAX))
            .collect();
            full.push(Line::default());
            view.sync(&app, width as u16);
            for from in [0, full.len() / 2, full.len().saturating_sub(20), 0] {
                let window = view.ensure_visible(&app, 20, full.len().saturating_sub(20 + from));
                assert_eq!(window.skip_rows, from);
                let rect = Rect::new(0, 0, width as u16, 20);
                let mut actual = Buffer::empty(rect);
                let mut expected = Buffer::empty(rect);
                let (mut hits, mut charts, mut slots, mut lines) =
                    (Vec::new(), Vec::new(), Vec::new(), Vec::new());
                view.paint(
                    &mut actual,
                    rect,
                    window.skip_rows,
                    true,
                    app.theme,
                    None,
                    PaintSinks {
                        hits: &mut hits,
                        charts: &mut charts,
                        slots: &mut slots,
                        lines: &mut lines,
                    },
                );
                for (row, line) in full.iter().skip(from).take(20).enumerate() {
                    expected.set_line(0, row as u16, line, width as u16);
                }
                assert_eq!(actual, expected);
                assert!(view.cache[0].lines.len() <= 276);
            }
            view.trim_with_budget(full.len(), 1, 0);
            assert!(view.cache[0].paged.is_none());
            assert!(!view.cache[0].rendered);
            let window = view.ensure_visible(&app, 20, 0);
            assert_eq!(window.skip_rows, full.len() - 20);
            let cached = &view.cache[0];
            assert_eq!(
                &cached.lines[window.skip_rows - cached.line_start..],
                &full[window.skip_rows..]
            );
        }
    }

    #[tokio::test]
    async fn background_layout_coalesces_without_losing_final_or_reusing_old_blocks() {
        use crate::protocol::AppEvent;
        use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
        async fn ready(events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) {
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    if matches!(
                        events.recv().await.expect("布局事件通道"),
                        AppEvent::LayoutReady
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("轻量布局应及时完成");
        }
        fn allow_render(view: &mut ScrollbackView) {
            view.cache[0].rendered_at =
                std::time::Instant::now() - std::time::Duration::from_secs(1);
        }
        let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.prompt_running = true;
        app.blocks.push(Entry::new(Block::Assistant {
            text: "Alpha [late] **正文** and `code`.\n\n".repeat(2200),
        }));
        let mut view = ScrollbackView::background();
        app.graphics = Some(GraphicsSupport {
            proto: GraphicsProto::Kitty,
            cell: CellPixels { w: 8, h: 18 },
        });
        view.sync(&app, 80);
        app.blocks[0].push_text("\n\n| 项 | 值 |\n|---|---|\n| A | 1 |\n| B | 2 |\n");
        let first = app.blocks[0].clone();
        allow_render(&mut view);
        view.sync(&app, 80);
        assert!(view.pending_layout.is_some());
        assert!(!view.has_deferred_render(), "等待后台结果不忙画空帧");
        // 后加的引用定义会改变前文的渲染，不能用盲目的文本尾部拼接替代原 parser。
        app.blocks[0].push_text("\n[late]: https://example.com\n\n新的末尾");
        ready(&mut receiver).await;
        view.sync(&app, 80);
        assert_eq!(
            view.cache[0].key.0, first.generation,
            "追加期间仍能发布完成的中间帧"
        );
        let Block::Assistant { text } = &first.block else {
            unreachable!()
        };
        assert_eq!(
            view.cache[0].lines,
            assistant_message(text, &first, &app, 80, true).lines
        );
        allow_render(&mut view);
        view.sync(&app, 80);
        ready(&mut receiver).await;
        view.sync(&app, 80);
        let expected = render_entry(0, &app.blocks[0], &app, 80);
        assert_eq!(view.cache[0].key.0, app.blocks[0].generation);
        assert_eq!(view.cache[0].lines, expected.lines);
        assert_eq!(
            format!("{:?}", view.cache[0].tables),
            format!("{:?}", expected.tables)
        );
        assert!(
            view.cache[0].tables[0].button.is_some(),
            "表格落定后的交互入口不能丢失"
        );

        app.blocks[0].push_text("最终尾部");
        allow_render(&mut view);
        view.sync(&app, 80);
        assert!(view.pending_layout.is_some());
        app.blocks[0].stamp_prompt(1_700_000_000_000, 2000);
        app.prompt_running = false;
        view.sync(&app, 80);
        let final_key = view.cache[0].key;
        assert_eq!(
            view.cache[0].lines,
            render_entry(0, &app.blocks[0], &app, 80).lines
        );
        assert!(!view.has_deferred_render());
        ready(&mut receiver).await;
        view.sync(&app, 80);
        assert_eq!(view.cache[0].key, final_key, "迟到流式结果不能覆盖终态");

        app.prompt_running = true;
        app.blocks[0].elapsed_ms = None;
        app.blocks[0].touch();
        view.sync(&app, 80);
        app.blocks[0].push_text("旧会话排队内容");
        allow_render(&mut view);
        view.sync(&app, 80);
        assert!(view.pending_layout.is_some());
        app.blocks = vec![Entry::new(Block::Assistant {
            text: "新的会话".into(),
        })];
        view.sync(&app, 80);
        ready(&mut receiver).await;
        view.sync(&app, 80);
        assert_eq!(
            view.cache[0].lines,
            render_entry(0, &app.blocks[0], &app, 80).lines
        );
        assert!(view.pending_layout.is_none());
    }

    #[test]
    fn groups_same_name_tools_from_one_batch() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.register_tool_batch("batch".into(), ["call_a".to_owned(), "call_b".to_owned()]);
        app.start_tool_block(ToolCallStart {
            id: "call_a".into(),
            name: "read".into(),
            parent_id: None,
            arguments: r#"{"path":"a.rs"}"#.into(),
        });
        app.start_tool_block(ToolCallStart {
            id: "call_b".into(),
            name: "read".into(),
            parent_id: None,
            arguments: r#"{"path":"b.rs"}"#.into(),
        });

        let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("read (2)"));
        assert!(text.contains("a.rs"));
        assert!(text.contains("b.rs"));
        for _ in 0..8 {
            app.blocks[0].touch();
        }
        let mut view = ScrollbackView::new();
        view.sync(&app, 80);
        let before = view.cache[0].key;
        app.blocks[1].touch();
        view.sync(&app, 80);
        assert_ne!(
            view.cache[0].key, before,
            "较低版本成员变化也必须使整批失效"
        );
        assert_eq!(render_entry(1, &app.blocks[1], &app, 80).lines.len(), 0);
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
            let arguments = r#"{"command":"run"}"#.to_owned();
            app.start_tool_block(ToolCallStart {
                id: id.clone(),
                name: "bash".into(),
                parent_id: None,
                arguments: arguments.clone(),
            });
            app.finish_tool_block(ToolCallFinish {
                id: id.clone(),
                name: "bash".into(),
                parent_id: None,
                arguments,
                status: ToolStatus::Completed,
                result: ToolResult {
                    tool_call_id: id,
                    content: result.clone(),
                    is_error: false,
                    images: Vec::new(),
                },
                duration_ms: Some(1),
                diff: None,
            });
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
            } else {
                assert_eq!(
                    cached.lines.capacity(),
                    0,
                    "离屏缓存不得保留整条消息的行数组"
                );
                assert_eq!(cached.tables.capacity(), 0);
                assert_eq!(cached.images.capacity(), 0);
            }
            offset = next;
        }
        assert!(covered > 0, "视口应落在至少一个块上");

        app.blocks = vec![Entry::new(Block::Assistant {
            text: "x".repeat(65537),
        })];
        app.prompt_running = true;
        let mut streaming = ScrollbackView::new();
        streaming.sync(&app, 80);
        let old_key = streaming.cache[0].key;
        streaming.cache[0].render_cost = std::time::Duration::from_millis(100);
        streaming.cache[0].rendered_at =
            std::time::Instant::now() + std::time::Duration::from_secs(60);
        app.blocks[0].push_text("最后一段");
        streaming.sync(&app, 80);
        assert!(streaming.has_deferred_render());
        assert_eq!(streaming.cache[0].key, old_key);
        app.prompt_running = false;
        streaming.sync(&app, 80);
        assert!(!streaming.has_deferred_render(), "终态不得延迟");
        assert_eq!(streaming.cache[0].key.0, app.blocks[0].generation);
        assert!(
            streaming.cache[0]
                .lines
                .iter()
                .map(ToString::to_string)
                .collect::<String>()
                .contains("最后一段")
        );
    }

    /// 含表格的正文：估算高度必须等于真实渲染高度，切换成图表后依然成立。
    ///
    /// 表格列宽收缩、入口按钮、图表占位行都只在渲染期算得出来，任何一处
    /// 走「折行近似」都会让滚动范围与懒渲染窗口一起失真。
    #[test]
    fn table_blocks_estimate_exactly_like_render() {
        use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        app.scrollback_area = ratatui::layout::Rect::new(3, 0, 80, 24);
        let text = "结论如下：\n\n| 项 | 耗时 | 内存 |\n|---|---|---|\n| a | 10 ms | 2 MiB |\n| b | 20 ms | 4 MiB |\n\n仅供参考。";
        app.push_block(Block::Assistant { text: text.into() });

        let rendered = render_entry(0, &app.blocks[0], &app, width);
        assert!(
            rendered
                .lines
                .iter()
                .all(|line| line.width() <= usize::from(width)),
            "表格行不得超出转写区宽度"
        );
        let heights = |app: &App| {
            (
                estimate_entry_height_at(app, 0, width),
                render_entry(0, &app.blocks[0], app, width).lines.len(),
            )
        };
        let (estimate, rendered) = heights(&app);
        assert_eq!(estimate, rendered, "表格块：估算与渲染必须同源");
        assert!(
            rendered > wrapped_rows(text, usize::from(width)),
            "表格实际比折行近似更高（边框行）"
        );

        app.graphics = Some(GraphicsSupport {
            proto: GraphicsProto::Kitty,
            cell: CellPixels { w: 8, h: 18 },
        });
        let (estimate, rendered) = heights(&app);
        assert_eq!(estimate, rendered, "支持图形时（多出入侵按钮行）也必须同源");

        app.toggle_table_chart(0, 0);
        assert!(app.blocks[0].table_chart(0).is_some());
        let (estimate, rendered) = heights(&app);
        assert_eq!(estimate, rendered, "切换成图表后（占位行）也必须同源");
    }

    /// 绘制登记：表格入口的列区间等于按钮宽度；切换后占位区落在正文列上。
    #[test]
    fn paint_registers_entry_hits_and_bitmap_slots() {
        use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
        use ratatui::Terminal;
        use ratatui::backend::TestBackend;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.graphics = Some(GraphicsSupport {
            proto: GraphicsProto::Kitty,
            cell: CellPixels { w: 8, h: 18 },
        });
        app.push_block(Block::Assistant {
            // 两个同量纲、同单位的测量列 → 分组条形图（图例 + 2 类别 × 2 系列 + 轴）
            text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
        });

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("首帧绘制成功");

        let hit = *app.chart_hits.first().expect("表格下应有入口命中");
        assert_eq!(hit.entry, 0);
        assert_eq!(hit.table, 0);
        assert_eq!(hit.w as usize, str_width("▸ 可视化图表"));
        assert_eq!(
            hit.x,
            app.scrollback_area.x + super::PROSE_INDENT as u16,
            "入口与正文左缘对齐"
        );
        assert!(app.image_slots.is_empty(), "表格形态没有位图槽位");

        app.toggle_table_chart(0, 0);
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("切换后绘制成功");
        let hit = *app.chart_hits.first().expect("展开后仍有入口");
        assert_eq!(hit.w as usize, str_width("▾ 隐藏图表"), "按钮变成隐藏");
        let slot = app.image_slots.first().expect("图表应登记位图槽位").clone();
        let kind = app.blocks[0]
            .table_chart(0)
            .map(|state| state.spec.kind)
            .expect("图表态仍在");
        assert_eq!(slot.cols, 90, "正文可用 91 列，但 720px 上限收窄到 90");
        assert_eq!(slot.x, app.scrollback_area.x + super::PROSE_INDENT as u16);
        assert_eq!(
            kind,
            moc_core::chart::ChartKind::Grouped,
            "两个同量纲测量列应画成分组条形图"
        );
        assert_eq!(slot.rows, 6, "图例 + 2 类别 × 2 系列 + 轴");
        assert_eq!(slot.y, hit.row + 2, "图表在入口与一行空白之后");
        assert!(
            slot.y + slot.rows <= app.scrollback_area.y + app.scrollback_area.height,
            "槽位必须整块落在视口内"
        );
    }

    /// M25：懒块的估算高度与渲染同规则。
    ///
    /// 估算按全文计高（工具结果 2000 行即 2000 行、折叠态也按展开算）时，滚动范围、
    /// 滚动条与懒渲染的可见区间都失真一个数量级。
    #[test]
    fn estimates_follow_render_rules() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let long_result = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");

        app.register_tool_batch("batch".into(), ["call_a".to_owned(), "call_b".to_owned()]);
        app.start_tool_block(ToolCallStart {
            id: "call_bash".into(),
            name: "bash".into(),
            parent_id: None,
            arguments: r#"{"command":"print lines"}"#.into(),
        });
        app.start_tool_block(ToolCallStart {
            id: "call_a".into(),
            name: "read".into(),
            parent_id: None,
            arguments: r#"{"path":"a.rs"}"#.into(),
        });
        app.start_tool_block(ToolCallStart {
            id: "call_b".into(),
            name: "read".into(),
            parent_id: None,
            arguments: r#"{"path":"b.rs"}"#.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_bash".into(),
            name: "bash".into(),
            parent_id: None,
            arguments: r#"{"command":"print lines"}"#.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_bash".into(),
                content: long_result,
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(42),
            diff: None,
        });
        run_tool(
            &mut app,
            "call_todo",
            "todo",
            r#"{"op":"init","list":[{"phase":"阶段","items":["很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述"]}]}"#,
            ToolStatus::Completed,
            "阶段\n  [ ] 很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述很长的任务描述\n",
        );
        // run_code：三档 + 失败的估高都要与渲染同源
        run_tool(
            &mut app,
            "call_rc",
            "run_code",
            &serde_json::json!({"description":"核对","code":"const a = 1;\nawait tools.bash({command:'ls'})"}).to_string(),
            ToolStatus::Completed,
            r#"[{"status":"fulfilled","value":{"content":" M a.rs\n M b.rs","isError":false}}]"#,
        );
        run_tool(
            &mut app,
            "call_rc_bad",
            "run_code",
            r#"{"code":"throw 1"}"#,
            ToolStatus::Error,
            "脚本执行失败",
        );
        // 嵌套调用树：run_code 带子调用，兄弟连线进估高
        app.start_tool_block(ToolCallStart {
            id: "call_parent".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: r#"{"description":"父"}"#.into(),
        });
        for (id, summary) in [("call_child1", "one"), ("call_child2", "two")] {
            app.start_tool_block(ToolCallStart {
                id: id.into(),
                name: "bash".into(),
                parent_id: Some("call_parent".into()),
                arguments: format!(r#"{{"description":"{summary}","command":"echo {summary}"}}"#)
                    .into(),
            });
            app.finish_tool_block(ToolCallFinish {
                id: id.into(),
                name: "bash".into(),
                parent_id: Some("call_parent".into()),
                arguments: String::new(),
                status: ToolStatus::Completed,
                result: ToolResult {
                    tool_call_id: id.into(),
                    content: "done".into(),
                    is_error: false,
                    images: Vec::new(),
                },
                duration_ms: Some(3),
                diff: None,
            });
        }
        app.finish_tool_block(ToolCallFinish {
            id: "call_parent".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_parent".into(),
                content: "[]".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(9),
            diff: None,
        });
        // todo 多阶段：简略档折叠已关闭阶段与历史已完成任务
        run_tool(
            &mut app,
            "call_todo2",
            "todo",
            r#"{"op":"start","task":"乙"}"#,
            ToolStatus::Completed,
            "一\n  [x] 甲\n  [-] 弃\n二\n  [x] 丙\n  [/] 乙\n  [ ] 丁\n",
        );

        let rendered = |app: &App, index: usize| {
            render_entry(index, &app.blocks[index], app, width)
                .lines
                .len()
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
            2,
            "隐藏档只剩标题与留白"
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

    /// 批次代次一遍算清：所有成员共用布局代次，非批次块保留自身代次。
    /// 精确成员序列比较避免漏刷新，整帧工作量仍为 O(块数)。
    #[test]
    fn batch_members_share_one_generation_scan() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.register_tool_batch(
            "batch".into(),
            [
                "call_a".to_owned(),
                "call_b".to_owned(),
                "call_c".to_owned(),
            ],
        );
        for id in ["call_a", "call_b", "call_c"] {
            app.start_tool_block(ToolCallStart {
                id: id.into(),
                name: "read".into(),
                parent_id: None,
                arguments: r#"{"path":"a.rs"}"#.into(),
            });
        }
        app.push_note("独立块".into(), false);
        // 只有第二个成员继续变化：全批仍须使用同一个失效标记。
        app.tool_entry_mut("call_b").expect("批次成员").touch();
        let latest = app.blocks[1].generation;
        assert!(latest > app.blocks[0].generation, "成员各自推进");

        let mut generations = Vec::new();
        fill_block_generations(&app.blocks, &mut generations);
        assert_eq!(generations.len(), app.blocks.len());
        assert_eq!(
            &generations[..3],
            &[generations[0], generations[0], generations[0]],
            "同批成员共用布局代次"
        );
        assert_eq!(
            generations[3], app.blocks[3].generation,
            "非批次块保留自身代次"
        );

        // 缓存 key 用的就是这份代次
        let mut view = ScrollbackView::new();
        view.sync(&app, 80);
        let key = view.cache[0].key;
        assert_eq!(key, (view.generations[0], app.view_gen));
        assert_eq!(view.cache[1].key.0, key.0);
        view.sync(&app, 80);
        assert_eq!(view.cache[0].key, key, "无成员变化时布局代次稳定");
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
        app.start_tool_block(ToolCallStart {
            id: "call_bash".into(),
            name: "bash".into(),
            parent_id: None,
            arguments: r#"{"command":"run"}"#.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_bash".into(),
            name: "bash".into(),
            parent_id: None,
            arguments: r#"{"command":"run"}"#.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_bash".into(),
                content: result,
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(10),
            diff: None,
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width).lines;
        // 标题（命令放得下，进标题）+ 结果尾 4 + 省略提示 1 + 留白 1
        assert_eq!(brief.len(), 7, "简略档行数固定");
        let brief_text = text_of(&brief);
        assert!(
            brief_text.contains("⋯ 还有 196 行"),
            "略去行数按源行计：{brief_text}"
        );
        assert!(
            brief_text.contains("line-196") && brief_text.contains("line-199"),
            "bash 只留尾部最新四行：{brief_text}"
        );
        assert!(
            !brief_text.contains("line-195") && !brief_text.contains("line-0"),
            "更早的输出被省略：{brief_text}"
        );
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            brief.len(),
            "简略档估算与渲染同源"
        );

        app.tool_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, width).lines;
        assert_eq!(
            full.len(),
            200 + 2,
            "完整档 = 标题 + 全部 200 行 + 留白（命令已在标题）"
        );
        assert!(!text_of(&full).contains("还有"), "完整档不出现省略提示");
        assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());

        app.tool_detail = DetailLevel::Hidden;
        let hidden = render_entry(0, &app.blocks[0], &app, width).lines;
        assert_eq!(hidden.len(), 2, "隐藏档 = 标题 + 留白");
        assert!(!text_of(&hidden).contains("line-"), "隐藏档无正文");
        assert_eq!(estimate_entry_height_at(&app, 0, width), hidden.len());
    }

    /// read 始终只显示摘要；失败原因仍可展开查看。
    #[test]
    fn read_summary_never_expands_but_errors_remain_visible() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let result = (0..200)
            .map(|index| format!("line-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        let read_arguments = r#"{"path":"crates/moc-tui/src/app.rs","offset":1,"limit":200}"#;
        app.start_tool_block(ToolCallStart {
            id: "call_read".into(),
            name: "read".into(),
            parent_id: None,
            arguments: read_arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_read".into(),
            name: "read".into(),
            parent_id: None,
            arguments: read_arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_read".into(),
                content: result.clone(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        assert!(!app.blocks[0].block.foldable());

        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            let lines = render_entry(0, &app.blocks[0], &app, width).lines;
            assert_eq!(lines.len(), 2, "{level:?} 档 read 只占标题行 + 尾随留白");
            let text = lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>();
            assert!(
                text.contains("crates/moc-tui/src/app.rs:1-201"),
                "摘要带行号范围：{text}"
            );
            assert_eq!(estimate_entry_height_at(&app, 0, width), 2);
        }

        // 失败时正文是错误原因，仍可展开。
        app.finish_tool_block(ToolCallFinish {
            id: "call_read".into(),
            name: "read".into(),
            parent_id: None,
            arguments: String::new(),
            status: ToolStatus::Error,
            result: ToolResult {
                tool_call_id: "call_read".into(),
                content: "读取失败：权限不足".into(),
                is_error: true,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        app.tool_detail = DetailLevel::Brief;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
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

    /// grep 始终只报计数和警告，不显示匹配正文。
    #[test]
    fn grep_summary_reports_counts_and_warnings() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let arguments = r#"{"pattern":"alpha"}"#;
        let result = "src/a.rs:1:alpha\nsrc/a.rs-2-2026-10-19 12:34:56\nsrc/b.rs:3:alpha beta\n[1 files, 2 matches shown; limit reached]";
        app.start_tool_block(ToolCallStart {
            id: "call_grep".into(),
            name: "grep".into(),
            parent_id: None,
            arguments: arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_grep".into(),
            name: "grep".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_grep".into(),
                content: result.into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        assert!(!app.blocks[0].block.foldable());
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            let lines = render_entry(0, &app.blocks[0], &app, width).lines;
            assert!(
                text_of(&lines).contains("limit reached"),
                "截断警告必须可见"
            );
            let title = plain(&lines[0]);
            assert!(
                title.contains("2 matches · 2 files"),
                "{level:?} 档计数进标题尾列：{title}"
            );
            assert!(
                !text_of(&lines).contains("alpha beta"),
                "{level:?} 档不显示匹配正文"
            );
            assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
        }

        // 零匹配同样给出计数行。
        app.finish_tool_block(ToolCallFinish {
            id: "call_grep".into(),
            name: "grep".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_grep".into(),
                content: "No matches found".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        app.tool_detail = DetailLevel::Brief;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        assert!(plain(&lines[0]).contains("0 matches · 0 files"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    /// find 始终只报路径数和扫描警告。
    #[test]
    fn find_summary_reports_file_count_and_warnings() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let arguments = r#"{"glob":["**/*.rs"]}"#;
        let result = "src/a.rs\nsrc/b.rs\nsrc/[weird].rs\n[limit 200 reached; narrow the glob]\n[Scan incomplete (time limit, scan-entry limit)]";
        app.start_tool_block(ToolCallStart {
            id: "call_find".into(),
            name: "find".into(),
            parent_id: None,
            arguments: arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_find".into(),
            name: "find".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_find".into(),
                content: result.into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            let lines = render_entry(0, &app.blocks[0], &app, width).lines;
            assert!(text_of(&lines).contains("limit 200 reached"));
            assert!(text_of(&lines).contains("Scan incomplete"));
            let title = plain(&lines[0]);
            assert!(
                title.contains("3 files"),
                "{level:?} 档计数进标题尾列：{title}"
            );
            assert!(
                !text_of(&lines).contains("src/a.rs"),
                "{level:?} 档不显示路径"
            );
            assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
        }

        app.finish_tool_block(ToolCallFinish {
            id: "call_find".into(),
            name: "find".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_find".into(),
                content: "No files found matching pattern".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        app.tool_detail = DetailLevel::Brief;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        assert!(text_of(&lines).contains("0 files"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    /// bash 简略档 = description 标题 + 单行命令 + 末尾 4 行输出。
    #[test]
    fn bash_brief_uses_description_and_single_line_command() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let arguments =
            r#"{"description":"Search the renderer","command":"rg -n foo\n  crates/moc-tui/src"}"#;
        let result = (0..10)
            .map(|index| format!("out-{index}"))
            .collect::<Vec<_>>()
            .join("\n");
        app.start_tool_block(ToolCallStart {
            id: "call_bash".into(),
            name: "bash".into(),
            parent_id: None,
            arguments: arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_bash".into(),
            name: "bash".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_bash".into(),
                content: result.clone(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width).lines;
        let brief_text = text_of(&brief);
        assert_eq!(brief.len(), 8, "标题 + 命令 + 4 行输出 + 省略 + 留白");
        assert!(
            brief_text.contains("Search the renderer"),
            "标题取 description"
        );
        assert!(
            brief[0]
                .spans
                .iter()
                .any(|span| span.content == "Search the renderer"
                    && span.style.add_modifier.contains(Modifier::BOLD)),
            "description 加粗与参数文本区分：{:?}",
            brief[0].spans
        );
        assert!(
            brief_text.contains("$ rg -n foo crates/moc-tui/src"),
            "命令折成单行：{brief_text}"
        );
        assert!(
            brief_text.contains("out-6") && brief_text.contains("out-9"),
            "只留末尾 4 行：{brief_text}"
        );
        assert!(!brief_text.contains("out-5"), "更早输出被省略");
        assert!(brief_text.contains("⋯ 还有 6 行"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), brief.len());

        app.tool_detail = DetailLevel::Hidden;
        let hidden = render_entry(0, &app.blocks[0], &app, width).lines;
        let hidden_text = text_of(&hidden);
        assert_eq!(hidden.len(), 2, "隐藏档 = 标题 + 留白");
        assert!(hidden_text.contains("Search the renderer"));
        assert!(!hidden_text.contains("rg -n"), "隐藏档不显示命令与输出");
        assert_eq!(estimate_entry_height_at(&app, 0, width), hidden.len());

        app.tool_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, width).lines;
        let full_text = text_of(&full);
        assert!(
            full_text.contains("crates/moc-tui/src"),
            "完整档显示命令全文"
        );
        assert!(full_text.contains("out-0") && full_text.contains("out-9"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());
    }

    /// grep 结果计数：上下文行与提示行都不算匹配，旧的一行式与新的分组式都要认。
    #[test]
    fn grep_counts_match_rows_and_ignore_context() {
        // 一行式（历史会话回放仍然是这个形态）。
        let legacy = "src/a.rs:1:alpha\nsrc/a.rs-2-2026-10-19 12:34:56\nsrc/b.rs:3:beta\n[1 files, 2 matches shown; limit reached]";
        assert_eq!(grep_counts(legacy), (2, 2));
        assert_eq!(grep_counts("No matches found"), (0, 0));
        assert_eq!(grep_counts("src/a.rs:1:only"), (1, 1));
        // 分组式：标题行加缩进行，正文里的 12:34:56 不能被当成命中。
        let grouped = "src/a.rs\n  1:alpha\n  2-2026-10-19 12:34:56\n  3:beta\nsrc/b.rs:4:gamma\n[1 files, 3 matches shown; limit reached]";
        assert_eq!(grep_counts(grouped), (3, 2));
    }

    /// find 计数跳过 core 追加的提示行，但不误伤 `[` 开头的真实路径。
    #[test]
    fn find_count_skips_core_notices() {
        let result = "src/a.rs\nsrc/[weird].rs\n[limit 200 reached; narrow the glob]\n[Scan incomplete (time limit)]";
        assert_eq!(find_file_count(result), 2);
        assert_eq!(find_file_count("No files found matching pattern"), 0);
    }

    #[test]
    fn nested_tool_call_renders_like_a_normal_one() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.start_tool_block(ToolCallStart {
            id: "call_run".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: r#"{"code":"await tools.grep({pattern:'artifact'})"}"#.into(),
        });
        app.start_tool_block(ToolCallStart {
            id: "call_run:0".into(),
            name: "grep".into(),
            parent_id: Some("call_run".into()),
            arguments: r#"{"pattern":"artifact","path":"artifact://ses_1/job.log"}"#.into(),
        });

        let lines = render_entry(1, &app.blocks[1], &app, 80).lines;
        let title = lines.first().map(|line| plain(line)).unwrap_or_default();
        assert!(
            title.starts_with(" ├─ ") || title.starts_with(" └─ "),
            "嵌套调用带连线前缀：{title:?}"
        );
        assert!(!title.contains('◆'), "子调用不画 bullet：{title}");
        assert!(
            title.contains("grep artifact"),
            "嵌套调用的摘要同样取参数里的模式：{title}"
        );
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
        app.start_tool_block(ToolCallStart {
            id: "call_edit".into(),
            name: "edit".into(),
            parent_id: None,
            arguments: r#"{"path":"src/lib.rs"}"#.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_edit".into(),
            name: "edit".into(),
            parent_id: None,
            arguments: r#"{"path":"src/lib.rs"}"#.into(),
            status: moc_protocol::methods::ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: "call_edit".into(),
                content: String::new(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: Some(diff),
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width).lines;
        // 标题 + 6 行 diff + 省略提示 + 留白
        assert_eq!(brief.len(), 9, "简略档 diff 限 6 行");
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
        let full = render_entry(0, &app.blocks[0], &app, width).lines;
        assert_eq!(full.len(), total + 2, "完整档 = 全部 diff 行 + 留白 + 标题");
        assert!(!text_of(&full).contains("还有"));
        assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());
    }

    /// 标题计数只统计真实改动；空 diff 不显示 [+0/-0]。
    #[test]
    fn edit_diff_header_hides_zero_counts() {
        use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let changed = ToolDiff {
            files: vec![ToolFileDiff {
                path: "src/lib.rs".into(),
                hunks: vec![DiffHunk {
                    old_start: 1,
                    new_start: 1,
                    lines: vec![
                        DiffLine {
                            kind: DiffLineKind::Delete,
                            old_no: Some(1),
                            new_no: None,
                            text: "old".into(),
                        },
                        DiffLine {
                            kind: DiffLineKind::Insert,
                            old_no: None,
                            new_no: Some(1),
                            text: "new".into(),
                        },
                    ],
                }],
            }],
            truncated: false,
        };
        let empty = ToolDiff {
            files: vec![ToolFileDiff {
                path: "src/empty.rs".into(),
                hunks: vec![DiffHunk {
                    old_start: 1,
                    new_start: 1,
                    lines: vec![DiffLine {
                        kind: DiffLineKind::Equal,
                        old_no: Some(1),
                        new_no: Some(1),
                        text: "same".into(),
                    }],
                }],
            }],
            truncated: false,
        };
        for (id, path, diff) in [
            ("call_change", "src/lib.rs", changed),
            ("call_empty", "src/empty.rs", empty),
        ] {
            let arguments = format!(r#"{{"path":"{path}"}}"#);
            app.start_tool_block(ToolCallStart {
                id: id.into(),
                name: "edit".into(),
                parent_id: None,
                arguments: arguments.clone(),
            });
            app.finish_tool_block(ToolCallFinish {
                id: id.into(),
                name: "edit".into(),
                parent_id: None,
                arguments,
                status: moc_protocol::methods::ToolStatus::Completed,
                result: moc_protocol::ToolResult {
                    tool_call_id: id.into(),
                    content: String::new(),
                    is_error: false,
                    images: Vec::new(),
                },
                duration_ms: Some(1),
                diff: Some(diff),
            });
        }
        let title = |index: usize| {
            render_entry(index, &app.blocks[index], &app, width).lines[0]
                .spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        assert!(title(0).contains("+1 -1"), "真实改动要有计数：{}", title(0));
        assert!(
            !title(1).contains("+") && !title(1).contains("-0"),
            "空 diff 不显示计数：{}",
            title(1)
        );
    }

    /// 截断 diff：标题带 …，正文提示只出现一次且不再说“改动过多”。
    #[test]
    fn truncated_diff_marks_partial_and_prints_notice_once() {
        use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        let file = |path: &str| ToolFileDiff {
            path: path.into(),
            hunks: vec![DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Insert,
                    old_no: None,
                    new_no: Some(1),
                    text: "x".into(),
                }],
            }],
        };
        let arguments = r#"{"path":"a.rs"}"#.to_owned();
        app.start_tool_block(ToolCallStart {
            id: "call_truncated".into(),
            name: "edit".into(),
            parent_id: None,
            arguments: arguments.clone(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_truncated".into(),
            name: "edit".into(),
            parent_id: None,
            arguments,
            status: moc_protocol::methods::ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: "call_truncated".into(),
                content: String::new(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: Some(ToolDiff {
                files: vec![file("a.rs"), file("b.rs")],
                truncated: true,
            }),
        });
        app.tool_detail = DetailLevel::Full;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };
        let title = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        let body = text_of(&lines[1..]);
        assert!(title.contains("+2 -0 …"), "部分统计要带 …：{title}");
        assert_eq!(
            body.matches("其余改动超出").count(),
            1,
            "截断提示只出现一次：{body}"
        );
        assert!(!body.contains("改动过多"), "不应再出现误导性措辞：{body}");
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
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
        let preview = render_entry(0, &app.blocks[0], &app, width).lines;
        let text = text_of(&preview);
        assert!(text.contains("IRC ◀ 探查者"), "标题必须标明来源：{text}");
        assert!(text.contains("injected"), "标题必须标出投递结果：{text}");
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
        let expanded = render_entry(0, &app.blocks[0], &app, width).lines;
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
        let back = render_entry(0, &app.blocks[0], &app, width).lines;
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
            DetailLevel::Hidden,
            true,
            TreeIndex::EMPTY,
        );
        assert_eq!(hidden.lines.len(), 2, "隐藏档只剩标题行与尾随留白");
    }

    /// 发送侧 IRC 卡片：广播回执逐行展开，摘要给出总量。
    #[test]
    fn message_agent_card_lists_receipt_rows() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 60u16;
        let arguments = r#"{"target":"all","message":"请复查一下改动"}"#;
        app.start_tool_block(ToolCallStart {
            id: "call_msg".into(),
            name: "message_agent".into(),
            parent_id: None,
            arguments: arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_msg".into(),
            name: "message_agent".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
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
            duration_ms: Some(3),
            diff: None,
        });
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("IRC"), "工具卡标题必须显示 IRC：{text}");
        assert!(text.contains("▶ all"));
        assert!(text.contains("broadcast"));
        assert!(text.contains("1 delivered") && text.contains("1 failed"));
        assert!(
            text.contains("请复查一下改动"),
            "正文必须带消息预览：{text}"
        );
        assert!(text.contains("甲 injected"));
        assert!(text.contains("乙 failed：收件箱已满"));
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
        let arguments = r#"{"agent_ids":["ses_a","ses_b"]}"#;
        app.start_tool_block(ToolCallStart {
            id: "call_wait".into(),
            name: "wait_agent".into(),
            parent_id: None,
            arguments: arguments.into(),
        });
        app.set_tool_progress(
            "call_wait",
            vec![
                ToolProgressRow {
                    agent_id: "ses_a".into(),
                    title: "甲".into(),
                    status: AgentStatus::Running,
                    elapsed_ms: 3_200,
                    activity: Some("cargo test".into()),
                },
                ToolProgressRow {
                    agent_id: "ses_b".into(),
                    title: "乙".into(),
                    status: AgentStatus::Completed,
                    elapsed_ms: 0,
                    activity: None,
                },
            ],
        );
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("等待 2 个代理"));
        assert!(
            text.contains("▸ 甲 · running · 3.2s · cargo test"),
            "运行行带耗时与活动摘要：{text}"
        );
        assert!(text.contains("◆ 乙 · done"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            lines.len(),
            "进度态估算与渲染同源"
        );

        app.finish_tool_block(ToolCallFinish {
            id: "call_wait".into(),
            name: "wait_agent".into(),
            parent_id: None,
            arguments: arguments.into(),
            status: ToolStatus::Completed,
            result: ToolResult {
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
            duration_ms: Some(1_100),
            diff: None,
        });
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("◆ 甲 · done"), "结果行回落到快照：{text}");
        assert!(text.contains("▸ 乙 · running"));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            lines.len(),
            "结果态估算与渲染同源"
        );
    }

    /// sidekick 卡片：摘要带 mode 标注；落定后状态行带模型、结构化结果一行
    /// 摘要与 open_issue 逐条列出，note/report 照常。
    #[test]
    fn sidekick_card_renders_model_result_and_mode() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 60u16;
        app.start_tool_block(ToolCallStart {
            id: "call_sk".into(),
            name: "sidekick".into(),
            parent_id: None,
            arguments: r#"{"mode":"compact","message":"跑一轮验证"}"#.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_sk".into(),
            name: "sidekick".into(),
            parent_id: None,
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_sk".into(),
                content: serde_json::json!({
                    "agent_id": "ses_sk",
                    "delivery": "compacted",
                    "status": "completed",
                    "model": "test/gpt-cheap:off",
                    "result": {
                        "status": "done",
                        "changes": [{"path": "a.rs", "summary": "改"}],
                        "verification": [
                            {"command": "cargo test", "outcome": "passed"},
                            {"command": "lint", "outcome": "not_run"}
                        ],
                        "open_issues": ["还有一个待定项"]
                    },
                    "note": "switched model",
                    "report": "验证通过"
                })
                .to_string(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(900),
            diff: None,
        });
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        let text = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(text.contains("跑一轮验证 · 压缩后"), "摘要带 mode：{text}");
        assert!(
            text.contains("◆ Sidekick · compacted · completed · test/gpt-cheap:off"),
            "{text}"
        );
        assert!(
            text.contains("结果 done · 改动 1 个文件 · 验证 1 通过 1 未跑"),
            "{text}"
        );
        assert!(text.contains("! 还有一个待定项"), "{text}");
        assert!(
            text.contains("switched model") && text.contains("验证通过"),
            "{text}"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    /// ask 结果解析：header 优先 id 兜底、逐问「未回答」标记、整单取消与超时哨兵、
    /// 单问裸答案、自定义答案换行并入上一条。
    #[test]
    fn ask_answer_pairs_parses_headers_answers_and_sentinels() {
        use moc_protocol::methods::ToolStatus;

        let args: Value = serde_json::json!({
            "questions": [
                {"id": "theme", "question": "主题?", "header": "主题", "options": []},
                {"id": "features", "question": "特性?", "options": []},
                {"id": "lang", "question": "语言?", "header": "  ", "options": []}
            ]
        });
        let block = |result: &str| ToolCallBlock {
            id: "call_ask".into(),
            batch_id: None,
            parent_id: None,
            name: "ask".into(),
            arguments: "{}".into(),
            status: Some(ToolStatus::Completed),
            result: Some(result.to_owned()),
            diff: None,
            duration_ms: None,
            running: false,
            progress: None,
            images: Vec::new(),
            job_id: None,
            live_output: None,
        };

        let pairs = ask_answer_pairs(
            &block(
                "User answers:\ntheme: 墨黑\nfeatures: The user did not answer.\nlang: Rust, 日语",
            ),
            &args,
        );
        assert_eq!(
            pairs,
            vec![
                ("主题".to_owned(), "墨黑".to_owned(), false),
                ("features".to_owned(), "未回答".to_owned(), true),
                ("lang".to_owned(), "Rust, 日语".to_owned(), false),
            ],
            "header 优先、空 header 与缺 header 都回落到 id"
        );

        // 自定义答案里的换行并进上一条，不顶出新问。
        let pairs = ask_answer_pairs(
            &block("User answers:\ntheme: 墨黑\n底色加暗\nfeatures: x\nlang: Rust"),
            &args,
        );
        assert_eq!(pairs[0].1, "墨黑 底色加暗", "换行内容并入上一条答案");
        assert_eq!(pairs.len(), 3);

        // 单问：core 只写裸答案，没有 `id: ` 前缀。
        let single_args: Value = serde_json::json!({
            "questions": [{"id": "theme", "question": "主题?", "header": "主题", "options": []}]
        });
        let pairs = ask_answer_pairs(&block("墨黑配米色"), &single_args);
        assert_eq!(
            pairs,
            vec![("主题".to_owned(), "墨黑配米色".to_owned(), false)]
        );

        // 整单哨兵。
        assert_eq!(
            ask_answer_pairs(&block("The user did not answer."), &args),
            vec![(String::new(), "未回答".to_owned(), true)],
            "取消/跳过整单"
        );
        assert_eq!(
            ask_answer_pairs(
                &block(
                    "The user did not answer within the time limit; continue without an answer."
                ),
                &args
            ),
            vec![(String::new(), "超时未回答".to_owned(), true)],
            "超时哨兵"
        );
    }

    /// ask 转写卡渲染：每问一行、key 对齐、未回答翻译、长答案单行截断、
    /// 简略档行数上限 + 省略提示、估算与渲染同源。
    #[test]
    fn ask_tool_card_styles_answers_and_clamps_rows() {
        use moc_protocol::ToolResult;
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let width = 80u16;
        // 8 问超过简略档 6 行上限；q6 是巨长答案，验证单行截断不撑行。
        let long_answer = "很".repeat(200);
        let questions = (0..8)
            .map(|index| {
                format!(
                    r#"{{"id":"q{index}","question":"问题{index}?","header":"标题{index}","options":[]}}"#
                )
            })
            .collect::<Vec<_>>()
            .join(",");
        let arguments = format!(r#"{{"questions":[{questions}]}}"#);
        let result = [
            "User answers:".to_owned(),
            "q0: 墨黑".to_owned(),
            "q1: The user did not answer.".to_owned(),
            "q2: A, B (自定义)".to_owned(),
            "q3: 三".to_owned(),
            "q4: 四".to_owned(),
            "q5: 五".to_owned(),
            format!("q6: {long_answer}"),
            "q7: 七".to_owned(),
        ]
        .join("\n");
        app.start_tool_block(ToolCallStart {
            id: "call_ask".into(),
            name: "ask".into(),
            parent_id: None,
            arguments: arguments.clone(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "call_ask".into(),
            name: "ask".into(),
            parent_id: None,
            arguments,
            status: ToolStatus::Completed,
            result: ToolResult {
                tool_call_id: "call_ask".into(),
                content: result,
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(5),
            diff: None,
        });
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, width).lines;
        let full_text = text_of(&full);
        assert!(!full_text.contains("User answers:"), "协议头不进转写");
        assert!(!full_text.contains("The user did not answer"));
        assert!(full_text.contains("标题0"), "key 用 header 不用 id");
        assert!(!full_text.contains("q0"), "裸 id 不出现在卡片里");
        assert!(
            full_text.contains("标题1: 未回答"),
            "未回答标记:{full_text}"
        );
        assert!(full_text.contains("标题2: A, B (自定义)"));
        for line in &full {
            assert!(
                line.width() <= usize::from(width),
                "巨长答案也不撑行:{}",
                line.width()
            );
        }
        let answer_row = full
            .iter()
            .find(|line| text_of(std::slice::from_ref(line)).contains("标题6"))
            .expect("巨长答案行");
        assert!(
            text_of(std::slice::from_ref(answer_row)).contains('…'),
            "巨长答案截断加省略号"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, width).lines;
        let brief_text = text_of(&brief);
        assert!(brief_text.contains("标题5"), "简略档保留前 6 行答案");
        assert!(
            !brief_text.contains("标题6") && !brief_text.contains("标题7"),
            "超出简略档的答案行被省略"
        );
        assert!(brief_text.contains("⋯ 还有 2 行"), "省略提示:{brief_text}");
        assert_eq!(estimate_entry_height_at(&app, 0, width), brief.len());
    }

    /// 工具结果里的图片：正文多出的行数估算与渲染必须一致。
    #[test]
    fn tool_result_images_keep_estimate_and_render_in_sync() {
        use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
        use crate::render::image::{DecodedImage, ImageKey};
        use crate::transcript::ToolImage;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.graphics = Some(GraphicsSupport {
            proto: GraphicsProto::Kitty,
            cell: CellPixels { w: 8, h: 18 },
        });
        let key = ImageKey::from_bytes("image/png", "AAAA");
        app.images.complete(
            key.clone(),
            Ok(DecodedImage::Raster(image::RgbaImage::from_pixel(
                40,
                20,
                image::Rgba([9, 9, 9, 255]),
            ))),
        );
        app.push_block(Block::Tool(Box::new(ToolCallBlock {
            id: "shot".into(),
            batch_id: None,
            parent_id: None,
            name: "screenshot".into(),
            arguments: "{}".into(),
            status: Some(moc_protocol::methods::ToolStatus::Completed),
            result: None,
            diff: None,
            duration_ms: None,
            running: false,
            progress: None,
            images: vec![ToolImage::for_test(key, "image/png", Some("AAAA"))],
            job_id: None,
            live_output: None,
        })));

        let width = 80u16;
        let rendered = render_entry(0, &app.blocks[0], &app, width);
        assert_eq!(rendered.images.len(), 1, "一张图一张落位");
        let placement = &rendered.images[0];
        let prose_col = u16::try_from(PROSE_INDENT).expect("正文列应放得进 u16");
        assert_eq!(placement.x, prose_col, "图片落在正文列");
        assert!(
            placement.row + usize::from(placement.rows) <= rendered.lines.len(),
            "占位行必须真的存在"
        );
        assert!(rendered.lines[placement.row].width() >= usize::from(placement.cols));
        assert_eq!(
            estimate_entry_height_at(&app, 0, width),
            rendered.lines.len(),
            "图片块：估算与渲染必须同源"
        );
    }

    /// 绘制登记：图片槽位落在正文前缀上；比视口宽时不登记（会盖住边框）。
    #[test]
    fn paint_registers_tool_image_slots_within_the_viewport() {
        use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
        use crate::render::image::{DecodedImage, ImageKey};

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.graphics = Some(GraphicsSupport {
            proto: GraphicsProto::Kitty,
            cell: CellPixels { w: 8, h: 18 },
        });
        let key = ImageKey::from_bytes("image/png", "AAAA");
        app.images.complete(
            key.clone(),
            Ok(DecodedImage::Raster(image::RgbaImage::from_pixel(
                40,
                20,
                image::Rgba([9, 9, 9, 255]),
            ))),
        );
        app.push_block(Block::Tool(Box::new(ToolCallBlock {
            id: "shot".into(),
            batch_id: None,
            parent_id: None,
            name: "screenshot".into(),
            arguments: "{}".into(),
            status: Some(moc_protocol::methods::ToolStatus::Completed),
            result: None,
            diff: None,
            duration_ms: None,
            running: false,
            progress: None,
            images: vec![crate::transcript::ToolImage::for_test(
                key,
                "image/png",
                Some("AAAA"),
            )],
            job_id: None,
            live_output: None,
        })));

        use ratatui::Terminal;
        use ratatui::backend::TestBackend;
        let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("测试终端");
        let mut view = ScrollbackView::new();
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .expect("绘制成功");
        let slot = app.image_slots.first().expect("图片应登记槽位").clone();
        assert!(matches!(slot.source, SlotSource::Image(_)));
        let prose_col = u16::try_from(PROSE_INDENT).expect("正文列应放得进 u16");
        assert_eq!(slot.x, app.scrollback_area.x + prose_col, "落位对齐正文列");
        assert_eq!(slot.cols, 5, "40px 宽 ÷ 8px 单元格");
        assert_eq!(slot.rows, 2, "20px 高 ÷ 18px 单元格，向上取整");
        assert!(slot.placement_id != 0, "kitty 放置 ID 不得为零");

        // 视口比占位区窄：整块放不下就不登记，免得盖住边框
        let mut narrow = ScrollbackView::new();
        narrow.width = 100;
        narrow.cache = vec![CachedEntry {
            key: (0, 0),
            layout_epoch: 0,
            layout: TreeIndex::EMPTY,
            height: 3,
            paged: None,
            footer: Vec::new(),
            line_start: 0,
            lines: vec![
                Line::from("标题"),
                Line::from(" ".repeat(40)),
                Line::from(" ".repeat(40)),
            ],
            rendered: true,
            rendered_at: std::time::Instant::now(),
            render_cost: std::time::Duration::ZERO,
            tables: Vec::new(),
            images: vec![ImagePlacement {
                row: 1,
                x: 2,
                cols: 40,
                rows: 2,
                px: (320, 36),
                key: ImageKey::from_bytes("image/png", "zzz"),
            }],
        }];
        let mut buffer = Buffer::empty(Rect::new(0, 0, 20, 10));
        let mut slots = Vec::new();
        let mut hits = Vec::new();
        let mut charts = Vec::new();
        let mut lines = Vec::new();
        narrow.paint(
            &mut buffer,
            Rect::new(0, 0, 20, 10),
            0,
            false,
            Theme::global(),
            None,
            PaintSinks {
                hits: &mut hits,
                charts: &mut charts,
                slots: &mut slots,
                lines: &mut lines,
            },
        );
        assert!(slots.is_empty(), "占位区超出视口宽度时不登记");
    }

    #[test]
    fn distant_graphics_in_large_messages_stay_outside_the_viewport() {
        let mut view = ScrollbackView::new();
        view.cache.push(CachedEntry {
            key: (0, 0),
            layout_epoch: 0,
            layout: TreeIndex::EMPTY,
            height: 100_010,
            lines: vec![Line::from("可见正文")],
            paged: None,
            footer: Vec::new(),
            line_start: 0,
            rendered: true,
            rendered_at: std::time::Instant::now(),
            render_cost: std::time::Duration::ZERO,
            tables: vec![LocalTable {
                table: 0,
                button: Some(ButtonSpan {
                    row: 100_000,
                    x: 0,
                    w: 10,
                }),
                chart: Some(ChartPlacement {
                    row: 100_001,
                    cols: 5,
                    rows: 2,
                    px: (40, 36),
                }),
            }],
            images: vec![ImagePlacement {
                row: 100_004,
                x: 0,
                cols: 5,
                rows: 2,
                px: (40, 36),
                key: crate::render::image::ImageKey::from_bytes("image/png", "distant"),
            }],
        });
        let area = Rect::new(0, 5, 80, 10);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 80, 20));
        let (mut hits, mut charts, mut slots, mut lines) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        view.paint(
            &mut buffer,
            area,
            0,
            true,
            Theme::global(),
            None,
            PaintSinks {
                hits: &mut hits,
                charts: &mut charts,
                slots: &mut slots,
                lines: &mut lines,
            },
        );
        assert!(charts.is_empty());
        assert!(slots.is_empty());
        assert_eq!(lines, [(5, "可见正文".to_owned())]);
    }

    #[test]
    fn thinking_block_and_rows_apply_thinking_and_grayscale_palette() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(crate::theme::Theme::global(), events);
        let lines = thinking_block(
            "test line",
            Some(10),
            Some(1_000),
            &app,
            80,
            DetailLevel::Full,
        );

        // Title line: spans[1] is '◆ ' in accent_thinking, spans[2] is 'Thought' in gray_bright
        let title_spans = &lines[0].spans;
        assert_eq!(title_spans[1].content, "◆ ");
        assert_eq!(title_spans[1].style.fg, Some(app.theme.accent_thinking));
        assert_eq!(title_spans[2].content, "Thought");
        assert_eq!(title_spans[2].style.fg, Some(app.theme.gray_bright));

        // Thinking rows: first content row starts with '▏ ' in accent_thinking, content in gray
        assert!(lines.len() >= 2);
        let row_spans = &lines[1].spans;
        assert_eq!(row_spans[1].content, "▏ ");
        assert_eq!(row_spans[1].style.fg, Some(app.theme.accent_thinking));
        assert_eq!(row_spans[2].style.fg, Some(app.theme.gray));
    }

    /// 一行的纯文本。
    fn plain(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    /// 一行首个非空白字符所在的列。
    fn lead_col(line: &Line<'_>) -> usize {
        let text = plain(line);
        str_width(&text) - str_width(text.trim_start())
    }

    /// 开始并结束一次工具调用（正常路径：先 start 再 finish）。
    fn run_tool(
        app: &mut App,
        id: &str,
        name: &str,
        arguments: &str,
        status: moc_protocol::methods::ToolStatus,
        content: &str,
    ) {
        app.start_tool_block(ToolCallStart {
            id: id.into(),
            name: name.into(),
            parent_id: None,
            arguments: arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: id.into(),
            name: name.into(),
            parent_id: None,
            arguments: arguments.into(),
            status,
            result: moc_protocol::ToolResult {
                tool_call_id: id.into(),
                content: content.into(),
                is_error: status != moc_protocol::methods::ToolStatus::Completed,
                images: Vec::new(),
            },
            duration_ms: Some(1_200),
            diff: None,
        });
    }

    /// 整份转写按块顺序拼成的行（批次成员只由组首块产出）。
    fn transcript(app: &App, width: u16) -> Vec<Line<'static>> {
        (0..app.blocks.len())
            .flat_map(|index| render_entry(index, &app.blocks[index], app, width).lines)
            .collect()
    }

    /// 防复发守卫：所有块共用一套左缘网格，且任何行都不超出块宽。
    ///
    /// 标记（`>`、`◆`、批次 bullet）落在 [`TITLE_INDENT`]，正文（用户、助手、
    /// 工具输出、耗时尾注）落在 [`PROSE_INDENT`]；放不下的标题以 `…` 收尾，
    /// bash 命令只出现一次。
    #[test]
    fn transcript_blocks_share_one_left_grid() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        let width = 60u16;
        app.push_block(Block::User {
            text: "你好".into(),
        });
        app.push_block(Block::Assistant {
            text: "收到。".into(),
        });
        let command = "cargo test -p moc-tui --lib -- render::scrollback::tests --nocapture";
        run_tool(
            &mut app,
            "bash",
            "bash",
            &serde_json::json!({ "command": command }).to_string(),
            ToolStatus::Completed,
            "ok",
        );
        let pattern = "x".repeat(120);
        run_tool(
            &mut app,
            "grep",
            "grep",
            &serde_json::json!({ "pattern": pattern }).to_string(),
            ToolStatus::Completed,
            "src/a.rs:1:x",
        );
        app.register_tool_batch("batch".into(), ["r1".to_owned(), "r2".to_owned()]);
        for id in ["r1", "r2"] {
            run_tool(
                &mut app,
                id,
                "read",
                r#"{"path":"a.rs"}"#,
                ToolStatus::Completed,
                "body",
            );
        }

        for (index, entry) in app.blocks.iter().enumerate() {
            for line in render_entry(index, entry, &app, width).lines {
                assert!(
                    line.width() <= usize::from(width),
                    "第 {index} 块越界：{:?}",
                    plain(&line)
                );
            }
        }
        let block = |index: usize| -> Vec<Line<'static>> {
            render_entry(index, &app.blocks[index], &app, width)
                .lines
                .into_iter()
                .filter(|line| !plain(line).trim().is_empty())
                .collect()
        };

        let user = block(0);
        assert_eq!(lead_col(&user[0]), TITLE_INDENT, "用户标记与块 bullet 同列");
        assert!(plain(&user[0])[TITLE_INDENT..].starts_with("> 你好"));
        assert_eq!(lead_col(&block(1)[0]), PROSE_INDENT, "助手正文");

        let bash = block(2);
        assert_eq!(lead_col(&bash[0]), TITLE_INDENT, "工具标题");
        for line in &bash[1..] {
            assert_eq!(lead_col(line), PROSE_INDENT, "工具正文：{:?}", plain(line));
        }
        let mentions = bash
            .iter()
            .filter(|line| plain(line).contains("cargo test"))
            .count();
        assert_eq!(mentions, 1, "bash 命令只出现一次");

        let grep = block(3);
        assert!(
            plain(&grep[0]).contains(crate::render::wrap::ELLIPSIS),
            "超长摘要截断加 …（右端是耗时）：{:?}",
            plain(&grep[0])
        );

        let batch = block(4);
        assert_eq!(
            lead_col(&batch[0]),
            TITLE_INDENT,
            "批次 bullet 与块 bullet 同列"
        );
        for line in &batch[1..] {
            assert_eq!(
                lead_col(line),
                TITLE_INDENT,
                "批次成员连接符落在 bullet 正下方：{:?}",
                plain(line)
            );
        }
    }

    /// 防复发守卫：块与块之间恰好一行留白——不叠成两行，也不贴在一起。
    #[test]
    fn blocks_are_separated_by_exactly_one_blank_row() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        app.push_block(Block::User {
            text: "第一问".into(),
        });
        app.push_block(Block::Assistant {
            text: "答复。".into(),
        });
        run_tool(
            &mut app,
            "read",
            "read",
            r#"{"path":"a.rs"}"#,
            ToolStatus::Completed,
            "body",
        );
        app.push_block(Block::Assistant {
            text: "读完了。".into(),
        });
        app.push_block(Block::User {
            text: "第二问".into(),
        });
        app.register_tool_batch("batch".into(), ["r1".to_owned(), "r2".to_owned()]);
        for id in ["r1", "r2"] {
            run_tool(
                &mut app,
                id,
                "read",
                r#"{"path":"b.rs"}"#,
                ToolStatus::Completed,
                "body",
            );
        }
        app.push_block(Block::Assistant {
            text: "收尾。".into(),
        });

        let rows: Vec<bool> = transcript(&app, 80)
            .iter()
            .map(|line| plain(line).trim().is_empty())
            .collect();
        assert!(!rows[0], "转写首行不留白");
        for (index, pair) in rows.windows(2).enumerate() {
            assert!(!(pair[0] && pair[1]), "第 {index} 行起出现连续两行留白");
        }
        // 工具块的尾随留白由 gap 决定（树尾才有），其余块恒以一行留白结尾。
        let mut tree = TreeIndex::default();
        tree.sync(&app.blocks);
        let mut batches = BatchIndex::default();
        batches.sync(&app.blocks, &mut Vec::new());
        for index in 0..app.blocks.len() {
            let lines = render_entry(index, &app.blocks[index], &app, 80).lines;
            let Some(last) = lines.last() else {
                continue;
            };
            if matches!(app.blocks[index].block, Block::Tool(_)) {
                assert_eq!(
                    plain(last).is_empty(),
                    block_layout(&batches, &tree, index).gap,
                    "第 {index} 块尾随留白与 gap 规则不一致"
                );
            } else {
                assert!(plain(last).is_empty(), "第 {index} 块缺尾随留白");
            }
        }
    }

    /// 回归：批次里失败的成员必须可见，不能被计入完成。
    #[test]
    fn batch_marks_failed_members_and_counts_only_completed() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        app.register_tool_batch("batch".into(), ["g1".to_owned(), "g2".to_owned()]);
        run_tool(
            &mut app,
            "g1",
            "grep",
            r#"{"pattern":"ok"}"#,
            ToolStatus::Completed,
            "src/a.rs:1:ok",
        );
        run_tool(
            &mut app,
            "g2",
            "grep",
            r#"{"pattern":"bad"}"#,
            ToolStatus::Error,
            "regex parse error",
        );

        let lines: Vec<String> = render_entry(0, &app.blocks[0], &app, 80)
            .lines
            .iter()
            .map(plain)
            .collect();
        let failed = lines
            .iter()
            .find(|line| line.contains("bad"))
            .expect("失败成员行");
        assert!(failed.contains('✘'), "失败成员带 ✘：{failed:?}");
        // 统计进了组头右端尾列，不再有独立的统计行。
        let header = lines.first().expect("组头行");
        assert!(header.contains("1/2"), "只计成功：{header:?}");
        assert!(
            header.contains(crate::render::labels::status::FAILED),
            "组头点明失败：{header:?}"
        );
        assert!(
            lines[1..].iter().all(|line| !line.contains("/2")),
            "组尾不再有统计行：{lines:?}"
        );
        assert_eq!(
            estimate_entry_height_at(&app, 0, 80),
            lines.len(),
            "估高与渲染一致"
        );
    }

    /// 回归：思考简略档的预览预算不被段间空行吃掉，并提示还藏了多少行。
    #[test]
    fn brief_thinking_skips_blank_rows_and_marks_omission() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let app = App::new(crate::theme::Theme::global(), events);
        let text = (1..=8)
            .map(|index| format!("段落 {index}"))
            .collect::<Vec<_>>()
            .join("\n\n");
        let lines = thinking_block(&text, None, None, &app, 80, DetailLevel::Brief);
        let body: Vec<String> = lines[1..lines.len() - 1].iter().map(plain).collect();
        for row in &body {
            assert_ne!(row.trim(), "▏", "预览里不应出现空引文行");
        }
        assert!(
            body.iter().any(|row| row.contains("段落 8")),
            "保留最新内容"
        );
        assert!(
            body.last()
                .is_some_and(|row| row.contains(&crate::render::labels::more_lines(4))),
            "末行提示省略：{body:?}"
        );
    }

    /// run_code：简略档只有标题行（返回值 JSON 不倒出来）；失败时简略档显示错误文本。
    #[test]
    fn run_code_brief_shows_title_only_and_errors() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        run_tool(
            &mut app,
            "rc",
            "run_code",
            r#"{"description":"核对","code":"await tools.bash({command:'ls'})"}"#,
            ToolStatus::Completed,
            r#"[{"status":"fulfilled","value":{"content":"ok"}}]"#,
        );
        let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
        assert_eq!(lines.len(), 2, "简略档 = 标题 + 留白");
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(
            !text.contains("fulfilled") && !text.contains("status"),
            "{text}"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, 80), lines.len());

        run_tool(
            &mut app,
            "rc_bad",
            "run_code",
            r#"{"code":"throw 1"}"#,
            ToolStatus::Error,
            "脚本执行失败：boom",
        );
        let lines = render_entry(1, &app.blocks[1], &app, 80).lines;
        let text: String = lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect();
        assert!(text.contains("脚本执行失败：boom"), "失败文本可见：{text}");
        assert_eq!(estimate_entry_height_at(&app, 1, 80), lines.len());
    }

    /// run_code 完整档：行号槽脚本 + JSON 树；多行字符串标 `(N lines)`。
    #[test]
    fn run_code_full_renders_script_gutter_and_json_tree() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Full;
        run_tool(
            &mut app,
            "rc",
            "run_code",
            r#"{"description":"核对","code":"const a = 1;\nawait tools.bash({command:'ls'})"}"#,
            ToolStatus::Completed,
            r#"[{"status":"fulfilled","value":{"content":" M crates/moc-tui/src/render/scrollback.rs 很长很长很长的第一行\n M b.rs","isError":false,"structuredContent":null}}]"#,
        );
        let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
        let rows: Vec<String> = lines.iter().map(plain).collect();
        for row in &rows {
            eprintln!("{row}");
        }
        assert!(
            rows.iter().any(|row| row.contains("1 │ const a = 1;")),
            "脚本行号槽：{rows:?}"
        );
        assert!(
            rows.iter()
                .any(|row| row.contains("status:") && row.contains("fulfilled")),
            "JSON 树成员行：{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("(2 lines)")),
            "多行字符串标注：{rows:?}"
        );
        assert!(
            rows.iter().any(|row| row.contains("structuredContent:")),
            "null 标量成员：{rows:?}"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, 80), lines.len());

        // 窄宽度下多行字符串先扣 ` (N lines)` 后缀再截首行，后缀不被裁掉。
        let narrow: Vec<String> = render_entry(0, &app.blocks[0], &app, 44)
            .lines
            .iter()
            .map(plain)
            .collect();
        assert!(
            narrow.iter().any(|row| row.contains("(2 lines)")),
            "窄宽度仍显示 (N lines)：{narrow:?}"
        );
    }

    /// 嵌套连线：父已结束时首个子块 `├─`、末个 `└─`；父还在跑时末个仍 `├─`。
    /// 子调用不画 bullet，连接符本身就是标记；空行只出现在树尾。
    #[test]
    fn nested_children_draw_tree_connectors() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        // 两个子调用用不同名字：同名相邻子调用会合并成组，测不到各自的连接符。
        let spawn_children = |app: &mut App| {
            for (id, name, arguments) in [
                ("c1", "bash", r#"{"command":"echo c1"}"#),
                ("c2", "grep", r#"{"pattern":"x"}"#),
            ] {
                app.start_tool_block(ToolCallStart {
                    id: id.into(),
                    name: name.into(),
                    parent_id: Some("p".into()),
                    arguments: arguments.into(),
                });
                app.finish_tool_block(ToolCallFinish {
                    id: id.into(),
                    name: name.into(),
                    parent_id: Some("p".into()),
                    arguments: String::new(),
                    status: ToolStatus::Completed,
                    result: moc_protocol::ToolResult {
                        tool_call_id: id.into(),
                        content: "out".into(),
                        is_error: false,
                        images: Vec::new(),
                    },
                    duration_ms: Some(1),
                    diff: None,
                });
            }
        };
        // 父仍在运行：两个子块都画 ├─（更多子调用还会到来）。
        app.start_tool_block(ToolCallStart {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: r#"{"description":"父"}"#.into(),
        });
        spawn_children(&mut app);
        fn title_of(app: &App, index: usize) -> String {
            render_entry(index, &app.blocks[index], app, 80)
                .lines
                .into_iter()
                .next()
                .map(|line| plain(&line))
                .unwrap_or_default()
        }
        assert!(
            title_of(&app, 1).starts_with(" ├─ "),
            "运行中：{:?}",
            title_of(&app, 1)
        );
        assert!(
            title_of(&app, 2).starts_with(" ├─ "),
            "运行中末块仍 ├─：{:?}",
            title_of(&app, 2)
        );
        assert!(
            !title_of(&app, 1).contains('◆'),
            "子调用不画 bullet：{:?}",
            title_of(&app, 1)
        );

        // 父结束：末个子块翻成 └─ 并留下一行纯空行；中间的子块不留白。
        app.finish_tool_block(ToolCallFinish {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: "p".into(),
                content: "[]".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(9),
            diff: None,
        });
        assert!(
            title_of(&app, 1).starts_with(" ├─ "),
            "首块仍 ├─：{:?}",
            title_of(&app, 1)
        );
        assert!(
            title_of(&app, 2).starts_with(" └─ "),
            "末块 └─：{:?}",
            title_of(&app, 2)
        );
        let child1 = render_entry(1, &app.blocks[1], &app, 80).lines;
        let tail = child1.last().map(plain).unwrap_or_default();
        assert!(!tail.trim().is_empty(), "非末兄弟不留尾随空行：{tail:?}");
        let child2 = render_entry(2, &app.blocks[2], &app, 80).lines;
        assert_eq!(
            plain(child2.last().expect("末块末行")),
            "",
            "树尾空行不带连线前缀"
        );
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, 80),
                render_entry(index, &app.blocks[index], &app, 80)
                    .lines
                    .len(),
                "第 {index} 块估高与渲染一致"
            );
        }
    }

    /// 嵌套树的留白层级：父块运行中整棵树没有空行（父标题紧接第一个子行）；
    /// 父块结束后只有最末后代带一行完全为空的空行。第一个子调用到达会让
    /// 父块的 gap 翻转，缓存里的旧版式必须被重渲染掉。
    #[test]
    fn nested_tree_leaves_blank_only_after_the_last_descendant() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        let width = 80u16;
        let finish = |app: &mut App, id: &str, name: &str, parent: Option<&str>, content: &str| {
            app.finish_tool_block(ToolCallFinish {
                id: id.into(),
                name: name.into(),
                parent_id: parent.map(str::to_owned),
                arguments: String::new(),
                status: ToolStatus::Completed,
                result: moc_protocol::ToolResult {
                    tool_call_id: id.into(),
                    content: content.into(),
                    is_error: false,
                    images: Vec::new(),
                },
                duration_ms: Some(1),
                diff: None,
            });
        };
        app.start_tool_block(ToolCallStart {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: r#"{"description":"父"}"#.into(),
        });
        // 尚无子调用：顶层单块照常带一行尾随留白。
        let mut view = ScrollbackView::new();
        view.sync(&app, width);
        let tail = plain(view.cache[0].lines.last().expect("父块末行"));
        assert!(tail.is_empty(), "无子调用时父块照常留白：{tail:?}");

        // 两个不同名的子调用（不合并），父块仍在运行。
        app.start_tool_block(ToolCallStart {
            id: "c1".into(),
            name: "bash".into(),
            parent_id: Some("p".into()),
            arguments: r#"{"command":"echo hi"}"#.into(),
        });
        finish(&mut app, "c1", "bash", Some("p"), "out");
        app.start_tool_block(ToolCallStart {
            id: "c2".into(),
            name: "grep".into(),
            parent_id: Some("p".into()),
            arguments: r#"{"pattern":"x"}"#.into(),
        });
        finish(&mut app, "c2", "grep", Some("p"), "src/a.rs:1:x");

        // 第一个子调用到达时父块的 gap 已翻转：缓存里是重渲染后的新版式。
        view.sync(&app, width);
        let parent = &view.cache[0].lines;
        assert!(
            parent.iter().all(|line| !plain(line).trim().is_empty()),
            "有子调用的父块不留尾随空行：{parent:?}"
        );
        let parent_last = render_entry(0, &app.blocks[0], &app, width)
            .lines
            .last()
            .map(plain)
            .unwrap_or_default();
        assert!(
            !parent_last.trim().is_empty(),
            "父块渲染结果没有空行：{parent_last:?}"
        );

        // 父块还在跑：整棵树没有任何空行。
        let rows = transcript(&app, width);
        assert!(
            rows.iter().all(|line| !plain(line).trim().is_empty()),
            "父运行中全树无空行：{rows:?}"
        );
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, width),
                render_entry(index, &app.blocks[index], &app, width)
                    .lines
                    .len(),
                "第 {index} 块估高与渲染一致"
            );
        }

        // 父块结束：整棵树只剩一行空行，且完全为空、挂在最末子调用末尾。
        finish(&mut app, "p", "run_code", None, "[]");
        let rows = transcript(&app, width);
        let blanks: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(_, line)| plain(line).is_empty())
            .map(|(index, _)| index)
            .collect();
        assert_eq!(
            blanks.as_slice(),
            &[rows.len() - 1],
            "整棵树只有末尾一行空行：{blanks:?}"
        );
        let child2 = render_entry(2, &app.blocks[2], &app, width).lines;
        assert_eq!(
            plain(child2.last().expect("末子末行")),
            "",
            "树尾空行不带连线前缀"
        );
        let child1 = render_entry(1, &app.blocks[1], &app, width).lines;
        assert!(
            !plain(child1.last().expect("中间子末行")).trim().is_empty(),
            "中间子调用不留白"
        );
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, width),
                render_entry(index, &app.blocks[index], &app, width)
                    .lines
                    .len(),
                "父结束后第 {index} 块估高与渲染一致"
            );
        }
    }

    /// 同一父调用下连续同名的子调用合并成组（组头 `read (N)`，成员画在下一层）；
    /// 不同名打断成各自单块。父块结束后最末子调用翻成 `└─` 并带空行。
    #[test]
    fn nested_same_name_children_merge_into_one_group() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        let width = 80u16;
        app.start_tool_block(ToolCallStart {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: r#"{"description":"父"}"#.into(),
        });
        let children = [
            ("r1", "read", r#"{"path":"a.rs"}"#),
            ("r2", "read", r#"{"path":"b.rs"}"#),
            ("g", "grep", r#"{"pattern":"x"}"#),
            ("r3", "read", r#"{"path":"c.rs"}"#),
        ];
        for (id, name, arguments) in children {
            app.start_tool_block(ToolCallStart {
                id: id.into(),
                name: name.into(),
                parent_id: Some("p".into()),
                arguments: arguments.into(),
            });
            app.finish_tool_block(ToolCallFinish {
                id: id.into(),
                name: name.into(),
                parent_id: Some("p".into()),
                arguments: String::new(),
                status: ToolStatus::Completed,
                result: moc_protocol::ToolResult {
                    tool_call_id: id.into(),
                    content: "out".into(),
                    is_error: false,
                    images: Vec::new(),
                },
                duration_ms: Some(1),
                diff: None,
            });
        }

        // 前两个 read 合并成组：组头 `├─ read (2)`，成员挂在下一层。
        let group = render_entry(1, &app.blocks[1], &app, width).lines;
        let head = plain(&group[0]);
        assert!(
            head.starts_with(" ├─ ") && head.contains("read (2)"),
            "嵌套组头：{head}"
        );
        assert!(
            plain(&group[1]).starts_with(" │  ├─ ") && plain(&group[1]).contains("a.rs"),
            "组成员（非末位）：{:?}",
            plain(&group[1])
        );
        assert!(
            plain(&group[2]).starts_with(" │  └─ ") && plain(&group[2]).contains("b.rs"),
            "组成员（末位）：{:?}",
            plain(&group[2])
        );
        assert_eq!(
            render_entry(2, &app.blocks[2], &app, width).lines.len(),
            0,
            "非组首成员不产出任何行"
        );
        // 单独的同名单元不合并：grep 与最后一个 read 各自渲染。
        let grep = plain(
            render_entry(3, &app.blocks[3], &app, width)
                .lines
                .first()
                .expect("grep 标题"),
        );
        assert!(
            grep.starts_with(" ├─ ") && !grep.contains('('),
            "grep 单独渲染：{grep}"
        );
        let tail_read = render_entry(4, &app.blocks[4], &app, width).lines;
        assert!(
            plain(&tail_read[0]).starts_with(" ├─ "),
            "父运行中末子仍 ├─：{:?}",
            plain(&tail_read[0])
        );

        // 父块结束：最后一个 read 翻成 └─ 并带一行空行。
        app.finish_tool_block(ToolCallFinish {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: "p".into(),
                content: "[]".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(9),
            diff: None,
        });
        let tail_read = render_entry(4, &app.blocks[4], &app, width).lines;
        assert!(
            plain(&tail_read[0]).starts_with(" └─ "),
            "父结束末子 └─：{:?}",
            plain(&tail_read[0])
        );
        assert_eq!(plain(tail_read.last().expect("末行")), "", "树尾空行");
        for index in 0..app.blocks.len() {
            assert_eq!(
                estimate_entry_height_at(&app, index, width),
                render_entry(index, &app.blocks[index], &app, width)
                    .lines
                    .len(),
                "第 {index} 块估高与渲染一致"
            );
        }
    }

    /// 顶层批次：组头 `◆ name (N)`，成员 `├─` 落在 bullet 正下方；
    /// 统计挪进组头右端且只在有失败或运行中时才出现。
    #[test]
    fn top_level_batch_header_carries_stats_only_when_needed() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        let width = 80u16;
        app.register_tool_batch("b1".into(), ["r1".to_owned(), "r2".to_owned()]);
        for (id, path) in [("r1", "a.rs"), ("r2", "b.rs")] {
            run_tool(
                &mut app,
                id,
                "read",
                &format!(r#"{{"path":"{path}"}}"#),
                ToolStatus::Completed,
                "body",
            );
        }
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        let head = plain(&lines[0]);
        assert!(head.starts_with(" ◆ read (2)"), "组头：{head}");
        assert!(
            !head.contains(crate::render::labels::status::DONE),
            "全部完成时组头不带统计：{head}"
        );
        assert!(
            plain(&lines[1]).starts_with(" ├─ "),
            "成员连接符在 bullet 正下方：{:?}",
            plain(&lines[1])
        );
        assert!(plain(&lines[2]).starts_with(" └─ "));
        assert_eq!(plain(lines.last().expect("末行")), "", "组尾空行");
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());

        // 有失败成员：组头右端出现 done/failed 统计。
        app.register_tool_batch("b2".into(), ["g1".to_owned(), "g2".to_owned()]);
        run_tool(
            &mut app,
            "g1",
            "grep",
            r#"{"pattern":"ok"}"#,
            ToolStatus::Completed,
            "src/a.rs:1:ok",
        );
        run_tool(
            &mut app,
            "g2",
            "grep",
            r#"{"pattern":"bad"}"#,
            ToolStatus::Error,
            "regex parse error",
        );
        let lines = render_entry(2, &app.blocks[2], &app, width).lines;
        let head = plain(&lines[0]);
        assert!(
            head.contains(crate::render::labels::status::DONE)
                && head.contains(crate::render::labels::status::FAILED),
            "有失败时组头带统计：{head}"
        );
        assert_eq!(estimate_entry_height_at(&app, 2, width), lines.len());
    }

    #[test]
    fn fixed_summaries_keep_warnings_and_reject_expansion() {
        use moc_protocol::methods::ToolStatus;
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let notice = "[… 2048 bytes omitted; full output: artifact://large]";
        for name in [
            "read",
            "grep",
            "find",
            "web_fetch",
            "skill",
            "clm_context",
            "stop_agent",
        ] {
            run_tool(
                &mut app,
                name,
                name,
                r#"{"path":"a.rs"}"#,
                ToolStatus::Completed,
                &format!("secret-body\n{notice}"),
            );
        }
        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            for index in 0..app.blocks.len() {
                assert!(!app.blocks[index].block.foldable());
                app.blocks[index].toggle_level(level);
                assert!(app.blocks[index].level_override.is_none());
                let rendered = render_entry(index, &app.blocks[index], &app, 100);
                let text = rendered
                    .lines
                    .iter()
                    .map(plain_text)
                    .collect::<Vec<_>>()
                    .join("\n");
                assert!(!text.contains("secret-body"), "{text}");
                assert!(text.contains(notice), "{text}");
                assert_eq!(estimate_entry_height_at(&app, index, 100), rendered.height);
            }
        }
    }

    #[test]
    fn fixed_agent_previews_show_tasks_and_reports_not_receipts() {
        use moc_protocol::methods::ToolStatus;
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let report =
            "**检查边界**\n\n- 核对错误路径\n- 核对取消路径\n- 核对缓存\n- 核对结果\nsecret-tail";
        let tasks = serde_json::json!({"tasks":[{"title":"解析器","task":report,"role":"scout"},{"title":"渲染器","task":"检查布局","role":"scout"}]}).to_string();
        run_tool(
            &mut app,
            "spawn",
            "spawn_agent",
            &tasks,
            ToolStatus::Completed,
            "secret-receipt",
        );
        run_tool(
            &mut app,
            "yield",
            "yield",
            &serde_json::json!({"report":report}).to_string(),
            ToolStatus::Completed,
            "secret-receipt",
        );
        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            let text = transcript(&app, 80)
                .iter()
                .map(plain_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("解析器") && text.contains("渲染器") && text.contains("交付报告"),
                "{text}"
            );
            assert!(text.contains("检查边界") && !text.contains("**"), "{text}");
            assert!(
                !text.contains("secret-tail") && !text.contains("secret-receipt"),
                "{text}"
            );
            for index in 0..app.blocks.len() {
                assert!(!app.blocks[index].block.foldable());
                assert_eq!(
                    estimate_entry_height_at(&app, index, 80),
                    render_entry(index, &app.blocks[index], &app, 80).height
                );
            }
        }
    }

    #[test]
    fn fixed_fetch_summary_preserves_page_metadata_and_failures() {
        use moc_protocol::methods::ToolStatus;
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        let result = "Web fetch summary: [{\"url\":\"https://a.test\",\"title\":\"Rust 文档\",\"tokens\":1234,\"truncated\":true},{\"url\":\"https://b.test\",\"error\":\"HTTP 403\"}]\n\nsecret-body";
        run_tool(
            &mut app,
            "fetch",
            "web_fetch",
            r#"{"urls":["https://a.test","https://b.test"]}"#,
            ToolStatus::Completed,
            result,
        );
        for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
            app.tool_detail = level;
            let rendered = render_entry(0, &app.blocks[0], &app, 80);
            let text = rendered
                .lines
                .iter()
                .map(plain_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(
                text.contains("Rust 文档") && text.contains("约 1234 tokens"),
                "{text}"
            );
            assert!(
                text.contains("内容已截断") && text.contains("HTTP 403"),
                "{text}"
            );
            assert!(!text.contains("secret-body"), "{text}");
            assert_eq!(estimate_entry_height_at(&app, 0, 80), rendered.height);
        }
    }

    /// grep 简略档：计数内联进标题尾列，块只剩标题行和空行。
    #[test]
    fn grep_brief_inlines_count_into_title_tail() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.tool_detail = DetailLevel::Brief;
        run_tool(
            &mut app,
            "g",
            "grep",
            r#"{"pattern":"x"}"#,
            ToolStatus::Completed,
            "src/a.rs:1:x\nsrc/b.rs:2:x",
        );
        let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
        assert_eq!(lines.len(), 2, "标题 + 空行");
        let title = plain(&lines[0]);
        assert!(title.contains("matches ·"), "计数进标题尾列：{title}");
        assert!(plain(&lines[1]).is_empty(), "第二行是空行");
        assert_eq!(estimate_entry_height_at(&app, 0, 80), lines.len());
    }

    /// display_path：cwd 内相对化、等于 cwd 显示 `.`、前缀必须落在 `/` 边界、
    /// `$HOME` 折叠 `~`、相对路径与空 cwd 原样。
    #[test]
    fn display_path_relativizes_only_on_slash_boundary() {
        use crate::render::display_path;

        let home = std::env::var("HOME").expect("测试环境有 HOME");
        let cases: Vec<(&str, &str, &str)> = vec![
            ("/repo/src/a.rs", "/repo", "src/a.rs"),
            ("/repo", "/repo", "."),
            ("/repo2/a.rs", "/repo", "/repo2/a.rs"),
            ("src/a.rs", "/repo", "src/a.rs"),
            ("/repo/a.rs", "", "/repo/a.rs"),
            ("/repo/sub/a.rs", "/repo/", "sub/a.rs"),
        ];
        for (path, cwd, want) in cases {
            assert_eq!(
                &*display_path(path, cwd),
                want,
                "display_path({path:?}, {cwd:?})"
            );
        }
        let under_home = format!("{home}/x.rs");
        assert_eq!(&*display_path(&under_home, "/elsewhere"), "~/x.rs");
    }

    /// 缓存 key 含树形状：父块从 running 变结束后，末个子块必须重渲染成 └─。
    #[test]
    fn parent_finish_recolors_last_child_via_cached_shape() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.start_tool_block(ToolCallStart {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: r#"{"description":"父"}"#.into(),
        });
        app.start_tool_block(ToolCallStart {
            id: "c".into(),
            name: "bash".into(),
            parent_id: Some("p".into()),
            arguments: r#"{"command":"echo c"}"#.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: "c".into(),
            name: "bash".into(),
            parent_id: Some("p".into()),
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: "c".into(),
                content: "out".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: None,
        });
        let mut view = ScrollbackView::new();
        view.sync(&app, 80);
        assert!(
            plain(&view.cache[1].lines[0]).starts_with(" ├─"),
            "父运行中：{:?}",
            plain(&view.cache[1].lines[0])
        );
        app.finish_tool_block(ToolCallFinish {
            id: "p".into(),
            name: "run_code".into(),
            parent_id: None,
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: "p".into(),
                content: "[]".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(9),
            diff: None,
        });
        // 子块自身内容代次没变，形状变化也要让缓存失效。
        view.sync(&app, 80);
        assert!(
            plain(&view.cache[1].lines[0]).starts_with(" └─"),
            "父结束重渲染：{:?}",
            plain(&view.cache[1].lines[0])
        );
    }

    /// todo：简略档画快照树、隐藏档只有标题、已关闭阶段只剩标题行、⋯ N done 折叠。
    #[test]
    fn todo_call_renders_snapshot_tree_per_level() {
        use moc_protocol::methods::ToolStatus;

        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        run_tool(
            &mut app,
            "t1",
            "todo",
            r#"{"op":"start","task":"乙"}"#,
            ToolStatus::Completed,
            "一\n  [x] 甲\n  [-] 弃 (不需要了)\n二\n  [x] 丙\n  [x] 丁\n  [/] 乙\n  [ ] 戊\n",
        );
        let text_of = |lines: &[Line<'static>]| {
            lines
                .iter()
                .flat_map(|line| line.spans.iter())
                .map(|span| span.content.as_ref())
                .collect::<String>()
        };

        app.tool_detail = DetailLevel::Brief;
        let brief = render_entry(0, &app.blocks[0], &app, 80).lines;
        let brief_text = text_of(&brief);
        assert!(
            brief_text.contains("start 乙"),
            "摘要 op + 目标：{brief_text}"
        );
        assert!(
            brief_text.contains("├─ 一 · 1/2"),
            "已关闭阶段只留标题：{brief_text}"
        );
        assert!(
            brief_text.contains("└─ 二 · 2/4"),
            "未关闭阶段展开：{brief_text}"
        );
        assert!(
            !brief_text.contains("不需要了"),
            "已关闭阶段不展开：{brief_text}"
        );
        assert!(
            brief_text.contains("⋯ 1 done"),
            "更早已完成折叠：{brief_text}"
        );
        // `⋯ N done` 与任务行的状态符号同列。
        let col_of = |lines: &[Line<'static>], needle: &str| {
            lines
                .iter()
                .map(plain)
                .find(|row| row.contains(needle))
                .and_then(|row| row.find(needle))
                .expect("行存在")
        };
        assert_eq!(
            col_of(&brief, "⋯"),
            col_of(&brief, "☑"),
            "折叠行与状态符号同列：{brief_text}"
        );
        assert!(
            brief_text.contains("4/6"),
            "标题右端 done/total：{brief_text}"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, 80), brief.len());

        app.tool_detail = DetailLevel::Hidden;
        let hidden = render_entry(0, &app.blocks[0], &app, 80).lines;
        assert_eq!(hidden.len(), 2, "隐藏档只剩标题 + 留白");
        assert_eq!(estimate_entry_height_at(&app, 0, 80), hidden.len());

        app.tool_detail = DetailLevel::Full;
        let full = render_entry(0, &app.blocks[0], &app, 80).lines;
        let full_text = text_of(&full);
        assert!(full_text.contains("不需要了"), "完整档全展开：{full_text}");
        assert!(!full_text.contains("⋯ 1 done"), "完整档不折叠：{full_text}");
        assert_eq!(estimate_entry_height_at(&app, 0, 80), full.len());

        // 单阶段：省略阶段标题，任务直接作树叶，`⋯ N done` 与状态符号同列。
        run_tool(
            &mut app,
            "t2",
            "todo",
            r#"{"op":"start","task":"丙"}"#,
            ToolStatus::Completed,
            "单\n  [x] 甲\n  [x] 乙\n  [/] 丙\n",
        );
        app.tool_detail = DetailLevel::Brief;
        let single = render_entry(1, &app.blocks[1], &app, 60).lines;
        let single_text = text_of(&single);
        eprintln!(
            "单阶段简略档:\n{}",
            single.iter().map(plain).collect::<Vec<_>>().join("\n")
        );
        assert!(
            single_text.contains("├─ ⋯ 1 done"),
            "单阶段折叠行：{single_text}"
        );
        assert!(
            !single_text.contains("├─ 单"),
            "单阶段省略阶段标题：{single_text}"
        );
        assert_eq!(
            col_of(&single, "⋯"),
            col_of(&single, "☑"),
            "单阶段折叠行与状态符号同列：{single_text}"
        );
        assert_eq!(estimate_entry_height_at(&app, 1, 60), single.len());
    }

    /// 回归：用户消息与系统提示的估高必须与渲染完全一致（含首块与时间戳换行）。
    #[test]
    fn user_and_note_estimates_match_render() {
        let (events, _) = tokio::sync::mpsc::unbounded_channel();
        let mut app = App::new(crate::theme::Theme::global(), events);
        app.push_block(Block::User { text: "hi".into() });
        app.push_block(Block::User {
            text: "一二三四五六七八九十".repeat(6),
        });
        app.push_block(Block::Note {
            text: "压缩完成".repeat(20),
            error: false,
        });
        for width in [24u16, 60, 120] {
            for index in 0..app.blocks.len() {
                assert_eq!(
                    estimate_entry_height_at(&app, index, width),
                    render_entry(index, &app.blocks[index], &app, width)
                        .lines
                        .len(),
                    "宽 {width} 第 {index} 块"
                );
            }
        }
    }
}

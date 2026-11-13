//! Markdown → 终端行渲染（pulldown-cmark 事件流直接驱动）。
//!
//! inline 内容在缓冲区里以「片段序列（文本 + 已合成样式）」积累，段落 /
//! 标题结束时统一按显示宽度换行产出（续行悬挂缩进）；代码块整块交给
//! syntect 高亮。`mermaid` 围栏画成字符图，`svg` 围栏在有图形协议时换成位图。
//! 表格按内容定列宽，单元格折行，表头/行之间用细线分隔。
//!
//! 顶层表格下方会挂一个可点击入口（`▸ 可视化图表`）：点击后在表格下方追加
//! 像素图占位行（真正的位图由 `render::graphics` 在帧后贴上），再点
//! `▾ 隐藏图表` 收起。表格始终保留；入口只在终端支持图形协议时出现。

use moc_core::chart::RawTable;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::graphics::GraphicsSupport;
use super::image::{ImagePlacement, ImageStore};
use super::rows::RowBuffer;
use super::wrap::{line_width, str_width, truncate};
use crate::theme::Theme;
use crate::transcript::TableChartState;

/// 表格入口的两种形态：展开图表 / 隐藏图表。
const ENTRY_CHART: &str = "▸ 可视化图表";
const ENTRY_HIDE: &str = "▾ 隐藏图表";

/// 参考链接直接展示的上限：超出时收进可点入口，点击后全量列出。
const LINKS_SHOWN: usize = 5;

/// 表格列间距（格）。
const TABLE_COLUMN_GAP: usize = 2;

/// 列宽收缩时的可读下限（格）。
const TABLE_COLUMN_FLOOR: usize = 4;

/// 不可断词宽度的上限（格）：超长 token 允许硬折行，不能独占整列。
const TABLE_COLUMN_TOKEN_MAX: usize = 12;

/// 代码块续行的起头：与真正的下一行区分开（与左内边距同为 2 列）。
const CODE_CONTINUATION: &str = "↪ ";

/// 宽度契约生效的最小正文宽度：再窄时引用竖线与列表缩进本身就放不下。
const MIN_CONTRACT_WIDTH: usize = 24;

/// 渲染 markdown 文本为终端行（无表格视图态：表格永远画成表格）。
pub fn render_markdown(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    render_markdown_with(source, width, theme, RenderViews::default()).lines
}

/// 渲染期视图态：由转写层注入（进程内视图态），渲染层只读。
pub struct RenderViews<'a> {
    /// 每张顶层表格的图表态（已切换则 `Some`），下标与渲染顺序一致。
    pub charts: &'a [Option<TableChartState>],
    /// 终端图形能力；`None` 时表格下不出入口、图片回落文本。
    pub graphics: Option<GraphicsSupport>,
    /// 该块是否仍在流式接收：末尾表格的入口等它落地再出现。
    pub streaming: bool,
    /// 内联图片缓存；`None` 时不解析图片（表格切换等只读场景）。
    pub images: Option<&'a ImageStore>,
    /// 相对路径的基准目录（拼 markdown 里的本地图片路径）。
    pub cwd: Option<&'a std::path::Path>,
    /// `false` = 估高路径：只 `peek`，绝不触发解码。
    pub spawn_images: bool,
    /// 参考链接展开态：`false` 时超出 [`LINKS_SHOWN`] 的条目收进可点入口。
    /// 默认 `true`——非交互表面没有命中区，折叠了用户也无从点开，直接全量展示。
    pub links_expanded: bool,
}

impl Default for RenderViews<'_> {
    fn default() -> Self {
        Self {
            charts: &[],
            graphics: None,
            streaming: false,
            images: None,
            cwd: None,
            spawn_images: true,
            links_expanded: true,
        }
    }
}

/// 一张顶层表格在渲染结果里的落位信息（命中表与图像槽位登记用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSlot {
    /// 顶层表格序号（从 0 起，与 [`RawTable`] 顺序一致）。
    pub table: usize,
    /// 表格下方入口按钮所在行与列区间。
    pub button: Option<ButtonSpan>,
    /// 已展开图表时、位于入口下方的位图占位行区间。
    pub chart: Option<ChartPlacement>,
}

/// 入口按钮的行与列区间（屏幕坐标由转写层补上滚动区原点和正文缩进）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ButtonSpan {
    pub row: usize,
    pub x: u16,
    pub w: u16,
}

/// 图表占位区：终端里留出 `rows` 行空白，位图按 `cols × cell` 贴在上面。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartPlacement {
    pub row: usize,
    pub cols: u16,
    pub rows: u16,
    /// 位图像素尺寸，供编码器与命中判定对齐。
    pub px: (u32, u32),
}

/// 一次渲染的完整结果。
pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    /// 顶层表格的落位信息（顺序与 [`Rendered::raw`] 一致）。
    pub tables: Vec<TableSlot>,
    /// 顶层表格的纯文本快照：供「点击入口时就地分析」复用同一份解析结果。
    pub raw: Vec<RawTable>,
    /// 位图占位区（markdown 图片与 SVG 代码块），行号是块内局部行。
    pub images: Vec<ImagePlacement>,
    /// 参考链接折叠入口的行与列区间；链接数不超上限时为 `None`。
    pub links_button: Option<ButtonSpan>,
}

/// 渲染 markdown 文本，并带上表格视图态。
pub fn render_markdown_with(
    source: &str,
    width: usize,
    theme: &Theme,
    views: RenderViews<'_>,
) -> Rendered {
    let rendered = render_markdown_pages(source, width, theme, views, false);
    Rendered {
        lines: rendered.lines.into_vec(),
        tables: rendered.tables,
        raw: rendered.raw,
        images: rendered.images,
        links_button: rendered.links_button,
    }
}

pub(super) struct PagedRendered {
    pub lines: RowBuffer,
    pub tables: Vec<TableSlot>,
    pub raw: Vec<RawTable>,
    pub images: Vec<ImagePlacement>,
    pub links_button: Option<ButtonSpan>,
}

pub(super) fn render_markdown_pages(
    source: &str,
    width: usize,
    theme: &Theme,
    views: RenderViews<'_>,
    paged: bool,
) -> PagedRendered {
    let mut renderer = MdRenderer {
        theme,
        views,
        width: width.max(1),
        out: RowBuffer::new(paged),
        inline: Vec::new(),
        bold: 0,
        italic: 0,
        strike: 0,
        link_depth: 0,
        quote_depth: 0,
        heading: None,
        list_stack: Vec::new(),
        item_lead: None,
        heading_mark: None,
        code_gap: false,
        code_info: None,
        code_buf: String::new(),
        in_code: false,
        code_closed: true,
        table: None,
        table_cell: Vec::new(),
        in_table_head: false,
        in_table_cell: false,
        table_index: 0,
        buttons: Vec::new(),
        tails: false,
        slots: Vec::new(),
        raw: Vec::new(),
        images: Vec::new(),
        image: None,
        links: Vec::new(),
        current_link_url: None,
        current_link_text: String::new(),
        links_button: None,
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for (event, range) in Parser::new_ext(source, options).into_offset_iter() {
        if let Event::Start(Tag::CodeBlock(kind)) = &event {
            renderer.code_closed = match kind {
                CodeBlockKind::Fenced(_) => fence_is_closed(source, range),
                CodeBlockKind::Indented => true,
            };
        }
        renderer.handle(event);
    }
    renderer.finish()
}

/// inline 片段：文本 + 已合成的样式。
#[derive(Debug, Clone)]
struct Run {
    style: Style,
    text: String,
}

#[derive(Debug, Clone, Copy)]
struct ListFrame {
    ordered: bool,
    next_index: u64,
    /// 本层标记的起始列：父项正文列（顶层为 0），嵌套列表对齐父项文字而非固定 2 格。
    base: usize,
    /// 当前项正文的起始列（标记之后），续行与项内代码块悬挂到这里。
    content: usize,
}

mod resources;
mod tables;
use resources::fence_is_closed;
pub(crate) use resources::is_mermaid_fence_info;
pub(crate) use resources::is_svg_fence_info;

#[derive(Default)]
struct TableAcc {
    /// 各列对齐（`:---:` 等）；缺省列按左对齐。
    alignments: Vec<Alignment>,
    header: Vec<Vec<Run>>,
    rows: Vec<Vec<Vec<Run>>>,
}

/// 正在累积的图片：`![](dest)` 的链接目标与 alt 文本。
struct ImageAcc {
    dest: String,
    alt: String,
}

struct MdRenderer<'a> {
    theme: &'a Theme,
    views: RenderViews<'a>,
    width: usize,
    out: RowBuffer,
    inline: Vec<Run>,
    bold: u32,
    italic: u32,
    strike: u32,
    link_depth: u32,
    quote_depth: usize,
    heading: Option<HeadingLevel>,
    list_stack: Vec<ListFrame>,
    /// 当前列表项尚未落行的前缀（缩进 + bullet / 序号 / 复选框），由首个输出行消费。
    item_lead: Option<Span<'static>>,
    /// 当前标题的符号前缀；标题续行按它的宽度悬挂。
    heading_mark: Option<Span<'static>>,
    /// 刚压入行内代码：下一个正文片段紧贴文字时先补一个普通空格。
    code_gap: bool,
    code_info: Option<String>,
    code_buf: String,
    /// 是否位于代码块内（缩进代码块无 info 串，需与 [`Self::code_info`] 分开记）。
    in_code: bool,
    /// 当前围栏是否已经闭合。流式输出里未闭合的 mermaid 先保持代码块。
    code_closed: bool,
    table: Option<TableAcc>,
    table_cell: Vec<Run>,
    in_table_head: bool,
    in_table_cell: bool,
    /// 顶层表格序号（引用块 / 列表内嵌表格不计数，与渲染顺序一致）。
    table_index: usize,
    /// 待落定的入口按钮：`(表格序号, 插入前行号, 是否已展开图表)`。
    buttons: Vec<(usize, usize, bool)>,
    /// 最后一张表格之后是否已经出现别的块（流式尾部判定）。
    tails: bool,
    /// 表格落位信息（顺序与表格序号一致）。
    slots: Vec<TableSlot>,
    /// 表格纯文本快照，供点击入口时就地分析。
    raw: Vec<RawTable>,
    /// 图片占位区（块内局部行号）。
    images: Vec<ImagePlacement>,
    /// 当前正在累积 alt 文本的图片。
    image: Option<ImageAcc>,
    /// 文档内包含的链接列表 (展示文本, 目标URL)。
    links: Vec<(String, String)>,
    current_link_url: Option<String>,
    current_link_text: String,
    /// 参考链接折叠入口的落位（图表入口落定前的行号，收尾时统一修正）。
    links_button: Option<ButtonSpan>,
}

mod events;
mod inline;
mod media;
mod table_layout;

impl<'a> MdRenderer<'a> {
    /// 收尾：落定入口按钮并交出渲染结果。
    ///
    /// 流式接收期间，末尾表格的入口先不出现——它随时可能被后续 token 顶长，
    /// 提前落定会让用户点到只画了一半的表格。已经切换成图表的入口不受影响
    /// （那是用户主动打开的东西，必须随时能关掉）。
    fn finish(mut self) -> PagedRendered {
        self.flush_inline();
        // 附录只收「目标在文内不可见」的链接：URL（含 file://）的目标只在这一处
        // 出现；裸本地路径的行内 label 本已表达引用，仅空标签（文内什么都看不到）
        // 时才需要附录补上目标。
        let links: Vec<(String, String)> = self
            .links
            .iter()
            .filter(|(label, url)| url.contains("://") || label.is_empty())
            .cloned()
            .collect();
        if !links.is_empty() {
            self.ensure_block_gap();
            self.out.push(Line::from(Span::styled(
                "参考链接：",
                Style::new()
                    .fg(self.theme.gray_dim)
                    .add_modifier(Modifier::BOLD),
            )));
            let folded = links.len() > LINKS_SHOWN && !self.views.links_expanded;
            let shown = if folded { LINKS_SHOWN } else { links.len() };
            let cwd = self
                .views
                .cwd
                .and_then(|cwd| cwd.to_str())
                .unwrap_or_default();
            for (idx, (label, url)) in links.iter().enumerate().take(shown) {
                // 本地文件路径按 cwd 相对化（与工具标题同口径）；真 URL 原样展示。
                let target = self.local_path(url).map_or_else(
                    || url.clone(),
                    |path| super::display_path(&path.to_string_lossy(), cwd).into_owned(),
                );
                let num = format!("  [{}] ", idx + 1);
                let room = self.width.saturating_sub(str_width(&num));
                let label_prefix = if label.is_empty() || label == url {
                    String::new()
                } else {
                    // 标签与 URL 都可能很长：标签最多占一半，URL 优先保全。
                    format!(
                        "{}: ",
                        truncate(label.as_str(), (room / 2).saturating_sub(2))
                    )
                };
                let rem = room.saturating_sub(str_width(&label_prefix));
                self.out.push(Line::from(vec![
                    Span::styled(
                        format!("{num}{label_prefix}"),
                        Style::new().fg(self.theme.gray_dim),
                    ),
                    Span::styled(
                        truncate(target.as_str(), rem).into_owned(),
                        Style::new()
                            .fg(self.theme.link)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ]));
            }
            if links.len() > LINKS_SHOWN {
                let label = if folded {
                    format!("▸ 还有 {} 条链接", links.len() - shown)
                } else {
                    "▾ 收起链接".to_owned()
                };
                let row = self.out.len();
                let width = str_width(&label);
                self.links_button = Some(ButtonSpan {
                    row,
                    x: 2,
                    w: u16::try_from(width).unwrap_or(u16::MAX),
                });
                self.out.push(Line::from(vec![
                    Span::styled("  ", Style::new()),
                    Span::styled(
                        label,
                        Style::new()
                            .fg(self.theme.link)
                            .add_modifier(Modifier::UNDERLINED),
                    ),
                ]));
            }
        }
        let open_tail = self.views.streaming
            && !self.tails
            && self.buttons.last().is_some_and(|(_, _, chart)| !chart);
        if open_tail {
            self.buttons.pop();
        }
        let kept: Vec<(usize, usize, bool)> = std::mem::take(&mut self.buttons);
        // 落定第 k 个入口会让它之后的每一行下移一行：插入行号与图表占位行都按
        // 同一份偏移修正，绘制与命中才不会错位。
        for (order, (table, row, shown)) in kept.iter().enumerate() {
            let label = if *shown { ENTRY_HIDE } else { ENTRY_CHART };
            let label_width = str_width(label);
            let line = Line::from(vec![Span::styled(
                (*label).to_owned(),
                Style::new()
                    .fg(self.theme.link)
                    .add_modifier(Modifier::UNDERLINED),
            )]);
            let row = row + order;
            self.out.insert(row, line);
            if let Some(slot) = self.slots.get_mut(*table) {
                slot.button = Some(ButtonSpan {
                    row,
                    x: 0,
                    w: u16::try_from(label_width).unwrap_or(u16::MAX),
                });
            }
        }
        for slot in &mut self.slots {
            if let Some(chart) = slot.chart.as_mut() {
                // 图表位于自己的入口之后，所以连同一张表的按钮插入也要计入偏移。
                chart.row += kept
                    .iter()
                    .take_while(|(table, _, _)| *table <= slot.table)
                    .count();
            }
        }
        // 图片占位区同理：插入行号不大于它的入口都会把它往下推一行
        let mut images = std::mem::take(&mut self.images);
        for image in &mut images {
            image.row += kept.iter().filter(|(_, row, _)| *row <= image.row).count();
        }
        // 参考链接的折叠入口也在入口插入之后，同样按偏移修正。
        if let Some(button) = self.links_button.as_mut() {
            button.row += kept.iter().filter(|(_, row, _)| *row <= button.row).count();
        }
        // 宽度契约：每行不超过正文宽度。超宽内容必须在这里折行或带 `…` 截断，
        // 不能交给 buffer 静默裁掉。
        debug_assert!(
            self.width < MIN_CONTRACT_WIDTH
                || (0..self.out.len()).step_by(128).all(|start| self
                    .out
                    .window(start..(start + 128).min(self.out.len()))
                    .iter()
                    .all(|line| line_width(line) <= self.width)),
            "markdown 行超出正文宽度 {}",
            self.width
        );
        PagedRendered {
            lines: self.out,
            tables: self.slots,
            raw: self.raw,
            images,
            links_button: self.links_button,
        }
    }
}

/// 围栏 info 串里的语言名：首个空白或逗号之前（`rust,ignore`、`rust title="x"` → `rust`）。
fn code_language(info: &str) -> &str {
    info.trim()
        .split(|ch: char| ch.is_whitespace() || ch == ',')
        .next()
        .unwrap_or("")
}

/// 行内代码与这个相邻字符之间是否要补空格：字母、数字、汉字要；空白与标点（含全角）不要。
fn needs_code_gap(ch: char) -> bool {
    ch.is_alphanumeric()
}

#[cfg(test)]
mod tests;

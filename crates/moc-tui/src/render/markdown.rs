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

use moc_core::chart::{self, RawTable};
use std::path::PathBuf;

use pulldown_cmark::{Alignment, CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::PROSE_INDENT;
use super::graphics::GraphicsSupport;
use super::highlight;
use super::image::{
    ImageKey, ImagePlacement, ImageSourceSpec, ImageState, ImageStore, layout as image_layout,
};
use super::rows::RowBuffer;
use super::wrap::{line_width, str_width, truncate, wrap_spans, wrap_text};
use crate::theme::Theme;
use crate::transcript::TableChartState;

/// 表格入口的两种形态：展开图表 / 隐藏图表。
const ENTRY_CHART: &str = "▸ 可视化图表";
const ENTRY_HIDE: &str = "▾ 隐藏图表";

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
    }
}

pub(super) struct PagedRendered {
    pub lines: RowBuffer,
    pub tables: Vec<TableSlot>,
    pub raw: Vec<RawTable>,
    pub images: Vec<ImagePlacement>,
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

/// 代码块 info 串是否声明了 SVG：首个空白分隔 token，大小写不敏感。
///
/// 转写层的廉价预筛也走这个判定，两处必须完全一致——否则懒块的估高会用
/// 折行近似顶替真实的占位行。
pub(crate) fn is_svg_fence_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("svg"))
}

/// 代码块 info 串是否声明了 mermaid。判定口径与 [`is_svg_fence_info`] 相同。
pub(crate) fn is_mermaid_fence_info(info: &str) -> bool {
    info.split_whitespace()
        .next()
        .is_some_and(|tag| tag.eq_ignore_ascii_case("mermaid"))
}

/// 围栏代码块的源码区间是否以闭合围栏结尾。
///
/// pulldown 把未闭合的围栏收到文件末尾，并把空余量当成一次长度为 0 的闭合，
/// 所以区间终点必须落在与开围栏同字符且不更短的真围栏上才算闭合——内容行
/// 末尾的 `` ``` `` 在 ```` ```` ```` 块里并不算闭合。对不齐时当成已闭合：
/// 画不出来会退回代码块，比把已写完的图留在源码里好。
fn fence_is_closed(source: &str, range: std::ops::Range<usize>) -> bool {
    let bytes = source.as_bytes();
    if range.end == 0 || range.end > bytes.len() || range.start > bytes.len() {
        return true;
    }
    let Some((fence, needed)) = fence_marker(&bytes[range.start..]) else {
        return true;
    };
    let mut index = range.end;
    while index > range.start && matches!(bytes[index - 1], b' ' | b'\t' | b'\r' | b'\n') {
        index -= 1;
    }
    let mut count = 0;
    while index > 0 && bytes[index - 1] == fence {
        index -= 1;
        count += 1;
    }
    if count < needed {
        return false;
    }
    let mut spaces = 0;
    while index > 0 && bytes[index - 1] == b' ' {
        index -= 1;
        spaces += 1;
        if spaces > 3 {
            return false;
        }
    }
    index == 0 || bytes[index - 1] == b'\n'
}

/// 行首的围栏标记：至多 3 格缩进后跟 `` ``` `` / `~~~` 连续段。
/// 返回 `(字符, 长度)`；够不上围栏时返回 `None`。
fn fence_marker(bytes: &[u8]) -> Option<(u8, usize)> {
    let mut index = 0;
    let mut spaces = 0;
    while bytes.get(index) == Some(&b' ') {
        index += 1;
        spaces += 1;
        if spaces > 3 {
            return None;
        }
    }
    let fence = *bytes.get(index)?;
    if fence != b'`' && fence != b'~' {
        return None;
    }
    let start = index;
    while bytes.get(index) == Some(&fence) {
        index += 1;
    }
    let len = index - start;
    (len >= 3).then_some((fence, len))
}

/// `file://` 之后的部分 → 本地路径。
///
/// 按 RFC 8089 处理两种写法：`file:///abs/path`（空 host）与
/// `file://localhost/abs/path`。带其它 host 的是远程文件，本地读不到，直接拒绝。
/// 百分号编码在这里解开——路径里的空格在 markdown 里就是 `%20`。
fn file_url_path(rest: &str) -> Option<std::path::PathBuf> {
    let raw = if rest.starts_with('/') {
        rest.to_owned()
    } else {
        let (host, path) = rest.split_once('/')?;
        if !host.eq_ignore_ascii_case("localhost") {
            return None;
        }
        format!("/{path}")
    };
    Some(std::path::PathBuf::from(percent_decode(&raw)))
}

/// 解码 `%XX`；非法转义原样保留（宁可找不到文件，也不要静默改路径）。
fn percent_decode(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) =
                (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
        {
            out.push(high << 4 | low);
            index += 3;
            continue;
        }
        out.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// 本地图片的缓存键：路径 + mtime + 长度。文件被改写就必须重新解码。
///
/// 文件不存在 / 不是普通文件 / 超过解码上限时返回 `None`（回落 alt 文本）。
fn file_key(path: &std::path::Path) -> Option<ImageKey> {
    let meta = std::fs::metadata(path).ok()?;
    if !meta.is_file() || meta.len() > super::image::decode::MAX_FILE_BYTES {
        return None;
    }
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|elapsed| i128::try_from(elapsed.as_nanos()).ok())
        .unwrap_or(0);
    Some(ImageKey::File {
        path: std::sync::Arc::new(path.to_path_buf()),
        mtime_ns,
        len: meta.len(),
    })
}

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
}

impl<'a> MdRenderer<'a> {
    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.code(&code),
            Event::SoftBreak => {
                if self.image.is_some() {
                    // alt 文本里的软换行：图片不显示时就是一段普通文字，给个空格
                    if let Some(image) = &mut self.image {
                        image.alt.push(' ');
                    }
                } else if self.in_table_cell {
                    self.push_run(Run {
                        style: self.current_style(),
                        text: " ".to_owned(),
                    });
                } else {
                    self.flush_inline();
                }
            }
            Event::HardBreak => {
                if self.image.is_some() {
                    if let Some(image) = &mut self.image {
                        image.alt.push(' ');
                    }
                } else if self.in_table_cell {
                    self.push_run(Run {
                        style: self.current_style(),
                        text: " ".to_owned(),
                    });
                } else {
                    self.flush_inline();
                }
            }
            Event::Rule => {
                self.flush_inline();
                self.ensure_block_gap();
                self.out.push(Line::from(Span::styled(
                    "─".repeat(self.width.saturating_sub(2).min(self.width)),
                    Style::new().fg(self.theme.border),
                )));
                self.out.push(Line::from(String::new()));
            }
            Event::TaskListMarker(checked) => {
                let (marker, color) = if checked {
                    ("☑ ", self.theme.accent_success)
                } else {
                    ("☐ ", self.theme.gray)
                };
                // 无序列表的复选框顶替 `•`（同为 2 列），续行悬挂到复选框之后；
                // 有序列表保留序号，复选框跟在序号后。
                if let Some(lead) = self.item_lead.as_mut()
                    && let Some(indent) = lead.content.strip_suffix("• ")
                {
                    *lead = Span::styled(format!("{indent}{marker}"), Style::new().fg(color));
                } else {
                    self.push_run(Run {
                        style: Style::new().fg(color),
                        text: marker.into(),
                    });
                }
            }
            Event::FootnoteReference(_) | Event::InlineMath(_) | Event::DisplayMath(_) => {}
            Event::Html(html) | Event::InlineHtml(html) => {
                let style = Style::new().fg(self.theme.gray_dim);
                self.push_inline_text(&html, style);
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                self.block_boundary();
                if self.list_stack.is_empty() && self.quote_depth == 0 {
                    self.ensure_block_gap();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.block_boundary();
                self.ensure_block_gap();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                self.open_item_block();
                if self.quote_depth == 0 {
                    self.block_boundary();
                    self.ensure_block_gap();
                }
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.block_boundary();
                self.ensure_block_gap();
                self.open_item_block();
                // 缩进代码块没有 info 串，代码块判定必须与 info 分开记。
                self.in_code = true;
                self.code_info = match kind {
                    CodeBlockKind::Fenced(info) => Some(info.to_string()),
                    CodeBlockKind::Indented => None,
                };
                self.code_buf.clear();
            }
            Tag::List(start) => {
                self.flush_inline();
                if self.list_stack.is_empty() {
                    self.block_boundary();
                    self.ensure_block_gap();
                }
                self.open_item_block();
                let base = self.content_indent();
                self.list_stack.push(ListFrame {
                    ordered: start.is_some(),
                    next_index: start.unwrap_or(1),
                    base,
                    content: base,
                });
            }
            Tag::Item => {
                self.flush_inline();
                let border = Style::new().fg(self.theme.border);
                if let Some(frame) = self.list_stack.last_mut() {
                    let marker = if frame.ordered {
                        let marker = format!("{}. ", frame.next_index);
                        frame.next_index += 1;
                        marker
                    } else {
                        "• ".to_owned()
                    };
                    let lead = " ".repeat(frame.base) + &marker;
                    frame.content = str_width(&lead);
                    self.item_lead = Some(Span::styled(lead, border));
                }
            }
            Tag::Table(alignments) => {
                self.flush_inline();
                self.block_boundary();
                self.ensure_block_gap();
                self.open_item_block();
                self.table = Some(TableAcc {
                    alignments,
                    ..TableAcc::default()
                });
            }
            Tag::TableHead => self.in_table_head = true,
            Tag::TableRow => {
                if !self.in_table_head
                    && let Some(table) = self.table.as_mut()
                {
                    table.rows.push(Vec::new());
                }
            }
            Tag::TableCell => {
                self.table_cell.clear();
                self.code_gap = false;
                self.in_table_cell = true;
            }
            Tag::Emphasis => self.italic += 1,
            Tag::Strong => self.bold += 1,
            Tag::Strikethrough => self.strike += 1,
            Tag::Link { dest_url, .. } => {
                self.link_depth += 1;
                self.current_link_url = Some(dest_url.to_string());
                self.current_link_text.clear();
            }
            Tag::Image { dest_url, .. } => {
                // 先不 flush：能否出图要到 `TagEnd::Image` 才知道。无条件在这里
                // 结算 inline 会把「请看 ![远端图](https://…) 这个」劈成两行，
                // 远端图与不存在的本地文件本该原地显示 alt 文本。
                self.image = Some(ImageAcc {
                    dest: dest_url.to_string(),
                    alt: String::new(),
                });
            }
            Tag::MetadataBlock(_)
            | Tag::FootnoteDefinition(_)
            | Tag::DefinitionList
            | Tag::DefinitionListTitle
            | Tag::DefinitionListDefinition
            | Tag::HtmlBlock
            | Tag::Superscript
            | Tag::Subscript => {}
        }
    }

    fn end(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => self.flush_inline(),
            TagEnd::Heading(level) => {
                // 标题符号随层级变细；不用 `◆`——那是工具与 Thought 标题的 bullet。
                let (mark, color) = match level {
                    HeadingLevel::H1 => ("█ ", self.theme.accent_assistant),
                    HeadingLevel::H2 => ("▌ ", self.theme.accent_user),
                    HeadingLevel::H3 => ("▎ ", self.theme.accent_tool),
                    _ => ("◇ ", self.theme.text_secondary),
                };
                self.heading_mark = Some(Span::styled(
                    mark,
                    Style::new().fg(color).add_modifier(Modifier::BOLD),
                ));
                self.heading = None;
                self.flush_inline();
                self.heading_mark = None;
                self.out
                    .push(Line::from(Span::styled(String::new(), Style::new())));
            }
            TagEnd::BlockQuote(_) => self.quote_depth = self.quote_depth.saturating_sub(1),
            TagEnd::CodeBlock => {
                self.in_code = false;
                self.flush_code();
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
            }
            TagEnd::Item => {
                self.flush_inline();
                self.open_item_block();
            }
            TagEnd::Table => {
                self.flush_inline();
                if let Some(table) = self.table.take() {
                    self.finish_table(table);
                }
            }
            TagEnd::TableHead => self.in_table_head = false,
            TagEnd::TableRow => {}
            TagEnd::TableCell => {
                self.in_table_cell = false;
                let cell = std::mem::take(&mut self.table_cell);
                if let Some(table) = self.table.as_mut() {
                    if self.in_table_head {
                        table.header.push(cell);
                    } else if let Some(row) = table.rows.last_mut() {
                        row.push(cell);
                    }
                }
            }
            TagEnd::Emphasis => self.italic = self.italic.saturating_sub(1),
            TagEnd::Strong => self.bold = self.bold.saturating_sub(1),
            TagEnd::Strikethrough => self.strike = self.strike.saturating_sub(1),
            TagEnd::Link => {
                self.link_depth = self.link_depth.saturating_sub(1);
                if let Some(url) = self.current_link_url.take() {
                    let label = std::mem::take(&mut self.current_link_text);
                    if !url.is_empty() {
                        self.links.push((label, url));
                    }
                }
            }
            TagEnd::Image => self.finish_image(),
            TagEnd::MetadataBlock(_)
            | TagEnd::FootnoteDefinition
            | TagEnd::DefinitionList
            | TagEnd::DefinitionListTitle
            | TagEnd::DefinitionListDefinition
            | TagEnd::HtmlBlock
            | TagEnd::Superscript
            | TagEnd::Subscript => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code {
            self.code_buf.push_str(text);
            return;
        }
        if let Some(image) = &mut self.image {
            image.alt.push_str(text);
            return;
        }
        if self.link_depth > 0 {
            self.current_link_text.push_str(text);
        }
        let style = self.current_style();
        self.push_inline_text(text, style);
    }

    /// alt 文本里的内联代码：`![a `b` c](x.png)` 的 `` b `` 也要进 alt，
    /// 否则它会掉进段落正文，在图片本来就不显示时尤其突兀。
    fn code(&mut self, text: &str) {
        if let Some(image) = &mut self.image {
            image.alt.push(' ');
            image.alt.push_str(text);
            return;
        }
        // 行内代码只在紧贴文字时与正文隔开一个普通空格（不带底色）：源码里已有
        // 空白、全角标点或位于行首时不补，避免双空格与续行左缘漂移。
        if self.last_inline_char().is_some_and(needs_code_gap) {
            self.push_run(Run {
                style: Style::new(),
                text: " ".to_owned(),
            });
        }
        let style = self
            .current_style()
            .fg(self.theme.path)
            .bg(self.theme.code_bg);
        self.push_run(Run {
            style,
            text: text.to_owned(),
        });
        self.code_gap = true;
    }

    /// 逐行压入 inline 片段：事件可能整块携带换行（HTML 块等），换行
    /// 按软换行结算，保证输出行内不留 `\n`——换行与宽度计算都假设
    /// 「一行即一行」。
    fn push_inline_text(&mut self, text: &str, style: Style) {
        for (index, segment) in text.split('\n').enumerate() {
            if index > 0 {
                if self.in_table_cell {
                    self.push_run(Run {
                        style,
                        text: " ".to_owned(),
                    });
                } else {
                    self.flush_inline();
                }
            }
            if !segment.is_empty() {
                self.push_run(Run {
                    style,
                    text: segment.to_owned(),
                });
            }
        }
    }

    /// 合成当前 inline 样式（嵌套修饰叠加）。
    fn current_style(&self) -> Style {
        let mut style = if let Some(level) = self.heading {
            let fg = match level {
                HeadingLevel::H1 => self.theme.accent_assistant,
                HeadingLevel::H2 => self.theme.accent_user,
                HeadingLevel::H3 => self.theme.accent_tool,
                _ => self.theme.text_secondary,
            };
            Style::new().fg(fg).add_modifier(Modifier::BOLD)
        } else {
            Style::new().fg(self.theme.text_primary)
        };
        if self.link_depth > 0 {
            style = style.fg(self.theme.link);
        }
        if self.quote_depth > 0 {
            style = style.fg(self.theme.text_secondary);
        }
        if self.bold > 0 {
            style = style.add_modifier(Modifier::BOLD);
        }
        if self.italic > 0 {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if self.strike > 0 {
            style = style.add_modifier(Modifier::CROSSED_OUT);
        }
        style
    }

    fn push_run(&mut self, run: Run) {
        if run.text.is_empty() {
            return;
        }
        // 行内代码之后紧跟文字：补一个普通空格（见 [`Self::code`]）。
        if std::mem::take(&mut self.code_gap) && run.text.chars().next().is_some_and(needs_code_gap)
        {
            self.push_run(Run {
                style: Style::new(),
                text: " ".to_owned(),
            });
        }
        let runs = if self.in_table_cell {
            &mut self.table_cell
        } else {
            &mut self.inline
        };
        if let Some(last) = runs.last_mut()
            && last.style == run.style
        {
            last.text.push_str(&run.text);
            return;
        }
        runs.push(run);
    }

    /// 块级事件分界：最后一张表格之后出现了别的内容，它的入口就可以落定了。
    fn block_boundary(&mut self) {
        if !self.buttons.is_empty() {
            self.tails = true;
        }
    }

    /// 正文可用列数（转写层已扣掉正文缩进）。
    fn cols(&self) -> u16 {
        u16::try_from(self.width).unwrap_or(u16::MAX)
    }

    /// 图片落定：能出图就留占位行，否则回落 alt 文本（改动前的内联行为）。
    ///
    /// 只有真正出图时才把前面的正文结算成一行——图片必须独占块，段落在这里
    /// 被劈开是合理的；画不出图时 alt 直接回到 inline 缓冲，段落保持原样。
    fn finish_image(&mut self) {
        let Some(image) = self.image.take() else {
            return;
        };
        if !self.place_image(&image.dest) {
            let style = self.current_style();
            self.push_inline_text(&image.alt, style);
        }
    }

    /// 为本地图片留出占位行；返回 `false` 表示这一步画不出图。
    fn place_image(&mut self, dest: &str) -> bool {
        // 引用块 / 列表 / 表格单元格里的图片不贴：没有独立命中区，宽度也不够
        if self.quote_depth > 0 || !self.list_stack.is_empty() || self.in_table_cell {
            return false;
        }
        // 终端画不了像素图就别解码：解出来只会回落成 alt 文本，
        // 白白占几十 MB 解码缓冲与缓存预算。
        if self.views.graphics.is_none() {
            return false;
        }
        let Some(path) = self.local_path(dest) else {
            return false;
        };
        let Some(key) = file_key(&path) else {
            return false;
        };
        let Some(store) = self.views.images else {
            return false;
        };
        let state = if self.views.spawn_images {
            let source = path;
            store.state(&key, move || ImageSourceSpec::File(source))
        } else {
            match store.peek(&key) {
                Some(state) => state,
                None => return false,
            }
        };
        self.emit_placeholder(&key, state)
    }

    /// mermaid 代码块：画成字符图。引用、列表、未闭合的流式围栏和认不出的图种留下代码块。
    fn place_mermaid(&mut self, info: &str, code: &str) -> bool {
        if self.quote_depth > 0 || !self.list_stack.is_empty() {
            return false;
        }
        if !is_mermaid_fence_info(info) {
            return false;
        }
        if self.views.streaming && !self.code_closed {
            return false;
        }
        let Some(lines) = super::mermaid::render(code, self.width, self.theme) else {
            return false;
        };
        self.out.extend(lines);
        self.out.push(Line::from(String::new()));
        true
    }

    /// SVG 代码块：整块换成位图占位行（比一屏坐标文本有用得多）。
    fn place_svg(&mut self, info: &str, code: &str) -> bool {
        if self.quote_depth > 0 || !self.list_stack.is_empty() {
            return false;
        }
        if !is_svg_fence_info(info) || code.trim().is_empty() {
            return false;
        }
        // 终端画不了像素图就别解码：解出来也只会回落成代码块，
        // 白白占几十 MB 解码缓冲与缓存预算。
        if self.views.graphics.is_none() {
            return false;
        }
        let Some(store) = self.views.images else {
            return false;
        };
        let key = ImageKey::from_svg(code);
        let state = if self.views.spawn_images {
            let text = code.to_owned().into_boxed_str();
            store.state(&key, move || ImageSourceSpec::Svg(text))
        } else {
            match store.peek(&key) {
                Some(state) => state,
                None => return false,
            }
        };
        self.emit_placeholder(&key, state)
    }

    /// 解码就绪就按版式留空行并登记落位；其余状态返回 `false`（回落文本）。
    fn emit_placeholder(&mut self, key: &ImageKey, state: ImageState) -> bool {
        let (ImageState::Ready(prepared), Some(graphics)) = (state, self.views.graphics) else {
            return false;
        };
        let Some(layout) = image_layout(prepared.size(), graphics.cell, self.cols()) else {
            return false;
        };
        self.flush_inline();
        self.ensure_block_gap();
        let row = self.out.len();
        // 占位行只放空格：像素由 `graphics::emit` 在帧后贴上去
        for _ in 0..layout.rows {
            self.out.push(Line::from(" ".repeat(self.width)));
        }
        self.images.push(ImagePlacement {
            row,
            x: u16::try_from(PROSE_INDENT).unwrap_or(0),
            cols: layout.cols,
            rows: layout.rows,
            px: layout.px,
            key: key.clone(),
        });
        self.out.push(Line::from(String::new()));
        true
    }

    /// markdown 链接目标 → 本地路径。只认本地文件：`http(s)://` 与 `data:`
    /// 既不联网也不内联解码（那是另一条信任边界）。
    ///
    /// 不限制路径范围：`..`、绝对路径、`~` 都照常解析。图片是给人看的，
    /// 而会话里的模型本来就有 shell 权限，在这里加目录围栏挡不住任何东西，
    /// 只会让 `/tmp` 与 `$HOME` 下的合法图片打不开。
    fn local_path(&self, dest: &str) -> Option<std::path::PathBuf> {
        let dest = dest.trim();
        if dest.is_empty() {
            return None;
        }
        let raw = match dest.strip_prefix("file://").or_else(|| {
            // scheme 大小写不敏感（RFC 3986），`FILE://` 与 `file://` 等价
            dest.get(..7)
                .filter(|prefix| prefix.eq_ignore_ascii_case("file://"))
                .map(|prefix| &dest[prefix.len()..])
        }) {
            Some(rest) => file_url_path(rest)?,
            None if dest.contains("://") => return None,
            None => PathBuf::from(dest),
        };
        let path = match raw.to_str().and_then(|text| text.strip_prefix("~/")) {
            Some(rest) => PathBuf::from(std::env::var_os("HOME")?).join(rest),
            None => raw,
        };
        if path.is_absolute() {
            Some(path)
        } else {
            Some(self.views.cwd?.join(path))
        }
    }

    /// 表格收尾：表格始终保留；展开图表时在表格下方追加占位区与入口。
    fn finish_table(&mut self, table: TableAcc) {
        if table.header.is_empty() {
            return;
        }
        if self.quote_depth > 0 || !self.list_stack.is_empty() {
            // 引用块 / 列表内嵌表格不参与图表化：它们没有独立的命中区，
            // 序号也不与顶层表格对齐。
            self.render_table(table);
            return;
        }
        let index = self.table_index;
        self.table_index += 1;
        // 这张表成了新的「最后一张表格」：它后面还没有别的内容
        self.tails = false;
        let raw = raw_table(&table);
        let charted = self
            .views
            .charts
            .get(index)
            .and_then(|state| state.as_ref());
        let layout = charted
            .zip(self.views.graphics)
            .and_then(|(state, graphics)| {
                crate::render::chart::layout(&state.spec, self.cols(), graphics.cell)
            });
        // 表格行先落定：入口与图表都排在它后面。
        self.render_table(table);
        let button_row = self.out.len();
        if let Some(layout) = layout {
            // 入口由 finish() 插入到 button_row；空一行再把位图占位区铺在入口下方。
            self.out.push(Line::from(String::new()));
            let chart_row = self.out.len();
            let blank = " ".repeat(self.width);
            self.out.extend(std::iter::repeat_n(
                Line::from(blank),
                usize::from(layout.rows),
            ));
            self.slots.push(TableSlot {
                table: index,
                button: None,
                chart: Some(ChartPlacement {
                    row: chart_row,
                    cols: layout.cols,
                    rows: layout.rows,
                    px: layout.px,
                }),
            });
            self.buttons.push((index, button_row, true));
            self.raw.push(raw);
            return;
        }
        self.slots.push(TableSlot {
            table: index,
            button: None,
            chart: None,
        });
        // 入口只在「终端能画像素图 + 格子放得下字 + 这张表确实能图表化」时出现；
        // 已经切换过图表的表格必须保留隐藏入口，哪怕当前画不出图。
        let fits = self.width >= usize::from(crate::render::chart::MIN_COLS)
            && self
                .views
                .graphics
                .is_some_and(|graphics| crate::render::graphics::cell_supported(graphics.cell));
        let plannable = fits
            && chart::plan(&chart::analyze(&raw)).is_some_and(|plan| {
                crate::render::chart::rows_needed(plan.kind, raw.rows.len(), plan.series.len())
                    <= crate::render::chart::MAX_ROWS
            });
        let offers = charted.is_some() || plannable;
        if offers {
            // 用户可能马上点入口：后台先把字体解析好，别让第一张图卡在解析上
            crate::render::chart::warm_fonts();
            self.buttons.push((index, button_row, charted.is_some()));
        }
        self.raw.push(raw);
    }

    /// 收尾：落定入口按钮并交出渲染结果。
    ///
    /// 流式接收期间，末尾表格的入口先不出现——它随时可能被后续 token 顶长，
    /// 提前落定会让用户点到只画了一半的表格。已经切换成图表的入口不受影响
    /// （那是用户主动打开的东西，必须随时能关掉）。
    fn finish(mut self) -> PagedRendered {
        self.flush_inline();
        if !self.links.is_empty() {
            self.ensure_block_gap();
            self.out.push(Line::from(Span::styled(
                "参考链接：",
                Style::new()
                    .fg(self.theme.gray_dim)
                    .add_modifier(Modifier::BOLD),
            )));
            for (idx, (label, url)) in self.links.iter().enumerate() {
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
                        truncate(url.as_str(), rem).into_owned(),
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
        // 宽度契约：每行不超过正文宽度。超宽内容必须在这里折行或带 `…` 截断，
        // 不能交给 buffer 静默裁掉。
        debug_assert!(
            self.width < MIN_CONTRACT_WIDTH
                || (0..self.out.len()).step_by(128).all(|start| self.out.window(start..(start + 128).min(self.out.len())).iter().all(|line| line_width(line) <= self.width)),
            "markdown 行超出正文宽度 {}",
            self.width
        );
        PagedRendered {
            lines: self.out,
            tables: self.slots,
            raw: self.raw,
            images,
        }
    }

    /// 块级内容之间保留一行视觉间距，避免标题和段落连成一块。
    fn ensure_block_gap(&mut self) {
        if self.out.last().is_some_and(|line| line.width() > 0) {
            self.out.push(Line::from(String::new()));
        }
    }

    /// 段落 / 标题 / 列表项结束：inline 缓冲按宽度折行后产出。
    ///
    /// 折行走 [`wrap_spans`]（空格 / CJK 字间 / 标点后断行、避头尾、续行丢行首空格）。
    /// 每行先画引用竖线；首行接列表标记或标题符号，续行用等宽空格悬挂到正文起点。
    fn flush_inline(&mut self) {
        self.code_gap = false;
        if self.inline.is_empty() {
            return;
        }
        let runs = std::mem::take(&mut self.inline);
        let mut lead: Vec<Span<'static>> = Vec::with_capacity(2);
        lead.extend(self.item_lead.take());
        lead.extend(self.heading_mark.take());
        let lead_width = if lead.is_empty() {
            self.content_indent()
        } else {
            lead.iter().map(|span| str_width(&span.content)).sum()
        };
        let budget = self
            .width
            .saturating_sub(2 * self.quote_depth + lead_width)
            .max(1);
        let spans: Vec<Span<'static>> = runs
            .into_iter()
            .map(|run| Span::styled(run.text, run.style))
            .collect();
        let hang = " ".repeat(lead_width);
        super::wrap::wrap_spans_into(&spans, budget, |row| {
            let mut line = Vec::with_capacity(row.spans.len() + 2);
            if self.quote_depth > 0 {
                line.push(self.quote_span());
            }
            if !lead.is_empty() {
                line.append(&mut lead);
            } else if lead_width > 0 {
                line.push(Span::raw(hang.clone()));
            }
            line.extend(row.spans);
            self.out.push(Line::from(line));
        });
    }

    /// 列表项以代码块 / 表格 / 引用块 / 子列表开头：先把标记单独落成一行，
    /// 否则它会被留给下一段正文，或泄漏给下一项。
    fn open_item_block(&mut self) {
        let Some(lead) = self.item_lead.take() else {
            return;
        };
        let mut line = Vec::with_capacity(2);
        if self.quote_depth > 0 {
            line.push(self.quote_span());
        }
        line.push(lead);
        self.out.push(Line::from(line));
    }

    /// 当前列表项正文的起始列（不含引用竖线）；不在列表里为 0。
    fn content_indent(&self) -> usize {
        self.list_stack.last().map_or(0, |frame| frame.content)
    }

    /// 引用竖线（每层 `│ `，2 列）。
    fn quote_span(&self) -> Span<'static> {
        Span::styled(
            "│ ".repeat(self.quote_depth),
            Style::new().fg(self.theme.border),
        )
    }

    /// 当前 inline 缓冲（或单元格缓冲）的最后一个字符。
    fn last_inline_char(&self) -> Option<char> {
        let runs = if self.in_table_cell {
            &self.table_cell
        } else {
            &self.inline
        };
        runs.last().and_then(|run| run.text.chars().next_back())
    }

    /// 代码块结束：syntect 高亮并整体铺 code_bg，保留引用竖线与列表缩进。
    ///
    /// 超宽行按代码宽度折行，续行以 [`CODE_CONTINUATION`] 起头，与真正的下一行
    /// 区分开；内容绝不交给 buffer 静默裁掉。
    fn flush_code(&mut self) {
        let info = self.code_info.take().unwrap_or_default();
        // 制表符按 4 列展开：宽度计算把 `\t` 记作 0 列，不展开背景带就会错位。
        let code = std::mem::take(&mut self.code_buf).replace('\t', "    ");

        if self.place_mermaid(&info, &code) || self.place_svg(&info, &code) {
            return;
        }
        let lang = code_language(&info);
        let mut highlighter = highlight::LineHighlighter::new(lang);
        let indent = self.content_indent();
        // 背景带：引用竖线与列表缩进之后、右侧留 2 列；带内左侧 2 列内边距。
        let band = self
            .width
            .saturating_sub(2 * self.quote_depth + indent + 2)
            .max(1);
        let body = band.saturating_sub(2).max(1);
        let bg = self.theme.code_bg;
        let pad = Span::styled("  ", Style::new().bg(bg));

        if !lang.is_empty() {
            let tag = Span::styled(
                truncate(lang, body).into_owned(),
                Style::new()
                    .fg(self.theme.accent_tool)
                    .bg(bg)
                    .add_modifier(Modifier::BOLD),
            );
            self.push_code_row(indent, band, pad.clone(), vec![tag]);
        }
        if code.is_empty() {
            self.push_code_row(indent, band, pad.clone(), Vec::new());
        }
        let mark = Span::styled(
            CODE_CONTINUATION,
            Style::new().fg(self.theme.gray_dim).bg(bg),
        );
        for code_line in syntect::util::LinesWithEndings::from(code.as_str()) {
            let spans: Vec<Span<'static>> = highlighter.line(code_line)
                .into_iter()
                .map(|span| {
                    let fg = span
                        .fg
                        .map(highlight::rgb_to_color)
                        .unwrap_or(self.theme.text_primary);
                    Span::styled(span.text, Style::new().fg(fg).bg(bg))
                })
                .collect();
            let mut first = true;
            super::wrap::wrap_spans_into(&spans, body, |row| {
                let lead = if std::mem::take(&mut first) {
                    pad.clone()
                } else {
                    mark.clone()
                };
                self.push_code_row(indent, band, lead, row.spans);
            });
        }
        let mut gap = Vec::with_capacity(2);
        if self.quote_depth > 0 {
            gap.push(self.quote_span());
        }
        if indent > 0 {
            gap.push(Span::raw(" ".repeat(indent)));
        }
        self.out.push(Line::from(gap));
    }

    /// 代码块的一行：引用竖线 + 列表缩进 + 左内边距（或续行标记）+ 内容，背景补齐到带宽。
    fn push_code_row(
        &mut self,
        indent: usize,
        band: usize,
        lead: Span<'static>,
        content: Vec<Span<'static>>,
    ) {
        let used = str_width(&lead.content)
            + content
                .iter()
                .map(|span| str_width(&span.content))
                .sum::<usize>();
        let mut line = Vec::with_capacity(content.len() + 4);
        if self.quote_depth > 0 {
            line.push(self.quote_span());
        }
        if indent > 0 {
            line.push(Span::raw(" ".repeat(indent)));
        }
        line.push(lead);
        line.extend(content);
        if used < band {
            line.push(Span::styled(
                " ".repeat(band - used),
                Style::new().bg(self.theme.code_bg),
            ));
        }
        self.out.push(Line::from(line));
    }

    /// 表格：内容定列宽，单元格按列折行；表头与行之间用细线分隔。
    ///
    /// 超宽时优先把列压到可读下限再折行；连两格一列都放不下时，退化为
    /// 逐字段列表，保证任何宽度下都不丢单元格内容。
    fn render_table(&mut self, table: TableAcc) {
        if table.header.is_empty() {
            return;
        }
        let column_count = table
            .header
            .len()
            .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
        if column_count == 0 {
            return;
        }
        let (natural, longest_word, has_wide) = table_metrics(&table, column_count);
        let Some((widths, gap)) = table_widths(self.width, &natural, &longest_word, &has_wide)
        else {
            self.out
                .extend(stacked_table_lines(&table, self.width, self.theme));
            return;
        };
        let table_width: usize =
            widths.iter().sum::<usize>() + gap.saturating_mul(column_count - 1);
        self.out.extend(table_row_lines(
            &table.header,
            &widths,
            &table.alignments,
            gap,
            self.theme,
            true,
        ));
        self.out.push(table_rule(table_width, self.theme));
        for row in &table.rows {
            self.out.extend(table_row_lines(
                row,
                &widths,
                &table.alignments,
                gap,
                self.theme,
                false,
            ));
            self.out.push(table_rule(table_width, self.theme));
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

/// 表格单元格的纯文本：与 [`raw_table`] 用同一份去装饰结果。
fn table_cell_text(cells: &[Vec<Run>], index: usize) -> String {
    cells
        .get(index)
        .map(|cell| cell.iter().map(|run| run.text.as_str()).collect())
        .unwrap_or_default()
}

/// 每列的自然宽度、不可断词宽度与是否含宽字符。
fn table_metrics(table: &TableAcc, columns: usize) -> (Vec<usize>, Vec<usize>, Vec<bool>) {
    let mut natural = vec![1usize; columns];
    let mut longest_word = vec![1usize; columns];
    let mut has_wide = vec![false; columns];
    for cells in std::iter::once(&table.header).chain(table.rows.iter()) {
        for (index, cell) in cells.iter().enumerate().take(columns) {
            let text: String = cell.iter().map(|run| run.text.as_str()).collect();
            natural[index] = natural[index].max(str_width(&text));
            let word = text.split_whitespace().map(str_width).max().unwrap_or(1);
            longest_word[index] = longest_word[index].max(word);
            has_wide[index] |= text
                .chars()
                .any(|ch| unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0) > 1);
        }
    }
    (natural, longest_word, has_wide)
}

/// 计算可见列宽：放得下就按内容自然宽度（不拉伸，窄表不撑满整行）；超宽时先
/// 按可读下限收缩、再退到硬下限，单元格在列内折行。`None` 表示连两格一列都放不下。
fn table_widths(
    available: usize,
    natural: &[usize],
    longest_word: &[usize],
    has_wide: &[bool],
) -> Option<(Vec<usize>, usize)> {
    let columns = natural.len();
    if columns == 0 {
        return None;
    }
    let two_cell_min = columns.saturating_mul(2);
    let gap = if available >= two_cell_min + 2 * (columns - 1) {
        TABLE_COLUMN_GAP
    } else if available >= two_cell_min + (columns - 1) {
        1
    } else {
        return None;
    };
    let usable = available.saturating_sub(gap.saturating_mul(columns - 1));
    let mut widths = natural.to_vec();
    if widths.iter().sum::<usize>() > usable {
        let mut floors: Vec<usize> = (0..columns)
            .map(|index| {
                let token_floor =
                    longest_word[index].clamp(TABLE_COLUMN_FLOOR, TABLE_COLUMN_TOKEN_MAX);
                let mut floor = natural[index].min(token_floor);
                if has_wide[index] {
                    floor = floor.max(2);
                }
                floor.clamp(1, widths[index])
            })
            .collect();
        shrink_widths(&mut widths, &floors, usable);
        if widths.iter().sum::<usize>() > usable {
            for (index, floor) in floors.iter_mut().enumerate() {
                *floor = (if has_wide[index] { 2 } else { 1 }).min(widths[index]);
            }
            shrink_widths(&mut widths, &floors, usable);
        }
    }
    Some((widths, gap))
}

/// 把总宽压到 `target`：每次从“超下限最多”的列减一格，保持列宽均衡。
fn shrink_widths(widths: &mut [usize], floors: &[usize], target: usize) {
    let mut total: usize = widths.iter().sum();
    while total > target {
        let mut best: Option<(usize, usize)> = None;
        for (index, width) in widths.iter().enumerate() {
            let slack = width.saturating_sub(floors[index]);
            if slack > 0 && best.is_none_or(|(_, current)| slack > current) {
                best = Some((index, slack));
            }
        }
        let Some((index, _)) = best else {
            break;
        };
        widths[index] -= 1;
        total -= 1;
    }
}

/// 单元格折行：与正文同一套断点规则（[`wrap_spans`]），保留内联样式，按列对齐补齐到列宽。
fn wrap_cell_runs(
    runs: &[Run],
    width: usize,
    align: Alignment,
    base_style: Style,
) -> Vec<Vec<Span<'static>>> {
    let spans: Vec<Span<'static>> = runs
        .iter()
        .map(|run| Span::styled(run.text.clone(), base_style.patch(run.style)))
        .collect();
    wrap_spans(&spans, width)
        .into_iter()
        .map(|line| {
            let free = width.saturating_sub(line_width(&line));
            let (left, right) = match align {
                Alignment::Right => (free, 0),
                Alignment::Center => (free / 2, free - free / 2),
                Alignment::Left | Alignment::None => (0, free),
            };
            let mut row = Vec::with_capacity(line.spans.len() + 2);
            if left > 0 {
                row.push(Span::styled(" ".repeat(left), base_style));
            }
            row.extend(line.spans);
            if right > 0 {
                row.push(Span::styled(" ".repeat(right), base_style));
            }
            row
        })
        .collect()
}

/// 单行表格：每个单元格按列宽折行，保留单元格内联样式，行高取该行最多的行数。
fn table_row_lines(
    cells: &[Vec<Run>],
    widths: &[usize],
    alignments: &[Alignment],
    gap: usize,
    theme: &Theme,
    header: bool,
) -> Vec<Line<'static>> {
    let base_style = if header {
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::new().fg(theme.text_primary)
    };
    let wrapped: Vec<Vec<Vec<Span<'static>>>> = widths
        .iter()
        .enumerate()
        .map(|(index, width)| {
            let runs = cells.get(index).map_or(&[][..], Vec::as_slice);
            let align = alignments.get(index).copied().unwrap_or(Alignment::None);
            wrap_cell_runs(runs, (*width).max(1), align, base_style)
        })
        .collect();
    let height = wrapped.iter().map(Vec::len).max().unwrap_or(1);
    let gap_span = Span::styled(" ".repeat(gap), base_style);
    let mut lines = Vec::with_capacity(height);
    for line_index in 0..height {
        let mut spans = Vec::with_capacity(widths.len().saturating_mul(2));
        for (index, width) in widths.iter().enumerate() {
            if index > 0 {
                spans.push(gap_span.clone());
            }
            if let Some(cell_line) = wrapped[index].get(line_index) {
                spans.extend(cell_line.iter().cloned());
            } else {
                spans.push(Span::styled(" ".repeat(*width), base_style));
            }
        }
        lines.push(Line::from(spans));
    }
    lines
}

/// 表头与行之间、行与行之间的细线。
fn table_rule(width: usize, theme: &Theme) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width),
        Style::new().fg(theme.border_subtle),
    ))
}

/// 极窄终端：逐字段列出，保证不丢内容。
fn stacked_table_lines(table: &TableAcc, available: usize, theme: &Theme) -> Vec<Line<'static>> {
    let available = available.max(1);
    let style = Style::new().fg(theme.text_primary);
    let columns = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let header: Vec<String> = (0..columns)
        .map(|index| table_cell_text(&table.header, index).trim().to_owned())
        .collect();
    let mut lines = Vec::new();
    if table.rows.is_empty() {
        for line in wrap_text(&header.join("  "), available) {
            lines.push(Line::from(Span::styled(line, style)));
        }
        return lines;
    }
    for (row_index, row) in table.rows.iter().enumerate() {
        for (index, label) in header.iter().enumerate() {
            let value = table_cell_text(row, index);
            let text = match (label.is_empty(), value.is_empty()) {
                (true, _) => value,
                (false, true) => label.clone(),
                (false, false) => format!("{label}: {value}"),
            };
            for line in wrap_text(&text, available) {
                lines.push(Line::from(Span::styled(line, style)));
            }
        }
        if row_index + 1 < table.rows.len() {
            lines.push(table_rule(available, theme));
        }
    }
    lines
}

/// 累积表格 → 纯文本快照。
///
/// 走的是同一份去装饰结果（`**x**`、`` `x` ``、链接文字都已还原成可见文本），
/// 所以「点击入口时做的分析」与「屏幕上看到的表格」永远一致；行内单元格数量
/// 不足时补空串，列数以表头与最宽行为准。
fn raw_table(table: &TableAcc) -> RawTable {
    let column_count = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let cell = |runs: &[Run]| -> String {
        runs.iter()
            .map(|run| run.text.as_str())
            .collect::<String>()
            .trim()
            .to_owned()
    };
    let row = |cells: &[Vec<Run>]| -> Vec<String> {
        (0..column_count)
            .map(|index| cells.get(index).map_or_else(String::new, |runs| cell(runs)))
            .collect()
    };
    RawTable {
        header: row(&table.header),
        rows: table.rows.iter().map(|cells| row(cells)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::render::graphics::{CellPixels, GraphicsProto};
    use crate::theme::{ColorLevel, Theme};

    /// 一张能图表化的表：两行测量值 → 分组条形图（6 行 + 入口）。
    const NUMERIC_TABLE: &str =
        "| 项 | 耗时 | 内存 |\n|---|---|---|\n| a | 10 ms | 2 MiB |\n| b | 20 ms | 4 MiB |";

    fn graphics() -> Option<GraphicsSupport> {
        Some(GraphicsSupport {
            proto: GraphicsProto::Kitty,
            cell: CellPixels { w: 8, h: 18 },
        })
    }

    fn render(source: &str, width: usize, views: RenderViews<'_>) -> Rendered {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        render_markdown_with(source, width, &theme, views)
    }

    #[test]
    fn paged_layout_matches_full_layout_with_global_references_and_tables() {
        let source = format!("{}\n[ref]: https://example.com\n", "# 标题\n\n正文 **强调** 与 [链接][ref]。\n\n> 引用\n> - 子项\n\n```rust\n/* 连续\n注释 */ let x = 1;\n```\n\n| 月份 | 数量 |\n|---|---|\n| 一月 | 12 |\n| 二月 | 24 |\n\n".repeat(30));
        let theme = Theme::resolved(ColorLevel::TrueColor);
        for streaming in [false, true] {
            let views = || RenderViews {
                graphics: graphics(),
                streaming,
                ..Default::default()
            };
            let full = render_markdown_with(&source, 77, &theme, views());
            let paged = render_markdown_pages(&source, 77, &theme, views(), true);
            assert_eq!(full.tables, paged.tables);
            assert_eq!(full.lines.len(), paged.lines.len());
            for start in (0..full.lines.len()).step_by(63) {
                let end = (start + 97).min(full.lines.len());
                assert_eq!(paged.lines.window(start..end), full.lines[start..end]);
            }
        }
    }

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect()
    }

    /// 图表视图态：按当前渲染出的纯文本快照就地分析。
    fn chart_state(source: &str, table: usize, image_id: u32) -> TableChartState {
        let rendered = render(
            source,
            77,
            RenderViews {
                charts: &[],
                graphics: None,
                streaming: false,
                ..RenderViews::default()
            },
        );
        let raw = &rendered.raw[table];
        let analysis = chart::analyze(raw);
        let plan = chart::plan(&analysis).expect("数值表必须能规划出图型");
        TableChartState {
            spec: chart::build(&analysis, &plan),
            payload: None,
            px: (0, 0),
            image_id,
        }
    }

    fn lines(source: &str, width: usize) -> Vec<String> {
        text(&render_markdown(
            source,
            width,
            &Theme::resolved(ColorLevel::TrueColor),
        ))
    }

    #[test]
    fn tables_render_light_rules_and_keep_cell_content_in_order() {
        let single = lines("| 尝试 | 结果 |\n|---|---|\n| a | b |", 40);
        assert!(
            !single
                .iter()
                .any(|line| line.contains('┌') || line.contains('│')),
            "表格不再画全边框盒：{single:?}"
        );
        assert!(
            single.iter().any(|line| line.starts_with('─')),
            "表头/行之间应有细线：{single:?}"
        );
        assert!(
            single.iter().any(|line| line.contains("尝试")),
            "{single:?}"
        );

        let out = lines(
            "| 工具 | 用途 |\n|---|---|\n| `read` | 读取文件 |\n| `bash` | 执行命令 |",
            40,
        );
        let read_row = out.iter().position(|line| line.contains("read"));
        let bash_row = out.iter().position(|line| line.contains("bash"));
        assert!(read_row.is_some() && bash_row.is_some(), "{out:?}");
        assert!(read_row < bash_row, "表格行顺序错误：{out:?}");
        assert!(out.iter().any(|line| line.contains("读取文件")), "{out:?}");
        assert!(out.iter().any(|line| line.contains("执行命令")), "{out:?}");
    }

    /// 超宽表格按列折行，内容完整，线宽不超过正文宽度。
    #[test]
    fn wide_tables_wrap_instead_of_truncating() {
        let source = "| Harness | 请求体硬上限 | 发请求前的字节预算 | 单图目标 | 图片数量上限 | 超限行为 |\n|---|---|---|---|---|---|\n| moc | 无 | 全量序列化字节 50 MiB，高水位 -3 个槽位 | raw 3.75 MiB | 600 | 按旧→新淘汰图片，正文超限交上游 |\n| Codex | 无 | 无（纯 token 窗口 + compaction） | 2000px、base64 ≤5 MiB | 90 | 交上游处理 |\n| DSH | 无 | inline-image-byte-budget 24,000 字节 | 2 MiB / 4096px | 600 | 直接报 IMAGE_OFFLOAD_REQUIRED |";
        let out = lines(source, 120);
        for needle in [
            "Harness",
            "超限行为",
            "全量序列化字节",
            "inline-image-byte-budget",
            "按旧→新淘汰图片",
            "IMAGE_OFFLOAD",
        ] {
            assert!(
                out.iter().any(|line| line.contains(needle)),
                "缺内容 {needle}：{out:?}"
            );
        }
        assert!(
            !out.iter().any(|line| line.contains('…')),
            "表格不得截断：{out:?}"
        );
        assert!(
            out.iter().all(|line| str_width(line) <= 120),
            "表格行不得超宽：{out:?}"
        );
    }

    /// 代码块超宽行折行而非被 buffer 裁掉：内容完整、每行不超宽、续行带标记。
    #[test]
    fn long_code_lines_wrap_within_width() {
        let tail = "TAIL_MARKER";
        let source = format!("```text\n{} {tail}\n```", "x".repeat(70));
        let out = lines(&source, 40);
        assert!(out.iter().all(|line| str_width(line) <= 40), "{out:?}");
        assert!(out.iter().any(|line| line.contains(tail)), "{out:?}");
        assert!(
            out.iter().any(|line| line.contains(CODE_CONTINUATION)),
            "{out:?}"
        );
    }

    /// 引用续行保留竖线，列表续行悬挂到正文起点（不多也不少 1 列）。
    #[test]
    fn continuation_rows_keep_quote_bar_and_list_hang() {
        let quoted = lines(&format!("> {}", "引用正文".repeat(12)), 30);
        assert!(quoted.len() > 1, "{quoted:?}");
        assert!(
            quoted.iter().all(|line| line.starts_with("│ ")),
            "{quoted:?}"
        );

        let listed = lines("- alpha beta gamma delta epsilon zeta eta theta", 20);
        assert!(listed[0].starts_with("• alpha"), "{listed:?}");
        for line in &listed[1..] {
            assert!(
                line.starts_with("  ") && !line.starts_with("   "),
                "{listed:?}"
            );
        }
    }

    /// 以代码块开头的列表项保留自己的标记，且不泄漏给下一项。
    #[test]
    fn item_starting_with_code_keeps_its_bullet() {
        let out = lines("- ```\n  code\n  ```\n- next", 40);
        let bullets = out.iter().filter(|line| line.contains('•')).count();
        assert_eq!(bullets, 2, "{out:?}");
        assert!(out.iter().any(|line| line.starts_with("• next")), "{out:?}");
    }

    /// 放得下的表格按内容自然宽度，不把每列拉到上限。
    #[test]
    fn narrow_tables_keep_natural_widths() {
        let out = lines("| a | b |\n|---|---|\n| x | y |", 120);
        let rule = out
            .iter()
            .find(|line| line.starts_with('─'))
            .expect("表头细线");
        assert_eq!(str_width(rule), 1 + TABLE_COLUMN_GAP + 1, "{out:?}");
    }

    /// 行内代码只在紧贴文字时补空格；全角标点与已有空白旁不补。
    #[test]
    fn inline_code_gap_follows_neighbors() {
        assert_eq!(lines("调用`foo`即可", 40), vec!["调用 foo 即可"]);
        assert_eq!(lines("（`foo`）", 40), vec!["（foo）"]);
        assert_eq!(lines("run `foo` now", 40), vec!["run foo now"]);
    }

    /// 入口只在支持图形协议时出现；按钮左对齐且命中区间等于文字宽度。
    #[test]
    fn table_entry_appears_only_with_graphics_support() {
        let with = render(
            NUMERIC_TABLE,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        assert_eq!(with.tables.len(), 1, "一张顶层表格");
        let slot = with.tables[0];
        let button = slot.button.expect("支持图形时应有入口");
        let width = str_width(ENTRY_CHART);
        assert_eq!(button.w as usize, width);
        assert_eq!(button.x, 0, "与正文左缘对齐");
        assert_eq!(text(&with.lines)[button.row], "▸ 可视化图表");

        let without = render(
            NUMERIC_TABLE,
            80,
            RenderViews {
                charts: &[],
                graphics: None,
                streaming: false,
                ..RenderViews::default()
            },
        );
        assert!(without.tables[0].button.is_none(), "无图形能力时不出入口");
        assert!(
            text(&without.lines)
                .iter()
                .any(|line| line.contains("耗时")),
            "表格照常画"
        );
    }

    /// 流式尾部：末尾表格先不落定入口，其后已有正文的表格照常落定。
    #[test]
    fn streaming_tail_table_defers_its_entry() {
        let source = format!("{NUMERIC_TABLE}\n\n正文\n\n{NUMERIC_TABLE}");
        let streamed = render(
            &source,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: true,
                ..RenderViews::default()
            },
        );
        assert_eq!(streamed.tables.len(), 2);
        assert!(streamed.tables[0].button.is_some(), "后面的表格已落地");
        assert!(
            streamed.tables[1].button.is_none(),
            "末尾表格还在长，入口等收尾"
        );

        let settled = render(
            &source,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        assert!(settled.tables[1].button.is_some(), "收尾后入口出现");
    }

    /// 已切换成图表：表格保留，入口在表格下方，位图占位区在入口下方。
    #[test]
    fn chart_state_keeps_table_and_appends_placeholder_below() {
        let state = chart_state(NUMERIC_TABLE, 0, 7);
        let charts = [Some(state)];
        let rendered = render(
            NUMERIC_TABLE,
            80,
            RenderViews {
                charts: &charts,
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        let out = text(&rendered.lines);
        assert!(
            out.iter().any(|line| line.contains("耗时"))
                && out.iter().any(|line| line.contains("4 MiB")),
            "表格必须保留：{out:?}"
        );
        let slot = rendered.tables[0];
        let chart = slot.chart.expect("应有图表占位区");
        let button = slot.button.expect("应有隐藏入口");
        assert_eq!(chart.cols, 80, "正文宽度未超过 720px 上限");
        assert_eq!(chart.px, (80 * 8, u32::from(chart.rows) * 18));
        assert_eq!(button.row + 2, chart.row, "入口与图表之间应隔一个空行");
        assert!(
            out[button.row].starts_with("▾ 隐藏图表"),
            "表格下方是隐藏入口：{out:?}"
        );
        assert!(
            out[button.row + 1].trim().is_empty(),
            "入口与图表之间应为空行"
        );
        assert!(
            out[chart.row..chart.row + usize::from(chart.rows)]
                .iter()
                .all(|line| line.trim().is_empty()),
            "占位行必须是空白"
        );
    }

    /// 图表宽度固定为 720px 上限，即使正文更宽也不拉满；入口仍左对齐。
    #[test]
    fn chart_width_is_capped_at_720px() {
        let state = chart_state(NUMERIC_TABLE, 0, 7);
        let charts = [Some(state)];
        let rendered = render(
            NUMERIC_TABLE,
            200,
            RenderViews {
                charts: &charts,
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        let slot = rendered.tables[0];
        let chart = slot.chart.expect("应有图表占位区");
        let button = slot.button.expect("应有隐藏入口");
        assert_eq!(chart.cols, 90, "720 / 8 = 90 列");
        assert_eq!(chart.px.0, 720);
        assert_eq!(button.x, 0);
    }

    /// 多张表格：每张图的占位区都必须紧贴自己的入口，不能被后一张表的按钮挤歪。
    #[test]
    fn multiple_tables_keep_chart_rows_aligned() {
        let source = format!("{NUMERIC_TABLE}\n\n正文\n\n{NUMERIC_TABLE}");
        let charts = [Some(chart_state(NUMERIC_TABLE, 0, 7)), None];
        let rendered = render(
            &source,
            80,
            RenderViews {
                charts: &charts,
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        assert_eq!(rendered.tables.len(), 2);
        let first = rendered.tables[0];
        let second = rendered.tables[1];
        let first_chart = first.chart.expect("第一张表应展开图表");
        let first_button = first.button.expect("第一张表应有隐藏入口");
        let second_button = second.button.expect("第二张表应有可视化入口");
        assert_eq!(first_chart.row, first_button.row + 2);
        assert_eq!(second.chart, None);
        assert!(
            second_button.row > first_chart.row + usize::from(first_chart.rows),
            "第二张表的入口必须在第一张图之后"
        );
        let out = text(&rendered.lines);
        assert!(out[first_button.row].starts_with("▾ 隐藏图表"));
        assert!(out[second_button.row].starts_with("▸ 可视化图表"));
    }

    /// 纯文本快照与屏幕上的单元格一致：装饰标记不会漏进分析（`**7648**` → `7648`）。
    #[test]
    fn raw_snapshot_matches_visible_cells() {
        let source = "| 项 | 值 |\n|---|---|\n| **a** | `7648` |\n| [b](https://x) | 12 |";
        let rendered = render(
            source,
            80,
            RenderViews {
                charts: &[],
                graphics: None,
                streaming: false,
                ..RenderViews::default()
            },
        );
        let raw = &rendered.raw[0];
        assert_eq!(raw.header, vec!["项".to_owned(), "值".to_owned()]);
        assert_eq!(raw.rows[0], vec!["a".to_owned(), "7648".to_owned()]);
        assert_eq!(raw.rows[1], vec!["b".to_owned(), "12".to_owned()]);
    }

    /// 引用块 / 列表内嵌的表格不参与图表化：不进序号、不出入口，也不影响顶层序号。
    #[test]
    fn nested_tables_stay_out_of_the_chart_index() {
        let source =
            format!("> | 项 | 值 |\n> |---|---|---|\n> | a | 1 |\n> | b | 2 |\n\n{NUMERIC_TABLE}");
        let rendered = render(
            &source,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        assert_eq!(rendered.raw.len(), 1, "只有顶层表格进入快照");
        assert_eq!(rendered.tables.len(), 1);
        assert_eq!(rendered.tables[0].table, 0, "顶层序号从 0 起");
        assert!(rendered.tables[0].button.is_some());
    }

    /// 落一张图进缓存：解码结果只需要尺寸，位图内容与排版无关。
    fn ready_store(key: &ImageKey, size: (u32, u32)) -> ImageStore {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = ImageStore::new(events);
        store.complete(
            key.clone(),
            Ok(crate::render::image::DecodedImage::Raster(
                image::RgbaImage::from_pixel(size.0, size.1, image::Rgba([0x10, 0x20, 0x30, 0xff])),
            )),
        );
        store
    }

    /// 本地图片按格贴出：留出与版式一致的行数，并登记一张落位。
    #[test]
    fn local_image_reserves_rows_and_registers_a_placement() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("shot.png");
        std::fs::write(&path, b"not decoded here").expect("落盘");
        let key = file_key(&path).expect("文件存在");
        // 320×180 源图 → 640×360 之外的框里按 720 上限收窄（40 列 × 8px）
        let store = ready_store(&key, (320, 180));
        let source = "前文\n\n![](./shot.png)\n\n后文".to_owned();
        let rendered = render(
            &source,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                images: Some(&store),
                cwd: Some(directory.path()),
                spawn_images: false,
            },
        );
        let layout = image_layout((320, 180), CellPixels { w: 8, h: 18 }, 80).expect("版式");
        assert_eq!(rendered.images.len(), 1, "一张图一张落位");
        let placement = &rendered.images[0];
        assert_eq!(placement.key, key);
        assert_eq!((placement.cols, placement.rows), (layout.cols, layout.rows));
        assert_eq!(placement.x, u16::try_from(PROSE_INDENT).unwrap_or(0));
        // 占位行确实存在，且与落位行号对齐
        let lines = text(&rendered.lines);
        for (offset, line) in lines
            .iter()
            .skip(placement.row)
            .take(usize::from(placement.rows))
            .enumerate()
        {
            assert!(
                line.trim().is_empty(),
                "占位行 {} 必须留白：{line:?}",
                placement.row + offset
            );
        }
        assert!(
            lines.iter().any(|line| line.contains("后文")),
            "图片之后的正文照常输出"
        );
    }

    /// 文件不存在：回落 alt 文本，不留占位行（改动前的内联行为）。
    #[test]
    fn missing_image_falls_back_to_alt_text() {
        let directory = tempfile::tempdir().expect("临时目录");
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = ImageStore::new(events);
        let rendered = render(
            "![](./nope.png) 说明",
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                images: Some(&store),
                cwd: Some(directory.path()),
                spawn_images: false,
            },
        );
        assert!(rendered.images.is_empty());
        assert!(
            text(&rendered.lines)
                .iter()
                .any(|line| line.contains("说明"))
        );
    }

    /// 回归：画不出图的图片不该把段落劈成两行。
    ///
    /// 远端图、不存在的本地文件都走 alt 文本；改动前 `Tag::Image` 无条件
    /// 结算 inline 缓冲，连「永远不会显示」的图片也会把句子截断。
    #[test]
    fn non_renderable_image_keeps_the_paragraph_inline() {
        let rendered = render(
            "请看 ![远端图](https://example.com/a.png) 这个",
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                ..RenderViews::default()
            },
        );
        let lines = text(&rendered.lines);
        assert_eq!(lines.len(), 1, "段落被劈开了：{lines:?}");
        assert!(lines[0].contains("请看"));
        assert!(lines[0].contains("远端图"));
        assert!(lines[0].contains("这个"));
    }

    /// `file://` 的 host 段与百分号编码。
    #[test]
    fn file_url_path_handles_host_and_escapes() {
        assert_eq!(
            file_url_path("/tmp/a%20b.png").expect("空 host"),
            PathBuf::from("/tmp/a b.png"),
            "百分号编码要解开"
        );
        assert_eq!(
            file_url_path("localhost/tmp/a.png").expect("localhost"),
            PathBuf::from("/tmp/a.png"),
            "localhost 视作本机"
        );
        assert!(
            file_url_path("example.com/tmp/a.png").is_none(),
            "远程 host 拒绝"
        );
        // 非法转义原样保留：宁可找不到文件，也不静默改路径
        assert_eq!(percent_decode("a%2"), "a%2");
        assert_eq!(percent_decode("a%zz"), "a%zz");
    }

    /// 远端链接不联网、不渲染：只留 alt 文本。
    #[test]
    fn remote_images_never_render() {
        let directory = tempfile::tempdir().expect("临时目录");
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = ImageStore::new(events);
        let rendered = render(
            "![远端图](https://example.com/a.png)",
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                images: Some(&store),
                cwd: Some(directory.path()),
                spawn_images: true,
            },
        );
        assert!(rendered.images.is_empty());
        assert!(
            text(&rendered.lines)
                .iter()
                .any(|line| line.contains("远端图"))
        );
    }

    /// 引用块与列表里的图片不贴：没有独立命中区，宽度也不够。
    #[test]
    fn images_inside_containers_stay_textual() {
        let directory = tempfile::tempdir().expect("临时目录");
        let path = directory.path().join("shot.png");
        std::fs::write(&path, b"x").expect("落盘");
        let key = file_key(&path).expect("文件存在");
        let store = ready_store(&key, (64, 36));
        let source = "> ![](./shot.png)\n\n- ![](./shot.png)\n".to_owned();
        let rendered = render(
            &source,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                images: Some(&store),
                cwd: Some(directory.path()),
                spawn_images: false,
            },
        );
        assert!(rendered.images.is_empty(), "容器内不贴图");
    }

    /// SVG 代码块整块换成位图：就绪时不留一行代码。
    #[test]
    fn svg_fence_is_replaced_by_a_placeholder() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"200\" height=\"120\"><rect width=\"200\" height=\"120\" fill=\"#3b82f6\"/></svg>";
        let source = format!("```svg\n{svg}\n```");
        let key = ImageKey::from_svg(&format!("{svg}\n"));
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = ImageStore::new(events);
        store.complete(
            key,
            Ok(
                crate::render::image::decode::decode(crate::render::image::ImageSourceSpec::Svg(
                    format!("{svg}\n").into_boxed_str(),
                ))
                .expect("SVG 可解析"),
            ),
        );
        let rendered = render(
            &source,
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                images: Some(&store),
                cwd: None,
                spawn_images: false,
            },
        );
        assert_eq!(rendered.images.len(), 1, "一张 SVG 落位");
        assert!(
            !text(&rendered.lines)
                .iter()
                .any(|line| line.contains("<svg") || line.contains("rect")),
            "就绪时不再输出代码行"
        );
    }

    /// 还没解码完就照常出代码块，解码完成后自动换成像素图。
    #[test]
    fn pending_svg_falls_back_to_the_code_block() {
        let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"10\"><rect width=\"20\" height=\"10\" fill=\"#000\"/></svg>";
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let store = ImageStore::new(events);
        let rendered = render(
            &format!("```svg\n{svg}\n```"),
            80,
            RenderViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
                images: Some(&store),
                cwd: None,
                spawn_images: false,
            },
        );
        assert!(rendered.images.is_empty());
        assert!(
            text(&rendered.lines)
                .iter()
                .any(|line| line.contains("<svg")),
            "未就绪时保留代码块"
        );
    }

    fn joined(source: &str, views: RenderViews<'_>) -> String {
        text(&render(source, 40, views).lines).join("\n")
    }

    /// 闭合的流程图画成字符图；类图仍是代码块。
    #[test]
    fn mermaid_fence_draws_flowchart() {
        let source = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n";
        let picture = joined(source, RenderViews::default());
        assert!(
            picture.contains("Start") && picture.contains("End"),
            "{picture}"
        );
        assert!(picture.contains('▼'), "{picture}");
        assert!(!picture.contains("flowchart"), "{picture}");

        let class = joined(
            "```mermaid\nclassDiagram\n  Animal <|-- Duck\n```\n",
            RenderViews::default(),
        );
        assert!(class.contains("classDiagram"), "{class}");
    }

    /// 流式输出里未闭合的围栏保持源码；收尾后即使缺闭合围栏也尝试画。
    #[test]
    fn streaming_mermaid_stays_code_until_the_fence_closes() {
        let open = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n";
        let streaming = joined(
            open,
            RenderViews {
                streaming: true,
                ..RenderViews::default()
            },
        );
        assert!(streaming.contains("flowchart"), "{streaming}");

        let closed = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n后面还有字";
        let live = joined(
            closed,
            RenderViews {
                streaming: true,
                ..RenderViews::default()
            },
        );
        assert!(
            live.contains("Start") && !live.contains("flowchart"),
            "{live}"
        );

        let settled = joined(open, RenderViews::default());
        assert!(
            settled.contains("Start") && !settled.contains("flowchart"),
            "{settled}"
        );

        // CRLF 行尾与闭围栏后的尾随空白都算闭合。
        let crlf = "```mermaid\r\nflowchart TD\r\n  A[Start] --> B[End]\r\n```\r\n";
        let live_crlf = joined(
            crlf,
            RenderViews {
                streaming: true,
                ..RenderViews::default()
            },
        );
        assert!(
            live_crlf.contains("Start") && !live_crlf.contains("flowchart"),
            "{live_crlf}"
        );

        // ``` 内容行在 ```` 块里不算闭合。
        let nested = "````mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n";
        let nested_live = joined(
            nested,
            RenderViews {
                streaming: true,
                ..RenderViews::default()
            },
        );
        assert!(nested_live.contains("flowchart"), "{nested_live}");
    }

    /// 引用和列表里没有独立的图区，围栏保持代码块。
    #[test]
    fn mermaid_in_quote_or_list_stays_code() {
        let quoted = joined(
            "> ```mermaid\n> flowchart TD\n>   A[Start] --> B[End]\n> ```\n",
            RenderViews::default(),
        );
        assert!(quoted.contains("flowchart"), "{quoted}");

        let listed = joined(
            "- ```mermaid\n  flowchart TD\n  A[Start] --> B[End]\n  ```\n",
            RenderViews::default(),
        );
        assert!(listed.contains("flowchart"), "{listed}");
    }
}

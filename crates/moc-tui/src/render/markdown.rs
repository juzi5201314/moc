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

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::PROSE_INDENT;
use super::graphics::GraphicsSupport;
use super::highlight;
use super::image::{
    ImageKey, ImagePlacement, ImageSourceSpec, ImageState, ImageStore, layout as image_layout,
};
use super::wrap::{str_width, wrap_text};
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

/// 单列宽度上限：正文宽度的 30%，对齐 DSH Web 的 30vw。
const TABLE_COLUMN_CAP_PERCENT: usize = 30;

/// 单列宽度上限的下限与上限（格）。
const TABLE_COLUMN_CAP_FLOOR: usize = 12;
const TABLE_COLUMN_CAP_CEILING: usize = 40;

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
    let mut renderer = MdRenderer {
        theme,
        views,
        width: width.max(8),
        out: Vec::new(),
        inline: Vec::new(),
        bold: 0,
        italic: 0,
        strike: 0,
        link_depth: 0,
        quote_depth: 0,
        heading: None,
        list_stack: Vec::new(),
        item_prefix: String::new(),
        item_indent: 0,
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
    out: Vec<Line<'static>>,
    inline: Vec<Run>,
    bold: u32,
    italic: u32,
    strike: u32,
    link_depth: u32,
    quote_depth: usize,
    heading: Option<HeadingLevel>,
    list_stack: Vec<ListFrame>,
    /// 当前列表项的前缀（bullet / 序号 + 缩进），flush 时应用。
    item_prefix: String,
    item_indent: usize,
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
                self.push_run(Run {
                    style: Style::new().fg(color),
                    text: marker.into(),
                });
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
                self.list_stack.push(ListFrame {
                    ordered: start.is_some(),
                    next_index: start.unwrap_or(1),
                });
            }
            Tag::Item => {
                self.flush_inline();
                let depth = self.list_stack.len();
                let prefix = match self.list_stack.last_mut() {
                    Some(frame) if frame.ordered => {
                        let marker = format!("{}. ", frame.next_index);
                        frame.next_index += 1;
                        "  ".repeat(depth - 1) + &marker
                    }
                    Some(_) => "  ".repeat(depth - 1) + "• ",
                    None => String::new(),
                };
                self.item_indent = str_width(&prefix);
                self.item_prefix.push_str(&prefix);
            }
            Tag::Table(_) => {
                self.flush_inline();
                self.block_boundary();
                self.ensure_block_gap();
                self.table = Some(TableAcc::default());
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
                // 标题换行时逐级递减亮度 + 符号前缀（omp 的层级感）
                match level {
                    HeadingLevel::H1 => self.prefix_inline("█ ", self.theme.accent_assistant, true),
                    HeadingLevel::H2 => self.prefix_inline("▌ ", self.theme.accent_user, true),
                    HeadingLevel::H3 => self.prefix_inline("◆ ", self.theme.accent_tool, true),
                    _ => self.prefix_inline("◇ ", self.theme.text_secondary, true),
                }
                self.heading = None;
                self.flush_inline();
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
                self.item_indent = 0;
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

    /// 给 inline 缓冲头部注入一个带色前缀（标题符号）。
    fn prefix_inline(&mut self, prefix: &str, color: ratatui::style::Color, bold: bool) {
        let mut style = Style::new().fg(color);
        if bold {
            style = style.add_modifier(Modifier::BOLD);
        }
        self.inline.insert(
            0,
            Run {
                style,
                text: prefix.to_owned(),
            },
        );
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
        let style = Style::new().fg(self.theme.path).bg(self.theme.code_bg);
        self.push_run(Run {
            style,
            text: format!(" {text} "),
        });
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
    fn finish(mut self) -> Rendered {
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
                let label_prefix = if label.is_empty() || label == url {
                    String::new()
                } else {
                    format!("{label}: ")
                };
                let prefix = format!("{num}{label_prefix}");
                let prefix_width = str_width(&prefix);
                let rem = self.width.saturating_sub(prefix_width).max(8);
                self.out.push(Line::from(vec![
                    Span::styled(prefix, Style::new().fg(self.theme.gray_dim)),
                    Span::styled(
                        crate::render::modal::ellipsize(url, rem),
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
        Rendered {
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

    /// 段落 / 标题 / 列表项结束：inline 缓冲按宽度换行后产出。
    ///
    /// 首行带引用竖线 + 列表前缀；续行悬挂对齐到内容起点。
    /// 相同样式的相邻字符合并为单个 Span（大幅减少 Span 数量与内存）。
    fn flush_inline(&mut self) {
        if self.inline.is_empty() {
            return;
        }
        let prefix = if !self.item_prefix.is_empty() {
            std::mem::take(&mut self.item_prefix)
        } else if self.item_indent > 0 {
            " ".repeat(self.item_indent)
        } else {
            String::new()
        };
        let quote = "│ ".repeat(self.quote_depth);
        let full_prefix = format!("{quote}{prefix}");
        let prefix_width = str_width(&full_prefix);
        let runs = std::mem::take(&mut self.inline);
        // 展开为 (char, style) 序列再按宽度切行——跨片段换行时样式保持
        let chars: Vec<(char, Style)> = runs
            .iter()
            .flat_map(|run| run.text.chars().map(move |ch| (ch, run.style)))
            .collect();

        let mut line: Vec<Span<'static>> = Vec::new();
        if !full_prefix.is_empty() {
            line.push(Span::styled(
                full_prefix.clone(),
                Style::new().fg(self.theme.border),
            ));
        }
        let budget = self.width.saturating_sub(prefix_width);
        let hang = " ".repeat(prefix_width.saturating_sub(str_width(&quote)));
        let mut used = 0usize;
        let mut segment = String::new();
        let mut segment_style: Option<Style> = None;
        // 结算当前同样式片段
        macro_rules! flush_segment {
            () => {
                if let Some(style) = segment_style.take() {
                    if !segment.is_empty() {
                        line.push(Span::styled(std::mem::take(&mut segment), style));
                    }
                    segment.clear();
                }
            };
        }
        for (ch, style) in chars {
            let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + ch_width > budget && used > 0 {
                flush_segment!();
                self.out.push(Line::from(std::mem::take(&mut line)));
                line.push(Span::styled(hang.clone(), Style::new()));
                used = 0;
            }
            match segment_style {
                Some(current) if current == style => segment.push(ch),
                _ => {
                    flush_segment!();
                    segment.push(ch);
                    segment_style = Some(style);
                }
            }
            used += ch_width;
        }
        flush_segment!();
        if !line.is_empty() {
            self.out.push(Line::from(line));
        }
    }

    /// 代码块结束：syntect 高亮并整体铺 code_bg，保留引用竖线与列表缩进。
    fn flush_code(&mut self) {
        let info = self.code_info.take().unwrap_or_default();
        let code = std::mem::take(&mut self.code_buf);

        if self.place_mermaid(&info, &code) || self.place_svg(&info, &code) {
            return;
        }
        let highlighted = highlight::highlight(&info, &code);
        let bg_style = Style::new().bg(self.theme.code_bg);

        let list_indent = if self.item_indent > 0 {
            self.item_indent
        } else {
            self.list_stack.len() * 2
        };
        let quote = "│ ".repeat(self.quote_depth);
        let quote_style = Style::new().fg(self.theme.border);
        let prefix_width = str_width(&quote) + list_indent;
        let code_width = self.width.saturating_sub(prefix_width + 2).max(8);

        let make_prefix = || -> Vec<Span<'static>> {
            let mut p = Vec::new();
            if self.quote_depth > 0 {
                p.push(Span::styled(quote.clone(), quote_style));
            }
            if list_indent > 0 {
                p.push(Span::raw(" ".repeat(list_indent)));
            }
            p
        };

        if !info.trim().is_empty() {
            let tag = info.trim();
            let mut top_line = make_prefix();
            top_line.push(Span::styled("  ", bg_style));
            top_line.push(Span::styled(
                tag.to_owned(),
                Style::new()
                    .fg(self.theme.accent_tool)
                    .bg(self.theme.code_bg)
                    .add_modifier(Modifier::BOLD),
            ));
            let used = 2 + str_width(tag);
            if used < code_width {
                top_line.push(Span::styled(" ".repeat(code_width - used), bg_style));
            }
            self.out.push(Line::from(top_line));
        }
        if highlighted.lines.is_empty() {
            let mut line = make_prefix();
            line.push(Span::styled("  ", bg_style));
            if code_width > 2 {
                line.push(Span::styled(" ".repeat(code_width - 2), bg_style));
            }
            self.out.push(Line::from(line));
        } else {
            for spans in &highlighted.lines {
                let mut line = make_prefix();
                line.push(Span::styled("  ", bg_style));
                let mut line_used = 2;
                for span in spans {
                    let fg = span
                        .fg
                        .map(highlight::rgb_to_color)
                        .unwrap_or(self.theme.text_primary);
                    line_used += str_width(&span.text);
                    line.push(Span::styled(
                        span.text.clone(),
                        Style::new().fg(fg).bg(self.theme.code_bg),
                    ));
                }
                if line_used < code_width {
                    line.push(Span::styled(" ".repeat(code_width - line_used), bg_style));
                }
                self.out.push(Line::from(line));
            }
        }
        let gap = make_prefix();
        self.out.push(Line::from(gap));
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
            gap,
            self.theme,
            true,
        ));
        self.out.push(table_rule(table_width, self.theme));
        for row in &table.rows {
            self.out
                .extend(table_row_lines(row, &widths, gap, self.theme, false));
            self.out.push(table_rule(table_width, self.theme));
        }
    }
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

/// 计算可见列宽：先封顶，再在可用宽度内均衡拉伸；放不下时按可读下限
/// 收缩，最后退到硬下限。`None` 表示连两格一列都放不下。
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
    let cap = (available * TABLE_COLUMN_CAP_PERCENT / 100)
        .clamp(TABLE_COLUMN_CAP_FLOOR, TABLE_COLUMN_CAP_CEILING);
    let mut widths: Vec<usize> = natural.iter().map(|width| (*width).clamp(1, cap)).collect();
    let mut total: usize = widths.iter().sum();
    if total > usable {
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
        total = widths.iter().sum();
        if total > usable {
            for (index, floor) in floors.iter_mut().enumerate() {
                *floor = (if has_wide[index] { 2 } else { 1 }).min(widths[index]);
            }
            shrink_widths(&mut widths, &floors, usable);
            total = widths.iter().sum();
        }
    }
    if total < usable {
        fill_widths(&mut widths, cap, usable);
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

/// 把总宽拉向 `target`：每次给“离上限最远”的列加一格，封顶后停止。
fn fill_widths(widths: &mut [usize], cap: usize, target: usize) {
    let mut total: usize = widths.iter().sum();
    while total < target {
        let mut best: Option<(usize, usize)> = None;
        for (index, width) in widths.iter().enumerate() {
            let headroom = cap.saturating_sub(*width);
            if headroom > 0 && best.is_none_or(|(_, current)| headroom > current) {
                best = Some((index, headroom));
            }
        }
        let Some((index, _)) = best else {
            break;
        };
        widths[index] += 1;
        total += 1;
    }
}

/// 单元格内联样式折行：保留粗体、斜体、代码等样式并按列宽规整换行。
fn wrap_cell_runs(runs: &[Run], width: usize, base_style: Style) -> Vec<Vec<Span<'static>>> {
    if runs.is_empty() {
        return vec![vec![Span::styled(" ".repeat(width), base_style)]];
    }
    let chars: Vec<(char, Style)> = runs
        .iter()
        .flat_map(|run| {
            let s = base_style.patch(run.style);
            run.text.chars().map(move |c| (c, s))
        })
        .collect();

    if chars.is_empty() {
        return vec![vec![Span::styled(" ".repeat(width), base_style)]];
    }

    let mut result: Vec<Vec<Span<'static>>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut current_line_width = 0;
    let mut segment = String::new();
    let mut segment_style: Option<Style> = None;

    macro_rules! flush_seg {
        () => {
            if let Some(style) = segment_style.take() {
                if !segment.is_empty() {
                    current_line.push(Span::styled(std::mem::take(&mut segment), style));
                }
                segment.clear();
            }
        };
    }

    for (ch, style) in chars {
        let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if current_line_width > 0 && current_line_width + cw > width {
            flush_seg!();
            if current_line_width < width {
                current_line.push(Span::styled(
                    " ".repeat(width - current_line_width),
                    base_style,
                ));
            }
            result.push(std::mem::take(&mut current_line));
            current_line_width = 0;
        }
        match segment_style {
            Some(curr) if curr == style => segment.push(ch),
            _ => {
                flush_seg!();
                segment.push(ch);
                segment_style = Some(style);
            }
        }
        current_line_width += cw;
    }
    flush_seg!();

    if !current_line.is_empty() {
        if current_line_width < width {
            current_line.push(Span::styled(
                " ".repeat(width - current_line_width),
                base_style,
            ));
        }
        result.push(current_line);
    }

    if result.is_empty() {
        result.push(vec![Span::styled(" ".repeat(width), base_style)]);
    }

    result
}

/// 单行表格：每个单元格按列宽折行，保留单元格内联样式，行高取该行最多的行数。
fn table_row_lines(
    cells: &[Vec<Run>],
    widths: &[usize],
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
            let empty = Vec::new();
            let runs = cells.get(index).unwrap_or(&empty);
            wrap_cell_runs(runs, (*width).max(1), base_style)
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

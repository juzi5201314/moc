//! Markdown → 终端行渲染（pulldown-cmark 事件流直接驱动）。
//!
//! inline 内容在缓冲区里以「片段序列（文本 + 已合成样式）」积累，段落 /
//! 标题结束时统一按显示宽度换行产出（续行悬挂缩进）；代码块整块交给
//! syntect 高亮；表格按内容宽度画全边框盒。
//!
//! 顶层表格下方会挂一个可点击入口（`▸ 可视化图表`）：点击后该表格就地换成
//! 像素图占位行（真正的位图由 `render::graphics` 在帧后贴上），再点
//! `▾ 还原表格` 换回。入口只在终端支持图形协议时出现。

use moc_core::chart::{self, RawTable};

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::graphics::GraphicsSupport;
use super::highlight;
use super::wrap::str_width;
use crate::theme::Theme;
use crate::transcript::TableChartState;

/// 表格入口的两种形态：展开成图表 / 还原成表格。
const ENTRY_CHART: &str = "▸ 可视化图表";
const ENTRY_TABLE: &str = "▾ 还原表格";

/// 渲染 markdown 文本为终端行（无表格视图态：表格永远画成表格）。
pub fn render_markdown(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    render_markdown_with(
        source,
        width,
        theme,
        TableViews {
            charts: &[],
            graphics: None,
            streaming: false,
        },
    )
    .lines
}

/// 表格视图态：由转写层注入（进程内视图态），渲染层只读。
pub struct TableViews<'a> {
    /// 每张顶层表格的图表态（已切换则 `Some`），下标与渲染顺序一致。
    pub charts: &'a [Option<TableChartState>],
    /// 终端图形能力；`None` 时表格下不出入口（不支持的终端不该看到死按钮）。
    pub graphics: Option<GraphicsSupport>,
    /// 该块是否仍在流式接收：末尾表格的入口等它落地再出现。
    pub streaming: bool,
}

/// 一张顶层表格在渲染结果里的落位信息（命中表与图像槽位登记用）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TableSlot {
    /// 顶层表格序号（从 0 起，与 [`RawTable`] 顺序一致）。
    pub table: usize,
    /// 入口按钮所在行与列区间。
    pub button: Option<ButtonSpan>,
    /// 已切换成图表时的占位行区间。
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
}

/// 渲染 markdown 文本，并带上表格视图态。
pub fn render_markdown_with(
    source: &str,
    width: usize,
    theme: &Theme,
    views: TableViews<'_>,
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
        table: None,
        table_cell: Vec::new(),
        in_table_head: false,
        in_table_cell: false,
        table_index: 0,
        buttons: Vec::new(),
        tails: false,
        slots: Vec::new(),
        raw: Vec::new(),
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
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

#[derive(Default)]
struct TableAcc {
    header: Vec<Vec<Run>>,
    rows: Vec<Vec<Vec<Run>>>,
}

struct MdRenderer<'a> {
    theme: &'a Theme,
    views: TableViews<'a>,
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
    table: Option<TableAcc>,
    table_cell: Vec<Run>,
    in_table_head: bool,
    in_table_cell: bool,
    /// 顶层表格序号（引用块 / 列表内嵌表格不计数，与渲染顺序一致）。
    table_index: usize,
    /// 待落定的入口按钮：`(表格序号, 插入前行号, 是否为图表态)`。
    buttons: Vec<(usize, usize, bool)>,
    /// 最后一张表格之后是否已经出现别的块（流式尾部判定）。
    tails: bool,
    /// 表格落位信息（顺序与表格序号一致）。
    slots: Vec<TableSlot>,
    /// 表格纯文本快照，供点击入口时就地分析。
    raw: Vec<RawTable>,
}

impl<'a> MdRenderer<'a> {
    fn handle(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start(tag),
            Event::End(tag) => self.end(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => {
                let style = Style::new().fg(self.theme.path).bg(self.theme.code_bg);
                self.push_run(Run {
                    style,
                    text: format!(" {code} "),
                });
            }
            Event::SoftBreak => {
                if self.in_table_cell {
                    self.push_run(Run {
                        style: self.current_style(),
                        text: " ".to_owned(),
                    });
                } else {
                    self.flush_inline();
                }
            }
            Event::HardBreak => {
                if self.in_table_cell {
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
            Tag::Link { .. } => self.link_depth += 1,
            Tag::Image { .. }
            | Tag::MetadataBlock(_)
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
            TagEnd::Link => self.link_depth = self.link_depth.saturating_sub(1),
            TagEnd::Image
            | TagEnd::MetadataBlock(_)
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
        let style = self.current_style();
        self.push_inline_text(text, style);
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

    /// 表格收尾：顶层表格可能已被切换成图表，其余照常画成表格并挂入口。
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
        if let Some(layout) = layout {
            let row = self.out.len();
            let blank = " ".repeat(self.width);
            self.out.extend(std::iter::repeat_n(
                Line::from(blank),
                usize::from(layout.rows),
            ));
            self.slots.push(TableSlot {
                table: index,
                button: None,
                chart: Some(ChartPlacement {
                    row,
                    cols: layout.cols,
                    rows: layout.rows,
                    px: layout.px,
                }),
            });
            self.buttons.push((index, self.out.len(), true));
            self.raw.push(raw);
            return;
        }
        self.render_table(table);
        self.slots.push(TableSlot {
            table: index,
            button: None,
            chart: None,
        });
        // 入口只在「终端能画像素图 + 格子放得下字 + 这张表确实能图表化」时出现；
        // 已经切换过图表的表格必须保留还原入口，哪怕当前画不出图。
        let fits = self.width >= usize::from(crate::render::chart::MIN_COLS)
            && self
                .views
                .graphics
                .is_some_and(|graphics| crate::render::chart::cell_supported(graphics.cell));
        let plannable = fits
            && chart::plan(&chart::analyze(&raw)).is_some_and(|plan| {
                crate::render::chart::rows_needed(plan.kind, raw.rows.len(), plan.series.len())
                    <= crate::render::chart::MAX_ROWS
            });
        let offers = charted.is_some() || plannable;
        if offers {
            // 用户可能马上点入口：后台先把字体解析好，别让第一张图卡在解析上
            crate::render::chart::warm_fonts();
            self.buttons
                .push((index, self.out.len(), charted.is_some()));
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
        let open_tail = self.views.streaming
            && !self.tails
            && self.buttons.last().is_some_and(|(_, _, chart)| !chart);
        if open_tail {
            self.buttons.pop();
        }
        let kept: Vec<(usize, usize, bool)> = std::mem::take(&mut self.buttons);
        // 落定第 k 个入口会让它之后的每一行下移一行：插入行号与图表占位行都按
        // 同一份偏移修正，绘制与命中才不会错位。
        for (order, (table, row, chart)) in kept.iter().enumerate() {
            let label = if *chart { ENTRY_TABLE } else { ENTRY_CHART };
            let label_width = str_width(label);
            let x = self.width.saturating_sub(label_width);
            let line = Line::from(vec![
                Span::styled(" ".repeat(x), Style::new()),
                Span::styled(
                    (*label).to_owned(),
                    Style::new()
                        .fg(self.theme.link)
                        .add_modifier(Modifier::UNDERLINED),
                ),
            ]);
            let row = row + order;
            self.out.insert(row, line);
            if let Some(slot) = self.slots.get_mut(*table) {
                slot.button = Some(ButtonSpan {
                    row,
                    x: u16::try_from(x).unwrap_or(u16::MAX),
                    w: u16::try_from(label_width).unwrap_or(u16::MAX),
                });
            }
        }
        for slot in &mut self.slots {
            if let Some(chart) = slot.chart.as_mut() {
                chart.row += kept
                    .iter()
                    .take_while(|(table, _, _)| *table < slot.table)
                    .count();
            }
        }
        Rendered {
            lines: self.out,
            tables: self.slots,
            raw: self.raw,
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

    /// 代码块结束：syntect 高亮并整体铺 code_bg。
    fn flush_code(&mut self) {
        let info = self.code_info.take().unwrap_or_default();
        let code = std::mem::take(&mut self.code_buf);
        if code.trim().is_empty() && info.is_empty() {
            return;
        }
        let highlighted = highlight::highlight(&info, &code);
        let bg_style = Style::new().bg(self.theme.code_bg);
        let code_width = self.width.saturating_sub(2).max(8);
        if !info.trim().is_empty() {
            let tag = info.trim();
            let mut top_line = vec![
                Span::styled("  ", bg_style),
                Span::styled(
                    tag.to_owned(),
                    Style::new()
                        .fg(self.theme.accent_tool)
                        .bg(self.theme.code_bg)
                        .add_modifier(Modifier::BOLD),
                ),
            ];
            let used = 2 + str_width(tag);
            if used < code_width {
                top_line.push(Span::styled(" ".repeat(code_width - used), bg_style));
            }
            self.out.push(Line::from(top_line));
        }
        for spans in &highlighted.lines {
            let mut line: Vec<Span<'static>> = vec![Span::styled("  ", bg_style)];
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
        self.out.push(Line::from(String::new()));
    }

    /// 表格：内容宽度定列宽（整体不超过可用宽度），画全边框盒。
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
        let mut widths = vec![0usize; column_count];
        let measure = |cells: &[Vec<Run>], widths: &mut [usize]| {
            for (index, cell) in cells.iter().enumerate() {
                let text: String = cell.iter().map(|run| run.text.as_str()).collect();
                widths[index] = widths[index].max(str_width(&text));
            }
        };
        measure(&table.header, &mut widths);
        for row in &table.rows {
            measure(row, &mut widths);
        }
        // 整体超宽时按比例收缩；边框占用的列数必须从总宽度中完整扣除。
        let border_width = column_count.saturating_mul(5).saturating_add(1);
        let available = self.width.saturating_sub(border_width);
        let total: usize = widths.iter().sum();
        if total > available {
            let original = widths.clone();
            let minimum = usize::from(available >= column_count);
            let remaining = available.saturating_sub(minimum * column_count);
            for (index, width) in original.iter().enumerate() {
                widths[index] = minimum + remaining.saturating_mul(*width) / total.max(1);
            }
            let assigned: usize = widths.iter().sum();
            for index in 0..available.saturating_sub(assigned) {
                widths[index % column_count] += 1;
            }
        }
        let truncate = |text: &str, max: usize| -> String {
            let full_width = str_width(text);
            if full_width <= max {
                return text.to_owned();
            }
            if max == 0 {
                return String::new();
            }
            let mut out = String::new();
            let mut used = 0usize;
            for ch in text.chars() {
                let ch_width = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
                if used + ch_width + 1 > max {
                    break;
                }
                out.push(ch);
                used += ch_width;
            }
            out.push('…');
            out
        };
        let cell_text = |cells: &[Vec<Run>], index: usize| -> String {
            cells
                .get(index)
                .map(|cell| cell.iter().map(|run| run.text.as_str()).collect())
                .unwrap_or_default()
        };
        let render_cell = |text: &str, width: usize, bold: bool| -> Vec<Span<'static>> {
            let shown = truncate(text, width);
            let shown_width = str_width(&shown);
            let mut style = Style::new().fg(self.theme.text_primary);
            if bold {
                style = style.add_modifier(Modifier::BOLD);
            }
            vec![
                Span::styled("  ".to_owned(), style),
                Span::styled(shown, style),
                Span::styled(" ".repeat(width.saturating_sub(shown_width) + 2), style),
            ]
        };
        let render_row = |cells: &[Vec<Run>], bold: bool| -> Line<'static> {
            let mut spans: Vec<Span<'static>> = Vec::new();
            for (index, width) in widths.iter().enumerate() {
                spans.push(Span::styled(
                    "│".to_owned(),
                    Style::new().fg(self.theme.border_subtle),
                ));
                spans.extend(render_cell(&cell_text(cells, index), *width, bold));
            }
            spans.push(Span::styled(
                "│".to_owned(),
                Style::new().fg(self.theme.border_subtle),
            ));
            Line::from(spans)
        };
        let border_line = |position: &str| -> Line<'static> {
            let mut spans = Vec::new();
            for (index, width) in widths.iter().enumerate() {
                let (head, cross) = match position {
                    "top" => ("┌", "┬"),
                    "mid" => ("├", "┼"),
                    _ => ("└", "┴"),
                };
                spans.push(Span::styled(
                    format!(
                        "{}{}",
                        if index == 0 { head } else { cross },
                        "─".repeat(width + 4)
                    ),
                    Style::new().fg(self.theme.border_subtle),
                ));
            }
            let tail = match position {
                "top" => "┐",
                "mid" => "┤",
                _ => "┘",
            };
            spans.push(Span::styled(
                tail.to_owned(),
                Style::new().fg(self.theme.border_subtle),
            ));
            Line::from(spans)
        };
        self.out.push(border_line("top"));
        self.out.push(render_row(&table.header, true));
        self.out.push(border_line("mid"));
        for row in &table.rows {
            self.out.push(render_row(row, false));
        }
        self.out.push(border_line("bottom"));
    }
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

    fn render(source: &str, width: usize, views: TableViews<'_>) -> Rendered {
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
            TableViews {
                charts: &[],
                graphics: None,
                streaming: false,
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
    fn paragraphs_wrap_at_width() {
        // 宽 12：每行 6 个宽字符；宽度低于 8 时有保底
        let out = lines("一二三四五六七八", 12);
        assert_eq!(out.join("|"), "一二三四五六|七八", "实际输出：{out:?}");
    }

    #[test]
    fn headings_and_paragraphs_keep_visual_separation() {
        let out = lines("# 标题\n\n第一段\n\n第二段", 40);
        let heading = out.iter().position(|line| line.contains("标题")).unwrap();
        let first = out.iter().position(|line| line == "第一段").unwrap();
        let second = out.iter().position(|line| line == "第二段").unwrap();
        assert_eq!(first, heading + 2, "标题与正文之间应留出一行：{out:?}");
        assert_eq!(second, first + 2, "相邻段落应留出一行：{out:?}");
    }

    #[test]
    fn lists_are_indented_and_bulleted() {
        let out = lines("- 甲\n- 乙\n  - 丙", 40);
        assert!(out.iter().any(|line| line.contains("• 甲")), "{out:?}");
        assert!(out.iter().any(|line| line.contains("• 乙")), "{out:?}");
        assert!(
            out.iter()
                .any(|line| line.starts_with("  ") && line.contains("• 丙")),
            "{out:?}"
        );
    }

    #[test]
    fn fenced_code_blocks_render_via_highlighter() {
        let out = lines("```rust\nfn main() {}\n```", 40);
        // 代码内容保留（高亮可能把标识符切成多段）
        let joined = out.join("");
        assert!(joined.contains("fn"), "{out:?}");
        assert!(joined.contains("main"), "{out:?}");
    }

    /// 缩进代码块同样逐行渲染：ASCII 示意图不能挤成一行，行内也不许带换行。
    #[test]
    fn indented_code_blocks_keep_one_line_per_source_line() {
        let out = lines("    ╭──────╮\n    │ 对齐 │\n    ╰──────╯", 40);
        for (index, glyph) in ["╭──────╮", "│ 对齐 │", "╰──────╯"].iter().enumerate()
        {
            assert!(out[index].contains(glyph), "第 {index} 行：{out:?}");
        }
        assert!(
            out.iter().all(|line| !line.contains('\n')),
            "渲染行内不得残留换行：{out:?}"
        );
    }

    /// HTML 块整块送文本，也必须按行拆开而不是塞进同一行。
    #[test]
    fn html_blocks_split_into_lines() {
        let out = lines("<div>\nfoo\n</div>\n\n正文", 40);
        assert!(
            out.iter().all(|line| !line.contains('\n')),
            "渲染行内不得残留换行：{out:?}"
        );
        assert!(out.iter().any(|line| line.contains("正文")), "{out:?}");
        assert!(out.iter().any(|line| line.contains("foo")), "{out:?}");
    }

    #[test]
    fn tables_render_box_borders_and_keep_cell_content_in_order() {
        let single = lines("| 尝试 | 结果 |\n|---|---|\n| a | b |", 40);
        let joined = single.join("\n");
        assert!(joined.contains('┌') && joined.contains('┐'), "{single:?}");
        assert!(joined.contains('├') && joined.contains('┼'), "{single:?}");
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

    #[test]
    fn inline_code_and_bold_styles_merge_runs() {
        let out = lines("普通 **加粗** `code`", 60);
        let joined = out.join("");
        assert!(
            joined.contains("普通") && joined.contains("加粗") && joined.contains("code"),
            "{out:?}"
        );
    }

    #[test]
    fn soft_breaks_and_inline_code_padding_render_inline() {
        assert_eq!(
            lines("工具用途\n`bash`执行 shell 命令", 40),
            vec!["工具用途", " bash 执行 shell 命令"]
        );
        assert_eq!(lines("前缀`bash`后缀", 40), vec!["前缀 bash 后缀"]);
    }

    /// 入口只在支持图形协议时出现；按钮右对齐且命中区间等于文字宽度。
    #[test]
    fn table_entry_appears_only_with_graphics_support() {
        let with = render(
            NUMERIC_TABLE,
            80,
            TableViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
            },
        );
        assert_eq!(with.tables.len(), 1, "一张顶层表格");
        let slot = with.tables[0];
        let button = slot.button.expect("支持图形时应有入口");
        let width = str_width(ENTRY_CHART);
        assert_eq!(button.w as usize, width);
        assert_eq!(button.x as usize, 80 - width, "右对齐");
        assert_eq!(
            text(&with.lines)[button.row],
            format!("{}▸ 可视化图表", " ".repeat(80 - width)),
        );

        let without = render(
            NUMERIC_TABLE,
            80,
            TableViews {
                charts: &[],
                graphics: None,
                streaming: false,
            },
        );
        assert!(without.tables[0].button.is_none(), "无图形能力时不出入口");
        assert!(
            text(&without.lines).iter().any(|line| line.contains('┌')),
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
            TableViews {
                charts: &[],
                graphics: graphics(),
                streaming: true,
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
            TableViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
            },
        );
        assert!(settled.tables[1].button.is_some(), "收尾后入口出现");
    }

    /// 已切换成图表：表格被占位行取代，入口变成还原，占位区落在按钮之前。
    #[test]
    fn chart_state_replaces_table_with_placeholder_rows() {
        let state = chart_state(NUMERIC_TABLE, 0, 7);
        let charts = [Some(state)];
        let rendered = render(
            NUMERIC_TABLE,
            80,
            TableViews {
                charts: &charts,
                graphics: graphics(),
                streaming: false,
            },
        );
        let out = text(&rendered.lines);
        assert!(
            !out.iter().any(|line| line.contains('┌')),
            "不应再画表格边框：{out:?}"
        );
        let slot = rendered.tables[0];
        let chart = slot.chart.expect("应有图表占位区");
        let button = slot.button.expect("应有还原入口");
        assert_eq!(chart.cols, 80, "占满正文宽度");
        assert_eq!(chart.px, (80 * 8, u32::from(chart.rows) * 18));
        assert_eq!(
            button.row,
            chart.row + usize::from(chart.rows),
            "按钮紧跟占位区"
        );
        assert!(
            out[usize::from(chart.rows)].contains("▾ 还原表格"),
            "占位区之后是还原入口：{out:?}"
        );
        assert!(
            out[chart.row..chart.row + usize::from(chart.rows)]
                .iter()
                .all(|line| line.trim().is_empty()),
            "占位行必须是空白"
        );
    }

    /// 纯文本快照与屏幕上的单元格一致：装饰标记不会漏进分析（`**7648**` → `7648`）。
    #[test]
    fn raw_snapshot_matches_visible_cells() {
        let source = "| 项 | 值 |\n|---|---|\n| **a** | `7648` |\n| [b](https://x) | 12 |";
        let rendered = render(
            source,
            80,
            TableViews {
                charts: &[],
                graphics: None,
                streaming: false,
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
            TableViews {
                charts: &[],
                graphics: graphics(),
                streaming: false,
            },
        );
        assert_eq!(rendered.raw.len(), 1, "只有顶层表格进入快照");
        assert_eq!(rendered.tables.len(), 1);
        assert_eq!(rendered.tables[0].table, 0, "顶层序号从 0 起");
        assert!(rendered.tables[0].button.is_some());
    }
}

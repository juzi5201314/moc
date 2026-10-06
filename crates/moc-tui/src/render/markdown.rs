//! Markdown → 终端行渲染（pulldown-cmark 事件流直接驱动）。
//!
//! inline 内容在缓冲区里以「片段序列（文本 + 已合成样式）」积累，段落 /
//! 标题结束时统一按显示宽度换行产出（续行悬挂缩进）；代码块整块交给
//! syntect 高亮；表格按内容宽度画全边框盒。样式全部来自主题槽位。

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

use super::highlight;
use super::wrap::str_width;
use crate::theme::Theme;

/// 渲染 markdown 文本为终端行。
pub fn render_markdown(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut renderer = MdRenderer {
        theme,
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
        table: None,
        table_cell: Vec::new(),
        in_table_head: false,
        in_table_cell: false,
    };
    let options =
        Options::ENABLE_TABLES | Options::ENABLE_STRIKETHROUGH | Options::ENABLE_TASKLISTS;
    for event in Parser::new_ext(source, options) {
        renderer.handle(event);
    }
    // 流式渲染时未闭合的段落兜底 flush
    renderer.flush_inline();
    renderer.out
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
    table: Option<TableAcc>,
    table_cell: Vec<Run>,
    in_table_head: bool,
    in_table_cell: bool,
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
                self.push_run(Run {
                    style: Style::new().fg(self.theme.gray_dim),
                    text: html.to_string(),
                });
            }
        }
    }

    fn start(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.flush_inline();
                if self.list_stack.is_empty() && self.quote_depth == 0 {
                    self.ensure_block_gap();
                }
            }
            Tag::Heading { level, .. } => {
                self.flush_inline();
                self.ensure_block_gap();
                self.heading = Some(level);
            }
            Tag::BlockQuote(_) => {
                self.flush_inline();
                if self.quote_depth == 0 {
                    self.ensure_block_gap();
                }
                self.quote_depth += 1;
            }
            Tag::CodeBlock(kind) => {
                self.flush_inline();
                self.ensure_block_gap();
                self.code_info = match kind {
                    CodeBlockKind::Fenced(info) => Some(info.to_string()),
                    CodeBlockKind::Indented => None,
                };
                self.code_buf.clear();
            }
            Tag::List(start) => {
                self.flush_inline();
                if self.list_stack.is_empty() {
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
            TagEnd::CodeBlock => self.flush_code(),
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
                    self.render_table(table);
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
        if self.code_info.is_some() {
            self.code_buf.push_str(text);
            return;
        }
        self.push_run(Run {
            style: self.current_style(),
            text: text.to_owned(),
        });
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme::{ColorLevel, Theme};

    fn lines(source: &str, width: usize) -> Vec<String> {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        render_markdown(source, width, &theme)
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.to_string())
                    .collect::<String>()
            })
            .collect()
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

    #[test]
    fn tables_get_box_borders() {
        let out = lines("| 尝试 | 结果 |\n|---|---|\n| a | b |", 40);
        let joined = out.join("\n");
        assert!(joined.contains('┌') && joined.contains('┐'), "{out:?}");
        assert!(joined.contains('├') && joined.contains('┼'), "{out:?}");
        assert!(out.iter().any(|line| line.contains("尝试")), "{out:?}");
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
    fn tables_keep_cell_content_and_row_boundaries() {
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
    fn soft_breaks_remain_visible_line_breaks() {
        let out = lines("工具用途\n`bash`执行 shell 命令", 40);
        assert_eq!(out, vec!["工具用途", " bash 执行 shell 命令"]);
    }

    #[test]
    fn inline_code_has_readable_padding() {
        let out = lines("前缀`bash`后缀", 40);
        assert_eq!(out, vec!["前缀 bash 后缀"]);
    }
}

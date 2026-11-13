use super::super::wrap::str_width;
use super::ImageAcc;
use super::ListFrame;
use super::MdRenderer;
use super::Run;
use super::TableAcc;
use super::needs_code_gap;
use pulldown_cmark::CodeBlockKind;
use pulldown_cmark::Event;
use pulldown_cmark::HeadingLevel;
use pulldown_cmark::Tag;
use pulldown_cmark::TagEnd;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

impl<'a> MdRenderer<'a> {
    pub(super) fn handle(&mut self, event: Event<'_>) {
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

    pub(super) fn start(&mut self, tag: Tag<'_>) {
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

    pub(super) fn end(&mut self, tag: TagEnd) {
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

    pub(super) fn text(&mut self, text: &str) {
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
    pub(super) fn code(&mut self, text: &str) {
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
}

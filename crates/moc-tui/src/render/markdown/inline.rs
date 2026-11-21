use super::super::highlight;
use super::super::wrap::str_width;
use super::CODE_CONTINUATION;
use super::MdRenderer;
use super::Run;
use super::code_language;
use super::needs_code_gap;
use pulldown_cmark::HeadingLevel;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;

impl<'a> MdRenderer<'a> {
    /// 逐行压入 inline 片段：事件可能整块携带换行（HTML 块等），换行
    /// 按软换行结算，保证输出行内不留 `\n`——换行与宽度计算都假设
    /// 「一行即一行」。
    pub(super) fn push_inline_text(&mut self, text: &str, style: Style) {
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
    pub(super) fn current_style(&self) -> Style {
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

    pub(super) fn push_run(&mut self, run: Run) {
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
    pub(super) fn block_boundary(&mut self) {
        if !self.buttons.is_empty() {
            self.tails = true;
        }
    }

    /// 正文可用列数（转写层已扣掉正文缩进）。
    pub(super) fn cols(&self) -> u16 {
        u16::try_from(self.width).unwrap_or(u16::MAX)
    }

    /// 块级内容之间保留一行视觉间距，避免标题和段落连成一块。
    pub(super) fn ensure_block_gap(&mut self) {
        if self.out.last().is_some_and(|line| line.width() > 0) {
            self.out.push(Line::from(String::new()));
        }
    }

    /// 段落 / 标题 / 列表项结束：inline 缓冲按宽度折行后产出。
    ///
    /// 折行走 [`wrap_spans`]（空格 / CJK 字间 / 标点后断行、避头尾、续行丢行首空格）。
    /// 每行先画引用竖线；首行接列表标记或标题符号，续行用等宽空格悬挂到正文起点。
    pub(super) fn flush_inline(&mut self) {
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
        super::super::wrap::wrap_spans_into(&spans, budget, |row| {
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
    pub(super) fn open_item_block(&mut self) {
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
    pub(super) fn content_indent(&self) -> usize {
        self.list_stack.last().map_or(0, |frame| frame.content)
    }

    /// 引用竖线（每层 `│ `，2 列）。
    pub(super) fn quote_span(&self) -> Span<'static> {
        Span::styled(
            "│ ".repeat(self.quote_depth),
            Style::new().fg(self.theme.border),
        )
    }

    /// 当前 inline 缓冲（或单元格缓冲）的最后一个字符。
    pub(super) fn last_inline_char(&self) -> Option<char> {
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
    pub(super) fn flush_code(&mut self) {
        let info = self.code_info.take().unwrap_or_default();
        // 制表符按 4 列展开：宽度计算把 `\t` 记作 0 列，不展开背景带就会错位。
        let code = std::mem::take(&mut self.code_buf).replace('\t', "    ");

        if self.place_mermaid(&info, &code)
            || self.place_svg(&info, &code)
            || self.place_widget(&info, &code)
        {
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

        if code.is_empty() {
            self.push_code_row(indent, band, pad.clone(), Vec::new());
        }
        let mark = Span::styled(
            CODE_CONTINUATION,
            Style::new().fg(self.theme.gray_dim).bg(bg),
        );
        for code_line in syntect::util::LinesWithEndings::from(code.as_str()) {
            let spans: Vec<Span<'static>> = highlighter
                .line(code_line)
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
            super::super::wrap::wrap_spans_into(&spans, body, |row| {
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
    pub(super) fn push_code_row(
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
}

use super::ChartPlacement;
use super::MdRenderer;
use super::TableAcc;
use super::TableSlot;
use super::tables::raw_table;
use super::tables::stacked_table_lines;
use super::tables::table_metrics;
use super::tables::table_row_lines;
use super::tables::table_rule;
use super::tables::table_widths;
use moc_core::chart;
use ratatui::text::Line;

impl<'a> MdRenderer<'a> {
    /// 表格收尾：表格始终保留；展开图表时在表格下方追加占位区与入口。
    pub(super) fn finish_table(&mut self, table: TableAcc) {
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
        let cols = self.cols();
        let offers = charted.is_some()
            || self.views.graphics.is_some_and(|graphics| {
                cols >= crate::render::chart::MIN_COLS
                    && crate::render::graphics::cell_supported(graphics.cell)
                    && {
                        let analysis = chart::analyze(&raw);
                        chart::plan(&analysis).is_some_and(|plan| {
                            crate::render::chart::layout_for(
                                plan.kind,
                                analysis.rows.len(),
                                plan.series.len(),
                                cols,
                                graphics.cell,
                            )
                            .is_some()
                        })
                    }
            });
        if offers {
            // 用户可能马上点入口：后台先把字体解析好，别让第一张图卡在解析上
            crate::render::chart::warm_fonts();
            self.buttons.push((index, button_row, charted.is_some()));
        }
        self.raw.push(raw);
    }

    /// 表格：内容定列宽，单元格按列折行；表头与行之间用细线分隔。
    ///
    /// 超宽时优先把列压到可读下限再折行；连两格一列都放不下时，退化为
    /// 逐字段列表，保证任何宽度下都不丢单元格内容。
    pub(super) fn render_table(&mut self, table: TableAcc) {
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

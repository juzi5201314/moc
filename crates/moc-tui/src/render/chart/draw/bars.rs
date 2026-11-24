use moc_i18n::t;

use super::GAP;
use super::PAD_X;
use super::Painter;
use super::SWATCH;
use super::ValueScale;
use super::bar_height;
use super::bar_inset;
use super::format_value;
use super::max_point;
use super::mid_point;
use super::min_point;
use super::paired_deltas;
use super::point;
use super::progress_label;
use super::progress_ratio;
use moc_core::chart::Point;
use moc_core::chart::Polarity;
use moc_core::chart::Series;

/// 分面小面板两列之间的列间距（像素）。
const PANEL_COL_GAP: u32 = 28;
/// 分面小面板零值以上小条的最小可见胶囊宽度（像素），避免极小正数缩成看不见的竖线。
const PANEL_MIN_BAR_W: u32 = 6;

impl Painter<'_> {
    /// 分别统计负值与非负值文本的最大像素宽（负值标签画在条左端之外，需在轨道左侧留宽）。
    fn split_value_widths(
        &mut self,
        series: &[Series],
        scale: ValueScale,
        max_side: u32,
    ) -> (u32, u32) {
        let mut neg = Vec::new();
        let mut pos = Vec::new();
        for item in series {
            for value in item.points.iter().flatten() {
                if value.value < 0.0 {
                    neg.push(value.text.clone());
                } else {
                    pos.push(value.text.clone());
                }
            }
        }
        let left_w = if scale.has_negative() {
            self.widest(&neg).min(max_side)
        } else {
            0
        };
        let right_w = self.widest(&pos).min(max_side);
        (left_w, right_w)
    }

    /// 计算带可选负侧标签区与右侧余量的横向轨道区间。
    fn bar_track(&self, label_w: u32, left_w: u32, right_pad: u32) -> (u32, u32) {
        let left = PAD_X + label_w + GAP + if left_w > 0 { left_w + GAP } else { 0 };
        let right = self.w().saturating_sub(PAD_X + right_pad).max(left + 1);
        (left, right)
    }

    /// 组装条形 / 分组条 / 成对条共用的轴刻度文本。
    fn bar_axis_labels(
        &self,
        scale: ValueScale,
        series: &[Series],
        reference: &Series,
    ) -> (String, String, String) {
        if scale.is_log() {
            return scale.log_labels(reference);
        }
        let min_text = if scale.has_negative() {
            min_point(series).map_or_else(String::new, |point| point.text.clone())
        } else {
            String::new()
        };
        let mid_text = mid_point(series).map_or_else(String::new, |point| point.text.clone());
        let max_text = if scale.has_positive() {
            max_point(series).map_or_else(String::new, |point| point.text.clone())
        } else {
            String::new()
        };
        (min_text, mid_text, max_text)
    }

    /// 条形图：单系列，每类别一行。
    pub(super) fn bar(&mut self) {
        let spec = self.spec;
        let Some(series) = spec.series.first() else {
            return;
        };
        let scale = ValueScale::new(&spec.series);
        let label_w = self.label_width();
        let (left_w, right_w) = self.split_value_widths(&spec.series, scale, self.w() / 4);
        let right_pad = if right_w > 0 { right_w + GAP } else { 0 };
        let track = self.bar_track(label_w, left_w, right_pad);
        let origin = scale.origin(track);
        let legend_end = self.legend();
        self.scale_note(legend_end, scale);
        let color = self.series_color(0);
        let gray = self.palette.gray_dim;
        if (spec.diverging || (scale.has_negative() && scale.has_positive())) && !scale.is_log() {
            let bottom = self.row_top(u16::try_from(spec.categories.len()).unwrap_or(0) + 1);
            self.vline(origin, self.row_top(1), bottom, self.palette.border_subtle);
        }
        for (row, category) in spec.categories.iter().enumerate() {
            let line = u16::try_from(row + 1).unwrap_or(u16::MAX);
            let shown = self.fit(category, label_w);
            self.text(line, PAD_X, &shown, self.palette.text_primary);
            let Some(value) = point(series, row) else {
                self.text(line, track.0, "—", gray);
                continue;
            };
            self.bar_row(line, Some(value), track, scale, origin, left_w, color);
        }
        let axis_row = u16::try_from(spec.categories.len() + 1).unwrap_or(u16::MAX);
        let (min_text, mid_text, max_text) = self.bar_axis_labels(scale, &spec.series, series);
        self.value_axis(axis_row, track, scale, &min_text, &mid_text, &max_text);
    }

    /// 分组条形图：每类别占系列数行，行内只画圆角胶囊条与数值。
    pub(super) fn grouped(&mut self) {
        let spec = self.spec;
        let scale = ValueScale::new(&spec.series);
        let label_w = self.label_width();
        let (left_w, right_w) = self.split_value_widths(&spec.series, scale, self.w() / 4);
        let right_pad = if right_w > 0 { right_w + GAP } else { 0 };
        let track = self.bar_track(label_w, left_w, right_pad);
        let origin = scale.origin(track);
        let legend_end = self.legend();
        self.scale_note(legend_end, scale);
        let mut row = 1usize;
        for category in 0..spec.categories.len() {
            for (index, series) in spec.series.iter().enumerate() {
                let line = u16::try_from(row).unwrap_or(u16::MAX);
                if index == 0 {
                    let shown = self.fit(&spec.categories[category].clone(), label_w);
                    self.text(line, PAD_X, &shown, self.palette.text_primary);
                }
                self.bar_row(
                    line,
                    point(series, category),
                    track,
                    scale,
                    origin,
                    left_w,
                    self.series_color(index),
                );
                row += 1;
            }
        }
        let axis_row = u16::try_from(row).unwrap_or(u16::MAX);
        let reference = spec.series.first().expect("分组图至少一个系列");
        let (min_text, mid_text, max_text) = self.bar_axis_labels(scale, &spec.series, reference);
        self.value_axis(axis_row, track, scale, &min_text, &mid_text, &max_text);
    }

    /// 成对条形图：每类别两行（前灰后彩），行尾标差值与变化率。
    pub(super) fn paired(&mut self) {
        let spec = self.spec;
        let [before, after, ..] = spec.series.as_slice() else {
            return;
        };
        let scale = ValueScale::new(&spec.series);
        let label_w = self.label_width();
        let (left_w, right_w) = self.split_value_widths(&spec.series, scale, self.w() / 5);
        let deltas = paired_deltas(&spec.series);
        let delta_w = self.widest(&deltas).min(self.w() / 4);
        let right_pad = if right_w > 0 { right_w + GAP } else { 0 }
            + if delta_w > 0 { delta_w + GAP } else { 0 };
        let track = self.bar_track(label_w, left_w, right_pad);
        let origin = scale.origin(track);
        let legend_end = self.legend();
        self.scale_note(legend_end, scale);
        let polarity = after.polarity;
        let mut row = 1usize;
        for (category, label) in spec.categories.iter().enumerate() {
            let first = u16::try_from(row).unwrap_or(u16::MAX);
            let second = u16::try_from(row + 1).unwrap_or(u16::MAX);
            let shown = self.fit(label, label_w);
            self.text(first, PAD_X, &shown, self.palette.text_primary);
            let gray = self.palette.text_secondary;
            self.bar_row(
                first,
                point(before, category),
                track,
                scale,
                origin,
                left_w,
                gray,
            );
            let accent = self.series_color(1);
            self.bar_row(
                second,
                point(after, category),
                track,
                scale,
                origin,
                left_w,
                accent,
            );
            if let Some(delta) = deltas.get(category).filter(|delta| !delta.is_empty()) {
                let color = self.delta_color(category, polarity);
                self.text_end(second, self.w().saturating_sub(PAD_X), delta, color);
            }
            row += 2;
        }
        let axis_row = u16::try_from(row).unwrap_or(u16::MAX);
        let (min_text, mid_text, max_text) = self.bar_axis_labels(scale, &spec.series, after);
        self.value_axis(axis_row, track, scale, &min_text, &mid_text, &max_text);
    }

    /// 分面小面板：类别图例换行，窄图改为单列；各指标独立线性定标。
    pub(super) fn panels(&mut self) {
        let spec = self.spec;
        if spec.series.is_empty() || spec.categories.is_empty() {
            return;
        }
        let columns = super::super::panel_columns(self.w());
        let top = self.panel_legend(columns);
        let inner_w = self.w().saturating_sub(2 * PAD_X);
        let col_gap = if columns == 1 {
            0
        } else {
            PANEL_COL_GAP.min(inner_w / 6)
        };
        let panel_w =
            inner_w.saturating_sub(col_gap) / u32::try_from(columns).expect("面板列数为 1 或 2");
        let stride = spec.categories.len() + 2;
        for (idx, series) in spec.series.iter().enumerate() {
            let grid_row = idx / columns;
            let grid_col = idx % columns;
            let x0 =
                PAD_X + u32::try_from(grid_col).expect("面板列号最多为 1") * (panel_w + col_gap);
            let top_row = top + u16::try_from(grid_row * stride).expect("布局已限制总行数");
            self.draw_panel(x0, panel_w, top_row, series);
        }
    }

    /// 类别图例优先占位；编号与条行一致，长名称截断后仍可区分。
    fn panel_legend(&mut self, columns: usize) -> u16 {
        let spec = self.spec;
        let cell_w =
            self.w().saturating_sub(2 * PAD_X) / u32::try_from(columns).expect("图例列数为 1 或 2");
        for (index, name) in spec.categories.iter().enumerate() {
            let row = u16::try_from(index / columns).expect("类别数受规划层限制");
            let x = PAD_X + u32::try_from(index % columns).expect("图例列号最多为 1") * cell_w;
            let label = format!("{} {name}", index + 1);
            let shown = self.fit(&label, cell_w.saturating_sub(SWATCH + 2 * GAP));
            self.dot(x, row, self.series_color(index));
            self.text(row, x + SWATCH + GAP, &shown, self.palette.text_secondary);
        }
        let note_row =
            u16::try_from(spec.categories.len().div_ceil(columns)).expect("类别数受规划层限制");
        let note = self.fit(
            &t!("tui.chart.scaled_per_panel"),
            self.w().saturating_sub(2 * PAD_X),
        );
        self.text(note_row, PAD_X, &note, self.palette.gray_dim);
        note_row + 1
    }

    /// 单个分面小面板：顶行系列名（含单位），下方每类别一条圆角胶囊 + 数值。
    fn draw_panel(&mut self, x0: u32, panel_w: u32, top_row: u16, series: &Series) {
        let title = self.fit(&series.name, panel_w);
        self.text(top_row, x0, &title, self.palette.text_primary);

        let slice = std::slice::from_ref(series);
        let scale = ValueScale::linear(slice);
        let number_w = self.text_width(&self.spec.categories.len().to_string()) + GAP;
        let track_area = panel_w.saturating_sub(number_w);
        let (left_w, right_w) =
            self.split_value_widths(slice, scale, track_area.saturating_sub(3 * GAP + 2) / 2);
        let track_left = x0 + number_w + if left_w > 0 { left_w + GAP } else { 0 };
        let right_pad = if right_w > 0 { right_w + GAP + 2 } else { 0 };
        let track_right = (x0 + panel_w).saturating_sub(right_pad).max(track_left + 1);
        let track = (track_left, track_right);
        let origin = scale.origin(track);

        if scale.has_negative() && scale.has_positive() {
            let bottom =
                self.row_top(top_row + 1 + u16::try_from(self.spec.categories.len()).unwrap_or(0));
            self.vline(
                origin,
                self.row_top(top_row + 1),
                bottom,
                self.palette.border_subtle,
            );
        }

        let bar_h = bar_height(self.cell_h());
        let inset = bar_inset(self.cell_h());
        for cat_idx in 0..self.spec.categories.len() {
            let row = top_row + 1 + u16::try_from(cat_idx).unwrap_or(0);
            self.text(
                row,
                x0,
                &(cat_idx + 1).to_string(),
                self.series_color(cat_idx),
            );
            let Some(val) = point(series, cat_idx).filter(|point| point.value.is_finite()) else {
                self.text(row, track.0, "—", self.palette.gray_dim);
                continue;
            };
            let color = self.series_color(cat_idx);
            let mut x = scale.position(track, val.value);
            if val.value > 0.0 && x.saturating_sub(origin) < PANEL_MIN_BAR_W {
                x = (origin + PANEL_MIN_BAR_W).min(track.1);
            } else if val.value < 0.0 && origin.saturating_sub(x) < PANEL_MIN_BAR_W {
                x = origin.saturating_sub(PANEL_MIN_BAR_W).max(track.0);
            }
            let bar_y = self.row_top(row) + inset;
            let (left, right) = if x >= origin {
                (origin, x)
            } else {
                (x, origin)
            };
            // 负侧不足一个像素时不反向补条；保留负号数值标签表达该数据。
            let width = if val.value == 0.0 { 2 } else { right - left };
            self.pill(left, bar_y, width, bar_h, color);
            if val.value >= 0.0 {
                let text_x = left + width + GAP;
                let max_text = (x0 + panel_w).saturating_sub(text_x);
                let shown = self.fit(&val.text, max_text.max(1));
                self.text(row, text_x, &shown, self.palette.text_secondary);
            } else {
                let shown = if left_w > 0 {
                    self.fit(&val.text, left_w)
                } else {
                    val.text.clone()
                };
                self.text_end(
                    row,
                    x.saturating_sub(GAP),
                    &shown,
                    self.palette.text_secondary,
                );
            }
        }
    }

    /// 一行条形（条形 / 分组 / 成对共用；负值标签按预留左侧宽度截断，避免压入类别列）。
    #[expect(
        clippy::too_many_arguments,
        reason = "内部行绘制复用，参数与轨道几何一一对应"
    )]
    pub(super) fn bar_row(
        &mut self,
        row: u16,
        point: Option<&Point>,
        track: (u32, u32),
        scale: ValueScale,
        origin: u32,
        left_w: u32,
        color: [u8; 3],
    ) {
        let Some(value) = point.filter(|point| point.value.is_finite()) else {
            return;
        };
        let x = scale.position(track, value.value);
        let bar_y = self.row_top(row) + bar_inset(self.cell_h());
        let (left, width) = if value.value == 0.0 {
            (origin, 2)
        } else if value.value > 0.0 {
            (origin, x.saturating_sub(origin).max(2))
        } else {
            // 负侧不足一个像素时不反向补条；有余量时向左保底 2px。
            let left = x.min(origin.saturating_sub(2)).max(track.0);
            (left, origin.saturating_sub(left))
        };
        self.pill(left, bar_y, width, bar_height(self.cell_h()), color);
        if value.value >= 0.0 {
            self.text(
                row,
                left + width + GAP,
                &value.text,
                self.palette.text_secondary,
            );
        } else {
            let shown = if left_w > 0 {
                self.fit(&value.text, left_w)
            } else {
                value.text.clone()
            };
            self.text_end(
                row,
                left.saturating_sub(GAP),
                &shown,
                self.palette.text_secondary,
            );
        }
    }

    /// 差值颜色：按极性判定「变好 / 变差」，无极性时用中性灰。
    pub(super) fn delta_color(&mut self, row: usize, polarity: Option<Polarity>) -> [u8; 3] {
        let spec = self.spec;
        let (Some(before), Some(after)) = (spec.series.first(), spec.series.get(1)) else {
            return self.palette.text_secondary;
        };
        let (Some(first), Some(second)) = (point(before, row), point(after, row)) else {
            return self.palette.text_secondary;
        };
        let delta = second.value - first.value;
        if delta.abs() < f64::EPSILON {
            return self.palette.text_secondary;
        }
        match polarity {
            Some(Polarity::Lower) if delta < 0.0 => self.palette.accent_success,
            Some(Polarity::Lower) => self.palette.accent_error,
            Some(Polarity::Higher) if delta > 0.0 => self.palette.accent_success,
            Some(Polarity::Higher) => self.palette.accent_error,
            None => self.palette.text_secondary,
        }
    }

    /// 堆叠条形图：类别一行，段按系列顺序无缝拼成胶囊条；归一化时条长恒为满格。
    pub(super) fn stacked(&mut self) {
        let spec = self.spec;
        let Some(reference) = spec.series.first() else {
            return;
        };
        self.legend();
        let label_w = self.label_width();
        let totals: Vec<f64> = (0..spec.categories.len())
            .map(|row| {
                spec.series
                    .iter()
                    .filter_map(|series| point(series, row).map(|point| point.value.max(0.0)))
                    .sum()
            })
            .collect();
        let max_total = totals.iter().copied().fold(0.0f64, f64::max);
        let total_labels: Vec<String> = totals
            .iter()
            .map(|total| format_value(*total, reference.factor, &reference.unit))
            .collect();
        let total_w = if spec.normalized {
            0
        } else {
            self.widest(&total_labels).min(self.w() / 4)
        };
        let track = (
            PAD_X + label_w + GAP,
            self.w()
                .saturating_sub(PAD_X + total_w + GAP)
                .max(PAD_X + label_w + GAP + 1),
        );
        let length = track.1 - track.0;
        let bar_h = bar_height(self.cell_h());
        let inset = bar_inset(self.cell_h());
        for row in 0..spec.categories.len() {
            let line = u16::try_from(row + 1).unwrap_or(u16::MAX);
            let shown = self.fit(&spec.categories[row].clone(), label_w);
            self.text(line, PAD_X, &shown, self.palette.text_primary);
            let total = totals[row];
            let bar_y = self.row_top(line) + inset;
            if total <= 0.0 || max_total <= 0.0 {
                continue;
            }
            let target_len = if spec.normalized {
                length
            } else {
                ((total / max_total) * f64::from(length)).round().max(1.0) as u32
            }
            .min(length);
            // 段宽先各自取整，再把舍入误差补给占比最大的一段：条长在归一化时
            // 恒为满格，非归一化时与总量成比例且绝不超出轨道右缘。
            let mut widths: Vec<(usize, u32)> = spec
                .series
                .iter()
                .enumerate()
                .filter_map(|(index, series)| {
                    let value = point(series, row)?;
                    let ratio = (value.value.max(0.0) / total).clamp(0.0, 1.0);
                    Some((index, (f64::from(target_len) * ratio).round() as u32))
                })
                .collect();
            if let Some(widest) = widths
                .iter()
                .enumerate()
                .max_by_key(|(_, (_, width))| *width)
                .map(|(slot, _)| slot)
            {
                let sum: u32 = widths.iter().map(|(_, width)| *width).sum();
                let (index, width) = widths[widest];
                widths[widest] = if sum < target_len {
                    (index, width + (target_len - sum))
                } else {
                    (index, width.saturating_sub(sum - target_len))
                };
            }
            let segments: Vec<(u32, [u8; 3])> = widths
                .into_iter()
                .map(|(index, width)| (width, self.series_color(index)))
                .collect();
            self.svg
                .clipped_segments(track.0, bar_y, target_len, bar_h, &segments);
            if !spec.normalized {
                self.text_end(
                    line,
                    self.w().saturating_sub(PAD_X),
                    &total_labels[row],
                    self.palette.text_secondary,
                );
            }
        }
        let axis_row = u16::try_from(spec.categories.len() + 1).unwrap_or(u16::MAX);
        let y = self.row_top(axis_row);
        let border = self.palette.border_subtle;
        let gray = self.palette.gray_dim;
        self.hline(track.0, track.1, y, border);
        if spec.normalized {
            for (ratio, label) in [(0.0f64, "0"), (0.5, "50%"), (1.0, "100%")] {
                let x = track.0 + (f64::from(length) * ratio) as u32;
                let width = self.text_width(label);
                self.vline(x, y, y + 3, border);
                self.text(
                    axis_row,
                    x.saturating_sub(width / 2).min(track.1),
                    label,
                    gray,
                );
            }
        } else {
            let max_label = format_value(max_total, reference.factor, &reference.unit);
            self.text(axis_row, track.0, "0", gray);
            self.vline(track.0, y, y + 3, border);
            self.vline(track.1, y, y + 3, border);
            self.text_end(axis_row, track.1, &max_label, gray);
        }
    }

    /// 进度条：每类别一行，半透明圆角轨道 + 胶囊填充 + 右侧 `文本（百分比）`。
    pub(super) fn progress(&mut self) {
        let spec = self.spec;
        let Some(series) = spec.series.first() else {
            return;
        };
        let label_w = self.label_width();
        let labels: Vec<String> = (0..spec.categories.len())
            .map(|row| progress_label(series, row))
            .collect();
        let value_w = self.widest(&labels).min(self.w() / 2);
        let track = (
            PAD_X + label_w + GAP,
            self.w()
                .saturating_sub(PAD_X + value_w + GAP)
                .max(PAD_X + label_w + GAP + 1),
        );
        let length = track.1 - track.0;
        let track_h = (self.cell_h() / 4).max(4);
        let color = self.series_color(0);
        for row in 0..spec.categories.len() {
            let line = u16::try_from(row).unwrap_or(u16::MAX);
            let shown = self.fit(&spec.categories[row].clone(), label_w);
            self.text(line, PAD_X, &shown, self.palette.text_primary);
            let Some(value) = point(series, row) else {
                continue;
            };
            let y = self.row_top(line) + (self.cell_h() - track_h) / 2;
            self.svg
                .track_pill(track.0, y, length, track_h, self.palette.border);
            let ratio = progress_ratio(value);
            let filled = ((f64::from(length) * ratio).round() as u32).min(length);
            if filled > 0 {
                self.svg
                    .progress_fill(track.0, y, length, track_h, filled, color);
            }
            self.text_end(
                line,
                self.w().saturating_sub(PAD_X),
                &labels[row],
                self.palette.text_secondary,
            );
        }
    }
}

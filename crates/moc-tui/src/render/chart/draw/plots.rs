use rust_i18n::t;

use super::GAP;
use super::HEAT_STEPS;
use super::LOG_SCALE_RATIO;
use super::PAD_X;
use super::Painter;
use super::blend;
use super::data_bounds;
use super::data_extent;
use super::format_value;
use super::heat_alpha;
use super::linear_ratio;
use super::padded_bounds;
use super::point;

impl Painter<'_> {
    /// 折线图：圆点图例 + 抗锯齿折线绘图区（含 y 轴极值）+ 横轴刻度。
    pub(super) fn line(&mut self) {
        let spec = self.spec;
        let Some(reference) = spec.series.first() else {
            return;
        };
        self.legend();
        let rows = self.layout.rows;
        let plot_top = self.row_top(1);
        let plot_bottom = self.row_top(rows.saturating_sub(1));
        let (min_v, max_v) = data_bounds(&spec.series).unwrap_or((0.0, 1.0));
        let gray = self.palette.gray_dim;
        let y_labels = [
            format_value(max_v, reference.factor, &reference.unit),
            format_value(min_v, reference.factor, &reference.unit),
        ];
        let y_w = self.widest(&y_labels).min(self.w() / 4);
        let plot_left = PAD_X + y_w + GAP;
        let plot_right = self.w().saturating_sub(PAD_X).max(plot_left + 1);
        self.text_end(1, PAD_X + y_w, &y_labels[0], gray);
        let last_plot_row = rows.saturating_sub(2);
        self.text_end(last_plot_row, PAD_X + y_w, &y_labels[1], gray);
        let count = spec.categories.len().max(1);
        let x_of = |index: usize| -> f64 {
            if count <= 1 {
                f64::from(plot_left)
            } else {
                f64::from(plot_left)
                    + f64::from(plot_right - plot_left) * index as f64 / (count - 1) as f64
            }
        };
        let y_of = |value: f64| -> f64 {
            let height = f64::from((plot_bottom - plot_top).max(1));
            f64::from(plot_bottom) - linear_ratio(value, min_v, max_v) * height
        };
        let draw_dots = spec.categories.len() <= 12;
        for (index, series) in spec.series.iter().enumerate() {
            let color = self.series_color(index);
            let mut run: Vec<(f64, f64)> = Vec::new();
            for category in 0..spec.categories.len() {
                match point(series, category).filter(|point| point.value.is_finite()) {
                    Some(value) => run.push((x_of(category), y_of(value.value))),
                    None => {
                        self.svg.polyline(&run, 2.0, color);
                        if draw_dots || run.len() == 1 {
                            for &(x, y) in &run {
                                self.svg.circle(x, y, 2.6, color);
                            }
                        }
                        run.clear();
                    }
                }
            }
            self.svg.polyline(&run, 2.0, color);
            if draw_dots || run.len() == 1 {
                for &(x, y) in &run {
                    self.svg.circle(x, y, 2.6, color);
                }
            }
        }
        // 横轴：≤ 2 个类别时只画首尾，避免中位刻度与末尾重叠；≥ 3 时画首 / 中 / 尾
        let axis_row = rows.saturating_sub(1);
        let border = self.palette.border_subtle;
        self.hline(plot_left, plot_right, plot_bottom, border);
        let ticks: Vec<(usize, u8)> = match count {
            1 => vec![(0, 0)],
            2 => vec![(0, 0), (1, 2)],
            _ => vec![(0, 0), (count / 2, 1), (count - 1, 2)],
        };
        for (index, slot) in ticks {
            let Some(category) = spec.categories.get(index) else {
                continue;
            };
            let label = self.fit(&category.clone(), (plot_right - plot_left) / 2);
            let width = self.text_width(&label);
            let center = x_of(index);
            let x = if slot == 0 {
                plot_left
            } else if slot == 2 {
                plot_right.saturating_sub(width)
            } else {
                (center as u32).saturating_sub(width / 2)
            };
            self.vline(center as u32, plot_bottom, plot_bottom + 3, border);
            self.text(axis_row, x, &label, gray);
        }
    }

    /// 热力图：顶部圆角渐变色标 + 列头 + 每类别一行圆角色块。
    pub(super) fn heatmap(&mut self) {
        let spec = self.spec;
        let Some(reference) = spec.series.first() else {
            return;
        };
        let label_w = self.label_width();
        let grid_left = PAD_X + label_w + GAP;
        let grid_right = self.w().saturating_sub(PAD_X).max(grid_left + 1);
        let columns = spec.series.len().max(1);
        let cell_w = ((grid_right - grid_left) / columns as u32).max(2);
        let (min_v, max_v) = data_bounds(&spec.series).unwrap_or((0.0, 1.0));
        let span = (max_v - min_v).max(f64::EPSILON);
        let log = min_v > 0.0 && max_v / min_v >= LOG_SCALE_RATIO;
        let ratio = |value: f64| -> f64 {
            if log {
                ((value.max(min_v) / min_v).ln() / (max_v / min_v).ln()).clamp(0.0, 1.0)
            } else {
                ((value - min_v) / span).clamp(0.0, 1.0)
            }
        };
        let hue = self.series_color(0);

        // 第 0 行：顶部色标（低值 → 渐变 → 高值），不再占用数据行。
        let low = format_value(min_v, reference.factor, &reference.unit);
        let high = format_value(max_v, reference.factor, &reference.unit);
        let legend = if log {
            t!("tui.chart.scale_log")
        } else {
            t!("tui.chart.scale")
        };
        let legend_w = self.text_width(&legend);
        let low_w = self.text_width(&low);
        let high_w = self.text_width(&high);
        let bar_left = PAD_X + legend_w + GAP + low_w + GAP;
        let bar_right = grid_right.saturating_sub(high_w + GAP).max(bar_left + 1);
        let bar_h = (self.cell_h() / 4).max(4);
        let bar_y = self.row_top(0) + (self.cell_h() - bar_h) / 2;
        let bar_span = (bar_right - bar_left).max(1);
        let segment = (bar_span / HEAT_STEPS as u32).max(1);
        let segments: Vec<(u32, [u8; 3])> = (0..HEAT_STEPS)
            .map(|step| (segment, blend(self.palette.bg_base, hue, heat_alpha(step))))
            .collect();
        self.svg.clipped_segments(
            bar_left,
            bar_y,
            segment * HEAT_STEPS as u32,
            bar_h,
            &segments,
        );
        self.text(0, PAD_X, &legend, self.palette.text_secondary);
        self.text(0, PAD_X + legend_w + GAP, &low, self.palette.gray_dim);
        self.text_end(0, grid_right, &high, self.palette.gray_dim);

        // 第 1 行：列头（系列名居中于各自色块列）。
        for (index, series) in spec.series.iter().enumerate() {
            let center = grid_left + cell_w * index as u32 + cell_w / 2;
            let label = self.fit(&series.name, cell_w.saturating_sub(2));
            let width = self.text_width(&label);
            self.text(
                1,
                center.saturating_sub(width / 2),
                &label,
                self.palette.text_secondary,
            );
        }

        let text_color = self.palette.text_primary;
        let dark = [0x10, 0x10, 0x10];
        for row in 0..spec.categories.len() {
            let line = u16::try_from(row + 2).unwrap_or(u16::MAX);
            let shown = self.fit(&spec.categories[row].clone(), label_w);
            self.text(line, PAD_X, &shown, self.palette.text_primary);
            let y = self.row_top(line) + 2;
            let height = self.cell_h().saturating_sub(4).max(2);
            for (index, series) in spec.series.iter().enumerate() {
                let x = grid_left + cell_w * index as u32;
                let Some(value) = point(series, row) else {
                    continue;
                };
                let step = ((ratio(value.value) * (HEAT_STEPS - 1) as f64).round() as usize)
                    .min(HEAT_STEPS - 1);
                let fill = blend(self.palette.bg_base, hue, heat_alpha(step));
                let inset = if cell_w >= 4 { 1 } else { 0 };
                let width = cell_w.saturating_sub(2 * inset).max(1);
                let rx = if width >= 8 && height >= 8 { 3.0 } else { 0.0 };
                self.svg.rect(x + inset, y, width, height, rx, fill);
                // 格内数值：格宽足够才写，否则宁可不写
                let label = &value.text;
                let text_w = self.text_width(label);
                if width >= text_w + 6 {
                    let luma = (299 * u32::from(fill[0])
                        + 587 * u32::from(fill[1])
                        + 114 * u32::from(fill[2]))
                        / 1000;
                    let ink = if luma > 140 { dark } else { text_color };
                    let baseline = self.baseline(line);
                    self.svg
                        .text(x + (cell_w - text_w) / 2, baseline, label, ink);
                }
            }
        }
    }

    /// 散点图：x / y 两个量纲，12 行绘图区 + 两行轴标签。
    pub(super) fn scatter(&mut self) {
        let spec = self.spec;
        let [x_series, y_series, ..] = spec.series.as_slice() else {
            return;
        };
        self.legend();
        let pairs: Vec<(f64, f64)> = (0..spec.categories.len())
            .filter_map(|row| {
                let x = point(x_series, row)?;
                let y = point(y_series, row)?;
                Some((x.value, y.value))
            })
            .collect();
        let rows = self.layout.rows;
        let plot_top = self.row_top(1);
        let plot_bottom = self.row_top(rows.saturating_sub(2));
        let raw_x = data_extent(&pairs, |pair| pair.0);
        let raw_y = data_extent(&pairs, |pair| pair.1);
        let y_labels = [
            format_value(raw_y.1, y_series.factor, &y_series.unit),
            format_value(raw_y.0, y_series.factor, &y_series.unit),
        ];
        let y_w = self.widest(&y_labels).min(self.w() / 4);
        let plot_left = PAD_X + y_w + GAP;
        let plot_right = self.w().saturating_sub(PAD_X).max(plot_left + 1);
        self.text_end(1, PAD_X + y_w, &y_labels[0], self.palette.gray_dim);
        self.text_end(
            rows.saturating_sub(3),
            PAD_X + y_w,
            &y_labels[1],
            self.palette.gray_dim,
        );
        let (x_min, x_max) = padded_bounds(&pairs, |pair| pair.0);
        let (y_min, y_max) = padded_bounds(&pairs, |pair| pair.1);
        let x_span = (x_max - x_min).max(f64::EPSILON);
        let y_span = (y_max - y_min).max(f64::EPSILON);
        let map_x = |value: f64| -> f64 {
            f64::from(plot_left) + (value - x_min) / x_span * f64::from(plot_right - plot_left)
        };
        let map_y = |value: f64| -> f64 {
            f64::from(plot_bottom) - (value - y_min) / y_span * f64::from(plot_bottom - plot_top)
        };
        let color = self.series_color(0);
        let radius = (f64::from(self.cell_h()) * 0.14).clamp(2.2, 4.5);
        let border = self.palette.border_subtle;
        self.hline(plot_left, plot_right, plot_bottom, border);
        self.vline(plot_left, plot_top, plot_bottom, border);
        for (x, y) in &pairs {
            self.svg.circle(map_x(*x), map_y(*y), radius, color);
        }
        let axis_row = rows.saturating_sub(2);
        let x_low = format_value(raw_x.0, x_series.factor, &x_series.unit);
        let x_high = format_value(raw_x.1, x_series.factor, &x_series.unit);
        self.text(axis_row, plot_left, &x_low, self.palette.gray_dim);
        self.text_end(axis_row, plot_right, &x_high, self.palette.gray_dim);
        let caption = format!("{} → {}", x_series.name, y_series.name);
        let caption = self.fit(&caption, plot_right - plot_left);
        self.text(
            axis_row + 1,
            plot_left,
            &caption,
            self.palette.text_secondary,
        );
    }
}

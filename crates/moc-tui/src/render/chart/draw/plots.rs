use super::super::text;
use super::GAP;
use super::HEAT_STEPS;
use super::LOG_SCALE_RATIO;
use super::PAD_X;
use super::Painter;
use super::blend;
use super::data_bounds;
use super::data_extent;
use super::data_max;
use super::data_min;
use super::format_value;
use super::heat_alpha;
use super::padded_bounds;
use super::point;

impl Painter<'_> {
    /// 折线图：图例 + 绘图区（含 y 轴极值）+ 横轴刻度。
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
        let plot_right = self.w() - PAD_X;
        self.text_end(1, PAD_X + y_w, &y_labels[0], gray);
        let last_plot_row = rows.saturating_sub(2);
        self.text_end(last_plot_row, PAD_X + y_w, &y_labels[1], gray);
        let span = (max_v - min_v).max(f64::EPSILON);
        let count = spec.categories.len().max(1);
        let x_of = |index: usize| -> f64 {
            if count <= 1 {
                f64::from(plot_left)
            } else {
                f64::from(plot_left)
                    + (plot_right - plot_left) as f64 * index as f64 / (count - 1) as f64
            }
        };
        let y_of = |value: f64| -> f64 {
            let height = (plot_bottom - plot_top).max(1) as f64;
            f64::from(plot_bottom) - (value - min_v) / span * height
        };
        for (index, series) in spec.series.iter().enumerate() {
            let color = self.series_color(index);
            let color_id = self.canvas.color(color);
            let mut run: Vec<(f64, f64)> = Vec::new();
            for category in 0..spec.categories.len() {
                match point(series, category) {
                    Some(value) => run.push((x_of(category), y_of(value.value))),
                    None => {
                        self.polyline(&run, color_id);
                        run.clear();
                    }
                }
            }
            self.polyline(&run, color_id);
            if spec.categories.len() <= 12 {
                for &(x, y) in &run {
                    self.canvas.disc(x, y, 2.0, color_id);
                }
            }
        }
        // 横轴：首 / 中 / 尾三个刻度（类别多了写不下）
        let axis_row = rows.saturating_sub(1);
        let border = self.palette.border_subtle;
        self.hline(plot_left, plot_right, plot_bottom, border);
        let ticks = [0, count / 2, count - 1];
        for (slot, index) in ticks.iter().enumerate() {
            let Some(category) = spec.categories.get(*index) else {
                continue;
            };
            let label = self.fit(&category.clone(), (plot_right - plot_left) / 2);
            let width = self.text_width(&label);
            let center = x_of(*index);
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

    pub(super) fn polyline(&mut self, points: &[(f64, f64)], color: u8) {
        if points.len() >= 2 {
            self.canvas.polyline(points, color, 2);
        }
    }

    /// 热力图：顶部色标 + 列头 + 每类别一行色块。
    ///
    /// 所有格子共用系列 0 的色相：热力图表达同一量纲的强弱，逐列换色会把
    /// 「值」误读成「类别」。色深按全部系列统一定标，宽范围走对数映射；
    /// 色标放在顶部图例区，底部不再有与数据行同构的色标行。
    pub(super) fn heatmap(&mut self) {
        let spec = self.spec;
        let Some(reference) = spec.series.first() else {
            return;
        };
        let label_w = self.label_width();
        let grid_left = PAD_X + label_w + GAP;
        let grid_right = self.w() - PAD_X;
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
        let legend = if log { "色阶（对数）" } else { "色阶" };
        let legend_w = self.text_width(legend);
        let low_w = self.text_width(&low);
        let high_w = self.text_width(&high);
        let bar_left = PAD_X + legend_w + GAP + low_w + GAP;
        let bar_right = grid_right.saturating_sub(high_w + GAP).max(bar_left + 1);
        let bar_h = (self.cell_h() / 4).max(3);
        let bar_y = self.row_top(0) + (self.cell_h() - bar_h) / 2;
        let bar_span = (bar_right - bar_left).max(1);
        let segment = (bar_span / HEAT_STEPS as u32).max(1);
        for step in 0..HEAT_STEPS {
            let fill = blend(self.palette.bg_base, hue, heat_alpha(step));
            self.fill(
                bar_left + segment * step as u32,
                bar_y,
                segment,
                bar_h,
                fill,
            );
        }
        self.text(0, PAD_X, legend, self.palette.text_secondary);
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
                self.fill(x + inset, y, width, height, fill);
                // 格内数值：格宽足够才写，否则宁可不写（挤成一团更不可读）
                let label = &value.text;
                let text_w = self.text_width(label);
                if width >= text_w + 6 {
                    // 墨色按格子亮度选：浅底写深色，深底写浅色
                    let luma = (299 * u32::from(fill[0])
                        + 587 * u32::from(fill[1])
                        + 114 * u32::from(fill[2]))
                        / 1000;
                    let ink = if luma > 140 { dark } else { text_color };
                    let baseline = self.baseline(line);
                    let px = self.px();
                    text::draw(
                        &mut self.canvas,
                        x + (cell_w - text_w) / 2,
                        baseline,
                        label,
                        px,
                        ink,
                        fill,
                        self.fonts,
                    );
                }
            }
        }
    }

    /// 散点图：x / y 两个量纲，12 行绘图区 + 两行轴标签。
    pub(super) fn scatter(&mut self) {
        let spec = self.spec;
        // 规划保证散点是两个不同量纲的测量列；不足两列就什么都不画
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
        let y_labels = [
            format_value(data_max(y_series), y_series.factor, &y_series.unit),
            format_value(data_min(y_series), y_series.factor, &y_series.unit),
        ];
        let y_w = self.widest(&y_labels).min(self.w() / 4);
        let plot_left = PAD_X + y_w + GAP;
        let plot_right = self.w() - PAD_X;
        self.text_end(1, PAD_X + y_w, &y_labels[0], self.palette.gray_dim);
        self.text_end(
            rows.saturating_sub(3),
            PAD_X + y_w,
            &y_labels[1],
            self.palette.gray_dim,
        );
        let (x_min, x_max) = padded_bounds(&pairs, |pair| pair.0);
        let (y_min, y_max) = padded_bounds(&pairs, |pair| pair.1);
        let raw_x = data_extent(&pairs, |pair| pair.0);
        let x_span = (x_max - x_min).max(f64::EPSILON);
        let y_span = (y_max - y_min).max(f64::EPSILON);
        let map_x = |value: f64| -> f64 {
            f64::from(plot_left) + (value - x_min) / x_span * f64::from(plot_right - plot_left)
        };
        let map_y = |value: f64| -> f64 {
            f64::from(plot_bottom) - (value - y_min) / y_span * f64::from(plot_bottom - plot_top)
        };
        let color = self.canvas.color(self.series_color(0));
        let radius = (f64::from(self.cell_h()) * 0.08).clamp(1.5, 4.0);
        let border = self.palette.border_subtle;
        self.hline(plot_left, plot_right, plot_bottom, border);
        self.vline(plot_left, plot_top, plot_bottom, border);
        for (x, y) in &pairs {
            self.canvas.disc(map_x(*x), map_y(*y), radius, color);
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

use super::GAP;
use super::PAD_X;
use super::Painter;
use super::ValueScale;
use super::bar_height;
use super::bar_inset;
use super::collect_texts;
use super::format_value;
use super::max_point;
use super::mid_point;
use super::paired_deltas;
use super::point;
use super::progress_label;
use super::progress_ratio;
use moc_core::chart::Point;
use moc_core::chart::Polarity;

impl Painter<'_> {
    /// 条形图：单系列，每类别一行。
    pub(super) fn bar(&mut self) {
        let spec = self.spec;
        let Some(series) = spec.series.first() else {
            return;
        };
        let scale = ValueScale::new(&spec.series);
        let label_w = self.label_width();
        let labels: Vec<String> = (0..spec.categories.len())
            .filter_map(|row| point(series, row).map(|point| point.text.clone()))
            .collect();
        let value_w = self.widest(&labels).min(self.w() / 4);
        let track = (
            PAD_X + label_w + GAP,
            self.w()
                .saturating_sub(PAD_X + value_w + GAP)
                .max(PAD_X + label_w + GAP + 1),
        );
        let origin = scale.origin(track);
        let legend_end = self.legend();
        self.scale_note(legend_end, scale);
        let color = self.series_color(0);
        let gray = self.palette.gray_dim;
        if spec.diverging && !scale.is_log() {
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
            let x = scale.position(track, value.value);
            let bar_y = self.row_top(line) + bar_inset(self.cell_h());
            let bar_h = bar_height(self.cell_h());
            let (left, right) = if x >= origin {
                (origin, x)
            } else {
                (x, origin)
            };
            self.fill(left, bar_y, (right - left).max(1), bar_h, color);
            if value.value >= 0.0 {
                self.text(line, x + GAP, &value.text, self.palette.text_secondary);
            } else {
                self.text_end(
                    line,
                    x.saturating_sub(GAP),
                    &value.text,
                    self.palette.text_secondary,
                );
            }
        }
        let axis_row = u16::try_from(spec.categories.len() + 1).unwrap_or(u16::MAX);
        let (min_text, mid_text, max_text) = if scale.is_log() {
            scale.log_labels(series)
        } else {
            (
                String::new(),
                mid_point(series).map_or_else(String::new, |point| point.text.clone()),
                max_point(series).map_or_else(String::new, |point| point.text.clone()),
            )
        };
        self.value_axis(axis_row, track, scale, &min_text, &mid_text, &max_text);
    }

    /// 分组条形图：每类别占系列数行，行内只画条与数值（系列由色块图例区分）。
    pub(super) fn grouped(&mut self) {
        let spec = self.spec;
        let scale = ValueScale::new(&spec.series);
        let label_w = self.label_width();
        let value_w = self.widest(&collect_texts(&spec.series)).min(self.w() / 4);
        let track = (
            PAD_X + label_w + GAP,
            self.w()
                .saturating_sub(PAD_X + value_w + GAP)
                .max(PAD_X + label_w + GAP + 1),
        );
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
                if let Some(value) = point(series, category) {
                    let x = scale.position(track, value.value);
                    let bar_y = self.row_top(line) + bar_inset(self.cell_h());
                    let (left, right) = if x >= origin {
                        (origin, x)
                    } else {
                        (x, origin)
                    };
                    self.fill(
                        left,
                        bar_y,
                        (right - left).max(1),
                        bar_height(self.cell_h()),
                        self.series_color(index),
                    );
                    if value.value >= 0.0 {
                        self.text(line, x + GAP, &value.text, self.palette.text_secondary);
                    } else {
                        self.text_end(
                            line,
                            x.saturating_sub(GAP),
                            &value.text,
                            self.palette.text_secondary,
                        );
                    }
                }
                row += 1;
            }
        }
        let axis_row = u16::try_from(row).unwrap_or(u16::MAX);
        let reference = spec.series.first().expect("分组图至少一个系列");
        let (min_text, mid_text, max_text) = if scale.is_log() {
            scale.log_labels(reference)
        } else {
            let extreme = spec
                .series
                .iter()
                .filter_map(max_point)
                .max_by(|a, b| a.value.total_cmp(&b.value));
            (
                String::new(),
                mid_point(reference).map_or_else(String::new, |point| point.text.clone()),
                extreme.map_or_else(String::new, |point| point.text.clone()),
            )
        };
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
        let value_w = self.widest(&collect_texts(&spec.series)).min(self.w() / 5);
        let deltas = paired_deltas(&spec.series);
        let delta_w = self.widest(&deltas).min(self.w() / 4);
        let track = (
            PAD_X + label_w + GAP,
            self.w()
                .saturating_sub(PAD_X + value_w + delta_w + 2 * GAP)
                .max(PAD_X + label_w + GAP + 1),
        );
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
            self.bar_row(first, point(before, category), track, scale, origin, gray);
            let accent = self.series_color(1);
            self.bar_row(second, point(after, category), track, scale, origin, accent);
            if let Some(delta) = deltas.get(category).filter(|delta| !delta.is_empty()) {
                let color = self.delta_color(category, polarity);
                self.text_end(second, self.w() - PAD_X, delta, color);
            }
            row += 2;
        }
        let axis_row = u16::try_from(row).unwrap_or(u16::MAX);
        let (min_text, mid_text, max_text) = if scale.is_log() {
            scale.log_labels(after)
        } else {
            (
                String::new(),
                mid_point(after).map_or_else(String::new, |point| point.text.clone()),
                max_point(after).map_or_else(String::new, |point| point.text.clone()),
            )
        };
        self.value_axis(axis_row, track, scale, &min_text, &mid_text, &max_text);
    }

    /// 一行条形（成对图复用前 / 后两行）。
    pub(super) fn bar_row(
        &mut self,
        row: u16,
        point: Option<&Point>,
        track: (u32, u32),
        scale: ValueScale,
        origin: u32,
        color: [u8; 3],
    ) {
        let Some(value) = point else {
            return;
        };
        let x = scale.position(track, value.value);
        let bar_y = self.row_top(row) + bar_inset(self.cell_h());
        let (left, right) = if x >= origin {
            (origin, x)
        } else {
            (x, origin)
        };
        self.fill(
            left,
            bar_y,
            (right - left).max(1),
            bar_height(self.cell_h()),
            color,
        );
        if value.value >= 0.0 {
            self.text(row, x + GAP, &value.text, self.palette.text_secondary);
        } else {
            self.text_end(
                row,
                x.saturating_sub(GAP),
                &value.text,
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

    /// 堆叠条形图：类别一行，段按系列顺序拼；归一化时条长恒为满格。
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
        for row in 0..spec.categories.len() {
            let line = u16::try_from(row + 1).unwrap_or(u16::MAX);
            let shown = self.fit(&spec.categories[row].clone(), label_w);
            self.text(line, PAD_X, &shown, self.palette.text_primary);
            let total = totals[row];
            let bar_y = self.row_top(line) + bar_inset(self.cell_h());
            let bar_h = bar_height(self.cell_h());
            if total <= 0.0 {
                continue;
            }
            // 段宽先各自取整，再把舍入误差补给占比最大的一段：条的总长恒等于
            // 满格长度，读者才能横向比较不同类别的总量。
            let mut widths: Vec<(usize, u32)> = spec
                .series
                .iter()
                .enumerate()
                .filter_map(|(index, series)| {
                    let value = point(series, row)?;
                    let ratio = (value.value.max(0.0) / total).clamp(0.0, 1.0);
                    Some((index, (f64::from(length) * ratio).round() as u32))
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
                widths[widest] = (index, width + length.saturating_sub(sum));
            }
            let mut x = track.0;
            for (index, width) in widths {
                if width > 0 {
                    self.fill(x, bar_y, width, bar_h, self.series_color(index));
                }
                x += width;
            }
            if !spec.normalized {
                self.text_end(
                    line,
                    self.w() - PAD_X,
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

    /// 进度条：每类别一行，轨道 + 填充 + 右侧 `文本（百分比）`。
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
        let track_h = (self.cell_h() / 6).max(2);
        for row in 0..spec.categories.len() {
            let line = u16::try_from(row).unwrap_or(u16::MAX);
            let shown = self.fit(&spec.categories[row].clone(), label_w);
            self.text(line, PAD_X, &shown, self.palette.text_primary);
            let Some(value) = point(series, row) else {
                continue;
            };
            let y = self.row_top(line) + (self.cell_h() - track_h) / 2;
            self.fill(track.0, y, length, track_h, self.palette.bg_panel);
            let ratio = progress_ratio(value);
            let filled = ((f64::from(length) * ratio).round() as u32).min(length);
            self.fill(track.0, y, filled, track_h, self.series_color(0));
            self.text_end(
                line,
                self.w() - PAD_X,
                &labels[row],
                self.palette.text_secondary,
            );
        }
    }
}

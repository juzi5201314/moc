//! 八种图型的版式与绘制。
//!
//! 所有图型共用一套几何约定：
//! - 坐标一律整数像素，`(x, y)` 是左上角，`row_top(row)` 是该行首像素行；
//! - 文本基线固定在 `row_top(row) + ChartLayout::baseline`，因此同一行的文本、
//!   色块、条形天然对齐，不需要逐个度量做垂直居中；
//! - 左侧类别标签列宽度按内容取上限（不超过图宽的 [`LABEL_BUDGET`]），
//!   右侧给数值 / 差值文本留出宽裕度，条形只在中间的可变区伸缩。
//!
//! 数值一律以 `Point::text` 的原样书写显示（不二次格式化，避免把 `7648 / 176`
//! 写成 `7648`）；只有「和 / 差 / 刻度」这种派生量才用 [`format_value`]。

use moc_core::chart::{ChartKind, ChartSpec, Point, Polarity, Series};

use super::ChartLayout;
use super::canvas::Canvas;
use super::fonts::Fonts;
use super::text;
use crate::theme::RawPalette;

/// 左右留白（像素）。
const PAD_X: u32 = 10;
/// 元素之间的横向间距（像素）。
const GAP: u32 = 6;
/// 类别标签列占图宽的上限比例。
const LABEL_BUDGET: f64 = 0.35;
/// 条形高度占行高的比例。
const BAR_RATIO: f64 = 0.62;
/// 图例色块边长（像素）。
const SWATCH: u32 = 8;
/// 热力图的混色档数：档位越少调色板越稳，5 档已能看出层次。
const HEAT_STEPS: usize = 5;

/// 绘制入口：吃一张按格对齐的画布，返回同一张画布（尺寸不变）。
pub fn paint(
    canvas: Canvas,
    spec: &ChartSpec,
    layout: &ChartLayout,
    palette: &'static RawPalette,
    fonts: &mut Fonts,
) -> Canvas {
    let mut painter = Painter {
        canvas,
        fonts,
        spec,
        layout,
        palette,
    };
    match spec.kind {
        ChartKind::Bar => painter.bar(),
        ChartKind::Grouped => painter.grouped(),
        ChartKind::Paired => painter.paired(),
        ChartKind::Line => painter.line(),
        ChartKind::Stacked => painter.stacked(),
        ChartKind::Progress => painter.progress(),
        ChartKind::Heatmap => painter.heatmap(),
        ChartKind::Scatter => painter.scatter(),
    }
    painter.canvas
}

/// 基本单位 → 书写单位的紧凑格式（派生量专用：和、差、刻度、总量）。
fn format_value(value: f64, factor: f64, unit: &str) -> String {
    let scaled = if factor.abs() > f64::EPSILON {
        value / factor
    } else {
        value
    };
    let abs = scaled.abs();
    let number = if abs >= 100.0 {
        format!("{scaled:.0}")
    } else if abs >= 10.0 {
        format!("{scaled:.1}")
    } else if abs >= 1.0 {
        format!("{scaled:.2}")
    } else {
        format!("{scaled:.3}")
    };
    let number = trim_decimal(&number);
    match unit {
        "" => number,
        "%" => format!("{number}%"),
        unit => format!("{number} {unit}"),
    }
}

/// 去掉浮点格式化留下的尾零（`12.50` → `12.5`、`12.00` → `12`）。
fn trim_decimal(text: &str) -> String {
    if !text.contains('.') {
        return text.to_owned();
    }
    let trimmed = text.trim_end_matches('0').trim_end_matches('.');
    if trimmed.is_empty() || trimmed == "-" {
        "0".to_owned()
    } else {
        trimmed.to_owned()
    }
}

/// 全部系列的取值区间，恒含 0（条形从零基线起画，正负都能表达）。
fn bounds(series: &[Series]) -> Option<(f64, f64)> {
    let mut min = 0.0f64;
    let mut max = 0.0f64;
    let mut any = false;
    for item in series {
        for point in item.points.iter().flatten() {
            min = min.min(point.value);
            max = max.max(point.value);
            any = true;
        }
    }
    any.then_some((min, max))
}

/// 取某类别在某系列上的点。
fn point(series: &Series, row: usize) -> Option<&Point> {
    series.points.get(row).and_then(Option::as_ref)
}

/// 条形刻度的极差门槛：全部为正且 max/min 达到这个倍数时改用对数刻度。
///
/// 与 omp autoGraph 的 100 倍阈值一致：线性刻度下 12 与 884,736 同图时，
/// 小值的条只剩一两个像素，读者无法比较。
const LOG_SCALE_RATIO: f64 = 100.0;

/// 条形 / 分组条 / 成对条共用的横向刻度。
#[derive(Debug, Clone, Copy)]
enum ValueScale {
    /// 含零线性刻度：条从零基线起画，正负都能表达。
    Linear { min: f64, max: f64 },
    /// 全正数据的对数刻度：条从最小值起画，极差很大时仍可比较。
    Log { min: f64, max: f64 },
}

impl ValueScale {
    fn new(series: &[Series]) -> Self {
        let Some((min, max)) = data_bounds(series) else {
            return Self::Linear { min: 0.0, max: 1.0 };
        };
        if min > 0.0 && max / min >= LOG_SCALE_RATIO {
            Self::Log { min, max }
        } else {
            let (min, max) = bounds(series).unwrap_or((0.0, 1.0));
            Self::Linear { min, max }
        }
    }

    fn is_log(self) -> bool {
        matches!(self, Self::Log { .. })
    }

    /// 数值在横向轨道里的比例（0..1）。
    fn ratio(self, value: f64) -> f64 {
        match self {
            Self::Linear { min, max } => {
                let span = (max - min).max(f64::EPSILON);
                ((value - min) / span).clamp(0.0, 1.0)
            }
            Self::Log { min, max } => {
                let span = (max / min).ln();
                if span.abs() < f64::EPSILON {
                    0.0
                } else {
                    ((value.max(min) / min).ln() / span).clamp(0.0, 1.0)
                }
            }
        }
    }

    /// 条的起点：线性用零轴，对数用轨道左端（最小值）。
    fn origin(self, track: (u32, u32)) -> u32 {
        match self {
            Self::Linear { .. } => {
                track.0 + (self.ratio(0.0) * f64::from(track.1 - track.0)).round() as u32
            }
            Self::Log { .. } => track.0,
        }
        .min(track.1)
    }

    /// 数值在轨道里的横坐标。
    fn position(self, track: (u32, u32), value: f64) -> u32 {
        track.0 + (self.ratio(value) * f64::from(track.1 - track.0)).round() as u32
    }

    /// 对数刻度的三条轴标签（最小值 / 几何中值 / 最大值）；线性刻度由调用方用真实单元格文本。
    fn log_labels(self, reference: &Series) -> (String, String, String) {
        match self {
            Self::Linear { .. } => (String::new(), String::new(), String::new()),
            Self::Log { min, max } => (
                format_value(min, reference.factor, &reference.unit),
                format_value(min.sqrt() * max.sqrt(), reference.factor, &reference.unit),
                format_value(max, reference.factor, &reference.unit),
            ),
        }
    }
}

/// 按 8 位 alpha 把前景混到背景上。
fn blend(under: [u8; 3], over: [u8; 3], alpha: u32) -> [u8; 3] {
    let channel = |u: u8, o: u8| -> u8 {
        ((u32::from(u) * (255 - alpha) + u32::from(o) * alpha + 127) / 255) as u8
    };
    [
        channel(under[0], over[0]),
        channel(under[1], over[1]),
        channel(under[2], over[2]),
    ]
}

/// 数值 → 混色档位（0..HEAT_STEPS-1）：最浅几乎只有底色，最深接近纯系列色。
fn heat_alpha(step: usize) -> u32 {
    // 5 档：32 / 80 / 128 / 176 / 224。上限留一点底色，避免整块图过曝。
    32 + (step.min(HEAT_STEPS - 1) as u32 * 192) / (HEAT_STEPS as u32 - 1)
}

struct Painter<'a> {
    canvas: Canvas,
    fonts: &'a mut Fonts,
    spec: &'a ChartSpec,
    layout: &'a ChartLayout,
    palette: &'static RawPalette,
}

impl Painter<'_> {
    fn w(&self) -> u32 {
        self.canvas.w()
    }

    fn px(&self) -> u32 {
        self.layout.px_size
    }

    fn cell_h(&self) -> u32 {
        self.layout.cell.h
    }

    fn row_top(&self, row: u16) -> u32 {
        u32::from(row) * self.cell_h()
    }

    fn baseline(&self, row: u16) -> i32 {
        (self.row_top(row) + self.layout.baseline) as i32
    }

    fn series_color(&self, index: usize) -> [u8; 3] {
        self.palette.chart[index % self.palette.chart.len()]
    }

    /// 在 `row` 行左对齐写字，返回结束笔位。
    fn text(&mut self, row: u16, x: u32, label: &str, color: [u8; 3]) -> u32 {
        let baseline = self.baseline(row);
        let px = self.px();
        let under = self.palette.bg_base;
        text::draw(
            &mut self.canvas,
            x,
            baseline,
            label,
            px,
            color,
            under,
            self.fonts,
        )
    }

    /// 在 `row` 行右对齐写字：`right` 是文本右缘（不含）。
    fn text_end(&mut self, row: u16, right: u32, label: &str, color: [u8; 3]) -> u32 {
        let width = self.text_width(label);
        self.text(row, right.saturating_sub(width), label, color)
    }

    fn text_width(&mut self, label: &str) -> u32 {
        self.fonts.width(label, self.px())
    }

    /// 截断到 `max` 像素宽（超出加 `…`）。
    fn fit(&mut self, label: &str, max: u32) -> String {
        self.fonts.truncate(label, max, self.px())
    }

    fn fill(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
        let color = self.canvas.color(rgb);
        self.canvas.fill_rect(x, y, w, h, color);
    }

    fn hline(&mut self, x0: u32, x1: u32, y: u32, rgb: [u8; 3]) {
        let color = self.canvas.color(rgb);
        self.canvas.hline(x0, x1, y, color);
    }

    fn vline(&mut self, x: u32, y0: u32, y1: u32, rgb: [u8; 3]) {
        let color = self.canvas.color(rgb);
        self.canvas.vline(x, y0, y1, color);
    }

    /// 图例色块：垂直居中于该行。
    fn swatch(&mut self, x: u32, row: u16, rgb: [u8; 3]) {
        let y = self.row_top(row) + (self.cell_h().saturating_sub(SWATCH)) / 2;
        self.fill(x, y, SWATCH, SWATCH, rgb);
    }

    /// 类别标签列宽：按内容取，最多占图宽的 [`LABEL_BUDGET`]。
    fn label_width(&mut self) -> u32 {
        let spec = self.spec;
        let mut widest = 0;
        for category in &spec.categories {
            widest = widest.max(self.text_width(category));
        }
        let budget = (f64::from(self.w()) * LABEL_BUDGET) as u32;
        widest.min(budget).max(1)
    }

    /// 一组文本的最大像素宽（用于给数值 / 差值列留宽）。
    fn widest(&mut self, labels: &[String]) -> u32 {
        let mut widest = 0;
        for label in labels {
            widest = widest.max(self.text_width(label));
        }
        widest
    }

    /// 图例行：色块 + 系列名；单系列时顺带写出书写单位。返回结束笔位。
    fn legend(&mut self) -> u32 {
        let spec = self.spec;
        let limit = self.w().saturating_sub(PAD_X);
        let mut x = PAD_X;
        for (index, series) in spec.series.iter().enumerate() {
            if x + SWATCH + GAP >= limit {
                break;
            }
            self.swatch(x, 0, self.series_color(index));
            x += SWATCH + GAP;
            let label = if spec.series.len() == 1 && !series.unit.is_empty() {
                format!("{}（{}）", series.name, series.unit)
            } else {
                series.name.clone()
            };
            let room = limit.saturating_sub(x);
            let shown = self.fit(&label, room);
            let width = self.text_width(&shown);
            self.text(0, x, &shown, self.palette.text_secondary);
            x += width + 3 * GAP;
        }
        x
    }

    /// 底轴：基线 + 三个刻度。
    ///
    /// 线性刻度画零 / 中 / 最大；对数刻度画最小 / 几何中值 / 最大。
    /// 线性刻度的文本取自真实数据点，保证「轴上的字」与「表里的字」同一套写法。
    fn value_axis(
        &mut self,
        row: u16,
        track: (u32, u32),
        scale: ValueScale,
        min: &str,
        mid: &str,
        max: &str,
    ) {
        let y = self.row_top(row);
        let border = self.palette.border_subtle;
        let gray = self.palette.gray_dim;
        let center = track.0 + (track.1 - track.0) / 2;
        self.hline(track.0, track.1, y, border);
        if scale.is_log() {
            if !min.is_empty() {
                self.vline(track.0, y, y + 3, border);
                self.text(row, track.0, min, gray);
            }
        } else {
            let zero_x = scale.origin(track);
            self.vline(zero_x, y, y + 3, border);
            let zero_width = self.text_width("0");
            self.text(row, zero_x.saturating_sub(zero_width / 2), "0", gray);
        }
        if !mid.is_empty() {
            let width = self.text_width(mid);
            self.vline(center, y, y + 3, border);
            self.text(row, center.saturating_sub(width / 2), mid, gray);
        }
        if !max.is_empty() {
            let width = self.text_width(max);
            let right = track.1;
            self.vline(right.saturating_sub(width / 2), y, y + 3, border);
            self.text_end(row, right, max, gray);
        }
    }

    /// 对数刻度时在图例右侧补一条注记（空间不足就不写，避免压住系列名）。
    fn scale_note(&mut self, legend_end: u32, scale: ValueScale) {
        if !scale.is_log() {
            return;
        }
        let note = "对数刻度";
        let width = self.text_width(note);
        let right = self.w().saturating_sub(PAD_X);
        if legend_end.saturating_add(GAP) < right.saturating_sub(width) {
            self.text_end(0, right, note, self.palette.gray_dim);
        }
    }

    /// 条形图：单系列，每类别一行。
    fn bar(&mut self) {
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
    fn grouped(&mut self) {
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
    fn paired(&mut self) {
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
    fn bar_row(
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
    fn delta_color(&mut self, row: usize, polarity: Option<Polarity>) -> [u8; 3] {
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

    /// 折线图：图例 + 绘图区（含 y 轴极值）+ 横轴刻度。
    fn line(&mut self) {
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

    fn polyline(&mut self, points: &[(f64, f64)], color: u8) {
        if points.len() >= 2 {
            self.canvas.polyline(points, color, 2);
        }
    }

    /// 堆叠条形图：类别一行，段按系列顺序拼；归一化时条长恒为满格。
    fn stacked(&mut self) {
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
    fn progress(&mut self) {
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

    /// 热力图：顶部色标 + 列头 + 每类别一行色块。
    ///
    /// 所有格子共用系列 0 的色相：热力图表达同一量纲的强弱，逐列换色会把
    /// 「值」误读成「类别」。色深按全部系列统一定标，宽范围走对数映射；
    /// 色标放在顶部图例区，底部不再有与数据行同构的色标行。
    fn heatmap(&mut self) {
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
    fn scatter(&mut self) {
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

/// 条形在行内的上边距（剩余空间上下均分）。
fn bar_inset(cell_h: u32) -> u32 {
    (cell_h - bar_height(cell_h)) / 2
}

/// 条形高度：行高的 62%，至少 2px（再细就看不出量级）。
fn bar_height(cell_h: u32) -> u32 {
    ((f64::from(cell_h) * BAR_RATIO).round() as u32).max(2)
}

/// 数据区间（不强制含 0）：折线 / 热力图用它做定标。
fn data_bounds(series: &[Series]) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for item in series {
        for point in item.points.iter().flatten() {
            min = min.min(point.value);
            max = max.max(point.value);
        }
    }
    (min <= max).then_some((min, max))
}

fn data_min(series: &Series) -> f64 {
    series
        .points
        .iter()
        .flatten()
        .map(|point| point.value)
        .fold(f64::INFINITY, f64::min)
}

fn data_max(series: &Series) -> f64 {
    series
        .points
        .iter()
        .flatten()
        .map(|point| point.value)
        .fold(f64::NEG_INFINITY, f64::max)
}

/// 取每个系列的最值点（轴刻度文本用），并列取先出现的。
fn max_point(series: &Series) -> Option<&Point> {
    series
        .points
        .iter()
        .flatten()
        .max_by(|a, b| a.value.total_cmp(&b.value))
}

/// 取最接近中值的点：轴上的中间刻度直接用真实单元格的写法。
fn mid_point(series: &Series) -> Option<&Point> {
    let (min, max) = (data_min(series), data_max(series));
    if !min.is_finite() || !max.is_finite() {
        return None;
    }
    if (max - min).abs() < f64::EPSILON {
        return series.points.iter().flatten().next();
    }
    let middle = min + (max - min) / 2.0;
    series.points.iter().flatten().min_by(|a, b| {
        (a.value - middle)
            .abs()
            .total_cmp(&(b.value - middle).abs())
    })
}

fn collect_texts(series: &[Series]) -> Vec<String> {
    series
        .iter()
        .flat_map(|item| item.points.iter().flatten().map(|point| point.text.clone()))
        .collect()
}

/// 成对图的差值文本：`Δ -1547（-20%）`；任一侧缺失时为空。
fn paired_deltas(series: &[Series]) -> Vec<String> {
    let Some(before) = series.first() else {
        return Vec::new();
    };
    let Some(after) = series.get(1) else {
        return Vec::new();
    };
    let rows = before.points.len().min(after.points.len());
    (0..rows)
        .map(|row| match (point(before, row), point(after, row)) {
            (Some(first), Some(second)) => {
                let delta = second.value - first.value;
                let change = if first.value.abs() > f64::EPSILON {
                    format!("（{:+.0}%）", delta / first.value.abs() * 100.0)
                } else {
                    String::new()
                };
                format!(
                    "Δ {}{change}",
                    format_value(delta, before.factor, &before.unit)
                )
            }
            _ => String::new(),
        })
        .collect()
}

/// 进度填充比例：百分比按 100 定标，分数按分母定标（`12/12` → 1.0）。
fn progress_ratio(point: &Point) -> f64 {
    let total = point.total.unwrap_or(if point.text.ends_with('%') {
        100.0
    } else {
        1.0
    });
    if total.abs() < f64::EPSILON {
        return 0.0;
    }
    (point.value / total).clamp(0.0, 1.0)
}

/// 进度条右侧文本：分式补上百分比（`12/12（100%）`），已是百分比的只写一次。
fn progress_label(series: &Series, row: usize) -> String {
    let Some(value) = point(series, row) else {
        return String::new();
    };
    if value.text.contains('/') {
        return format!("{}（{:.0}%）", value.text, progress_ratio(value) * 100.0);
    }
    if value.text.ends_with('%') {
        return value.text.clone();
    }
    format!("{}（{:.0}%）", value.text, progress_ratio(value) * 100.0)
}

/// 数据本身的区间（不内缩）：轴标签写真实极值，便于和表格逐格对照。
fn data_extent(pairs: &[(f64, f64)], pick: impl Fn(&(f64, f64)) -> f64) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for pair in pairs {
        let value = pick(pair);
        min = min.min(value);
        max = max.max(value);
    }
    if min.is_finite() && max.is_finite() {
        (min, max)
    } else {
        (0.0, 1.0)
    }
}

/// 带 6% 内边距的区间：散点不让点贴在边框上。
fn padded_bounds(pairs: &[(f64, f64)], pick: impl Fn(&(f64, f64)) -> f64) -> (f64, f64) {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for pair in pairs {
        let value = pick(pair);
        min = min.min(value);
        max = max.max(value);
    }
    if !min.is_finite() || !max.is_finite() {
        return (0.0, 1.0);
    }
    let span = (max - min).max(f64::EPSILON);
    (min - span * 0.06, max + span * 0.06)
}

#[cfg(test)]
mod tests {
    use super::*;

    use moc_core::chart::{Dimension, Point};
    use ratatui::style::Color;

    use crate::render::chart::with_fonts;
    use crate::render::graphics::CellPixels;
    use crate::theme::{ColorLevel, Theme};

    fn cell() -> CellPixels {
        CellPixels { w: 8, h: 18 }
    }

    fn point(value: f64, text: &str) -> Option<Point> {
        Some(Point {
            value,
            text: text.to_owned(),
            upper: None,
            total: None,
        })
    }

    fn series(name: &str, values: &[(f64, &str)]) -> Series {
        Series {
            name: name.to_owned(),
            dim: Dimension::Count,
            unit: String::new(),
            factor: 1.0,
            polarity: None,
            points: values
                .iter()
                .map(|(value, text)| point(*value, text))
                .collect(),
        }
    }

    fn spec(kind: ChartKind, categories: &[&str], series: Vec<Series>) -> ChartSpec {
        ChartSpec {
            kind,
            axis: "类别".into(),
            categories: categories.iter().map(|name| (*name).to_owned()).collect(),
            series,
            normalized: false,
            diverging: false,
            notes: vec![None; categories.len()],
        }
    }

    /// 栅格化一张测试图：48 列 × 8×18 的单元格。
    fn raster(spec: &ChartSpec) -> (Canvas, ChartLayout) {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let layout = super::super::layout(spec, 48, cell()).expect("测试版式应当可用");
        let canvas = with_fonts(|fonts| {
            let canvas = Canvas::new(layout.px.0, layout.px.1, theme.raw().bg_base);
            paint(canvas, spec, &layout, theme.raw(), fonts)
        });
        (canvas, layout)
    }

    fn rgb_at(canvas: &Canvas, x: u32, y: u32) -> [u8; 3] {
        canvas.palette()[usize::from(canvas.index_at(x, y))]
    }

    /// 某一行里连续等于 `rgb` 的像素数（用于量条形长度）。
    fn width_of(canvas: &Canvas, y: u32, rgb: [u8; 3]) -> u32 {
        (0..canvas.w())
            .filter(|x| rgb_at(canvas, *x, y) == rgb)
            .count() as u32
    }

    /// 某一行里 `rgb` 的最左 / 最右像素（用于量条形位置）。
    fn span_of(canvas: &Canvas, y: u32, rgb: [u8; 3]) -> Option<(u32, u32)> {
        let mut span: Option<(u32, u32)> = None;
        for x in 0..canvas.w() {
            if rgb_at(canvas, x, y) == rgb {
                span = Some(match span {
                    None => (x, x),
                    Some((left, _)) => (left, x),
                });
            }
        }
        span
    }

    fn chart_rgb(theme: &Theme, index: usize) -> [u8; 3] {
        theme.raw().chart[index]
    }

    fn bar_spec() -> ChartSpec {
        spec(
            ChartKind::Bar,
            &["a", "b", "c"],
            vec![series("耗时", &[(10.0, "10"), (20.0, "20"), (30.0, "30")])],
        )
    }

    /// 条形长度与数值成正比，且条从零基线起画、用系列 0 的颜色。
    #[test]
    fn bar_lengths_follow_values() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let color = chart_rgb(&theme, 0);
        let (canvas, _) = raster(&bar_spec());
        // 第 1/2/3 行分别是 a/b/c 的条（第 0 行是图例）
        let widths: Vec<u32> = (1..=3)
            .map(|row| width_of(&canvas, row * 18 + 9, color))
            .collect();
        assert!(
            widths[0] > 0 && widths[1] > 0 && widths[2] > 0,
            "{widths:?}"
        );
        assert!(
            widths[0] < widths[1] && widths[1] < widths[2],
            "长度必须随数值递增：{widths:?}"
        );
        let ratio = f64::from(widths[2]) / f64::from(widths[0]);
        assert!(
            (ratio - 3.0).abs() < 0.2,
            "10 与 30 的条长比应约为 3：{widths:?}"
        );
        // 三条的左端必须一致（同一零基线）
        let lefts: Vec<u32> = (1..=3)
            .map(|row| span_of(&canvas, row * 18 + 9, color).expect("有条形").0)
            .collect();
        assert!(
            lefts.iter().all(|left| *left == lefts[0]),
            "零基线不一致：{lefts:?}"
        );
    }

    /// 极差 100 倍以上时线性会把小值压成 1px；对数刻度下中值仍可读。
    #[test]
    fn bar_uses_log_scale_for_wide_positive_ranges() {
        let spec = spec(
            ChartKind::Bar,
            &["a", "b", "c"],
            vec![series(
                "量",
                &[(10.0, "10"), (1_000.0, "1000"), (100_000.0, "100000")],
            )],
        );
        let scale = ValueScale::new(&spec.series);
        assert!(scale.is_log(), "10000 倍极差应走对数刻度");
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let color = chart_rgb(&theme, 0);
        let (canvas, _) = raster(&spec);
        let widths: Vec<u32> = (1..=3)
            .map(|row| width_of(&canvas, row * 18 + 9, color))
            .collect();
        assert!(widths[0] <= 3, "最小值只需少量像素：{widths:?}");
        assert!(widths[1] > 40, "中值在对数刻度下必须明显可见：{widths:?}");
        assert!(widths[0] < widths[1] && widths[1] < widths[2], "{widths:?}");
    }

    /// 分组图：每个类别占系列数行，且每行只用对应系列的颜色。
    #[test]
    fn grouped_gives_each_series_its_own_row() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let spec = spec(
            ChartKind::Grouped,
            &["a", "b"],
            vec![
                series("读", &[(10.0, "10"), (20.0, "20")]),
                series("写", &[(30.0, "30"), (5.0, "5")]),
            ],
        );
        let (canvas, layout) = raster(&spec);
        assert_eq!(layout.rows, 1 + 2 * 2 + 1, "图例 + 2 类别 × 2 系列 + 轴");
        let first = chart_rgb(&theme, 0);
        let second = chart_rgb(&theme, 1);
        // 类别 a：行 1 = 读（系列 0），行 2 = 写（系列 1）
        assert!(
            width_of(&canvas, 18 + 9, first) > 0,
            "第 2 行应是系列 0 的条"
        );
        assert!(
            width_of(&canvas, 18 + 9, second) == 0,
            "第 2 行不该出现系列 1"
        );
        assert!(
            width_of(&canvas, 36 + 9, second) > 0,
            "第 3 行应是系列 1 的条"
        );
    }

    /// 成对图：每类别两行，行尾按极性给差值上色（耗时下降 = 变好 = 成功色）。
    #[test]
    fn paired_colours_the_delta_by_polarity() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let mut before = series("改前", &[(100.0, "100"), (80.0, "80")]);
        let mut after = series("改后", &[(60.0, "60"), (90.0, "90")]);
        after.polarity = Some(Polarity::Lower);
        before.polarity = Some(Polarity::Lower);
        let spec = spec(ChartKind::Paired, &["a", "b"], vec![before, after]);
        let (canvas, layout) = raster(&spec);
        assert_eq!(layout.rows, 1 + 2 * 2 + 1);
        let success = theme.raw().accent_success;
        let error = theme.raw().accent_error;
        // 差值文本在每对的后一行：a 下降（成功色）、b 上升（错误色）
        let row_a = 2 * 18 + 9;
        let row_b = 4 * 18 + 9;
        assert!(
            (0..canvas.w()).any(|x| rgb_at(&canvas, x, row_a) == success),
            "a 的差值应用成功色"
        );
        assert!(
            (0..canvas.w()).any(|x| rgb_at(&canvas, x, row_b) == error),
            "b 的差值应用错误色"
        );
    }

    /// 折线：绘图区里能同时看到系列色折线与顶点圆点。
    #[test]
    fn line_connects_the_points() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let color = chart_rgb(&theme, 0);
        let spec = spec(
            ChartKind::Line,
            &["a", "b", "c"],
            vec![series("延迟", &[(10.0, "10"), (30.0, "30"), (20.0, "20")])],
        );
        let (canvas, layout) = raster(&spec);
        assert_eq!(layout.rows, 1 + 6 + 1, "折线固定 6 行绘图区");
        let ink = (0..canvas.h())
            .flat_map(|y| (0..canvas.w()).map(move |x| (x, y)))
            .filter(|(x, y)| rgb_at(&canvas, *x, *y) == color)
            .count();
        assert!(ink > 60, "折线与圆点应有足够像素：{ink}");
        // 顶点必须落在绘图区内（图例行第 0 行有图例色块，不算）
        let top = 18u32;
        let bottom = 6 * 18 + 18;
        let rows: Vec<u32> = (0..canvas.h())
            .filter(|y| *y >= top)
            .filter(|y| (0..canvas.w()).any(|x| rgb_at(&canvas, x, *y) == color))
            .collect();
        assert!(!rows.is_empty(), "绘图区里应有折线");
        assert!(
            rows.iter().all(|y| *y <= bottom),
            "折线越出绘图区：{rows:?}"
        );
    }

    /// 堆叠图：各段之和恰好等于条长（不允许舍入留缝）。
    #[test]
    fn stacked_segments_fill_the_track() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let spec = spec(
            ChartKind::Stacked,
            &["a", "b"],
            vec![
                series("读", &[(80.0, "80"), (10.0, "10")]),
                series("写", &[(20.0, "20"), (10.0, "10")]),
            ],
        );
        let (canvas, _) = raster(&spec);
        let first = chart_rgb(&theme, 0);
        let second = chart_rgb(&theme, 1);
        for row in 1..=2u32 {
            let y = row * 18 + 9;
            let total = width_of(&canvas, y, first) + width_of(&canvas, y, second);
            // 同一类别的满格长度必须一致（条总长不由数值决定，而由轨道长度决定）
            assert!(total > 0, "第 {row} 行应有色块");
            let span = span_of(&canvas, y, first)
                .zip(span_of(&canvas, y, second))
                .expect("两段都应存在");
            assert!(span.0.0 < span.1.0, "系列顺序应从左到右：{span:?}");
            assert_eq!(
                span.1.1 - span.0.0 + 1,
                total,
                "两段之间不该有空隙（第 {row} 行）"
            );
        }
        // a 行（30/100）的第一段应短于 b 行（10/20）
        assert!(
            width_of(&canvas, 18 + 9, first) > width_of(&canvas, 36 + 9, first),
            "份额大的类别第一段更长"
        );
    }

    /// 进度条：填充长度与比例一致，超出 100% 的被夹紧。
    #[test]
    fn progress_fills_by_ratio() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let mut progress = series("进度", &[(50.0, "50%"), (120.0, "120%")]);
        progress.dim = Dimension::Percent;
        for point in progress.points.iter_mut().flatten() {
            point.total = Some(100.0);
        }
        let spec = spec(ChartKind::Progress, &["半", "满"], vec![progress]);
        let (canvas, layout) = raster(&spec);
        assert_eq!(layout.rows, 2, "进度图只有类别行");
        let color = chart_rgb(&theme, 0);
        let half_width = width_of(&canvas, 9, color);
        let full_width = width_of(&canvas, 27, color);
        assert!(half_width > 0, "半程应有填充");
        assert!(full_width > half_width, "满程应长于半程");
        assert!(
            (f64::from(full_width) / f64::from(half_width) - 2.0).abs() < 0.2,
            "比例应约为 2：{half_width} vs {full_width}"
        );
        let _ = Color::Reset;
    }

    /// 热力图：数值越大越接近系列色（亮度递增），且每格有填充。
    #[test]
    fn heatmap_darkens_by_value() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let spec = spec(
            ChartKind::Heatmap,
            &["a", "b", "c"],
            vec![
                series("读", &[(1.0, "1"), (5.0, "5"), (9.0, "9")]),
                series("写", &[(9.0, "9"), (5.0, "5"), (1.0, "1")]),
            ],
        );
        let (canvas, layout) = raster(&spec);
        assert_eq!(layout.rows, 2 + 3, "顶部色标 + 列头 + 3 类别，无底部色标行");
        let brightness = |x: u32, y: u32| -> u32 {
            let rgb = rgb_at(&canvas, x, y);
            u32::from(rgb[0]) + u32::from(rgb[1]) + u32::from(rgb[2])
        };
        // 数据行从第 2 行开始：列 0（读）的 row a（值 1）应比 row c（值 9）暗。
        let x = (PAD_X + 60).min(canvas.w() - 1);
        let dark = brightness(x, 2 * 18 + 9);
        let bright = brightness(x, 4 * 18 + 9);
        assert!(bright > dark, "大值应更亮：{dark} vs {bright}");
        let _ = theme;
    }

    /// 散点：每个数据点附近有圆点像素，且点数不超过类别数。
    #[test]
    fn scatter_places_points() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let color = chart_rgb(&theme, 0);
        let rows = (0..8)
            .map(|index| (index as f64 * 10.0, format!("{:.0}", index as f64 * 10.0)))
            .collect::<Vec<_>>();
        let ys: Vec<(f64, String)> = (0..8)
            .map(|index| {
                (
                    index as f64 * 5.0 + 1.0,
                    format!("{:.0}", index as f64 * 5.0 + 1.0),
                )
            })
            .collect();
        let x_series = series(
            "x",
            &rows
                .iter()
                .map(|(value, text)| (*value, text.as_str()))
                .collect::<Vec<_>>(),
        );
        let y_series = series(
            "y",
            &ys.iter()
                .map(|(value, text)| (*value, text.as_str()))
                .collect::<Vec<_>>(),
        );
        let spec = spec(
            ChartKind::Scatter,
            &["0", "1", "2", "3", "4", "5", "6", "7"],
            vec![x_series, y_series],
        );
        let (canvas, layout) = raster(&spec);
        assert_eq!(layout.rows, 1 + 12 + 2);
        let ink = (0..canvas.h())
            .flat_map(|y| (0..canvas.w()).map(move |x| (x, y)))
            .filter(|(x, y)| rgb_at(&canvas, *x, *y) == color)
            .count();
        assert!(ink > 8, "至少落下 8 个点：{ink}");
        assert!(ink < 8 * 64, "点数远小于绘图区面积：{ink}");
    }

    /// 用户示例表：3 个数学列 + 73,728 倍极差 → 分组条 + 对数刻度，三条都可见。
    #[test]
    fn preview_sample_table_uses_grouped_log_scale() {
        let theme = Theme::resolved(ColorLevel::TrueColor);
        let spec = spec(
            ChartKind::Grouped,
            &["1", "2", "3", "4", "5", "6"],
            vec![
                series(
                    "数值",
                    &[
                        (12.0, "12"),
                        (27.0, "27"),
                        (45.0, "45"),
                        (68.0, "68"),
                        (83.0, "83"),
                        (96.0, "96"),
                    ],
                ),
                series(
                    "平方",
                    &[
                        (144.0, "144"),
                        (729.0, "729"),
                        (2_025.0, "2,025"),
                        (4_624.0, "4,624"),
                        (6_889.0, "6,889"),
                        (9_216.0, "9,216"),
                    ],
                ),
                series(
                    "立方",
                    &[
                        (1_728.0, "1,728"),
                        (19_683.0, "19,683"),
                        (91_125.0, "91,125"),
                        (314_432.0, "314,432"),
                        (571_787.0, "571,787"),
                        (884_736.0, "884,736"),
                    ],
                ),
            ],
        );
        let layout = super::super::layout(&spec, 90, cell()).expect("示例表可出图");
        assert_eq!(layout.px.0, 720, "90 列 × 8px");
        assert_eq!(layout.rows, 20, "图例 + 6 类别 × 3 系列 + 轴");
        let scale = ValueScale::new(&spec.series);
        assert!(scale.is_log(), "极差 73,728 倍必须走对数刻度");
        let canvas = with_fonts(|fonts| {
            let canvas = Canvas::new(layout.px.0, layout.px.1, theme.raw().bg_base);
            paint(canvas, &spec, &layout, theme.raw(), fonts)
        });
        for index in 0..3 {
            let color = chart_rgb(&theme, index);
            let ink = (18..canvas.h())
                .flat_map(|y| (0..canvas.w()).map(move |x| (x, y)))
                .filter(|(x, y)| rgb_at(&canvas, *x, *y) == color)
                .count();
            assert!(ink > 100, "系列 {index} 的条必须可见：{ink}");
        }
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/chart-preview");
        std::fs::create_dir_all(&dir).expect("创建预览目录");
        std::fs::write(
            dir.join("Sample-Grouped.png"),
            super::super::encode::png(&canvas),
        )
        .expect("写出示例预览图");
    }

    /// 预览落盘：八种图型各出一张 PNG 供肉眼验收（顺带钉住尺寸与 PNG 头）。
    #[test]
    fn preview_renders_every_kind() {
        use moc_core::chart::ChartKind as Kind;

        let cases: Vec<(Kind, ChartSpec)> = vec![
            (Kind::Bar, bar_spec()),
            (
                Kind::Grouped,
                spec(
                    Kind::Grouped,
                    &["解析", "渲染", "测试"],
                    vec![
                        series("改前", &[(120.0, "120"), (80.0, "80"), (40.0, "40")]),
                        series("改后", &[(60.0, "60"), (50.0, "50"), (20.0, "20")]),
                    ],
                ),
            ),
            {
                let mut before = series("改前", &[(120.0, "120"), (80.0, "80"), (40.0, "40")]);
                let mut after = series("改后", &[(60.0, "60"), (95.0, "95"), (20.0, "20")]);
                before.polarity = Some(Polarity::Lower);
                after.polarity = Some(Polarity::Lower);
                (
                    Kind::Paired,
                    spec(Kind::Paired, &["解析", "渲染", "测试"], vec![before, after]),
                )
            },
            (
                Kind::Line,
                spec(
                    Kind::Line,
                    &["周一", "周二", "周三", "周四", "周五"],
                    vec![
                        series(
                            "延迟",
                            &[
                                (10.0, "10"),
                                (30.0, "30"),
                                (20.0, "20"),
                                (45.0, "45"),
                                (15.0, "15"),
                            ],
                        ),
                        series(
                            "错误",
                            &[(2.0, "2"), (5.0, "5"), (1.0, "1"), (8.0, "8"), (3.0, "3")],
                        ),
                    ],
                ),
            ),
            {
                let mut item = spec(
                    Kind::Stacked,
                    &["解析", "渲染", "测试"],
                    vec![
                        series("读", &[(30.0, "30"), (10.0, "10"), (5.0, "5")]),
                        series("写", &[(70.0, "70"), (10.0, "10"), (5.0, "5")]),
                    ],
                );
                item.normalized = true;
                (Kind::Stacked, item)
            },
            {
                let mut half = series("进度", &[(50.0, "50%"), (100.0, "100%"), (80.0, "80%")]);
                half.dim = Dimension::Percent;
                for point in half.points.iter_mut().flatten() {
                    point.total = Some(100.0);
                }
                (
                    Kind::Progress,
                    spec(Kind::Progress, &["半程", "完成", "八成"], vec![half]),
                )
            },
            (
                Kind::Heatmap,
                spec(
                    Kind::Heatmap,
                    &["解析", "渲染", "测试"],
                    vec![
                        series("读", &[(12.0, "12"), (48.0, "48"), (30.0, "30")]),
                        series("写", &[(8.0, "8"), (52.0, "52"), (21.0, "21")]),
                        series("等", &[(4.0, "4"), (33.0, "33"), (9.0, "9")]),
                    ],
                ),
            ),
            {
                let x = series(
                    "输入 token",
                    &[
                        (1.0, "1"),
                        (2.0, "2"),
                        (3.0, "3"),
                        (4.0, "4"),
                        (5.0, "5"),
                        (6.0, "6"),
                    ],
                );
                let y = series(
                    "输出 token",
                    &[
                        (9.0, "9"),
                        (4.0, "4"),
                        (7.0, "7"),
                        (2.0, "2"),
                        (8.0, "8"),
                        (5.0, "5"),
                    ],
                );
                (
                    Kind::Scatter,
                    spec(Kind::Scatter, &["a", "b", "c", "d", "e", "f"], vec![x, y]),
                )
            },
        ];
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/chart-preview");
        std::fs::create_dir_all(&dir).expect("创建预览目录");
        for (kind, spec) in cases {
            let (canvas, layout) = raster(&spec);
            assert_eq!(
                canvas.w() * canvas.h(),
                layout.px.0 * layout.px.1,
                "{kind:?} 画布尺寸必须等于版式像素尺寸"
            );
            let png = super::super::encode::png(&canvas);
            assert_eq!(&png[..8], b"\x89PNG\r\n\x1a\n", "{kind:?} PNG 头");
            let path = dir.join(format!("{kind:?}.png"));
            std::fs::write(&path, &png).expect("写出预览图");
            assert!(png.len() > 512, "{kind:?} 预览图不应为空");
        }
    }
}

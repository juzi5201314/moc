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

use moc_core::chart::{ChartKind, ChartSpec, Point, Series};

use super::ChartLayout;
use super::fonts::Fonts;
use super::text;
use crate::render::canvas::Canvas;
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

mod bars;
mod plots;

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
mod tests;

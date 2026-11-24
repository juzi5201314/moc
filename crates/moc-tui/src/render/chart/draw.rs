//! 九种图型的矢量 SVG 生成（透明背景、圆角胶囊条、圆点图例）。
//!
//! 所有图型共用一套几何约定：
//! - 坐标一律按画布像素计算，`(x, y)` 是左上角，`row_top(row)` 是该行首像素行；
//! - 文本基线固定在 `row_top(row) + ChartLayout::baseline`，因此同一行的文本、
//!   圆点图例、胶囊条天然垂直对齐；
//! - 左侧类别标签列宽度按内容取上限（不超过图宽的 [`LABEL_BUDGET`]），
//!   右侧给数值 / 差值文本留出宽裕度，条形只在中间的可变区伸缩；
//! - 根元素不铺设任何不透明背景矩形，由 `resvg` 产出透明 RGBA 融入终端背景。
//!
//! 数值一律以 `Point::text` 的原样书写显示（不二次格式化）；
//! 只有「和 / 差 / 刻度」这种派生量才用 [`format_value`]。

use std::fmt::Write as _;

use moc_core::chart::{ChartKind, ChartSpec, Point, Series};
use moc_i18n::t;

use super::ChartLayout;
use super::fonts::Fonts;
use crate::theme::RawPalette;

/// 左右留白（像素）。
const PAD_X: u32 = 10;
/// 元素之间的横向间距（像素）。
const GAP: u32 = 6;
/// 类别标签列占图宽的上限比例。
const LABEL_BUDGET: f64 = 0.35;
/// 条形高度占行高的比例：留足上下呼吸感，视觉利落不糊成墙。
const BAR_RATIO: f64 = 0.50;
/// 图例圆点半径（像素）。
const DOT_R: f64 = 4.0;
/// 图例圆点直径占位（像素）。
const SWATCH: u32 = 8;
/// 热力图的混色档数：5 档渐变层次清晰。
const HEAT_STEPS: usize = 5;

/// 渲染出口：生成与 [`ChartLayout`] 同尺寸的透明 SVG 文本。
pub fn render_svg(
    spec: &ChartSpec,
    layout: &ChartLayout,
    palette: &'static RawPalette,
    fonts: &mut Fonts,
) -> String {
    let svg = SvgBuilder::new(layout.px.0, layout.px.1, fonts.css_family(), layout.px_size);
    let mut painter = Painter {
        svg,
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
        ChartKind::Panels => painter.panels(),
    }
    painter.svg.finish()
}

/// 轻量 SVG 矢量构建器（无背景矩形，保留透明通道）。
pub(super) struct SvgBuilder {
    buf: String,
    clip_seq: u32,
}

impl SvgBuilder {
    pub(super) fn new(w: u32, h: u32, css_family: &str, px: u32) -> Self {
        Self::with_capacity(4096, w, h, css_family, px)
    }

    pub(super) fn with_capacity(
        capacity: usize,
        w: u32,
        h: u32,
        css_family: &str,
        px: u32,
    ) -> Self {
        let mut buf = String::with_capacity(capacity);
        let escaped_font = xml_escape(css_family);
        let _ = write!(
            buf,
            "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{w}\" height=\"{h}\" viewBox=\"0 0 {w} {h}\">\
             <g font-family=\"{escaped_font}\" font-size=\"{px}\">"
        );
        Self { buf, clip_seq: 0 }
    }

    pub(super) fn finish(mut self) -> String {
        self.buf.push_str("</g></svg>");
        self.buf
    }

    fn rect(&mut self, x: u32, y: u32, w: u32, h: u32, rx: f64, rgb: [u8; 3]) {
        if w == 0 || h == 0 {
            return;
        }
        let color = hex(rgb);
        if rx > 0.0 {
            let _ = write!(
                self.buf,
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"{rx:.2}\" ry=\"{rx:.2}\" fill=\"{color}\"/>"
            );
        } else {
            let _ = write!(
                self.buf,
                "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" fill=\"{color}\"/>"
            );
        }
    }

    /// 半透明圆角胶囊底槽（进度条背景轨道，融入透明终端背景）。
    fn track_pill(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
        if w == 0 || h == 0 {
            return;
        }
        let rx = f64::from(h) / 2.0;
        let color = hex(rgb);
        let _ = write!(
            self.buf,
            "<rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"{rx:.2}\" ry=\"{rx:.2}\" fill=\"{color}\" fill-opacity=\"0.55\"/>"
        );
    }

    /// 按底槽轮廓裁切填充，满值时与轨道的圆角完全重合。
    fn progress_fill(&mut self, x: u32, y: u32, w: u32, h: u32, filled: u32, rgb: [u8; 3]) {
        self.clip_seq += 1;
        let id = self.clip_seq;
        let rx = f64::from(h) / 2.0;
        let _ = write!(
            self.buf,
            "<defs><clipPath id=\"c{id}\"><rect x=\"{x}\" y=\"{y}\" width=\"{w}\" height=\"{h}\" rx=\"{rx:.2}\"/></clipPath></defs><g clip-path=\"url(#c{id})\">"
        );
        self.rect(x, y, filled.min(w), h, 0.0, rgb);
        self.buf.push_str("</g>");
    }

    /// 圆角胶囊条：圆角半径取 `min(h/2, w/2, 4.0)`，兼顾短条与长条比例。
    fn pill(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
        if w == 0 || h == 0 {
            return;
        }
        let rx = (f64::from(h) / 2.0).min(f64::from(w) / 2.0).min(4.0);
        self.rect(x, y, w, h, rx, rgb);
    }

    /// 堆叠分段：用外层圆角 clipPath 裁出胶囊两端，内部各整数段紧密相邻不留缝。
    fn clipped_segments(
        &mut self,
        x: u32,
        y: u32,
        total_w: u32,
        h: u32,
        segments: &[(u32, [u8; 3])],
    ) {
        if total_w == 0 || h == 0 || segments.is_empty() {
            return;
        }
        self.clip_seq += 1;
        let id = self.clip_seq;
        let rx = (f64::from(h) / 2.0).min(f64::from(total_w) / 2.0).min(4.0);
        let _ = write!(
            self.buf,
            "<defs><clipPath id=\"c{id}\"><rect x=\"{x}\" y=\"{y}\" width=\"{total_w}\" height=\"{h}\" rx=\"{rx:.2}\" ry=\"{rx:.2}\"/></clipPath></defs>\
             <g clip-path=\"url(#c{id})\">"
        );
        let mut cursor = x;
        for &(w, rgb) in segments {
            if w > 0 {
                self.rect(cursor, y, w, h, 0.0, rgb);
                cursor += w;
            }
        }
        self.buf.push_str("</g>");
    }

    fn circle(&mut self, cx: f64, cy: f64, r: f64, rgb: [u8; 3]) {
        let color = hex(rgb);
        let _ = write!(
            self.buf,
            "<circle cx=\"{cx:.2}\" cy=\"{cy:.2}\" r=\"{r:.2}\" fill=\"{color}\"/>"
        );
    }

    fn line(&mut self, x1: f64, y1: f64, x2: f64, y2: f64, stroke_w: f64, rgb: [u8; 3]) {
        let color = hex(rgb);
        let _ = write!(
            self.buf,
            "<line x1=\"{x1:.2}\" y1=\"{y1:.2}\" x2=\"{x2:.2}\" y2=\"{y2:.2}\" stroke=\"{color}\" stroke-width=\"{stroke_w:.2}\"/>"
        );
    }

    fn polyline(&mut self, pts: &[(f64, f64)], stroke_w: f64, rgb: [u8; 3]) {
        if pts.len() < 2 {
            return;
        }
        let color = hex(rgb);
        let _ = write!(
            self.buf,
            "<polyline fill=\"none\" stroke=\"{color}\" stroke-width=\"{stroke_w:.2}\" stroke-linecap=\"round\" stroke-linejoin=\"round\" points=\""
        );
        for (idx, (x, y)) in pts.iter().enumerate() {
            if idx > 0 {
                self.buf.push(' ');
            }
            let _ = write!(self.buf, "{x:.2},{y:.2}");
        }
        self.buf.push_str("\"/>");
    }

    pub(super) fn text(&mut self, x: u32, baseline: i32, content: &str, rgb: [u8; 3]) {
        if content.is_empty() {
            return;
        }
        let color = hex(rgb);
        let escaped = xml_escape(content);
        let _ = write!(
            self.buf,
            "<text x=\"{x}\" y=\"{baseline}\" fill=\"{color}\">{escaped}</text>"
        );
    }
}

fn hex(rgb: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2])
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            '\u{9}'
            | '\u{a}'
            | '\u{d}'
            | '\u{20}'..='\u{d7ff}'
            | '\u{e000}'..='\u{fffd}'
            | '\u{10000}'..='\u{10ffff}' => out.push(ch),
            _ => out.push('\u{fffd}'),
        }
    }
    out
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
        for point in item
            .points
            .iter()
            .flatten()
            .filter(|point| point.value.is_finite())
        {
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
const LOG_SCALE_RATIO: f64 = 100.0;

/// 条形 / 分组条 / 成对条 / 分面共用的横向刻度。
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

    fn linear(series: &[Series]) -> Self {
        let (min, max) = bounds(series).unwrap_or((0.0, 1.0));
        Self::Linear { min, max }
    }

    fn is_log(self) -> bool {
        matches!(self, Self::Log { .. })
    }

    fn has_negative(self) -> bool {
        matches!(self, Self::Linear { min, .. } if min < 0.0)
    }

    fn has_positive(self) -> bool {
        matches!(self, Self::Linear { max, .. } if max > 0.0)
    }

    /// 数值在横向轨道里的比例（0..1）。
    fn ratio(self, value: f64) -> f64 {
        match self {
            Self::Linear { min, max } => linear_ratio(value, min, max),
            Self::Log { min, max } => log_ratio(value, min, max),
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

/// 非零跨度不设绝对下限；跨符号极值先减半，避免两个有限端点相减溢出。
fn linear_ratio(value: f64, min: f64, max: f64) -> f64 {
    if !value.is_finite() || !min.is_finite() || !max.is_finite() || min >= max {
        return 0.0;
    }
    let span = max - min;
    let ratio = if span.is_finite() {
        (value - min) / span
    } else {
        (value * 0.5 - min * 0.5) / (max * 0.5 - min * 0.5)
    };
    ratio.clamp(0.0, 1.0)
}

/// 全正对数比例：用对数差替代比值取对数，避免极端正跨度 `max / min` 溢出。
fn log_ratio(value: f64, min: f64, max: f64) -> f64 {
    if !value.is_finite() || !min.is_finite() || !max.is_finite() || min <= 0.0 || min >= max {
        return 0.0;
    }
    let min_ln = min.ln();
    let span = max.ln() - min_ln;
    if span == 0.0 {
        0.0
    } else {
        ((value.max(min).ln() - min_ln) / span).clamp(0.0, 1.0)
    }
}

/// 按 8 位 alpha 把前景混到背景上（热力图 5 档格色）。
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
    32 + (step.min(HEAT_STEPS - 1) as u32 * 192) / (HEAT_STEPS as u32 - 1)
}

struct Painter<'a> {
    svg: SvgBuilder,
    fonts: &'a mut Fonts,
    spec: &'a ChartSpec,
    layout: &'a ChartLayout,
    palette: &'static RawPalette,
}

mod bars;
mod plots;

impl Painter<'_> {
    fn w(&self) -> u32 {
        self.layout.px.0
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
        let width = self.text_width(label);
        self.svg.text(x, baseline, label, color);
        x.saturating_add(width)
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

    fn pill(&mut self, x: u32, y: u32, w: u32, h: u32, rgb: [u8; 3]) {
        self.svg.pill(x, y, w, h, rgb);
    }

    fn hline(&mut self, x0: u32, x1: u32, y: u32, rgb: [u8; 3]) {
        let lo = x0.min(x1);
        let hi = x0.max(x1);
        self.svg.line(
            f64::from(lo),
            f64::from(y) + 0.5,
            f64::from(hi),
            f64::from(y) + 0.5,
            1.0,
            rgb,
        );
    }

    fn vline(&mut self, x: u32, y0: u32, y1: u32, rgb: [u8; 3]) {
        let lo = y0.min(y1);
        let hi = y0.max(y1);
        self.svg.line(
            f64::from(x) + 0.5,
            f64::from(lo),
            f64::from(x) + 0.5,
            f64::from(hi),
            1.0,
            rgb,
        );
    }

    /// 图例圆点：与该行文本中心对齐。
    fn dot(&mut self, x: u32, row: u16, rgb: [u8; 3]) {
        let cx = f64::from(x) + DOT_R;
        let cy = f64::from(self.row_top(row)) + f64::from(self.cell_h()) / 2.0;
        self.svg.circle(cx, cy, DOT_R, rgb);
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

    /// 图例行：圆点 + 系列名；单系列时顺带写出书写单位。返回结束笔位。
    fn legend(&mut self) -> u32 {
        let spec = self.spec;
        let limit = self.w().saturating_sub(PAD_X);
        let mut x = PAD_X;
        for (index, series) in spec.series.iter().enumerate() {
            if x + SWATCH + GAP >= limit {
                break;
            }
            self.dot(x, 0, self.series_color(index));
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

    /// 底轴：基线 + 刻度。
    ///
    /// 线性刻度画最小（含负值时）/ 零 / 中 / 最大；对数刻度画最小 / 几何中值 / 最大。
    /// 刻度文本按互斥区间排版，间距不足时不硬叠画。
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
        let mut occupied: Vec<(u32, u32)> = Vec::with_capacity(4);
        let mut try_label = |painter: &mut Self, x: u32, text: &str| {
            let width = painter.text_width(text);
            let right = x.saturating_add(width);
            if occupied
                .iter()
                .any(|&(lo, hi)| x < hi.saturating_add(2) && right.saturating_add(2) > lo)
            {
                return;
            }
            occupied.push((x, right));
            painter.text(row, x, text, gray);
        };
        if scale.is_log() {
            if !min.is_empty() {
                self.vline(track.0, y, y + 3, border);
                try_label(self, track.0, min);
            }
            if !max.is_empty() {
                let width = self.text_width(max);
                let right = track.1;
                self.vline(right.saturating_sub(width / 2), y, y + 3, border);
                try_label(self, right.saturating_sub(width), max);
            }
            if !mid.is_empty() {
                let width = self.text_width(mid);
                self.vline(center, y, y + 3, border);
                try_label(self, center.saturating_sub(width / 2), mid);
            }
            return;
        }
        if !min.is_empty() {
            self.vline(track.0, y, y + 3, border);
            try_label(self, track.0, min);
        }
        if !max.is_empty() {
            let width = self.text_width(max);
            let right = track.1;
            self.vline(right, y, y + 3, border);
            try_label(self, right.saturating_sub(width), max);
        }
        let zero_x = scale.origin(track);
        self.vline(zero_x, y, y + 3, border);
        let zero_width = self.text_width("0");
        let zero_text_x = if zero_x >= track.1.saturating_sub(zero_width / 2) {
            track.1.saturating_sub(zero_width)
        } else {
            zero_x.saturating_sub(zero_width / 2)
        };
        try_label(self, zero_text_x, "0");
        if !mid.is_empty() {
            let width = self.text_width(mid);
            self.vline(center, y, y + 3, border);
            try_label(self, center.saturating_sub(width / 2), mid);
        }
    }

    /// 对数刻度时在图例右侧补一条注记（空间不足就不写，避免压住系列名）。
    fn scale_note(&mut self, legend_end: u32, scale: ValueScale) {
        if !scale.is_log() {
            return;
        }
        let note = t!("tui.chart.log_note");
        let width = self.text_width(&note);
        let right = self.w().saturating_sub(PAD_X);
        if legend_end.saturating_add(GAP) < right.saturating_sub(width) {
            self.text_end(0, right, &note, self.palette.gray_dim);
        }
    }
}

/// 条形在行内的上边距（剩余空间上下均分）。
fn bar_inset(cell_h: u32) -> u32 {
    (cell_h - bar_height(cell_h)) / 2
}

/// 条形高度：行高的 50%，至少 4px。
fn bar_height(cell_h: u32) -> u32 {
    ((f64::from(cell_h) * BAR_RATIO).round() as u32).max(4)
}

/// 数据区间（不强制含 0）：折线 / 热力图用它做定标。
fn data_bounds(series: &[Series]) -> Option<(f64, f64)> {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    for item in series {
        for point in item
            .points
            .iter()
            .flatten()
            .filter(|point| point.value.is_finite())
        {
            min = min.min(point.value);
            max = max.max(point.value);
        }
    }
    (min <= max).then_some((min, max))
}

/// 取全部系列里的最小值点（负侧轴刻度用），并列取先出现的。
fn min_point(series: &[Series]) -> Option<&Point> {
    series
        .iter()
        .flat_map(|item| item.points.iter().flatten())
        .filter(|point| point.value.is_finite())
        .min_by(|a, b| a.value.total_cmp(&b.value))
}

/// 取全部系列里的最大值点（轴刻度文本用），并列取先出现的。
fn max_point(series: &[Series]) -> Option<&Point> {
    series
        .iter()
        .flat_map(|item| item.points.iter().flatten())
        .filter(|point| point.value.is_finite())
        .max_by(|a, b| a.value.total_cmp(&b.value))
}

/// 取最接近全部系列中值的点：轴上的中间刻度直接用真实单元格的写法。
fn mid_point(series: &[Series]) -> Option<&Point> {
    let (min, max) = data_bounds(series)?;
    if min == max {
        return series
            .iter()
            .flat_map(|item| item.points.iter().flatten())
            .find(|point| point.value.is_finite());
    }
    let middle = min * 0.5 + max * 0.5;
    series
        .iter()
        .flat_map(|item| item.points.iter().flatten())
        .filter(|point| point.value.is_finite())
        .min_by(|a, b| {
            (a.value - middle)
                .abs()
                .total_cmp(&(b.value - middle).abs())
        })
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

#[cfg(test)]
mod tests;

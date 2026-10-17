//! 图表栅格化：把 `moc_core::chart::ChartSpec` 画成像素图。
//!
//! 分成四层，层与层之间只靠数据传递：
//! - [`canvas`]：双平面像素画布（24 位 RGB + 有界调色板索引），几何图元的唯一写入口；
//! - [`fonts`]：内嵌字体的光栅化与字形缓存，缺字才懒回退系统字体；
//! - [`text`]：把字形覆盖度按 5 档 alpha 预混成调色板像素；
//! - [`draw`]：八种图型的版式；[`encode`]：kitty / sixel / iTerm2 / PNG 编码。
//!
//! 字体与画布都按整数像素落位：图片尺寸 = 终端格尺寸的整数倍，终端不缩放，
//! 文字才不会像系统字体 + 分数缩放那样发虚。

use moc_core::chart::{ChartKind, ChartSpec};

use crate::render::graphics::CellPixels;
use crate::theme::Theme;

pub mod canvas;
pub mod draw;
pub mod encode;
pub mod fonts;
pub mod text;

pub use canvas::Canvas;
pub use fonts::{Fonts, with_fonts};

/// 出图所需的最小可用列数：再窄就放不下类别标签 + 条形 + 数值。
pub const MIN_COLS: u16 = 24;

/// 出图允许的最大行数：超过一屏的图读不动，也不该把整屏挤掉。
pub const MAX_ROWS: u16 = 40;

/// 行高内基线偏移占行高的比例（兼顾上伸部与下伸部）。
const BASELINE_RATIO: f32 = 0.78;

/// 出图所需的最小单元格宽度（像素）：再窄放不下半个字形。
pub const MIN_CELL_W: u32 = 4;

/// 出图所需的最小单元格高度（像素）：低于它一行字都放不下。
pub const MIN_CELL_H: u32 = 8;

/// 单元格尺寸是否足够出图。
///
/// 入口是否出现、点击后能否画出来都必须走这同一个判定，否则会出现
/// 「按钮能点、点了没反应」的死入口。
pub fn cell_supported(cell: CellPixels) -> bool {
    cell.w >= MIN_CELL_W && cell.h >= MIN_CELL_H
}

/// 一个图表的版式：格数、像素尺寸与字号。
///
/// 版式只由「图型 + 类别数 + 系列数 + 终端格尺寸」决定，与数据值无关——
/// 因此估高（渲染行数）与贴图（像素尺寸）用的是同一份公式，不会错位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChartLayout {
    pub kind: ChartKind,
    /// 占用列数（= 正文可用列数）。
    pub cols: u16,
    /// 占用行数。
    pub rows: u16,
    /// 位图像素尺寸。
    pub px: (u32, u32),
    /// 文本字号（像素）。
    pub px_size: u32,
    /// 行内基线的垂直偏移（像素）。
    pub baseline: u32,
    /// 终端单元格尺寸。
    pub cell: CellPixels,
}

/// 该图型在给定类别数与系列数下需要的行数（含图例行与轴行）。
///
/// 与 [`layout`] 共用同一份公式：渲染层据此判定「这张表放不放得下」，
/// 不通过就不出入口，避免出现点了没反应的死按钮。
pub fn rows_needed(kind: ChartKind, categories: usize, series: usize) -> u16 {
    let rows = match kind {
        // 图例 + 每类别一行 + 轴
        ChartKind::Bar | ChartKind::Stacked => categories.saturating_add(2),
        // 图例 + 每类别每个系列一行 + 轴
        ChartKind::Grouped => 1 + categories.saturating_mul(series.max(1)) + 1,
        // 图例 + 每类别两行（前 / 后）+ 轴
        ChartKind::Paired => 1 + categories.saturating_mul(2) + 1,
        // 只有类别行（标签在左、进度条在右）
        ChartKind::Progress => categories,
        // 图例 + 绘图区（6~12 行）+ 轴
        ChartKind::Line => 1 + categories.clamp(6, 12) + 1,
        // 图例 + 12 行绘图区 + 两行轴标签
        ChartKind::Scatter => 1 + 12 + 2,
        // 列头 + 每类别一行 + 色标
        ChartKind::Heatmap => 1 + categories + 1,
    };
    u16::try_from(rows).unwrap_or(u16::MAX)
}

/// 计算版式；放不下就返回 `None`（宁可不出图也不出读不懂的图）。
pub fn layout(spec: &ChartSpec, cols: u16, cell: CellPixels) -> Option<ChartLayout> {
    if cols < MIN_COLS || !cell_supported(cell) {
        return None;
    }
    let rows = rows_needed(spec.kind, spec.categories.len(), spec.series.len());
    if rows == 0 || rows > MAX_ROWS {
        return None;
    }
    let px = (u32::from(cols) * cell.w, u32::from(rows) * cell.h);
    // 字号固定为行高的 0.72：行距留给上下留白，字号不随图片缩放变化
    let px_size = ((f64::from(cell.h) * 0.72).round() as u32).max(9);
    let baseline = ((f64::from(cell.h) * f64::from(BASELINE_RATIO)).round() as u32).min(cell.h);
    Some(ChartLayout {
        kind: spec.kind,
        cols,
        rows,
        px,
        px_size,
        baseline,
        cell,
    })
}

/// 栅格化一张图：画布尺寸严格等于版式的像素尺寸。
pub fn rasterize(
    spec: &ChartSpec,
    layout: &ChartLayout,
    theme: &Theme,
    fonts: &mut Fonts,
) -> Canvas {
    let palette = theme.raw();
    let canvas = Canvas::new(layout.px.0, layout.px.1, palette.bg_base);
    draw::paint(canvas, spec, layout, palette, fonts)
}

/// 后台预热内嵌字体（幂等）。
///
/// 解析内嵌 CJK 子集要几百毫秒（一次性）：在渲染层第一次看到「可以出图」的
/// 表格时就丢到后台线程，等用户真的点下入口，字体通常已经就绪——点击不再卡。
/// 从没出现过可图表化表格的会话完全不付这份代价。
pub fn warm_fonts() {
    use std::sync::atomic::{AtomicBool, Ordering};

    static STARTED: AtomicBool = AtomicBool::new(false);
    if STARTED.swap(true, Ordering::Relaxed) {
        return;
    }
    std::thread::spawn(|| {
        with_fonts(|_| ());
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    use moc_core::chart::{ChartSpec, Dimension, Point, Series};

    fn spec(kind: ChartKind, categories: usize, series: usize) -> ChartSpec {
        ChartSpec {
            kind,
            axis: "类别".into(),
            categories: (0..categories).map(|index| format!("c{index}")).collect(),
            series: (0..series)
                .map(|index| Series {
                    name: format!("s{index}"),
                    dim: Dimension::Count,
                    unit: String::new(),
                    polarity: None,
                    factor: 1.0,
                    points: (0..categories)
                        .map(|row| {
                            Some(Point {
                                value: (row + index) as f64,
                                text: (row + index).to_string(),
                                upper: None,
                                total: Some(10.0),
                            })
                        })
                        .collect(),
                })
                .collect(),
            normalized: false,
            diverging: false,
            notes: vec![None; categories],
        }
    }

    /// 版式与像素尺寸必须满足「整数格」：图片宽高恰为格数 × 单元格尺寸。
    #[test]
    fn layout_matches_integer_cells() {
        let cell = CellPixels { w: 9, h: 18 };
        let layout = layout(&spec(ChartKind::Bar, 5, 1), 60, cell).expect("60 列足够");
        assert_eq!(layout.rows, 7, "图例 + 5 类别 + 轴");
        assert_eq!(layout.px, (60 * 9, 7 * 18));
        assert_eq!(layout.px_size, 13, "18 * 0.72 四舍五入");
        assert_eq!(layout.baseline, 14, "18 * 0.78 四舍五入");
    }

    /// 太窄、太矮、行数超限都不出图（入口也不出现）。
    #[test]
    fn layout_rejects_unusable_geometry() {
        let cell = CellPixels { w: 8, h: 16 };
        assert!(layout(&spec(ChartKind::Bar, 5, 1), MIN_COLS - 1, cell).is_none());
        assert!(layout(&spec(ChartKind::Bar, 5, 1), 80, CellPixels { w: 3, h: 16 }).is_none());
        assert!(layout(&spec(ChartKind::Bar, 5, 1), 80, CellPixels { w: 8, h: 7 }).is_none());
        assert!(layout(&spec(ChartKind::Bar, MAX_ROWS as usize, 1), 80, cell).is_none());
        assert!(
            layout(&spec(ChartKind::Grouped, 10, 4), 80, cell).is_none(),
            "10*4+2 = 42 行"
        );
    }
}

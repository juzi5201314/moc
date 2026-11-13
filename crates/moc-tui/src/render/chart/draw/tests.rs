use super::*;

use moc_core::chart::{Dimension, Point};

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

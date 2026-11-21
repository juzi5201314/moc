use super::*;

use moc_core::chart::{Dimension, Point};

use crate::render::canvas::Canvas;
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
    let canvas = with_fonts(|fonts| super::super::rasterize(spec, &layout, &theme, fonts));
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

/// 堆叠图：各段之和紧贴不留缝；非归一化按总量等比，归一化铺满整条轨道。
#[test]
fn stacked_segments_fill_the_track() {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    let mut chart = spec(
        ChartKind::Stacked,
        &["a", "b"],
        vec![
            series("读", &[(80.0, "80"), (10.0, "10")]),
            series("写", &[(20.0, "20"), (10.0, "10")]),
        ],
    );
    let (canvas, _) = raster(&chart);
    let first = chart_rgb(&theme, 0);
    let second = chart_rgb(&theme, 1);
    let mut totals = [0u32; 2];
    for row in 1..=2u32 {
        let y = row * 18 + 9;
        let total = width_of(&canvas, y, first) + width_of(&canvas, y, second);
        totals[row as usize - 1] = total;
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
    assert!(
        totals[0] > totals[1] * 3,
        "非归一化时 a(100) 的条长应远大于 b(20)：{totals:?}"
    );
    chart.normalized = true;
    let (norm, _) = raster(&chart);
    let norm_a = width_of(&norm, 18 + 9, first) + width_of(&norm, 18 + 9, second);
    let norm_b = width_of(&norm, 36 + 9, first) + width_of(&norm, 36 + 9, second);
    assert_eq!(norm_a, norm_b, "归一化时两行总条长必须相等");
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

/// 负值发散条形保留左侧数值区、折线孤立点与 2 类别、散点缺失对均安全绘制。
#[test]
fn diverging_bars_and_sparse_plots_stay_within_bounds() {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    let mut diverging = spec(
        ChartKind::Bar,
        &["cat_a", "cat_b", "cat_c"],
        vec![series("Δ", &[(-40.0, "-40"), (-10.0, "-10"), (5.0, "+5")])],
    );
    diverging.diverging = true;
    let (canvas, _) = raster(&diverging);
    let bar_color = chart_rgb(&theme, 0);
    // 最负条形的条左缘必须在标签与负值预留区右侧，不会盖进左端标签区
    let (left, _) = span_of(&canvas, 18 + 9, bar_color).expect("负条形应绘制");
    assert!(left > PAD_X + 20, "负值标签不得覆盖左侧类别区：left={left}");

    let sparse_line = spec(
        ChartKind::Line,
        &["s0", "s1"],
        vec![Series {
            name: "v".into(),
            dim: Dimension::Count,
            unit: String::new(),
            factor: 1.0,
            polarity: None,
            points: vec![point(10.0, "10"), None],
        }],
    );
    let (line_canvas, _) = raster(&sparse_line);
    let line_ink = (0..line_canvas.h())
        .flat_map(|y| (0..line_canvas.w()).map(move |x| (x, y)))
        .filter(|(x, y)| rgb_at(&line_canvas, *x, *y) == bar_color)
        .count();
    assert!(line_ink > 0, "缺值前的孤立点必须画出圆点");

    let sparse_scatter = spec(
        ChartKind::Scatter,
        &["0", "1", "2", "3", "4", "5"],
        vec![
            Series {
                name: "x".into(),
                dim: Dimension::Count,
                unit: String::new(),
                factor: 1.0,
                polarity: None,
                points: vec![
                    point(1.0, "1"),
                    point(2.0, "2"),
                    point(3.0, "3"),
                    point(4.0, "4"),
                    point(5.0, "5"),
                    point(999.0, "999"),
                ],
            },
            Series {
                name: "y".into(),
                dim: Dimension::Duration,
                unit: "s".into(),
                factor: 1.0,
                polarity: None,
                points: vec![
                    point(2.0, "2"),
                    point(4.0, "4"),
                    point(6.0, "6"),
                    point(8.0, "8"),
                    point(10.0, "10"),
                    None,
                ],
            },
        ],
    );
    let (scatter_canvas, _) = raster(&sparse_scatter);
    assert!(scatter_canvas.w() > 0);
}

/// 分面小面板：透明 RGBA 背景（角点 alpha == 0），且 2×2 面板各自独立出条。
#[test]
fn panels_render_transparent_rgba_with_four_facets() {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    let panels_spec = spec(
        ChartKind::Panels,
        &["100 条", "1 万条", "8 MiB 末条"],
        vec![
            series("s1", &[(4.6, "4.6 ms"), (150.0, "150 ms"), (64.0, "64 ms")]),
            series("s2", &[(4.2, "4.2 ms"), (3.8, "3.8 ms"), (127.0, "127 ms")]),
            series("s3", &[(339.0, "339 ms"), (488.0, "488 ms"), (1040.0, "1.04 s")]),
            series("s4", &[(40.2, "40.2 MiB"), (51.1, "51.1 MiB"), (158.0, "158 MiB")]),
        ],
    );
    let (canvas, layout) = raster(&panels_spec);
    assert_eq!(layout.rows, 1 + 2 * (3 + 2));
    let rgba = canvas.rgba().expect("SVG 光栅化应保留直通 RGBA 通道");
    assert_eq!(rgba[3], 0, "左上角背景像素必须全透明");
    // 四个面板的最后一行（cat 2，用 chart_rgb(2)）都应落出实色像素
    let cat2 = chart_rgb(&theme, 2);
    for (grid_row, grid_col) in [(0, 0), (0, 1), (1, 0), (1, 1)] {
        let y = (1 + grid_row * 5 + 3) * 18 + 9;
        let x0 = if grid_col == 0 { 0 } else { canvas.w() / 2 };
        let x1 = if grid_col == 0 { canvas.w() / 2 } else { canvas.w() };
        let painted = (x0..x1).filter(|&x| rgb_at(&canvas, x, y) == cat2).count();
        assert!(painted >= 4, "面板 ({grid_row}, {grid_col}) 应画出第 3 类胶囊条");
    }
}

//! 绘制数据模型：把 [`ChartPlan`] 落成绘制层可直接消费的 [`ChartSpec`]。
//!
//! 这里只做映射，不做任何排版或格式化：点的文本沿用单元格的原样书写，
//! 类别顺序沿用表格顺序（排序由绘制层决定），缺失值保留为 `None` 而不是填零。

use super::plan::{ChartKind, ChartPlan, Polarity};
use super::table::{Cell, Column, Dimension, Number, TableAnalysis, is_status_word};

/// 一个数据点。
#[derive(Debug, Clone, PartialEq)]
pub struct Point {
    /// 基本单位下的数值。
    pub value: f64,
    /// 原样书写文本（绘制层直接画这个串，不再二次格式化）。
    pub text: String,
    /// 区间上界（`24–72 h`）；绘制层当前只用 [`Point::text`]。
    pub upper: Option<f64>,
    /// 该点的「满值」分母（`12/12` → `Some(12.0)`；`87%` → `Some(100.0)`；其余 `None`）。
    /// 进度条用它算填充比例；与 [`Point::value`] 同单位，不需要再乘除 [`Series::factor`]。
    pub total: Option<f64>,
}

/// 一条系列。
#[derive(Debug, Clone, PartialEq)]
pub struct Series {
    /// 系列名（列的表头）。
    pub name: String,
    /// 量纲。
    pub dim: Dimension,
    /// 书写单位（与 [`Series::dim`] 配对出现的那个单位）。
    pub unit: String,
    /// 展示换算：显示值 = `value / factor`（与 [`super::table::Number::factor`] 同义）。
    /// 堆叠总量与坐标轴刻度需要它把基本单位（秒、字节、计数）换回书写单位。
    pub factor: f64,
    /// 极性。
    pub polarity: Option<Polarity>,
    /// 与 [`ChartSpec::categories`] 逐项对齐的点；非数值为 `None`。
    pub points: Vec<Option<Point>>,
}

/// 绘制层的数据模型：图型 + 类别 + 系列 + 备注。
#[derive(Debug, Clone, PartialEq)]
pub struct ChartSpec {
    /// 图型。
    pub kind: ChartKind,
    /// 类别列的表头（横轴名）。
    pub axis: String,
    /// 类别文本，保持表格顺序。
    pub categories: Vec<String>,
    /// 系列，顺序与 [`ChartPlan::series`] 一致。
    pub series: Vec<Series>,
    /// 是否按 100% 堆叠。
    pub normalized: bool,
    /// 是否围绕零发散。
    pub diverging: bool,
    /// 与类别逐项对齐的状态词备注（如 `TIMEOUT`），绘制层在图例后追加。
    pub notes: Vec<Option<String>>,
}

/// 按规划和语义分析生成绘制模型。
///
/// `plan.polarity` 与 `plan.series` 等长（[`super::plan::plan`] 保证），因此按同一槽位取值。
pub fn build(table: &TableAnalysis, plan: &ChartPlan) -> ChartSpec {
    let label = &table.columns[plan.label];
    let categories = table
        .rows
        .iter()
        .enumerate()
        .map(|(position, &row)| match label.cell_at(row) {
            Cell::Number(number) => number.figure.clone(),
            Cell::Text(text) => text.clone(),
            // 缺失的类别用行号兜底：图里不能出现空标签
            Cell::Missing => format!("#{}", position + 1),
        })
        .collect();
    let notes = table
        .rows
        .iter()
        .map(|&row| note_of(table, plan.label, row))
        .collect();
    let series = plan
        .series
        .iter()
        .enumerate()
        .map(|(slot, &index)| {
            let column = &table.columns[index];
            let points = table
                .rows
                .iter()
                .map(|&row| match column.cell_at(row) {
                    Cell::Number(number) => Some(Point {
                        value: number.value,
                        text: number.figure.clone(),
                        upper: number.upper,
                        total: point_total(number),
                    }),
                    _ => None,
                })
                .collect();
            Series {
                name: column.header.clone(),
                dim: column.dim.unwrap_or(Dimension::Count),
                unit: column.unit.clone(),
                factor: series_factor(column, &table.rows),
                polarity: plan.polarity[slot],
                points,
            }
        })
        .collect();
    ChartSpec {
        kind: plan.kind,
        axis: label.header.clone(),
        categories,
        series,
        normalized: plan.normalize,
        diverging: plan.diverging,
        notes,
    }
}

/// 该点的「满值」：百分比按 100，其余量纲按单元格自带的分母（`12/12`），没有满值概念时为 `None`。
fn point_total(number: &Number) -> Option<f64> {
    if number.dim == Dimension::Percent {
        return Some(100.0);
    }
    number.denominator
}

/// 系列的展示换算因子：取该列任一数字的 `Number::factor`（列内单位一致，因子也一致）。
///
/// 无单位的计数列因子是 1.0；进入 `plan.series` 的列按构造必然含数字，
/// 这里的 `1.0` 只是「单位换算恒等」的本值。
fn series_factor(column: &Column, rows: &[usize]) -> f64 {
    rows.iter()
        .find_map(|&row| match column.cell_at(row) {
            Cell::Number(number) => Some(number.factor),
            _ => None,
        })
        .unwrap_or(1.0)
}

/// 该行的状态词备注：类别列之外第一个像状态词的文本单元格。
fn note_of(table: &TableAnalysis, label: usize, row: usize) -> Option<String> {
    table
        .columns
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != label)
        .find_map(|(_, column)| match column.cell_at(row) {
            Cell::Text(text) if is_status_word(text) => Some(text.clone()),
            _ => None,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::plan::plan;
    use crate::chart::table::{RawTable, analyze};

    fn table(header: &[&str], rows: &[&[&str]]) -> RawTable {
        RawTable {
            header: header.iter().map(|head| (*head).to_owned()).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
                .collect(),
        }
    }

    fn spec_of(raw: &RawTable) -> ChartSpec {
        let analysis = analyze(raw);
        let planned = plan(&analysis).expect("这张表应当能规划成图表");
        build(&analysis, &planned)
    }

    #[test]
    fn maps_points_and_collects_status_notes() {
        // 状态词不参与绘图（点为空），但会作为备注挂在对应类别上
        let raw = table(
            &["地区", "耗时 ms"],
            &[
                &["华北", "12"],
                &["华南", "TIMEOUT"],
                &["华东", "9"],
                &["西南", "15"],
            ],
        );
        let spec = spec_of(&raw);
        assert_eq!(spec.kind, ChartKind::Bar);
        assert_eq!(spec.axis, "地区");
        assert_eq!(spec.categories, vec!["华北", "华南", "华东", "西南"]);
        assert_eq!(spec.series[0].name, "耗时 ms");
        assert_eq!(spec.series[0].unit, "ms");
        assert_eq!(spec.series[0].dim, Dimension::Duration);
        let point = spec.series[0].points[0].as_ref().expect("华北应当有数值");
        assert_eq!(point.text, "12");
        assert!((point.value - 0.012).abs() < 1e-12);
        assert!(spec.series[0].points[1].is_none());
        assert_eq!(spec.notes[1], Some("TIMEOUT".to_owned()));
        assert_eq!(spec.notes[0], None);
    }

    #[test]
    fn falls_back_to_row_numbers_for_missing_categories() {
        let raw = table(&["#", "耗时 ms"], &[&["", "12"], &["-", "15"]]);
        let spec = spec_of(&raw);
        assert_eq!(spec.axis, "#");
        assert_eq!(spec.categories, vec!["#1", "#2"]);
        assert_eq!(spec.notes, vec![None, None]);
    }

    #[test]
    fn carries_normalize_and_diverging_flags() {
        let raw = table(
            &["服务", "读", "写", "合计"],
            &[&["A", "1", "1", "2"], &["B", "2", "6", "8"]],
        );
        let spec = spec_of(&raw);
        assert_eq!(spec.kind, ChartKind::Stacked);
        assert!(spec.normalized);
        assert!(!spec.diverging);
        // 堆叠只画部分列，合计列不进系列
        assert_eq!(spec.series.len(), 2);
    }

    #[test]
    fn carries_display_factor_matching_the_cell() {
        // 系列因子必须与该列单元格的 `Number::factor` 一致：绘制层用 value / factor 还原书写值
        let raw = table(&["地区", "耗时 ms"], &[&["华北", "12.5"], &["华南", "20"]]);
        let analysis = analyze(&raw);
        let planned = plan(&analysis).expect("这张表应当能规划成图表");
        let spec = build(&analysis, &planned);
        let Cell::Number(cell) = analysis.columns[1].cell_at(0) else {
            panic!("耗时列应当是数值")
        };
        assert_eq!(spec.series[0].factor, cell.factor);
        assert_eq!(spec.series[0].factor, 0.001);
        let point = spec.series[0].points[0].as_ref().expect("华北应当有数值");
        assert!((point.value / spec.series[0].factor - 12.5).abs() < 1e-9);
        // 无单位的计数列因子是恒等 1.0
        let raw = table(
            &["方案", "Q1", "Q2"],
            &[&["C1", "10", "20"], &["C2", "12", "18"], &["C3", "9", "25"]],
        );
        assert_eq!(spec_of(&raw).series[0].factor, 1.0);
    }

    #[test]
    fn carries_point_totals_for_progress() {
        // 百分比以 100 为满值，分数以分母为满值，普通计数没有满值
        let raw = table(
            &["任务", "进度"],
            &[&["A", "12/12"], &["B", "87%"], &["C", "42"]],
        );
        let spec = spec_of(&raw);
        let totals: Vec<Option<f64>> = spec.series[0]
            .points
            .iter()
            .map(|point| point.as_ref().and_then(|point| point.total))
            .collect();
        assert_eq!(totals, vec![Some(12.0), Some(100.0), None]);
    }
}

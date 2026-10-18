//! 图型规划：把表格的语义分析结果映射成一种可绘制的图型。
//!
//! 判定顺序固定、先命中先返回（计划里的 1–11 条规则），顺序本身是契约的一部分：
//! 例如 `Heatmap` 排在 `Stacked` 之前，所以「读 / 写 / 合计 / 其他」这种至少 4 列、
//! 至少 3 行的表会画成热力图；`Stacked` 只在行数不足 3 行或主组不足 4 列时才轮得到。
//! 阈值一律命名常量并写清理由，避免以后再猜「为什么是 0.8」。

use super::table::{Cell, Column, ColumnRole, Composition, Dimension, TableAnalysis};

/// 数值偏好的方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Polarity {
    /// 越大越好（得分、通过率、吞吐）。
    Higher,
    /// 越小越好（耗时、内存、错误数）。
    Lower,
}

/// 图型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChartKind {
    /// 单系列条形图。
    Bar,
    /// 分组条形图：每个类别占系列数行，每行一个系列的条。
    Grouped,
    /// 成对条形图：改前 / 改后两行，行尾标差值。
    Paired,
    /// 折线图。
    Line,
    /// 堆叠条形图（份额组合）。
    Stacked,
    /// 进度条：分数或百分比。
    Progress,
    /// 热力图：类别 × 系列矩阵。
    Heatmap,
    /// 散点图：两个不同量纲做 x / y。
    Scatter,
}

/// 一次规划的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChartPlan {
    /// 图型。
    pub kind: ChartKind,
    /// 类别列下标（[`TableAnalysis::label`]）。
    pub label: usize,
    /// 参与绘制的列下标：`Bar` 一列，`Stacked` 是部分列，`Scatter` 是 x / y，其余是主组。
    pub series: Vec<usize>,
    /// 是否行列互换；当前规划不换轴，始终为 `false`（字段留给绘制层判断版式）。
    pub transpose: bool,
    /// 堆叠是否按 100% 归一化（只在 `Stacked` 上可能为 `true`）。
    pub normalize: bool,
    /// 数据是否围绕零发散（只在 `Bar` 上可能为 `true`），绘制层据此画零轴。
    pub diverging: bool,
    /// 与 [`ChartPlan::series`] 对齐的极性。
    pub polarity: Vec<Option<Polarity>>,
}

/// 可绘图的数据行数下限：只有 1 行没有可比性。
const MIN_ROWS: usize = 2;
/// 可绘图的数据行数上限：再多的类别在终端里也挤不成可读的图。
const MAX_ROWS: usize = 200;
/// `Grouped` / `Heatmap` 的系列数上限：每类别占一行系列的版式超过 4 个系列就读不动。
const GROUP_MAX: usize = 4;
/// `Heatmap` 需要的系列数下限：与 omp autoGraph 一致，至少 4 列才成矩阵。
/// 3 个系列交给分组条形图，避免把普通数值表画成满屏色块。
const HEATMAP_MIN_MEMBERS: usize = 4;
/// `Heatmap` 需要的数据行数下限。
const HEATMAP_MIN_ROWS: usize = 3;
/// `Paired` 的箭头单元占比（两列合计）：超过这个比例说明单元格本身就在写「改前 → 改后」。
const PAIRED_ARROW_RATIO: f64 = 0.6;
/// `Paired` 固定两列。
const PAIRED_SERIES: usize = 2;
/// `Scatter` 需要的数据行数下限：点太少看不出关系。
const SCATTER_MIN_ROWS: usize = 6;
/// `Bar` 的发散判定：带显式符号（`+ - ±`）的单元格占比。
const DIVERGING_SIGNED_RATIO: f64 = 0.2;
/// `Line` 的单调列判定：类别列之前的测量列至少有这个比例的行有值。
const LINE_FILL_RATIO: f64 = 0.8;
/// `Stacked` 的归一化判定：各行总量最大值 / 最小值达到这个倍数就按 100% 堆叠。
const STACKED_NORMALIZE_RATIO: f64 = 4.0;
/// `Progress` 的百分比上限。
const PROGRESS_MAX_PERCENT: f64 = 100.0;

/// 「越低越好」表头词表。
const POLARITY_LOWER: &[&str] = &[
    "cost", "price", "latency", "time", "duration", "error", "fail", "loss", "memory", "size",
    "tokens", "cpu", "alloc", "耗时", "错误", "失败", "内存", "体积", "行数", "轮次",
];

/// 「越高越好」表头词表。
const POLARITY_HIGHER: &[&str] = &[
    "pass",
    "score",
    "accuracy",
    "rate",
    "speedup",
    "throughput",
    "qps",
    "rps",
    "tps",
    "savings",
    "通过",
    "命中",
    "得分",
    "加速",
    "节省",
    "覆盖",
    "准确率",
];

/// 成对方向的前侧词（表头包含即可；单字词必须整格相等）。
const PAIRED_BEFORE: &[&str] = &[
    "改前",
    "优化前",
    "之前",
    "前",
    "before",
    "baseline",
    "old",
    "prior",
];

/// 成对方向的后侧词。
const PAIRED_AFTER: &[&str] = &["改后", "优化后", "之后", "后", "after", "new", "current"];

/// 横轴进度词的类别列表头（锚定匹配）。
const LINE_HEADERS: &[&str] = &[
    "run", "step", "iter", "day", "week", "month", "周", "月", "轮", "版本", "version", "迭代",
];

/// 差值表头记号。
const DELTA_MARKS: &[&str] = &["Δ", "变化", "差值", "增减"];

/// 把分析结果规划成图型；无法成图时返回 `None`。
pub fn plan(table: &TableAnalysis) -> Option<ChartPlan> {
    // 规则 1：类别列、测量列、行数三者缺一不可
    let label = table.label?;
    if table.measures.is_empty() {
        return None;
    }
    let rows = table.rows.len();
    if !(MIN_ROWS..=MAX_ROWS).contains(&rows) {
        return None;
    }

    // 规则 2：主组 = 同 `(量纲, 单位)` 成员最多的组
    let main = main_group(table);

    // 规则 3：进度形态
    if main.len() == 1 && is_progress(table, main[0]) {
        return Some(finish(
            table,
            ChartKind::Progress,
            label,
            main,
            false,
            false,
        ));
    }
    // 规则 4：成对方向
    if main.len() == PAIRED_SERIES && is_paired(table, &main) {
        return Some(finish(table, ChartKind::Paired, label, main, false, false));
    }
    // 规则 5：横轴是时间 / 顺序
    if is_line(table, label) {
        return Some(finish(table, ChartKind::Line, label, main, false, false));
    }
    // 规则 6：两个不同量纲 → 散点
    if let Some(series) = scatter_series(table) {
        return Some(finish(
            table,
            ChartKind::Scatter,
            label,
            series,
            false,
            false,
        ));
    }
    // 规则 7：同组系列 ≥ 4 → 热力图
    if main.len() >= HEATMAP_MIN_MEMBERS && rows >= HEATMAP_MIN_ROWS {
        return Some(finish(table, ChartKind::Heatmap, label, main, false, false));
    }
    // 规则 8：份额组合 → 堆叠（部分列必须非负，否则堆叠段会互相抵消）
    if let Some(composition) = &table.composition
        && composition.parts.len() >= 2
        && parts_non_negative(table, composition)
    {
        let normalize = composition.exact && needs_normalize(table, composition);
        return Some(finish(
            table,
            ChartKind::Stacked,
            label,
            composition.parts.clone(),
            normalize,
            false,
        ));
    }
    // 规则 9：2–4 个同组系列 → 分组条形
    if (2..=GROUP_MAX).contains(&main.len()) {
        return Some(finish(table, ChartKind::Grouped, label, main, false, false));
    }
    // 规则 10：单系列 → 条形；带符号或差值表头时画零轴
    let diverging = is_diverging(table, main[0]);
    Some(finish(
        table,
        ChartKind::Bar,
        label,
        vec![main[0]],
        false,
        diverging,
    ))
}

/// 组装规划结果，并按表头关键词填上极性。
fn finish(
    table: &TableAnalysis,
    kind: ChartKind,
    label: usize,
    series: Vec<usize>,
    normalize: bool,
    diverging: bool,
) -> ChartPlan {
    let polarity = series
        .iter()
        .map(|&index| polarity_of(&table.columns[index].header))
        .collect();
    ChartPlan {
        kind,
        label,
        series,
        transpose: false,
        normalize,
        diverging,
        polarity,
    }
}

/// 主组：measure 列按 `(量纲, 单位)` 分组后成员最多的组；并列取列下标最小的组。
///
/// measures 按列序升序，分组也按首次出现的顺序建立，因此「严格大于」在并列时保留先出现者。
fn main_group(table: &TableAnalysis) -> Vec<usize> {
    let mut groups: Vec<(Option<Dimension>, &str, Vec<usize>)> = Vec::new();
    for &measure in &table.measures {
        let column = &table.columns[measure];
        match groups
            .iter_mut()
            .find(|(dim, unit, _)| *dim == column.dim && *unit == column.unit.as_str())
        {
            Some(group) => group.2.push(measure),
            None => groups.push((column.dim, column.unit.as_str(), vec![measure])),
        }
    }
    let mut best: Vec<usize> = Vec::new();
    for group in groups {
        if group.2.len() > best.len() {
            best = group.2;
        }
    }
    best
}

/// 进度形态：唯一测量列的取值要么是分数（分子落在 `0..=分母`），要么是不超过 100 的百分比。
///
/// 计划原文写「0.0..=1.0 且带 denominator」，但它给的例子 `12/12` 的 `value` 是分子（12），
/// 因此这里按「分子 / 分母落在 0..=1」判定，与分数形式的 0.0..=1.0 等价。
fn is_progress(table: &TableAnalysis, measure: usize) -> bool {
    let column = &table.columns[measure];
    let mut seen = 0;
    for &row in &table.rows {
        let Cell::Number(number) = column.cell_at(row) else {
            continue;
        };
        seen += 1;
        match number.denominator {
            Some(denominator) if (0.0..=denominator).contains(&number.value) => {}
            Some(_) => return false,
            None => {
                if number.dim != Dimension::Percent
                    || !(0.0..=PROGRESS_MAX_PERCENT).contains(&number.value)
                {
                    return false;
                }
            }
        }
    }
    seen > 0
}

/// 成对方向：表头是同一族的前 / 后词，或者两列的箭头单元占比达到阈值。
fn is_paired(table: &TableAnalysis, main: &[usize]) -> bool {
    let first = &table.columns[main[0]];
    let second = &table.columns[main[1]];
    if paired_headers(&first.header, &second.header) {
        return true;
    }
    let arrows = table
        .rows
        .iter()
        .filter(|&&row| has_arrow(first, row))
        .count()
        + table
            .rows
            .iter()
            .filter(|&&row| has_arrow(second, row))
            .count();
    arrows as f64 / (2.0 * table.rows.len() as f64) >= PAIRED_ARROW_RATIO
}

/// 该行是否带箭头目标（`43 → 28`）。
fn has_arrow(column: &Column, row: usize) -> bool {
    matches!(column.cell_at(row), Cell::Number(number) if number.to.is_some())
}

/// 两列表头是否是一对前 / 后方向词。
fn paired_headers(first: &str, second: &str) -> bool {
    (matches_direction(first, PAIRED_BEFORE) && matches_direction(second, PAIRED_AFTER))
        || (matches_direction(first, PAIRED_AFTER) && matches_direction(second, PAIRED_BEFORE))
}

/// 多字方向词按包含匹配，单字方向词（`前` / `后`）必须整格相等，避免误伤「前提」「后续」。
fn matches_direction(header: &str, words: &[&str]) -> bool {
    let header = header.trim();
    let lower = header.to_ascii_lowercase();
    words.iter().any(|word| {
        if word.chars().count() == 1 {
            header.eq_ignore_ascii_case(word)
        } else {
            lower.contains(word)
        }
    })
}

/// 横轴形态：类别列是时间 / 顺序列，或表头是进度词，或类别列之前存在单调递增的测量列。
fn is_line(table: &TableAnalysis, label: usize) -> bool {
    let column = &table.columns[label];
    if matches!(column.role, ColumnRole::Temporal | ColumnRole::Sequence)
        || is_line_header(&column.header)
    {
        return true;
    }
    table.measures.iter().any(|&measure| {
        measure < label && is_filled_increasing(&table.columns[measure], &table.rows)
    })
}

/// 类别列表头是否是进度词。
fn is_line_header(header: &str) -> bool {
    let header = header.trim();
    LINE_HEADERS
        .iter()
        .any(|word| header.eq_ignore_ascii_case(word))
}

/// 该列 ≥ [`LINE_FILL_RATIO`] 的行有值，且数值单调递增。
fn is_filled_increasing(column: &Column, rows: &[usize]) -> bool {
    let mut values = 0;
    let mut previous = None;
    for &row in rows {
        let Some(value) = column.value_at(row) else {
            continue;
        };
        values += 1;
        if previous.is_some_and(|previous| value < previous) {
            return false;
        }
        previous = Some(value);
    }
    values as f64 >= LINE_FILL_RATIO * rows.len() as f64
}

/// 散点：前两个量纲不同、且都不是百分比 / 分数的测量列（先出现的当 x，后者当 y）。
fn scatter_series(table: &TableAnalysis) -> Option<Vec<usize>> {
    if table.measures.len() < 2 || table.rows.len() < SCATTER_MIN_ROWS {
        return None;
    }
    let mut picked: Vec<usize> = Vec::with_capacity(2);
    for &measure in &table.measures {
        let column = &table.columns[measure];
        if matches!(column.dim, Some(Dimension::Percent | Dimension::Fraction)) {
            continue;
        }
        if picked.is_empty() || table.columns[picked[0]].dim != column.dim {
            picked.push(measure);
        }
        if picked.len() == 2 {
            return Some(picked);
        }
    }
    None
}

/// 份额组合的部分列数值是否全部非负。
fn parts_non_negative(table: &TableAnalysis, composition: &Composition) -> bool {
    composition.parts.iter().all(|&part| {
        table.rows.iter().all(|&row| {
            table.columns[part]
                .value_at(row)
                .is_none_or(|value| value >= 0.0)
        })
    })
}

/// 归一化：精确组合且各行总量最大 / 最小达到 [`STACKED_NORMALIZE_RATIO`]；
/// 有行总量为 0 时百分比无意义，不归一化。
fn needs_normalize(table: &TableAnalysis, composition: &Composition) -> bool {
    let mut min = f64::INFINITY;
    let mut max = f64::NEG_INFINITY;
    let mut seen = 0;
    for &row in &table.rows {
        let mut total = 0.0;
        let mut complete = true;
        for &part in &composition.parts {
            match table.columns[part].value_at(row) {
                Some(value) => total += value,
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete {
            continue;
        }
        seen += 1;
        min = min.min(total);
        max = max.max(total);
    }
    seen >= 2 && min > 0.0 && max / min >= STACKED_NORMALIZE_RATIO
}

/// 发散：差值与增减的表头，或带显式符号的单元格占比达到阈值。
fn is_diverging(table: &TableAnalysis, measure: usize) -> bool {
    let column = &table.columns[measure];
    if matches_delta_header(&column.header) {
        return true;
    }
    let signed = table
        .rows
        .iter()
        .filter(|&&row| matches!(column.cell_at(row), Cell::Number(number) if number.signed))
        .count();
    signed as f64 / table.rows.len() as f64 >= DIVERGING_SIGNED_RATIO
}

/// 表头是否带差值记号（`Δ` / `delta` / `变化` / `差值` / `增减`）。
fn matches_delta_header(header: &str) -> bool {
    header.to_ascii_lowercase().contains("delta")
        || DELTA_MARKS.iter().any(|mark| header.contains(mark))
}

/// 表头关键词决定极性：先查「越低越好」词表，再查「越高越好」词表，未命中为中性。
///
/// 顺序是有意的：`error rate`、`耗时 rate` 这类复合表头同时命中两边时按「越低越好」处理 ——
/// 方向由名词（error / 耗时）决定，而不是由 `rate` 决定。
fn polarity_of(header: &str) -> Option<Polarity> {
    let lower = header.to_ascii_lowercase();
    if POLARITY_LOWER.iter().any(|word| lower.contains(word)) {
        return Some(Polarity::Lower);
    }
    if POLARITY_HIGHER.iter().any(|word| lower.contains(word)) {
        return Some(Polarity::Higher);
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chart::spec::{ChartSpec, build};
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

    /// 规划并生成 spec；不可成图时 panic，保证测试只验证成功路径。
    fn chart(raw: &RawTable) -> (ChartPlan, ChartSpec) {
        let analysis = analyze(raw);
        let planned = plan(&analysis).expect("这张表应当能规划成图表");
        let spec = build(&analysis, &planned);
        (planned, spec)
    }

    #[test]
    fn plans_bar_with_polarity_and_table_order() {
        let raw = table(
            &["地区", "耗时 ms"],
            &[&["华北", "12.5"], &["华南", "20"], &["华东", "9"]],
        );
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Bar);
        assert_eq!(planned.label, 0);
        assert_eq!(planned.series, vec![1]);
        assert_eq!(planned.polarity, vec![Some(Polarity::Lower)]);
        assert!(!planned.diverging);
        assert!(!planned.transpose);
        assert_eq!(spec.axis, "地区");
        assert_eq!(spec.categories, vec!["华北", "华南", "华东"]);
    }

    #[test]
    fn plans_grouped_for_same_dimension_measures() {
        let raw = table(
            &["方案", "Q1", "Q2"],
            &[&["C1", "10", "20"], &["C2", "12", "18"], &["C3", "9", "25"]],
        );
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Grouped);
        assert_eq!(planned.series, vec![1, 2]);
        assert_eq!(spec.series.len(), 2);
        assert_eq!(spec.series[0].name, "Q1");
    }

    #[test]
    fn plans_paired_for_direction_headers() {
        let raw = table(
            &["指标", "改前", "改后"],
            &[&["P95", "120", "80"], &["P99", "200", "150"]],
        );
        let (planned, _) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Paired);
        assert_eq!(planned.series, vec![1, 2]);
        assert_eq!(planned.polarity, vec![None, None]);
    }

    #[test]
    fn plans_line_for_progress_axis() {
        let raw = table(
            &["day", "耗时 ms"],
            &[&["1", "12"], &["2", "15"], &["3", "9"]],
        );
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Line);
        assert_eq!(planned.series, vec![1]);
        assert_eq!(spec.categories, vec!["1", "2", "3"]);
    }

    #[test]
    fn plans_scatter_for_two_dimensions() {
        let raw = table(
            &["城市", "耗时 ms", "体积 KiB"],
            &[
                &["北京", "12.5", "3"],
                &["上海", "20", "5"],
                &["广州", "9", "2"],
                &["深圳", "15", "4"],
                &["杭州", "18", "6"],
                &["成都", "11", "1"],
            ],
        );
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Scatter);
        assert_eq!(planned.series, vec![1, 2]);
        assert_eq!(spec.series[0].dim, Dimension::Duration);
        assert_eq!(spec.series[1].dim, Dimension::Bytes);
    }

    /// 3 个同量纲系列走分组条形：普通数值表不该被画成满屏热力色块。
    #[test]
    fn plans_grouped_for_three_numeric_measures() {
        let raw = table(
            &["序号", "数值", "平方", "立方"],
            &[
                &["1", "12", "144", "1,728"],
                &["2", "27", "729", "19,683"],
                &["3", "45", "2,025", "91,125"],
            ],
        );
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Grouped);
        assert_eq!(planned.series, vec![1, 2, 3]);
        assert_eq!(spec.categories, vec!["1", "2", "3"]);
    }

    /// 至少 4 个同量纲系列才成矩阵，与 omp autoGraph 的热力图门槛一致。
    #[test]
    fn plans_heatmap_for_four_same_unit_measures() {
        let raw = table(
            &["区域", "Q1", "Q2", "Q3", "Q4"],
            &[
                &["北", "1", "2", "3", "4"],
                &["中", "5", "6", "7", "8"],
                &["南", "9", "10", "11", "12"],
            ],
        );
        let (planned, _) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Heatmap);
        assert_eq!(planned.series, vec![1, 2, 3, 4]);
    }

    #[test]
    fn plans_progress_for_percent_and_fraction() {
        let raw = table(&["模块", "覆盖"], &[&["解析", "87%"], &["渲染", "12%"]]);
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Progress);
        assert_eq!(spec.series[0].dim, Dimension::Percent);

        let raw = table(&["模块", "用例"], &[&["解析", "12/12"], &["渲染", "3/12"]]);
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Progress);
        assert_eq!(spec.series[0].dim, Dimension::Fraction);
    }

    #[test]
    fn plans_stacked_with_normalization() {
        // 两行数据：热力图的最小行数是 3，所以这里轮得到堆叠（顺序即契约）
        let raw = table(
            &["服务", "读", "写", "合计"],
            &[&["A", "1", "2", "3"], &["B", "2", "5", "7"]],
        );
        let (planned, _) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Stacked);
        assert_eq!(planned.series, vec![1, 2]);
        assert!(!planned.normalize);

        // 各行总量 2 与 8 相差 4 倍 → 按 100% 堆叠
        let raw = table(
            &["服务", "读", "写", "合计"],
            &[&["A", "1", "1", "2"], &["B", "2", "6", "8"]],
        );
        let (planned, _) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Stacked);
        assert!(planned.normalize);
    }

    #[test]
    fn plans_diverging_bar_for_signed_cells() {
        let raw = table(
            &["区域", "Δ 耗时"],
            &[&["北", "-10"], &["中", "-40"], &["南", "5"]],
        );
        let (planned, _) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Bar);
        assert!(planned.diverging);
        assert_eq!(planned.polarity, vec![Some(Polarity::Lower)]);
    }

    #[test]
    fn refuses_tables_without_chart_semantics() {
        // 全是文本：没有测量列
        let raw = table(&["地区", "状态"], &[&["北", "OK"], &["南", "FAIL"]]);
        assert!(plan(&analyze(&raw)).is_none());
        // 只有一行数据
        let raw = table(&["地区", "耗时 ms"], &[&["北", "12"]]);
        assert!(plan(&analyze(&raw)).is_none());
    }

    #[test]
    fn keeps_order_and_prefers_composition_over_grouped() {
        // 类别顺序不重排：规划只做映射，排序由绘制层决定
        let raw = table(
            &["方案", "Q1", "Q2"],
            &[&["C3", "1", "2"], &["C1", "3", "4"], &["C2", "5", "6"]],
        );
        let (planned, spec) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Grouped);
        assert_eq!(spec.categories, vec!["C3", "C1", "C2"]);

        // 份额组合存在时走堆叠，而不是分组条形
        let raw = table(
            &["服务", "读", "写", "合计"],
            &[&["A", "1", "2", "3"], &["B", "2", "5", "7"]],
        );
        let (planned, _) = chart(&raw);
        assert_eq!(planned.kind, ChartKind::Stacked);
        assert_eq!(planned.series, vec![1, 2]);
    }
}

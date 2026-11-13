//! 表格语义解析：把 Markdown 表格的纯文本解析成可绘图的数值语义。
//!
//! 三步走：[`analyze`] 逐格去装饰并扫描数值（手写扫描器，不引入正则依赖），
//! 再按数值占比 / 时间占比 / 表头关键词判定 [`ColumnRole`]，
//! 最后汇总出 label 列、measure 列、合计行与份额组合（[`Composition`]）。
//!
//! 数值一律归一化到量纲的基本单位（秒、字节、百分点、计数），见 [`Number::value`]；
//! [`Number::factor`] 只负责展示换算（显示值 = `value / factor`）。
//! 解析全程不做防御性兜底：认不出的文本就当备注或标签，不会伪造数值。

use std::borrow::Cow;

/// 表格的原始形态：表头与数据行都是未去装饰的单元格文本。
#[derive(Debug, Clone, Default)]
pub struct RawTable {
    /// 表头单元格，元素个数即列数。
    pub header: Vec<String>,
    /// 数据行；短行按缺失补齐，长于表头的行同样参与解析。
    pub rows: Vec<Vec<String>>,
}

/// 数值量纲；[`Number::value`] 归一化到该量纲的基本单位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    /// 计数（无量纲）。基本单位：个。
    Count,
    /// 百分比。基本单位：百分点（`12.5%` → `12.5`）。
    Percent,
    /// 时长。基本单位：秒（`12.5 ms` → `0.0125`）。
    Duration,
    /// 字节数。基本单位：字节（`2 KiB` → `2048`）。
    Bytes,
    /// 货币金额。基本单位：书写数值本身（`$250` → `250`）。
    Currency,
    /// 比值或倍数（`1.8x`）。基本单位：倍。
    Ratio,
    /// 分数（`12/176`）。基本单位：分子，分母见 [`Number::denominator`]。
    Fraction,
    /// 速率（`12500 tok/s`）。基本单位：书写数值本身，量级由书写单位承载。
    Rate,
}

/// 归一化后的数值单元格。
///
/// `value` 是基本单位下的数值，`factor` 是展示换算因子：**显示值 = `value / factor`**。
/// 例：`12.5 ms` 的 `value = 0.0125`、`factor = 0.001`；`2 KiB` 的 `value = 2048`、`factor = 1024`；
/// `3 M` 的 `value = 3e6`、`factor = 1e6`。
#[derive(Debug, Clone, PartialEq)]
pub struct Number {
    /// 基本单位下的数值。
    pub value: f64,
    /// 展示换算因子（显示值 = `value / factor`）。
    pub factor: f64,
    /// 量纲。
    pub dim: Dimension,
    /// 书写单位（`ms`、`KiB`、`$`、`tok/s`、`%`）；无单位时为空串。
    pub unit: String,
    /// 原样书写文本（`~$250`、`12.5 ms`、`7648 / 176`）。
    pub figure: String,
    /// 是否带近似记号（`~ ≈ < <= > >= ≤ ≥ 约`）。
    pub approx: bool,
    /// 是否带显式符号（`+ - − ±`）。
    pub signed: bool,
    /// 区间的上界（`24–72 h` → 上界 72 h 的基本单位值）。
    pub upper: Option<f64>,
    /// 箭头的目标值（`12 → 15` → 15 的基本单位值）。
    pub to: Option<f64>,
    /// 分数的分母（`12/176` → 176）；分子是 [`Number::value`]。
    pub denominator: Option<f64>,
}

/// 一个单元格的解析结果。
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// 数值。
    Number(Number),
    /// 缺失（空、`-`、`n/a`、`?` 等）。
    Missing,
    /// 非数值文本（日期、时间、版本、状态词、标签……）。
    Text(String),
}

/// 列在表格中的语义角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnRole {
    /// 测量列：数值占比高，是绘图的候选数据源。
    Measure,
    /// 标签列：文本类别。
    Label,
    /// 时间列：日期/时间文本占比 ≥ [`TEMPORAL_RATIO`]。
    Temporal,
    /// 编号列：表头是编号词，或数值是步长 1 的连续整数。
    Index,
    /// 顺序列：表头是阶段/轮次词，且数值单调不减。
    Sequence,
}

/// 一列的解析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct Column {
    /// 列下标（与 [`RawTable::header`] 对齐）。
    pub index: usize,
    /// 去装饰后的表头文本；表头缺失时为空串。
    pub header: String,
    /// 语义角色。
    pub role: ColumnRole,
    /// 单元格，与 [`RawTable::rows`] 逐行对齐（含合计行）；统计只看 [`TableAnalysis::rows`]。
    pub cells: Vec<Cell>,
    /// 出现次数最多的量纲。
    pub dim: Option<Dimension>,
    /// 与 [`Column::dim`] 配对出现的书写单位。
    pub unit: String,
    /// 参与绘图的行里数值单元格的个数。
    pub numeric: usize,
    /// 参与绘图的行里数值是否全部相等（常量列不进入 [`TableAnalysis::measures`]）。
    pub constant: bool,
}

/// 份额组合：几列之和等于（或不超过）另一列，例如「读 + 写 = 合计」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Composition {
    /// 合计列下标；检测只在找到合计列时成立，因此成功时必为 `Some`。
    pub whole: Option<usize>,
    /// 部分列下标，最少 2 列、最多 4 列。
    pub parts: Vec<usize>,
    /// `true` 表示每行「和 ≈ 合计」（相对误差 ≤ [`COMPOSITION_TOLERANCE`]）；
    /// `false` 表示「和 ≤ 合计且至少一行占到合计的 [`COMPOSITION_SHARE`] 以上」的近似组合。
    pub exact: bool,
}

/// 一张表的语义分析结果。
#[derive(Debug, Clone, PartialEq)]
pub struct TableAnalysis {
    /// 全部列（含未参与绘图的部分）。
    pub columns: Vec<Column>,
    /// 类别列下标。
    pub label: Option<usize>,
    /// 候选测量列（列序升序，已剔除常量列）。
    pub measures: Vec<usize>,
    /// 参与绘图的数据行下标（指向 [`RawTable::rows`]，升序）。
    pub rows: Vec<usize>,
    /// 被剔除的合计行下标（指向 [`RawTable::rows`]，升序）。
    pub totals: Vec<usize>,
    /// 份额组合。
    pub composition: Option<Composition>,
}

/// 日期/时间文本在该列中的占比达到此值即认定 [`ColumnRole::Temporal`]。
const TEMPORAL_RATIO: f64 = 0.8;
/// 数值占比达到此值即认定 [`ColumnRole::Measure`]。
const MEASURE_RATIO: f64 = 0.8;
/// 宽松数值占比：允许少量状态词（`TIMEOUT`、`失败`）混在测量列里。
const MEASURE_RATIO_LOOSE: f64 = 0.6;
/// 宽松路径下非数值单元格的最大字符数：更长的说明文字说明该列不是测量列。
const MEASURE_TEXT_MAX: usize = 24;
/// 连续整数列至少要有这么多数值才判定 [`ColumnRole::Index`]：两行数据的偶然递增不算编号。
const INDEX_MIN_VALUES: usize = 3;
/// 份额组合的相对误差容忍度（0.5%）。
const COMPOSITION_TOLERANCE: f64 = 0.005;
/// 近似组合里部分之和至少要在某一行占到整体的这个比例（5%）。
const COMPOSITION_SHARE: f64 = 0.05;
/// 份额组合的部分列数范围。
const COMPOSITION_MIN_PARTS: usize = 2;
/// 份额组合的部分列数上限。
const COMPOSITION_MAX_PARTS: usize = 4;
/// 斜杠列表（`a / b / c`）的项数范围。
const SLASH_LIST_MIN: usize = 2;
/// 斜杠列表的项数上限。
const SLASH_LIST_MAX: usize = 4;
/// 状态词备注的最大字符数。
const NOTES_MAX_CHARS: usize = 24;

/// 整格等于其中之一即视为缺失（比较时忽略 ASCII 大小写）。
///
/// 状态记号（`✗` 等）在去装饰阶段已被剥离，只剩记号时会退化成空串，同样落进 [`Cell::Missing`]。
const MISSING_TEXT: &[&str] = &[
    "-", "—", "–", "−", "n/a", "na", "none", "?", "…", "...", "null", "nan", "tbd", "·", "x",
];

/// 合计行的首格词表（允许尾随 `:` / `：`）。
const TOTAL_LABELS: &[&str] = &[
    "total",
    "totals",
    "sum",
    "overall",
    "grand total",
    "subtotal",
    "合计",
    "总计",
    "总和",
    "小计",
    "平均",
    "均值",
    "中位数",
];

/// 编号列表头词表。
const INDEX_HEADERS: &[&str] = &[
    "#", "no", "no.", "id", "rank", "row", "idx", "index", "序号", "编号", "行号", "版本",
];

/// 顺序列表头词表（配合单调不减的数值）。
const SEQUENCE_HEADERS: &[&str] = &[
    "step",
    "phase",
    "stage",
    "round",
    "tier",
    "level",
    "iter",
    "iteration",
    "run",
    "batch",
    "day",
    "week",
    "month",
    "quarter",
    "n",
    "阶段",
    "轮次",
    "迭代",
    "批次",
    "第",
];

/// 单位词表项：`mult` 把书写数值放大到基本单位，`factor` 用于展示换算（两者对同一单位相等）。
#[derive(Debug, Clone, Copy)]
struct UnitEntry {
    /// 书写形式。
    text: &'static str,
    /// 量纲。
    dim: Dimension,
    /// 展示换算因子（显示值 = value / factor）。
    factor: f64,
    /// 基本单位乘数。
    mult: f64,
}

mod units;
use units::header_unit;
mod text;
use text::clean;
use text::is_missing;
pub(crate) use text::is_status_word;
use text::is_structured_text;
use text::is_total_label;
mod numbers;
use numbers::scan_number;
mod classification;
use classification::choose_label;
use classification::classify;
use classification::detect_composition;
use classification::dominant_unit;

/// 一次单位命中（书写切片 + 换算信息）。
#[derive(Debug, Clone, Copy)]
struct UnitHit<'a> {
    /// 书写形式。
    text: &'a str,
    /// 量纲。
    dim: Dimension,
    /// 展示换算因子。
    factor: f64,
    /// 基本单位乘数。
    mult: f64,
}

impl UnitEntry {
    /// 转成命中结果（借用静态词表）。
    fn hit(self) -> UnitHit<'static> {
        UnitHit {
            text: self.text,
            dim: self.dim,
            factor: self.factor,
            mult: self.mult,
        }
    }
}

/// 分析一张表：解析单元格、判定列角色、挑出绘图行与份额组合。
pub fn analyze(table: &RawTable) -> TableAnalysis {
    let width = table
        .header
        .len()
        .max(table.rows.iter().map(Vec::len).max().unwrap_or(0));
    let row_count = table.rows.len();

    // 合计行先剔除：角色判定若把「合计」行也算进去，连续编号、常量列都会被它带偏。
    let mut rows = Vec::with_capacity(row_count);
    let mut totals = Vec::new();
    for (index, row) in table.rows.iter().enumerate() {
        let first = row.first().map_or(Cow::Borrowed(""), |cell| clean(cell));
        if is_total_label(first.trim()) {
            totals.push(index);
        } else {
            rows.push(index);
        }
    }

    let mut columns = Vec::with_capacity(width);
    for index in 0..width {
        let header = table
            .header
            .get(index)
            .map_or(String::new(), |head| clean(head).trim().to_owned());
        let hint = header_unit(&header);
        let cells: Vec<Cell> = (0..row_count)
            .map(|row| {
                table.rows[row]
                    .get(index)
                    .map_or(Cell::Missing, |cell| parse_cell(cell, hint))
            })
            .collect();
        let view: Vec<&Cell> = rows.iter().map(|&row| &cells[row]).collect();
        let numeric = view
            .iter()
            .filter(|cell| matches!(cell, Cell::Number(_)))
            .count();
        let mut constant = numeric > 0;
        let mut first_value = None;
        for cell in &view {
            let Cell::Number(number) = cell else { continue };
            if first_value.is_some_and(|value| value != number.value) {
                constant = false;
                break;
            }
            first_value = first_value.or(Some(number.value));
        }
        let (dim, unit) = dominant_unit(&view);
        columns.push(Column {
            index,
            role: classify(&header, &view),
            header,
            cells,
            dim,
            unit,
            numeric,
            constant,
        });
    }

    let measures: Vec<usize> = columns
        .iter()
        .filter(|column| column.role == ColumnRole::Measure && !column.constant)
        .map(|column| column.index)
        .collect();
    let label = choose_label(&columns, &rows);
    let composition = detect_composition(&columns, &measures, &rows);
    TableAnalysis {
        columns,
        label,
        measures,
        rows,
        totals,
        composition,
    }
}

impl Column {
    /// 取指定行（[`RawTable::rows`] 下标）的单元格。
    pub(crate) fn cell_at(&self, row: usize) -> &Cell {
        &self.cells[row]
    }

    /// 取指定行的数值；非数值单元格返回 `None`。
    pub(crate) fn value_at(&self, row: usize) -> Option<f64> {
        match self.cell_at(row) {
            Cell::Number(n) => Some(n.value),
            _ => None,
        }
    }
}

/// 解析单个单元格；`hint` 是表头标注的列单位（`耗时 (ms)`），只用于补齐没写单位的裸数字。
fn parse_cell(raw: &str, hint: Option<UnitHit<'_>>) -> Cell {
    let cleaned = clean(raw);
    let text = cleaned.trim();
    if text.is_empty() || is_missing(text) {
        return Cell::Missing;
    }
    if is_structured_text(text) {
        return Cell::Text(text.to_owned());
    }
    match scan_number(text, hint) {
        Some(number) => Cell::Number(number),
        None => Cell::Text(text.to_owned()),
    }
}

// ---------------------------------------------------------------- 数值扫描

/// 数值扫描器：只在文本上按字节位置推进，不分配中间字符串。
struct Scanner<'a> {
    /// 被扫描的（去装饰后的）单元格文本。
    text: &'a str,
    /// 当前字节位置。
    pos: usize,
}

#[cfg(test)]
mod tests;

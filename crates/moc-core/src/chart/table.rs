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

/// 时间单位（基本单位：秒）。
const TIME_UNITS: &[UnitEntry] = &[
    UnitEntry {
        text: "ns",
        dim: Dimension::Duration,
        factor: 1e-9,
        mult: 1e-9,
    },
    UnitEntry {
        text: "µs",
        dim: Dimension::Duration,
        factor: 1e-6,
        mult: 1e-6,
    },
    UnitEntry {
        text: "μs",
        dim: Dimension::Duration,
        factor: 1e-6,
        mult: 1e-6,
    },
    UnitEntry {
        text: "us",
        dim: Dimension::Duration,
        factor: 1e-6,
        mult: 1e-6,
    },
    UnitEntry {
        text: "ms",
        dim: Dimension::Duration,
        factor: 1e-3,
        mult: 1e-3,
    },
    UnitEntry {
        text: "s",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "sec",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "secs",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "second",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "seconds",
        dim: Dimension::Duration,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "m",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "min",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "mins",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "minute",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "minutes",
        dim: Dimension::Duration,
        factor: 60.0,
        mult: 60.0,
    },
    UnitEntry {
        text: "h",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hr",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hrs",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hour",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "hours",
        dim: Dimension::Duration,
        factor: 3600.0,
        mult: 3600.0,
    },
    UnitEntry {
        text: "d",
        dim: Dimension::Duration,
        factor: 86400.0,
        mult: 86400.0,
    },
    UnitEntry {
        text: "day",
        dim: Dimension::Duration,
        factor: 86400.0,
        mult: 86400.0,
    },
    UnitEntry {
        text: "days",
        dim: Dimension::Duration,
        factor: 86400.0,
        mult: 86400.0,
    },
];

/// 字节单位（基本单位：字节）；`iB` 系是 1024 进制，`B` 系是 1000 进制。
const BYTE_UNITS: &[UnitEntry] = &[
    UnitEntry {
        text: "KiB",
        dim: Dimension::Bytes,
        factor: 1024.0,
        mult: 1024.0,
    },
    UnitEntry {
        text: "MiB",
        dim: Dimension::Bytes,
        factor: 1_048_576.0,
        mult: 1_048_576.0,
    },
    UnitEntry {
        text: "GiB",
        dim: Dimension::Bytes,
        factor: 1_073_741_824.0,
        mult: 1_073_741_824.0,
    },
    UnitEntry {
        text: "TiB",
        dim: Dimension::Bytes,
        factor: 1_099_511_627_776.0,
        mult: 1_099_511_627_776.0,
    },
    UnitEntry {
        text: "KB",
        dim: Dimension::Bytes,
        factor: 1e3,
        mult: 1e3,
    },
    UnitEntry {
        text: "MB",
        dim: Dimension::Bytes,
        factor: 1e6,
        mult: 1e6,
    },
    UnitEntry {
        text: "GB",
        dim: Dimension::Bytes,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "TB",
        dim: Dimension::Bytes,
        factor: 1e12,
        mult: 1e12,
    },
    UnitEntry {
        text: "bytes",
        dim: Dimension::Bytes,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "byte",
        dim: Dimension::Bytes,
        factor: 1.0,
        mult: 1.0,
    },
    UnitEntry {
        text: "B",
        dim: Dimension::Bytes,
        factor: 1.0,
        mult: 1.0,
    },
];

/// 量级单位（基本单位：计数）。大小写敏感：`k` 千、`M` 百万、`G` 十亿、`T` 万亿、`b`/`bn` 十亿。
const MAGNITUDE_UNITS: &[UnitEntry] = &[
    UnitEntry {
        text: "bn",
        dim: Dimension::Count,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "k",
        dim: Dimension::Count,
        factor: 1e3,
        mult: 1e3,
    },
    UnitEntry {
        text: "M",
        dim: Dimension::Count,
        factor: 1e6,
        mult: 1e6,
    },
    UnitEntry {
        text: "G",
        dim: Dimension::Count,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "T",
        dim: Dimension::Count,
        factor: 1e12,
        mult: 1e12,
    },
    UnitEntry {
        text: "b",
        dim: Dimension::Count,
        factor: 1e9,
        mult: 1e9,
    },
    UnitEntry {
        text: "pp",
        dim: Dimension::Percent,
        factor: 1.0,
        mult: 1.0,
    },
];

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

/// 该文本是否像「状态词」（`TIMEOUT`、`失败`）：既不是日期/时间/版本等结构化文本，也不是长说明。
pub(crate) fn is_status_word(text: &str) -> bool {
    !is_structured_text(text) && text.chars().count() <= NOTES_MAX_CHARS
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

/// 表头里的单位：`耗时 (ms)`、`体积（KiB）`、`吞吐 tok/s`、`覆盖率 %`。
///
/// 单元格自己写了单位时以单元格为准；表头单位只补齐裸数字，
/// 否则不同量纲的列会全被当成无量纲计数并进同一组，散点与堆叠都会判错。
fn header_unit(header: &str) -> Option<UnitHit<'_>> {
    let body = trailing_group(header).unwrap_or_else(|| last_word(header));
    let mut scanner = Scanner { text: body, pos: 0 };
    let hit = scanner.take_unit()?;
    scanner.skip_spaces();
    (scanner.pos == body.len()).then_some(hit)
}

/// 末尾括号组的内容（`耗时 (ms)` → `ms`）。
fn trailing_group(header: &str) -> Option<&str> {
    let start = header.rfind(['(', '（'])?;
    let open = header[start..].chars().next()?.len_utf8();
    let close = header[start + open..].find([')', '）'])?;
    Some(header[start + open..start + open + close].trim())
}

/// 最后一个空白分隔的词；没有空白时就是整个表头。
fn last_word(header: &str) -> &str {
    header.rsplit(char::is_whitespace).next().unwrap_or(header)
}

/// 整格是否是无值记号。
fn is_missing(text: &str) -> bool {
    MISSING_TEXT
        .iter()
        .any(|mark| text.eq_ignore_ascii_case(mark))
}

/// 首格是否是合计词。
fn is_total_label(text: &str) -> bool {
    let trimmed = text.trim().trim_end_matches([':', '：']).trim();
    TOTAL_LABELS
        .iter()
        .any(|label| trimmed.eq_ignore_ascii_case(label))
}

// ---------------------------------------------------------------- 去装饰

/// 去装饰：还原 `\|`、保留 `` `code` `` 内容、剥离强调/删除线标记与链接 URL、
/// HTML 标签换成空格、剥离状态记号、折叠空白。
///
/// 没有需要处理的字符时直接借用原串（数值单元格是热路径，避免无谓分配）。
fn clean(cell: &str) -> Cow<'_, str> {
    if !needs_cleaning(cell) {
        return Cow::Borrowed(cell.trim());
    }
    let mut out = String::with_capacity(cell.len());
    let mut pending_space = false;
    let mut pos = 0;
    while pos < cell.len() {
        let ch = cell[pos..].chars().next().expect("pos 始终落在字符边界");
        match ch {
            '\\' => {
                if cell[pos + 1..].starts_with('|') {
                    push_clean(&mut out, &mut pending_space, '|');
                    pos += 2;
                } else {
                    push_clean(&mut out, &mut pending_space, '\\');
                    pos += 1;
                }
            }
            '`' => match cell[pos + 1..].find('`') {
                Some(end) => {
                    for inner in cell[pos + 1..pos + 1 + end].chars() {
                        push_clean(&mut out, &mut pending_space, inner);
                    }
                    pos += 1 + end + 1;
                }
                None => pos += 1,
            },
            // 强调/删除线标记；单个 `~` 是近似记号，必须留给扫描器
            '*' | '_' => pos += 1,
            '~' => {
                if cell[pos + 1..].starts_with('~') {
                    pos += 2;
                } else {
                    push_clean(&mut out, &mut pending_space, '~');
                    pos += 1;
                }
            }
            '[' => match link_text(&cell[pos..]) {
                Some((text, len)) => {
                    for inner in text.chars() {
                        push_clean(&mut out, &mut pending_space, inner);
                    }
                    pos += len;
                }
                None => {
                    push_clean(&mut out, &mut pending_space, '[');
                    pos += 1;
                }
            },
            '!' if cell[pos + 1..].starts_with('[') => pos += 1,
            '<' => match html_tag_len(&cell[pos..]) {
                Some(len) => {
                    if !out.is_empty() {
                        pending_space = true;
                    }
                    pos += len;
                }
                None => {
                    push_clean(&mut out, &mut pending_space, '<');
                    pos += 1;
                }
            },
            mark if is_status_mark(mark) => pos += mark.len_utf8(),
            blank if blank.is_whitespace() => {
                if !out.is_empty() {
                    pending_space = true;
                }
                pos += blank.len_utf8();
            }
            other => {
                push_clean(&mut out, &mut pending_space, other);
                pos += other.len_utf8();
            }
        }
    }
    Cow::Owned(out)
}

/// 是否需要走 [`clean`]：含标记、状态记号、HTML 标签前缀或需要折叠的空白。
fn needs_cleaning(text: &str) -> bool {
    if text.contains("![") || text.contains("~~") {
        return true;
    }
    let mut prev_space = false;
    for ch in text.chars() {
        match ch {
            '\\' | '`' | '*' | '_' | '[' | '<' => return true,
            mark if is_status_mark(mark) => return true,
            blank if blank.is_whitespace() => {
                if blank != ' ' || prev_space {
                    return true;
                }
                prev_space = true;
                continue;
            }
            _ => {}
        }
        prev_space = false;
    }
    false
}

/// 追加一个字符，必要时先补一个空格（同时抑制行首与结尾空格）。
fn push_clean(out: &mut String, pending_space: &mut bool, ch: char) {
    if *pending_space && !out.is_empty() {
        out.push(' ');
    }
    *pending_space = false;
    out.push(ch);
}

/// 状态记号：含义由相邻数值承载，单独出现时只是噪声。
fn is_status_mark(ch: char) -> bool {
    matches!(
        ch,
        '✅' | '❌' | '⚠' | '✓' | '✗' | '✔' | '★' | '⭐' | '↑' | '↓' | '▲' | '▼'
    )
}

/// `[text](url)` / `[text]` 的文本部分；返回 `(文本, 整段字节长度)`。
fn link_text(section: &str) -> Option<(&str, usize)> {
    let rest = &section[1..];
    let end = rest.find(']')?;
    let text = &rest[..end];
    let after = &rest[end + 1..];
    if after.starts_with('(') {
        let close = after.find(')')?;
        Some((text, 1 + end + 1 + close + 1))
    } else {
        Some((text, 1 + end + 1))
    }
}

/// HTML 标签的字节长度（含 `<>`）：`<` 后必须是字母或 `/`/`!`/`?`，且 64 字节内闭合。
///
/// `< 5`、`<=10` 不是标签，必须原样留给近似记号扫描器。
fn html_tag_len(section: &str) -> Option<usize> {
    let rest = &section[1..];
    let first = rest.chars().next()?;
    if !(first.is_ascii_alphabetic() || matches!(first, '/' | '!' | '?')) {
        return None;
    }
    let mut limit = rest.len().min(64);
    while !rest.is_char_boundary(limit) {
        limit -= 1;
    }
    let end = rest[..limit].find('>')?;
    Some(1 + end + 1)
}

// ---------------------------------------------------------------- 结构化文本

/// 日期、时间、版本号、`#123`、十六进制串：都是「看起来像数字但不能当数值画」的文本。
fn is_structured_text(text: &str) -> bool {
    is_date_like(text)
        || is_time_like(text)
        || is_version_like(text)
        || is_hex_like(text)
        || is_issue_ref(text)
}

/// 日期形态：`2024-01-05`、`2024/01/05`、`1/5/24`、`2024-01`（年-月）、含 `年` 的中文写法。
fn is_date_like(text: &str) -> bool {
    if text.contains('年') {
        return true;
    }
    let Some((first, second, third)) =
        split_numeric(text, '-').or_else(|| split_numeric(text, '/'))
    else {
        return false;
    };
    match third {
        // `YYYY-MM-DD`：首段 4 位年份；`M/D/YY`：末段 2 位或 4 位年份。
        Some(third) => {
            (first.len() == 4
                && (1..=12).contains(&count(second))
                && (1..=31).contains(&count(third)))
                || ((1..=12).contains(&count(first))
                    && (1..=31).contains(&count(second))
                    && matches!(third.len(), 2 | 4))
        }
        // `YYYY-MM`：年-月，不按区间解析，否则「月」会被当成区间上界。
        None => first.len() == 4 && (1..=12).contains(&count(second)),
    }
}

/// 时间形态：`MM:SS`、`HH:MM:SS`（秒/分不超过 59）。
fn is_time_like(text: &str) -> bool {
    let Some((first, second, third)) = split_numeric(text, ':') else {
        return false;
    };
    let in_range = |part: &str| count(part) <= 59;
    match third {
        Some(third) => first.len() <= 3 && in_range(second) && in_range(third),
        None => first.len() <= 3 && in_range(second),
    }
}

/// 版本形态：`v1.2.3`、`1.2.3`（至少两个点，或带 `v` 前缀的 `1.2`）。
fn is_version_like(text: &str) -> bool {
    let (body, tagged) = match text.strip_prefix(['v', 'V']) {
        Some(body) => (body, true),
        None => (text, false),
    };
    let mut parts = 0;
    for part in body.split('.') {
        if part.is_empty() || !is_digits(part) {
            return false;
        }
        parts += 1;
    }
    parts >= 3 || (tagged && parts >= 2)
}

/// 十六进制串：`0x` 前缀，或 7–40 位且至少含一个字母的十六进制（纯数字是计数，不是哈希）。
fn is_hex_like(text: &str) -> bool {
    if let Some(rest) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return (1..=40).contains(&rest.len()) && rest.bytes().all(|byte| byte.is_ascii_hexdigit());
    }
    (7..=40).contains(&text.len())
        && text.bytes().all(|byte| byte.is_ascii_hexdigit())
        && text.bytes().any(|byte| byte.is_ascii_alphabetic())
}

/// `#123` 形态的引用编号。
fn is_issue_ref(text: &str) -> bool {
    text.strip_prefix('#').is_some_and(is_digits)
}

/// 按分隔符切成 2–3 段数字；段数不符或出现非数字段时返回 `None`。
fn split_numeric(text: &str, sep: char) -> Option<(&str, &str, Option<&str>)> {
    let mut parts = text.split(sep);
    let first = parts.next()?;
    let second = parts.next()?;
    let third = parts.next();
    if parts.next().is_some() {
        return None;
    }
    if !is_digits(first) || !is_digits(second) {
        return None;
    }
    if let Some(third) = third
        && !is_digits(third)
    {
        return None;
    }
    Some((first, second, third))
}

/// 是否非空的纯 ASCII 数字串。
fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// 数字串的数值；用于范围判断，溢出时按最大值处理（日期/时间的段不会溢出）。
fn count(text: &str) -> u32 {
    text.parse().unwrap_or(u32::MAX)
}

// ---------------------------------------------------------------- 数值扫描

/// 数值扫描器：只在文本上按字节位置推进，不分配中间字符串。
struct Scanner<'a> {
    /// 被扫描的（去装饰后的）单元格文本。
    text: &'a str,
    /// 当前字节位置。
    pos: usize,
}

impl<'a> Scanner<'a> {
    /// 剩余文本。
    fn rest(&self) -> &'a str {
        &self.text[self.pos..]
    }

    /// 跳过空白。
    fn skip_spaces(&mut self) {
        while let Some(ch) = self.rest().chars().next() {
            if ch.is_whitespace() {
                self.pos += ch.len_utf8();
            } else {
                break;
            }
        }
    }

    /// 近似记号（`~ ≈ < <= > >= ≤ ≥ 约`）；返回是否出现。
    fn take_approx(&mut self) -> bool {
        let mut seen = false;
        loop {
            let rest = self.rest();
            let len = if rest.starts_with("<=") || rest.starts_with(">=") {
                2
            } else {
                match rest.chars().next() {
                    Some(mark @ ('~' | '≈' | '<' | '>' | '≤' | '≥' | '约')) => {
                        mark.len_utf8()
                    }
                    _ => 0,
                }
            };
            if len == 0 {
                return seen;
            }
            self.pos += len;
            seen = true;
            self.skip_spaces();
        }
    }

    /// 符号：返回 `(是否负号, 是否出现显式符号)`；`±` 记正号但算显式符号。
    fn take_sign(&mut self) -> (bool, bool) {
        let rest = self.rest();
        if rest.starts_with('+') {
            self.pos += 1;
            (false, true)
        } else if rest.starts_with('−') {
            self.pos += '−'.len_utf8();
            (true, true)
        } else if rest.starts_with('-') {
            self.pos += 1;
            (true, true)
        } else if rest.starts_with('±') {
            self.pos += '±'.len_utf8();
            (false, true)
        } else {
            (false, false)
        }
    }

    /// 货币符号。
    fn take_currency(&mut self) -> Option<&'static str> {
        let currency = match self.rest().chars().next()? {
            '$' => "$",
            '€' => "€",
            '£' => "£",
            '¥' => "¥",
            _ => return None,
        };
        self.pos += currency.len();
        Some(currency)
    }

    /// 单位：速率（`tok/s`）、`%`、词表单位、倍数记号（`×`/`x`）。
    fn take_unit(&mut self) -> Option<UnitHit<'a>> {
        let rest = self.rest();
        // 速率必须先于普通单位判定，否则 `MB/s` 只会吃到 `MB`
        if let Some((text, len)) = rate_unit(rest) {
            self.pos += len;
            return Some(UnitHit {
                text,
                dim: Dimension::Rate,
                factor: 1.0,
                mult: 1.0,
            });
        }
        if rest.starts_with('%') {
            self.pos += 1;
            return Some(UnitHit {
                text: "%",
                dim: Dimension::Percent,
                factor: 1.0,
                mult: 1.0,
            });
        }
        if let Some(hit) = unit_token(rest) {
            self.pos += hit.text.len();
            return Some(hit);
        }
        if rest.starts_with('×') {
            self.pos += '×'.len_utf8();
            return Some(UnitHit {
                text: "×",
                dim: Dimension::Ratio,
                factor: 1.0,
                mult: 1.0,
            });
        }
        if rest.starts_with('x') && !is_alnum(rest[1..].chars().next()) {
            self.pos += 1;
            return Some(UnitHit {
                text: "x",
                dim: Dimension::Ratio,
                factor: 1.0,
                mult: 1.0,
            });
        }
        None
    }

    /// 「数字 + 可选单位」，数值以基本单位返回；失败时位置保持在调用前。
    fn take_number(&mut self) -> Option<(f64, Option<UnitHit<'a>>)> {
        let save = self.pos;
        self.skip_spaces();
        let Some((magnitude, end)) = scan_decimal(self.text, self.pos) else {
            self.pos = save;
            return None;
        };
        self.pos = end;
        let after_mantissa = self.pos;
        self.skip_spaces();
        let hit = match self.take_unit() {
            Some(hit) => Some(hit),
            None => {
                self.pos = after_mantissa;
                None
            }
        };
        Some((magnitude, hit))
    }

    /// 区间右端：`a–b`、`a—b`（可带空格）与 `a-b`（ASCII 连字符必须紧贴数字）。
    fn take_range(&mut self) -> Option<(f64, Option<UnitHit<'a>>)> {
        let save = self.pos;
        self.skip_spaces();
        let rest = self.rest();
        let dash = if rest.starts_with('–') || rest.starts_with('—') {
            '–'.len_utf8()
        } else if rest.starts_with('-')
            && self
                .text
                .as_bytes()
                .get(self.pos + 1)
                .is_some_and(u8::is_ascii_digit)
        {
            1
        } else {
            self.pos = save;
            return None;
        };
        self.pos += dash;
        match self.take_number() {
            Some(pair) => Some(pair),
            None => {
                self.pos = save;
                None
            }
        }
    }

    /// 箭头右端：`→`、`->`、`⇒`。
    fn take_arrow(&mut self) -> Option<(f64, Option<UnitHit<'a>>)> {
        let save = self.pos;
        self.skip_spaces();
        let rest = self.rest();
        let len = if rest.starts_with('→') {
            '→'.len_utf8()
        } else if rest.starts_with("->") {
            2
        } else if rest.starts_with('⇒') {
            '⇒'.len_utf8()
        } else {
            self.pos = save;
            return None;
        };
        self.pos += len;
        match self.take_number() {
            Some(pair) => Some(pair),
            None => {
                self.pos = save;
                None
            }
        }
    }

    /// 紧贴的分数 `a/b`：返回分母（分子是已解析的 `value`）；`12 / 12` 这种带空格的形式不算。
    fn take_fraction(&mut self) -> Option<f64> {
        if !self.rest().starts_with('/') {
            return None;
        }
        let save = self.pos;
        self.pos += 1;
        match scan_decimal(self.text, self.pos) {
            Some((denominator, end)) => {
                self.pos = end;
                Some(denominator)
            }
            None => {
                self.pos = save;
                None
            }
        }
    }

    /// 空格斜杠列表（`7648 / 176`、`a / b / c`）：剩余部分恰好是 1–3 段「/ 数值(单位)」。
    /// 值取第一项（已解析），其余各项只用于确认「这是一组并列数值」，不做换算。
    fn take_slash_list(&mut self) -> bool {
        let start = self.pos;
        let mut items = 1;
        loop {
            let save = self.pos;
            self.skip_spaces();
            if !self.rest().starts_with('/') {
                self.pos = save;
                break;
            }
            self.pos += 1;
            match self.take_number() {
                Some(_) => items += 1,
                None => {
                    self.pos = save;
                    break;
                }
            }
            if items > SLASH_LIST_MAX {
                self.pos = start;
                return false;
            }
        }
        if items < SLASH_LIST_MIN {
            self.pos = start;
            return false;
        }
        let tail = self.pos;
        self.skip_spaces();
        if self.pos == self.text.len() {
            true
        } else {
            self.pos = tail;
            false
        }
    }
}

/// 扫描一个数值单元格；返回 `None` 表示该文本不是数值。
fn scan_number(text: &str, hint: Option<UnitHit<'_>>) -> Option<Number> {
    let mut scanner = Scanner { text, pos: 0 };
    scanner.skip_spaces();
    let approx = scanner.take_approx();
    scanner.skip_spaces();
    let (negative, signed) = scanner.take_sign();
    scanner.skip_spaces();
    let currency = scanner.take_currency();
    scanner.skip_spaces();
    let (mantissa, end) = scan_decimal(text, scanner.pos)?;
    scanner.pos = end;

    // 单位紧跟数字或隔一个空格；没匹配到就把位置退回数字之后，尾部留给后缀规则
    let after_mantissa = scanner.pos;
    scanner.skip_spaces();
    let first_unit = scanner.take_unit();
    if first_unit.is_none() {
        scanner.pos = after_mantissa;
    }

    // 区间与箭头互斥；右端带单位时左端可以继承（`24–72 h`）
    let mut upper_input = None;
    let mut to_input = None;
    if let Some(pair) = scanner.take_range() {
        upper_input = Some(pair);
    } else if let Some(pair) = scanner.take_arrow() {
        to_input = Some(pair);
    }
    let right_unit = upper_input
        .as_ref()
        .and_then(|(_, hit)| *hit)
        .or_else(|| to_input.as_ref().and_then(|(_, hit)| *hit));
    let inherited = first_unit.or(right_unit);
    let (mut dim, mut factor, mult, mut unit) = match inherited {
        Some(hit) => (hit.dim, hit.factor, hit.mult, hit.text),
        None => (Dimension::Count, 1.0, 1.0, ""),
    };

    let mut value = mantissa * mult;
    // 复合时长（`1m 56s`）累加到秒：展示单位统一成 `s`，`1m 56s` 才和 `45s` 同组可比。
    if dim == Dimension::Duration {
        let mut compound = false;
        loop {
            let save = scanner.pos;
            match scanner.take_number() {
                Some((part, Some(hit))) if hit.dim == Dimension::Duration => {
                    value += part * hit.mult;
                    compound = true;
                }
                _ => {
                    scanner.pos = save;
                    break;
                }
            }
        }
        if compound {
            factor = 1.0;
            unit = "s";
        }
    }

    let mut upper = upper_input.map(|(magnitude, hit)| in_base(magnitude, hit, mult));
    let mut to = to_input.map(|(magnitude, hit)| in_base(magnitude, hit, mult));

    // 分数与斜杠列表：`12/12` 记分母，`7648 / 176` 只取第一项
    let mut denominator = None;
    if upper.is_none() && to.is_none() {
        match scanner.take_fraction() {
            Some(parsed) => {
                denominator = Some(parsed);
                dim = Dimension::Fraction;
                factor = 1.0;
                unit = "";
            }
            None => {
                scanner.take_slash_list();
            }
        }
    }
    if let Some(currency) = currency {
        dim = Dimension::Currency;
        unit = currency;
    }
    // 表头标注的单位由裸数字继承（列单位只在单元格自己没写单位时生效）
    if let Some(hint) = hint
        && dim == Dimension::Count
        && unit.is_empty()
    {
        dim = hint.dim;
        factor = hint.factor;
        unit = hint.text;
        value *= hint.mult;
        upper = upper.map(|bound| bound * hint.mult);
        to = to.map(|target| target * hint.mult);
    }

    Some(Number {
        value: if negative { -value } else { value },
        factor,
        dim,
        unit: unit.to_owned(),
        figure: text.trim().to_owned(),
        approx,
        signed,
        upper,
        to,
        denominator,
    })
}

/// 把书写数值折算到基本单位：右端自带单位就用右端的，否则继承左端单位。
fn in_base(magnitude: f64, hit: Option<UnitHit<'_>>, fallback: f64) -> f64 {
    magnitude * hit.map_or(fallback, |hit| hit.mult)
}

/// 从 `start` 起解析十进制字面量（千分位 `,`、小数、`e/E` 指数），返回 `(值, 结束位置)`。
fn scan_decimal(text: &str, start: usize) -> Option<(f64, usize)> {
    let bytes = text.as_bytes();
    let mut pos = start;
    let mut mantissa = 0.0;
    let mut digits = 0;
    while pos < bytes.len() {
        let byte = bytes[pos];
        if byte.is_ascii_digit() {
            mantissa = mantissa * 10.0 + f64::from(byte - b'0');
            digits += 1;
            pos += 1;
        } else if byte == b',' && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) {
            pos += 1;
        } else {
            break;
        }
    }
    if digits == 0 {
        return None;
    }
    let mut value = mantissa;
    if bytes.get(pos) == Some(&b'.') && bytes.get(pos + 1).is_some_and(u8::is_ascii_digit) {
        pos += 1;
        let mut scale = 0.1;
        while pos < bytes.len() && bytes[pos].is_ascii_digit() {
            value += f64::from(bytes[pos] - b'0') * scale;
            scale *= 0.1;
            pos += 1;
        }
    }
    if matches!(bytes.get(pos), Some(b'e' | b'E')) {
        let mut cursor = pos + 1;
        let mut negative = false;
        if matches!(bytes.get(cursor), Some(b'+' | b'-')) {
            negative = bytes[cursor] == b'-';
            cursor += 1;
        }
        let exp_start = cursor;
        let mut exponent = 0i32;
        while cursor < bytes.len() && bytes[cursor].is_ascii_digit() {
            exponent = (exponent * 10 + i32::from(bytes[cursor] - b'0')).min(1_000);
            cursor += 1;
        }
        if cursor > exp_start {
            value *= 10f64.powi(if negative { -exponent } else { exponent });
            pos = cursor;
        }
    }
    Some((value, pos))
}

/// 速率单位（`tok/s`、`MB/s`、`it/s`）：字母开头的一段 + `/` + 字母开头的一段。
fn rate_unit(rest: &str) -> Option<(&str, usize)> {
    let head_end = unit_chars(rest);
    if !rest[..head_end].chars().any(is_unit_letter) {
        return None;
    }
    if !rest[head_end..].starts_with('/') {
        return None;
    }
    let tail = &rest[head_end + 1..];
    let tail_end = unit_chars(tail);
    if !tail[..tail_end].chars().any(is_unit_letter) {
        return None;
    }
    let len = head_end + 1 + tail_end;
    Some((&rest[..len], len))
}

/// 从头部吃下单位字符（ASCII 字母数字与微符号），返回字节长度。
fn unit_chars(text: &str) -> usize {
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if ch.is_ascii_alphanumeric() || matches!(ch, 'µ' | 'μ') {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    end
}

/// 是否算速率的字母段（ASCII 字母或微符号）。
fn is_unit_letter(ch: char) -> bool {
    ch.is_ascii_alphabetic() || matches!(ch, 'µ' | 'μ')
}

/// 词表单位：按最长匹配取，并要求后续不是 ASCII 字母数字（`msx` 不是 `ms`）。
fn unit_token(rest: &str) -> Option<UnitHit<'static>> {
    let mut best: Option<UnitHit<'static>> = None;
    for entry in TIME_UNITS.iter().chain(BYTE_UNITS).chain(MAGNITUDE_UNITS) {
        if rest.starts_with(entry.text)
            && !is_alnum(rest[entry.text.len()..].chars().next())
            && best.is_none_or(|current| entry.text.len() > current.text.len())
        {
            best = Some(entry.hit());
        }
    }
    best
}

/// 字符是否是 ASCII 字母数字（用于单位边界判断）。
fn is_alnum(ch: Option<char>) -> bool {
    ch.is_some_and(|ch| ch.is_ascii_alphanumeric())
}

// ---------------------------------------------------------------- 列角色

/// 判定列角色。顺序即优先级：时间 → 编号 → 顺序 → 测量 → 标签。
fn classify(header: &str, cells: &[&Cell]) -> ColumnRole {
    if cells.is_empty() {
        return ColumnRole::Label;
    }
    let total = cells.len() as f64;
    let temporal = cells
        .iter()
        .filter(|cell| matches!(cell, Cell::Text(text) if is_date_like(text) || is_time_like(text)))
        .count();
    if temporal as f64 / total >= TEMPORAL_RATIO {
        return ColumnRole::Temporal;
    }
    if is_index_header(header) {
        return ColumnRole::Index;
    }
    if is_step_sequence(cells) {
        return ColumnRole::Index;
    }
    if is_sequence_header(header) && is_non_decreasing(cells) {
        return ColumnRole::Sequence;
    }
    let numeric = cells
        .iter()
        .filter(|cell| matches!(cell, Cell::Number(_)))
        .count();
    let ratio = numeric as f64 / total;
    if ratio >= MEASURE_RATIO {
        return ColumnRole::Measure;
    }
    if ratio >= MEASURE_RATIO_LOOSE
        && cells.iter().all(
            |cell| !matches!(cell, Cell::Text(text) if text.chars().count() > MEASURE_TEXT_MAX),
        )
    {
        return ColumnRole::Measure;
    }
    ColumnRole::Label
}

/// 表头是否是编号词。
fn is_index_header(header: &str) -> bool {
    let header = header.trim();
    INDEX_HEADERS
        .iter()
        .any(|word| header.eq_ignore_ascii_case(word))
}

/// 表头是否是顺序词。
fn is_sequence_header(header: &str) -> bool {
    let header = header.trim();
    SEQUENCE_HEADERS
        .iter()
        .any(|word| header.eq_ignore_ascii_case(word))
}

/// 数值是否是步长 1 的连续整数（`1,2,3`、`01,02,03`），且数值占比 ≥ 80%。
fn is_step_sequence(cells: &[&Cell]) -> bool {
    let mut seen = 0;
    let mut previous = None;
    let mut ok = true;
    for cell in cells {
        let Cell::Number(number) = cell else { continue };
        seen += 1;
        if number.value.fract() != 0.0 {
            ok = false;
        }
        if let Some(previous) = previous
            && number.value != previous + 1.0
        {
            ok = false;
        }
        previous = Some(number.value);
    }
    ok && seen >= INDEX_MIN_VALUES && seen * 5 >= cells.len() * 4
}

/// 数值是否单调不减（无数值视为满足：文本阶段列本身就是有序类别）。
fn is_non_decreasing(cells: &[&Cell]) -> bool {
    let mut previous = None;
    for cell in cells {
        let Cell::Number(number) = cell else { continue };
        if previous.is_some_and(|previous| number.value < previous) {
            return false;
        }
        previous = Some(number.value);
    }
    true
}

/// 该列出现次数最多的 `(量纲, 单位)`；并列取最先出现的。
fn dominant_unit(cells: &[&Cell]) -> (Option<Dimension>, String) {
    let mut seen: Vec<(Dimension, &str, usize)> = Vec::new();
    for cell in cells {
        let Cell::Number(number) = cell else { continue };
        match seen
            .iter_mut()
            .find(|(dim, unit, _)| *dim == number.dim && *unit == number.unit)
        {
            Some(entry) => entry.2 += 1,
            None => seen.push((number.dim, number.unit.as_str(), 1)),
        }
    }
    let mut best: Option<(Dimension, &str)> = None;
    let mut best_count = 0;
    for (dim, unit, count) in seen {
        if count > best_count {
            best = Some((dim, unit));
            best_count = count;
        }
    }
    match best {
        Some((dim, unit)) => (Some(dim), unit.to_owned()),
        None => (None, String::new()),
    }
}

/// 类别列：优先「有 ≥ 2 个互异文本」的 Label 列，其次任意 Label；
/// 再退到时间列与编号/顺序列（否则时间序列图永远选不出类别列）。
fn choose_label(columns: &[Column], rows: &[usize]) -> Option<usize> {
    let labelled = |column: &Column| column.role == ColumnRole::Label;
    if let Some(index) = columns
        .iter()
        .find(|column| labelled(column) && distinct_texts(column, rows) >= 2)
        .map(|column| column.index)
    {
        return Some(index);
    }
    columns
        .iter()
        .find(|column| labelled(column) || column.role == ColumnRole::Temporal)
        .or_else(|| {
            columns
                .iter()
                .find(|column| matches!(column.role, ColumnRole::Sequence | ColumnRole::Index))
        })
        .map(|column| column.index)
}

/// 该列参与绘图的行里有几个互异文本（最多数到 2 个就够了）。
fn distinct_texts(column: &Column, rows: &[usize]) -> usize {
    let mut seen: Vec<&str> = Vec::new();
    for &row in rows {
        let Cell::Text(text) = column.cell_at(row) else {
            continue;
        };
        if !seen.iter().any(|item| item == &text.as_str()) {
            seen.push(text);
            if seen.len() >= 2 {
                break;
            }
        }
    }
    seen.len()
}

// ---------------------------------------------------------------- 份额组合

/// 找出份额组合：几列之和等于另一列（`exact`），或不超过另一列且至少一行占到 5%。
///
/// 部分列必须同量纲同单位；合计列要求同量纲（单位可以不同，数值已归一化）。
/// 并列时取部分列最多的组合，仍然并列取先枚举到的（列序升序）。
fn detect_composition(
    columns: &[Column],
    measures: &[usize],
    rows: &[usize],
) -> Option<Composition> {
    if measures.len() < COMPOSITION_MIN_PARTS + 1 {
        return None;
    }
    let mut best: Option<Composition> = None;
    for &whole in measures {
        let Some(dim) = columns[whole].dim else {
            continue;
        };
        let pool: Vec<usize> = measures
            .iter()
            .copied()
            .filter(|&part| part != whole && columns[part].dim == Some(dim))
            .collect();
        let mut units: Vec<&str> = Vec::new();
        for &part in &pool {
            let unit = columns[part].unit.as_str();
            if !units.contains(&unit) {
                units.push(unit);
            }
        }
        for unit in units {
            // 部分列必须同量纲同单位：归一化只统一到基本单位，量级不同的列相加没有意义
            let group: Vec<usize> = pool
                .iter()
                .copied()
                .filter(|&part| columns[part].unit == unit)
                .collect();
            if group.len() < COMPOSITION_MIN_PARTS {
                continue;
            }
            for size in COMPOSITION_MIN_PARTS..=COMPOSITION_MAX_PARTS.min(group.len()) {
                let mut picked = Vec::with_capacity(size);
                enumerate(&group, size, 0, &mut picked, &mut |parts| {
                    if best
                        .as_ref()
                        .is_some_and(|current| current.parts.len() >= parts.len())
                    {
                        return;
                    }
                    if let Some(exact) = composition_relation(columns, parts, whole, rows) {
                        best = Some(Composition {
                            whole: Some(whole),
                            parts: parts.to_vec(),
                            exact,
                        });
                    }
                });
            }
        }
    }
    best
}

/// 枚举 `pool` 中长度为 `size` 的组合。
fn enumerate(
    pool: &[usize],
    size: usize,
    start: usize,
    picked: &mut Vec<usize>,
    check: &mut impl FnMut(&[usize]),
) {
    if picked.len() == size {
        check(picked);
        return;
    }
    for index in start..pool.len() {
        picked.push(pool[index]);
        enumerate(pool, size, index + 1, picked, check);
        picked.pop();
    }
}

/// 逐行验证「部分之和 ↔ 合计」：返回 `Some(exact)`，不成立返回 `None`。
///
/// 缺值的行跳过（不参与判定），但至少要有一行完整数据；任何一行「和 > 合计」都会否掉组合。
fn composition_relation(
    columns: &[Column],
    parts: &[usize],
    whole: usize,
    rows: &[usize],
) -> Option<bool> {
    let mut checked = 0;
    let mut exact = true;
    let mut share = false;
    for &row in rows {
        let Some(total) = columns[whole].value_at(row) else {
            continue;
        };
        let mut sum = 0.0;
        let mut complete = true;
        for &part in parts {
            match columns[part].value_at(row) {
                Some(value) => sum += value,
                None => {
                    complete = false;
                    break;
                }
            }
        }
        if !complete || total == 0.0 {
            continue;
        }
        checked += 1;
        if (sum - total).abs() > COMPOSITION_TOLERANCE * total.abs() {
            exact = false;
        }
        if sum > total * (1.0 + COMPOSITION_TOLERANCE) {
            return None;
        }
        if total > 0.0 && sum >= COMPOSITION_SHARE * total {
            share = true;
        }
    }
    if checked < 2 {
        return None;
    }
    if exact {
        Some(true)
    } else if share {
        Some(false)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(header: &[&str], rows: &[&[&str]]) -> RawTable {
        RawTable {
            header: header.iter().map(|head| (*head).to_owned()).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|cell| (*cell).to_owned()).collect())
                .collect(),
        }
    }

    /// 断言单元格一定是数值。
    fn number(text: &str) -> Number {
        match parse_cell(text, None) {
            Cell::Number(number) => number,
            other => panic!("`{text}` 应当解析成数值，实际是 {other:?}"),
        }
    }

    /// 单列表的列角色。
    fn role(header: &str, cells: &[&str]) -> ColumnRole {
        let raw = RawTable {
            header: vec![header.to_owned()],
            rows: cells.iter().map(|cell| vec![(*cell).to_owned()]).collect(),
        };
        analyze(&raw).columns[0].role
    }

    #[test]
    fn parses_plain_numbers_and_magnitudes() {
        // 千分位、小数、指数归一化成同一个 f64；量级单位折算到基本单位，`factor` 只负责展示
        let parsed = number("1,234.5");
        assert_eq!(parsed.value, 1234.5);
        assert_eq!(parsed.dim, Dimension::Count);
        assert_eq!(parsed.figure, "1,234.5");
        assert_eq!(number("1.5e3").value, 1500.0);
        // 前导小数点没有整数部分，不算数字（解析器只接受「数字开头」的书写）
        assert!(matches!(parse_cell(".5", None), Cell::Text(_)));
        let parsed = number("12k");
        assert_eq!(
            (parsed.value, parsed.factor, parsed.unit.as_str()),
            (12_000.0, 1_000.0, "k")
        );
        assert_eq!(number("3 M").value, 3e6);
        assert_eq!(number("3 b").value, 3e9);
        assert_eq!(number("2 T").value, 2e12);
        assert_eq!(number("5 bn").value, 5e9);
        assert_eq!(number("7 pp").dim, Dimension::Percent);
    }

    #[test]
    fn parses_approx_signs_and_currency() {
        // 近似记号剥离后仍记 `approx`；货币记量纲与符号，量级前缀照常折算
        let parsed = number("~$250");
        assert!(parsed.approx);
        assert!(!parsed.signed);
        assert_eq!(
            (parsed.dim, parsed.value, parsed.unit.as_str()),
            (Dimension::Currency, 250.0, "$")
        );
        let parsed = number("$1.2M");
        assert_eq!(
            (parsed.dim, parsed.value, parsed.factor),
            (Dimension::Currency, 1.2e6, 1e6)
        );
        let parsed = number("≥ 90%");
        assert!(parsed.approx);
        assert_eq!(parsed.value, 90.0);
        let parsed = number("约 200");
        assert!(parsed.approx);
        assert_eq!(parsed.value, 200.0);
        let parsed = number("-1547");
        assert!(parsed.signed);
        assert_eq!(parsed.value, -1547.0);
        assert_eq!(number("+12").value, 12.0);
    }

    #[test]
    fn parses_percent_and_duration() {
        let parsed = number("12.5%");
        assert_eq!(
            (parsed.dim, parsed.value, parsed.factor),
            (Dimension::Percent, 12.5, 1.0)
        );
        let parsed = number("12.5 ms");
        assert_eq!(parsed.dim, Dimension::Duration);
        assert!((parsed.value - 0.0125).abs() < 1e-12);
        assert_eq!((parsed.factor, parsed.unit.as_str()), (0.001, "ms"));
        assert_eq!(number("2 hours").value, 7200.0);
        // 复合时长累加到秒，和 `45s` 同组可比
        let parsed = number("1m 56s");
        assert_eq!(
            (parsed.value, parsed.factor, parsed.unit.as_str()),
            (116.0, 1.0, "s")
        );
        // 时钟写法是「时间」文本，不当作计数：否则时间点会被画成数值
        assert!(matches!(parse_cell("4:14", None), Cell::Text(_)));
        assert!(matches!(parse_cell("12:30:45", None), Cell::Text(_)));
    }

    #[test]
    fn byte_units_are_case_sensitive() {
        // `B` 是字节、`b` 是十亿、`M` 是百万、`m` 是分钟 —— 大小写必须区分
        let parsed = number("2 KiB");
        assert_eq!(
            (
                parsed.dim,
                parsed.value,
                parsed.factor,
                parsed.unit.as_str()
            ),
            (Dimension::Bytes, 2048.0, 1024.0, "KiB")
        );
        assert_eq!(number("3 MB").value, 3e6);
        assert_eq!(number("3 MB").factor, 1e6);
        let bytes = number("3 B");
        assert_eq!((bytes.dim, bytes.value), (Dimension::Bytes, 3.0));
        let billions = number("3 b");
        assert_eq!((billions.dim, billions.value), (Dimension::Count, 3e9));
        let minutes = number("3 m");
        assert_eq!((minutes.dim, minutes.value), (Dimension::Duration, 180.0));
        let millions = number("3 M");
        assert_eq!((millions.dim, millions.value), (Dimension::Count, 3e6));
        // 未登记的单位不吃：`Mb` 不是字节单位，`12 samples` 里的 `s` 也不是秒
        assert_eq!(number("3 nsx").value, 3.0);
        assert_eq!(number("12 samples").value, 12.0);
    }

    #[test]
    fn parses_range_arrow_fraction_and_slash_list() {
        let parsed = number("24–72 h");
        assert_eq!(
            (
                parsed.dim,
                parsed.value,
                parsed.factor,
                parsed.unit.as_str()
            ),
            (Dimension::Duration, 86400.0, 3600.0, "h")
        );
        assert_eq!(parsed.upper, Some(259200.0));
        let parsed = number("4.5-5.5");
        assert_eq!((parsed.value, parsed.upper), (4.5, Some(5.5)));
        // `12 - 15` 的空格连字符不当区间（与正文里的减法歧义），只保留左值
        assert_eq!(number("12 - 15").upper, None);
        let parsed = number("43 → 28");
        assert_eq!((parsed.value, parsed.to), (43.0, Some(28.0)));
        let parsed = number("12 -> 15 ms");
        assert_eq!(
            (parsed.dim, parsed.value, parsed.to),
            (Dimension::Duration, 0.012, Some(0.015))
        );
        let parsed = number("12/12");
        assert_eq!(
            (parsed.dim, parsed.value, parsed.denominator),
            (Dimension::Fraction, 12.0, Some(12.0))
        );
        let parsed = number("7648 / 176");
        assert_eq!(
            (parsed.dim, parsed.value, parsed.denominator),
            (Dimension::Count, 7648.0, None)
        );
        let parsed = number("3 MB/s");
        assert_eq!(
            (parsed.dim, parsed.value, parsed.unit.as_str()),
            (Dimension::Rate, 3.0, "MB/s")
        );
        assert_eq!(number("1.8x").dim, Dimension::Ratio);
    }

    #[test]
    fn treats_missing_and_status_marks() {
        for text in [
            "", "-", "—", "–", "n/a", "N/A", "none", "?", "…", "...", "null", "nan", "tbd", "·",
        ] {
            assert!(
                matches!(parse_cell(text, None), Cell::Missing),
                "`{text}` 应当是缺失值"
            );
        }
        // 状态记号剥离后只剩空串，同样算缺失
        assert!(matches!(parse_cell("⚠", None), Cell::Missing));
        assert_eq!(number("✅ 100").value, 100.0);
        assert_eq!(number("↑ 12.5%").value, 12.5);
        assert!(matches!(parse_cell("TIMEOUT", None), Cell::Text(_)));
    }

    #[test]
    fn structured_texts_are_not_numbers() {
        for text in [
            "2024-01-05",
            "2024/01/05",
            "1/5/24",
            "2024-01",
            "2024年1月5日",
            "4:14",
            "12:30:45",
            "v1.2.3",
            "1.2.3",
            "#123",
            "9f86d081",
            "0xdeadbeef",
        ] {
            assert!(
                matches!(parse_cell(text, None), Cell::Text(_)),
                "`{text}` 应当是非数值文本"
            );
        }
        // 单个点是小数、7 位纯数字是计数、6 位十六进制串只是短文本
        assert_eq!(number("1.5").value, 1.5);
        assert_eq!(number("1234567").value, 1_234_567.0);
        assert!(matches!(parse_cell("abcdef", None), Cell::Text(_)));
    }

    #[test]
    fn cleans_decorations_once() {
        // `\|` 还原、代码块保留内容、强调/删除线剥离、链接只留文本、HTML 标签换空格
        let raw = table(
            &["**地区**", "耗时 `ms`"],
            &[&["[华北](http://x)\\|1", "<b>12</b>"]],
        );
        let analysis = analyze(&raw);
        assert_eq!(analysis.columns[0].header, "地区");
        assert_eq!(analysis.columns[1].header, "耗时 ms");
        assert!(matches!(analysis.columns[0].cell_at(0), Cell::Text(text) if text == "华北|1"));
        // 表头 `耗时 \`ms\`` 的单位被裸数字继承，因此 12 归一化成 0.012 秒
        assert_eq!(analysis.columns[1].value_at(0), Some(0.012));
        // 近似记号 `~` 与 `<` 不能被去装饰吃掉
        assert!(number("~250").approx);
        assert!(number("< 5").approx);
    }

    #[test]
    fn classifies_column_roles() {
        // 时间占比 ≥ 80% → Temporal
        assert_eq!(
            role(
                "时间",
                &["2024-01-01", "2024-02-01", "2024-03-01", "2024-04-01", "x"]
            ),
            ColumnRole::Temporal
        );
        // 表头是编号词 → Index
        assert_eq!(role("序号", &["1", "2", "3"]), ColumnRole::Index);
        // 步长 1 的连续整数 → Index
        assert_eq!(role("n", &["1", "2", "3", "4"]), ColumnRole::Index);
        // 顺序词表头 + 文本阶段 → Sequence
        assert_eq!(
            role("step", &["准备", "执行", "收尾"]),
            ColumnRole::Sequence
        );
        // 数值占比 ≥ 80% → Measure
        assert_eq!(role("耗时", &["12", "15", "9"]), ColumnRole::Measure);
        // 宽松路径：1/4 状态词仍然算测量列
        assert_eq!(
            role("耗时", &["12", "TIMEOUT", "9", "15"]),
            ColumnRole::Measure
        );
        // 长文本把宽松路径挡回去
        assert_eq!(
            role(
                "说明",
                &[
                    "12",
                    "这是一段远远超过二十四个字符的说明文字用于测试宽松路径被拒绝",
                    "9"
                ]
            ),
            ColumnRole::Label
        );
        assert_eq!(role("地区", &["华北", "华南", "华东"]), ColumnRole::Label);
    }

    #[test]
    fn drops_total_rows_and_constant_columns() {
        let raw = table(
            &["地区", "耗时 ms", "站点数"],
            &[
                &["华北", "12", "3"],
                &["华南", "15", "3"],
                &["合计：", "27", "6"],
            ],
        );
        let analysis = analyze(&raw);
        assert_eq!(
            (analysis.rows.clone(), analysis.totals.clone()),
            (vec![0, 1], vec![2])
        );
        // 合计行不参与统计：常量列（3、3）不进入 measures
        assert_eq!(analysis.columns[2].numeric, 2);
        assert!(analysis.columns[2].constant);
        assert_eq!(analysis.measures, vec![1]);
        assert_eq!(analysis.label, Some(0));
    }

    #[test]
    fn detects_exact_and_loose_composition() {
        let raw = table(
            &["服务", "读", "写", "合计"],
            &[&["A", "10", "20", "30"], &["B", "5", "5", "10"]],
        );
        let composition = analyze(&raw).composition.expect("应当识别出精确份额组合");
        assert_eq!(
            (
                composition.whole,
                composition.parts.clone(),
                composition.exact
            ),
            (Some(3), vec![1, 2], true)
        );

        let raw = table(
            &["服务", "命中", "落空", "请求量"],
            &[&["A", "80", "20", "120"], &["B", "90", "5", "100"]],
        );
        let composition = analyze(&raw).composition.expect("应当识别出近似份额组合");
        assert_eq!(
            (
                composition.whole,
                composition.parts.clone(),
                composition.exact
            ),
            (Some(3), vec![1, 2], false)
        );

        // 与整体无关的测量列不构成组合
        let raw = table(
            &["服务", "耗时 ms", "体积 KiB"],
            &[&["A", "12", "3"], &["B", "15", "4"]],
        );
        assert!(analyze(&raw).composition.is_none());
    }

    #[test]
    fn keeps_table_order_and_selects_labels() {
        let raw = table(
            &["地区", "状态", "结果"],
            &[
                &["华北", "OK", "成功"],
                &["华南", "FAIL", "失败"],
                &["华东", "OK", "成功"],
            ],
        );
        let analysis = analyze(&raw);
        // 类别列取第一个「有 ≥ 2 个互异文本」的 Label 列，不做任何排序
        assert_eq!(analysis.label, Some(0));
        assert_eq!(analysis.rows, vec![0, 1, 2]);
        let text: Vec<&str> = analysis
            .rows
            .iter()
            .map(|&row| match analysis.columns[0].cell_at(row) {
                Cell::Text(t) => t.as_str(),
                _ => "",
            })
            .collect();
        assert_eq!(text, vec!["华北", "华南", "华东"]);
    }

    #[test]
    fn falls_back_to_structured_label_columns() {
        // 只有时间列时，时间列本身当类别列（否则时间序列表永远没有类别）
        let raw = table(
            &["日期", "耗时 ms"],
            &[&["2024-01-01", "12"], &["2024-01-02", "15"]],
        );
        let analysis = analyze(&raw);
        assert_eq!(analysis.columns[0].role, ColumnRole::Temporal);
        assert_eq!(analysis.label, Some(0));
        assert_eq!(analysis.measures, vec![1]);
    }
}

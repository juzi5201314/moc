use super::COMPOSITION_MAX_PARTS;
use super::COMPOSITION_MIN_PARTS;
use super::COMPOSITION_SHARE;
use super::COMPOSITION_TOLERANCE;
use super::Cell;
use super::Column;
use super::ColumnRole;
use super::Composition;
use super::Dimension;
use super::INDEX_HEADERS;
use super::INDEX_MIN_VALUES;
use super::MEASURE_RATIO;
use super::MEASURE_RATIO_LOOSE;
use super::MEASURE_TEXT_MAX;
use super::SEQUENCE_HEADERS;
use super::TEMPORAL_RATIO;
use super::text::is_date_like;
use super::text::is_time_like;
use super::text::is_total_label;

// ---------------------------------------------------------------- 列角色

/// 判定列角色。顺序即优先级：时间 → 编号 → 顺序 → 测量 → 标签。
pub(super) fn classify(header: &str, cells: &[&Cell]) -> ColumnRole {
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
pub(super) fn is_index_header(header: &str) -> bool {
    let header = header.trim();
    INDEX_HEADERS
        .iter()
        .any(|word| header.eq_ignore_ascii_case(word))
}

/// 表头是否是顺序词。
pub(super) fn is_sequence_header(header: &str) -> bool {
    let header = header.trim();
    SEQUENCE_HEADERS
        .iter()
        .any(|word| header.eq_ignore_ascii_case(word))
}

/// 数值是否是步长 1 的连续整数（`1,2,3`、`01,02,03`），且数值占比 ≥ 80%。
pub(super) fn is_step_sequence(cells: &[&Cell]) -> bool {
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
pub(super) fn is_non_decreasing(cells: &[&Cell]) -> bool {
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
pub(super) fn dominant_unit(cells: &[&Cell]) -> (Option<Dimension>, String) {
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
pub(super) fn choose_label(columns: &[Column], rows: &[usize]) -> Option<usize> {
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
pub(super) fn distinct_texts(column: &Column, rows: &[usize]) -> usize {
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

/// 找出份额组合：几列之和等于另一列（`exact`），或不超过「合计/总计」类列
/// 且至少一行占到 5%（`exact = false`）。
///
/// 近似组合要求整列的表头明确写成总量类词：否则像 `数值 / 平方 / 立方`
/// 这种「小列相加恰好小于大列」的数学关系也会被误当成份额组合，画成堆叠图。
/// 部分列必须同量纲同单位；合计列要求同量纲（单位可以不同，数值已归一化）。
/// 并列时取部分列最多的组合，仍然并列取先枚举到的（列序升序）。
pub(super) fn detect_composition(
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
                        // 近似组合只在整列表头明确是总量时成立；否则「平方/立方」
                        // 这种数学列会被误读成份额。
                        if !exact && !is_total_label(&columns[whole].header) {
                            return;
                        }
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
pub(super) fn enumerate(
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
pub(super) fn composition_relation(
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

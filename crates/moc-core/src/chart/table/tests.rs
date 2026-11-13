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
        &["服务", "命中", "落空", "总计"],
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

    // 数学列（数值 / 平方 / 立方）即使满足「小列之和小于大列」也不是份额组合：
    // 近似组合只认「合计 / 总计」这类整列表头。
    let raw = table(
        &["序号", "数值", "平方", "立方"],
        &[
            &["1", "12", "144", "1,728"],
            &["2", "27", "729", "19,683"],
            &["3", "45", "2,025", "91,125"],
        ],
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

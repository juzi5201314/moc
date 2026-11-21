use super::*;
use crate::render::PROSE_INDENT;
use crate::render::image::ImageKey;
use crate::render::image::layout as image_layout;
use crate::render::markdown::resources::file_key;
use crate::render::markdown::resources::file_url_path;
use crate::render::markdown::resources::percent_decode;
use moc_core::chart;
use std::path::PathBuf;

use crate::render::graphics::{CellPixels, GraphicsProto};
use crate::theme::{ColorLevel, Theme};

/// 一张能图表化的表：两行测量值 → 分组条形图（6 行 + 入口）。
const NUMERIC_TABLE: &str =
    "| 项 | 耗时 | 内存 |\n|---|---|---|\n| a | 10 ms | 2 MiB |\n| b | 20 ms | 4 MiB |";

fn graphics() -> Option<GraphicsSupport> {
    Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    })
}

fn render(source: &str, width: usize, views: RenderViews<'_>) -> Rendered {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    render_markdown_with(source, width, &theme, views)
}

#[test]
fn paged_layout_matches_full_layout_with_global_references_and_tables() {
    let source = format!("{}\n[ref]: https://example.com\n", "# 标题\n\n正文 **强调** 与 [链接][ref]。\n\n> 引用\n> - 子项\n\n```rust\n/* 连续\n注释 */ let x = 1;\n```\n\n| 月份 | 数量 |\n|---|---|\n| 一月 | 12 |\n| 二月 | 24 |\n\n".repeat(30));
    let theme = Theme::resolved(ColorLevel::TrueColor);
    for streaming in [false, true] {
        let views = || RenderViews {
            graphics: graphics(),
            streaming,
            ..Default::default()
        };
        let full = render_markdown_with(&source, 77, &theme, views());
        let paged = render_markdown_pages(&source, 77, &theme, views(), true, false);
        assert_eq!(full.tables, paged.tables);
        assert_eq!(full.lines.len(), paged.lines.len());
        for start in (0..full.lines.len()).step_by(63) {
            let end = (start + 97).min(full.lines.len());
            assert_eq!(paged.lines.window(start..end), full.lines[start..end]);
        }
    }
}

fn text(lines: &[Line<'static>]) -> Vec<String> {
    lines
        .iter()
        .map(|line| {
            line.spans
                .iter()
                .map(|span| span.content.as_ref())
                .collect()
        })
        .collect()
}

/// 图表视图态：按当前渲染出的纯文本快照就地分析。
fn chart_state(source: &str, table: usize, image_id: u32) -> TableChartState {
    let rendered = render(
        source,
        77,
        RenderViews {
            charts: &[],
            graphics: None,
            streaming: false,
            ..RenderViews::default()
        },
    );
    let raw = &rendered.raw[table];
    let analysis = chart::analyze(raw);
    let plan = chart::plan(&analysis).expect("数值表必须能规划出图型");
    TableChartState {
        spec: chart::build(&analysis, &plan),
        payload: None,
        px: (0, 0),
        image_id,
        transmitted: false,
    }
}

fn lines(source: &str, width: usize) -> Vec<String> {
    text(&render_markdown(
        source,
        width,
        &Theme::resolved(ColorLevel::TrueColor),
    ))
}

#[test]
fn tables_render_light_rules_and_keep_cell_content_in_order() {
    let single = lines("| 尝试 | 结果 |\n|---|---|\n| a | b |", 40);
    assert!(
        !single
            .iter()
            .any(|line| line.contains('┌') || line.contains('│')),
        "表格不再画全边框盒：{single:?}"
    );
    assert!(
        single.iter().any(|line| line.starts_with('─')),
        "表头/行之间应有细线：{single:?}"
    );
    assert!(
        single.iter().any(|line| line.contains("尝试")),
        "{single:?}"
    );

    let out = lines(
        "| 工具 | 用途 |\n|---|---|\n| `read` | 读取文件 |\n| `bash` | 执行命令 |",
        40,
    );
    let read_row = out.iter().position(|line| line.contains("read"));
    let bash_row = out.iter().position(|line| line.contains("bash"));
    assert!(read_row.is_some() && bash_row.is_some(), "{out:?}");
    assert!(read_row < bash_row, "表格行顺序错误：{out:?}");
    assert!(out.iter().any(|line| line.contains("读取文件")), "{out:?}");
    assert!(out.iter().any(|line| line.contains("执行命令")), "{out:?}");
}

/// 超宽表格按列折行，内容完整，线宽不超过正文宽度。
#[test]
fn wide_tables_wrap_instead_of_truncating() {
    let source = "| Harness | 请求体硬上限 | 发请求前的字节预算 | 单图目标 | 图片数量上限 | 超限行为 |\n|---|---|---|---|---|---|\n| moc | 无 | 全量序列化字节 50 MiB，高水位 -3 个槽位 | raw 3.75 MiB | 600 | 按旧→新淘汰图片，正文超限交上游 |\n| Codex | 无 | 无（纯 token 窗口 + compaction） | 2000px、base64 ≤5 MiB | 90 | 交上游处理 |\n| DSH | 无 | inline-image-byte-budget 24,000 字节 | 2 MiB / 4096px | 600 | 直接报 IMAGE_OFFLOAD_REQUIRED |";
    let out = lines(source, 120);
    for needle in [
        "Harness",
        "超限行为",
        "全量序列化字节",
        "inline-image-byte-budget",
        "按旧→新淘汰图片",
        "IMAGE_OFFLOAD",
    ] {
        assert!(
            out.iter().any(|line| line.contains(needle)),
            "缺内容 {needle}：{out:?}"
        );
    }
    assert!(
        !out.iter().any(|line| line.contains('…')),
        "表格不得截断：{out:?}"
    );
    assert!(
        out.iter().all(|line| str_width(line) <= 120),
        "表格行不得超宽：{out:?}"
    );
}

/// 代码块超宽行折行而非被 buffer 裁掉：内容完整、每行不超宽、续行带标记。
#[test]
fn long_code_lines_wrap_within_width() {
    let tail = "TAIL_MARKER";
    let source = format!("```text\n{} {tail}\n```", "x".repeat(70));
    let out = lines(&source, 40);
    assert!(out.iter().all(|line| str_width(line) <= 40), "{out:?}");
    assert!(out.iter().any(|line| line.contains(tail)), "{out:?}");
    assert!(
        out.iter().any(|line| line.contains(CODE_CONTINUATION)),
        "{out:?}"
    );
}

/// 引用续行保留竖线，列表续行悬挂到正文起点（不多也不少 1 列）。
#[test]
fn continuation_rows_keep_quote_bar_and_list_hang() {
    let quoted = lines(&format!("> {}", "引用正文".repeat(12)), 30);
    assert!(quoted.len() > 1, "{quoted:?}");
    assert!(
        quoted.iter().all(|line| line.starts_with("│ ")),
        "{quoted:?}"
    );

    let listed = lines("- alpha beta gamma delta epsilon zeta eta theta", 20);
    assert!(listed[0].starts_with("• alpha"), "{listed:?}");
    for line in &listed[1..] {
        assert!(
            line.starts_with("  ") && !line.starts_with("   "),
            "{listed:?}"
        );
    }
}

/// 以代码块开头的列表项保留自己的标记，且不泄漏给下一项。
#[test]
fn item_starting_with_code_keeps_its_bullet() {
    let out = lines("- ```\n  code\n  ```\n- next", 40);
    let bullets = out.iter().filter(|line| line.contains('•')).count();
    assert_eq!(bullets, 2, "{out:?}");
    assert!(out.iter().any(|line| line.starts_with("• next")), "{out:?}");
}

/// 放得下的表格按内容自然宽度，不把每列拉到上限。
#[test]
fn narrow_tables_keep_natural_widths() {
    let out = lines("| a | b |\n|---|---|\n| x | y |", 120);
    let rule = out
        .iter()
        .find(|line| line.starts_with('─'))
        .expect("表头细线");
    assert_eq!(str_width(rule), 1 + TABLE_COLUMN_GAP + 1, "{out:?}");
}

/// 行内代码只在紧贴文字时补空格；全角标点与已有空白旁不补。
#[test]
fn inline_code_gap_follows_neighbors() {
    assert_eq!(lines("调用`foo`即可", 40), vec!["调用 foo 即可"]);
    assert_eq!(lines("（`foo`）", 40), vec!["（foo）"]);
    assert_eq!(lines("run `foo` now", 40), vec!["run foo now"]);
}

/// 入口只在支持图形协议时出现；按钮左对齐且命中区间等于文字宽度。
#[test]
fn table_entry_appears_only_with_graphics_support() {
    let with = render(
        NUMERIC_TABLE,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    assert_eq!(with.tables.len(), 1, "一张顶层表格");
    let slot = with.tables[0];
    let button = slot.button.expect("支持图形时应有入口");
    let width = str_width(ENTRY_CHART);
    assert_eq!(button.w as usize, width);
    assert_eq!(button.x, 0, "与正文左缘对齐");
    assert_eq!(text(&with.lines)[button.row], "▸ 可视化图表");

    let without = render(
        NUMERIC_TABLE,
        80,
        RenderViews {
            charts: &[],
            graphics: None,
            streaming: false,
            ..RenderViews::default()
        },
    );
    assert!(without.tables[0].button.is_none(), "无图形能力时不出入口");
    assert!(
        text(&without.lines)
            .iter()
            .any(|line| line.contains("耗时")),
        "表格照常画"
    );
}

/// 流式尾部：末尾表格先不落定入口，其后已有正文的表格照常落定。
#[test]
fn streaming_tail_table_defers_its_entry() {
    let source = format!("{NUMERIC_TABLE}\n\n正文\n\n{NUMERIC_TABLE}");
    let streamed = render(
        &source,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: true,
            ..RenderViews::default()
        },
    );
    assert_eq!(streamed.tables.len(), 2);
    assert!(streamed.tables[0].button.is_some(), "后面的表格已落地");
    assert!(
        streamed.tables[1].button.is_none(),
        "末尾表格还在长，入口等收尾"
    );

    let settled = render(
        &source,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    assert!(settled.tables[1].button.is_some(), "收尾后入口出现");
}

/// 已切换成图表：表格保留，入口在表格下方，位图占位区在入口下方。
#[test]
fn chart_state_keeps_table_and_appends_placeholder_below() {
    let state = chart_state(NUMERIC_TABLE, 0, 7);
    let charts = [Some(state)];
    let rendered = render(
        NUMERIC_TABLE,
        80,
        RenderViews {
            charts: &charts,
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    let out = text(&rendered.lines);
    assert!(
        out.iter().any(|line| line.contains("耗时"))
            && out.iter().any(|line| line.contains("4 MiB")),
        "表格必须保留：{out:?}"
    );
    let slot = rendered.tables[0];
    let chart = slot.chart.expect("应有图表占位区");
    let button = slot.button.expect("应有隐藏入口");
    assert_eq!(chart.cols, 80, "正文宽度未超过 720px 上限");
    assert_eq!(chart.px, (80 * 8, u32::from(chart.rows) * 18));
    assert_eq!(button.row + 2, chart.row, "入口与图表之间应隔一个空行");
    assert!(
        out[button.row].starts_with("▾ 隐藏图表"),
        "表格下方是隐藏入口：{out:?}"
    );
    assert!(
        out[button.row + 1].trim().is_empty(),
        "入口与图表之间应为空行"
    );
    assert!(
        out[chart.row..chart.row + usize::from(chart.rows)]
            .iter()
            .all(|line| line.trim().is_empty()),
        "占位行必须是空白"
    );
}

/// 图表宽度固定为 720px 上限，即使正文更宽也不拉满；入口仍左对齐。
#[test]
fn chart_width_is_capped_at_720px() {
    let state = chart_state(NUMERIC_TABLE, 0, 7);
    let charts = [Some(state)];
    let rendered = render(
        NUMERIC_TABLE,
        200,
        RenderViews {
            charts: &charts,
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    let slot = rendered.tables[0];
    let chart = slot.chart.expect("应有图表占位区");
    let button = slot.button.expect("应有隐藏入口");
    assert_eq!(chart.cols, 90, "720 / 8 = 90 列");
    assert_eq!(chart.px.0, 720);
    assert_eq!(button.x, 0);
}

/// 多张表格：每张图的占位区都必须紧贴自己的入口，不能被后一张表的按钮挤歪。
#[test]
fn multiple_tables_keep_chart_rows_aligned() {
    let source = format!("{NUMERIC_TABLE}\n\n正文\n\n{NUMERIC_TABLE}");
    let charts = [Some(chart_state(NUMERIC_TABLE, 0, 7)), None];
    let rendered = render(
        &source,
        80,
        RenderViews {
            charts: &charts,
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    assert_eq!(rendered.tables.len(), 2);
    let first = rendered.tables[0];
    let second = rendered.tables[1];
    let first_chart = first.chart.expect("第一张表应展开图表");
    let first_button = first.button.expect("第一张表应有隐藏入口");
    let second_button = second.button.expect("第二张表应有可视化入口");
    assert_eq!(first_chart.row, first_button.row + 2);
    assert_eq!(second.chart, None);
    assert!(
        second_button.row > first_chart.row + usize::from(first_chart.rows),
        "第二张表的入口必须在第一张图之后"
    );
    let out = text(&rendered.lines);
    assert!(out[first_button.row].starts_with("▾ 隐藏图表"));
    assert!(out[second_button.row].starts_with("▸ 可视化图表"));
}

/// 纯文本快照与屏幕上的单元格一致：装饰标记不会漏进分析（`**7648**` → `7648`）。
#[test]
fn raw_snapshot_matches_visible_cells() {
    let source = "| 项 | 值 |\n|---|---|\n| **a** | `7648` |\n| [b](https://x) | 12 |";
    let rendered = render(
        source,
        80,
        RenderViews {
            charts: &[],
            graphics: None,
            streaming: false,
            ..RenderViews::default()
        },
    );
    let raw = &rendered.raw[0];
    assert_eq!(raw.header, vec!["项".to_owned(), "值".to_owned()]);
    assert_eq!(raw.rows[0], vec!["a".to_owned(), "7648".to_owned()]);
    assert_eq!(raw.rows[1], vec!["b".to_owned(), "12".to_owned()]);
}

/// 引用块 / 列表内嵌的表格不参与图表化：不进序号、不出入口，也不影响顶层序号。
#[test]
fn nested_tables_stay_out_of_the_chart_index() {
    let source =
        format!("> | 项 | 值 |\n> |---|---|---|\n> | a | 1 |\n> | b | 2 |\n\n{NUMERIC_TABLE}");
    let rendered = render(
        &source,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    assert_eq!(rendered.raw.len(), 1, "只有顶层表格进入快照");
    assert_eq!(rendered.tables.len(), 1);
    assert_eq!(rendered.tables[0].table, 0, "顶层序号从 0 起");
    assert!(rendered.tables[0].button.is_some());
}

/// 落一张图进缓存：解码结果只需要尺寸，位图内容与排版无关。
fn ready_store(key: &ImageKey, size: (u32, u32)) -> ImageStore {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let store = ImageStore::new(events);
    store.complete(
        key.clone(),
        Ok(crate::render::image::DecodedImage::Raster(
            image::RgbaImage::from_pixel(size.0, size.1, image::Rgba([0x10, 0x20, 0x30, 0xff])),
        )),
    );
    store
}

/// 本地图片按格贴出：留出与版式一致的行数，并登记一张落位。
#[test]
fn local_image_reserves_rows_and_registers_a_placement() {
    let directory = tempfile::tempdir().expect("临时目录");
    let path = directory.path().join("shot.png");
    std::fs::write(&path, b"not decoded here").expect("落盘");
    let key = file_key(&path).expect("文件存在");
    // 320×180 源图 → 640×360 之外的框里按 720 上限收窄（40 列 × 8px）
    let store = ready_store(&key, (320, 180));
    let source = "前文\n\n![](./shot.png)\n\n后文".to_owned();
    let rendered = render(
        &source,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            images: Some(&store),
            cwd: Some(directory.path()),
            spawn_images: false,
            links_expanded: true,
        },
    );
    let layout = image_layout((320, 180), CellPixels { w: 8, h: 18 }, 80).expect("版式");
    assert_eq!(rendered.images.len(), 1, "一张图一张落位");
    let placement = &rendered.images[0];
    assert_eq!(placement.key, key);
    assert_eq!((placement.cols, placement.rows), (layout.cols, layout.rows));
    assert_eq!(placement.x, u16::try_from(PROSE_INDENT).unwrap_or(0));
    // 占位行确实存在，且与落位行号对齐
    let lines = text(&rendered.lines);
    for (offset, line) in lines
        .iter()
        .skip(placement.row)
        .take(usize::from(placement.rows))
        .enumerate()
    {
        assert!(
            line.trim().is_empty(),
            "占位行 {} 必须留白：{line:?}",
            placement.row + offset
        );
    }
    assert!(
        lines.iter().any(|line| line.contains("后文")),
        "图片之后的正文照常输出"
    );
}

/// 文件不存在：回落 alt 文本，不留占位行（改动前的内联行为）。
#[test]
fn missing_image_falls_back_to_alt_text() {
    let directory = tempfile::tempdir().expect("临时目录");
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let store = ImageStore::new(events);
    let rendered = render(
        "![](./nope.png) 说明",
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            images: Some(&store),
            cwd: Some(directory.path()),
            spawn_images: false,
            links_expanded: true,
        },
    );
    assert!(rendered.images.is_empty());
    assert!(
        text(&rendered.lines)
            .iter()
            .any(|line| line.contains("说明"))
    );
}

/// 回归：画不出图的图片不该把段落劈成两行。
///
/// 远端图、不存在的本地文件都走 alt 文本；改动前 `Tag::Image` 无条件
/// 结算 inline 缓冲，连「永远不会显示」的图片也会把句子截断。
#[test]
fn non_renderable_image_keeps_the_paragraph_inline() {
    let rendered = render(
        "请看 ![远端图](https://example.com/a.png) 这个",
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            ..RenderViews::default()
        },
    );
    let lines = text(&rendered.lines);
    assert_eq!(lines.len(), 1, "段落被劈开了：{lines:?}");
    assert!(lines[0].contains("请看"));
    assert!(lines[0].contains("远端图"));
    assert!(lines[0].contains("这个"));
}

/// `file://` 的 host 段与百分号编码。
#[test]
fn file_url_path_handles_host_and_escapes() {
    assert_eq!(
        file_url_path("/tmp/a%20b.png").expect("空 host"),
        PathBuf::from("/tmp/a b.png"),
        "百分号编码要解开"
    );
    assert_eq!(
        file_url_path("localhost/tmp/a.png").expect("localhost"),
        PathBuf::from("/tmp/a.png"),
        "localhost 视作本机"
    );
    assert!(
        file_url_path("example.com/tmp/a.png").is_none(),
        "远程 host 拒绝"
    );
    // 非法转义原样保留：宁可找不到文件，也不静默改路径
    assert_eq!(percent_decode("a%2"), "a%2");
    assert_eq!(percent_decode("a%zz"), "a%zz");
}

/// 远端链接不联网、不渲染：只留 alt 文本。
#[test]
fn remote_images_never_render() {
    let directory = tempfile::tempdir().expect("临时目录");
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let store = ImageStore::new(events);
    let rendered = render(
        "![远端图](https://example.com/a.png)",
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            images: Some(&store),
            cwd: Some(directory.path()),
            spawn_images: true,
            links_expanded: true,
        },
    );
    assert!(rendered.images.is_empty());
    assert!(
        text(&rendered.lines)
            .iter()
            .any(|line| line.contains("远端图"))
    );
}

/// 引用块与列表里的图片不贴：没有独立命中区，宽度也不够。
#[test]
fn images_inside_containers_stay_textual() {
    let directory = tempfile::tempdir().expect("临时目录");
    let path = directory.path().join("shot.png");
    std::fs::write(&path, b"x").expect("落盘");
    let key = file_key(&path).expect("文件存在");
    let store = ready_store(&key, (64, 36));
    let source = "> ![](./shot.png)\n\n- ![](./shot.png)\n".to_owned();
    let rendered = render(
        &source,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            images: Some(&store),
            cwd: Some(directory.path()),
            spawn_images: false,
            links_expanded: true,
        },
    );
    assert!(rendered.images.is_empty(), "容器内不贴图");
}

/// SVG 代码块整块换成位图：就绪时不留一行代码。
#[test]
fn svg_fence_is_replaced_by_a_placeholder() {
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"200\" height=\"120\"><rect width=\"200\" height=\"120\" fill=\"#3b82f6\"/></svg>";
    let source = format!("```svg\n{svg}\n```");
    let key = ImageKey::from_svg(&format!("{svg}\n"));
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let store = ImageStore::new(events);
    store.complete(
        key,
        Ok(
            crate::render::image::decode::decode(crate::render::image::ImageSourceSpec::Svg(
                format!("{svg}\n").into_boxed_str(),
            ))
            .expect("SVG 可解析"),
        ),
    );
    let rendered = render(
        &source,
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            images: Some(&store),
            cwd: None,
            spawn_images: false,
            links_expanded: true,
        },
    );
    assert_eq!(rendered.images.len(), 1, "一张 SVG 落位");
    assert!(
        !text(&rendered.lines)
            .iter()
            .any(|line| line.contains("<svg") || line.contains("rect")),
        "就绪时不再输出代码行"
    );
}

/// 还没解码完就照常出代码块，解码完成后自动换成像素图。
#[test]
fn pending_svg_falls_back_to_the_code_block() {
    let svg = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"20\" height=\"10\"><rect width=\"20\" height=\"10\" fill=\"#000\"/></svg>";
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let store = ImageStore::new(events);
    let rendered = render(
        &format!("```svg\n{svg}\n```"),
        80,
        RenderViews {
            charts: &[],
            graphics: graphics(),
            streaming: false,
            images: Some(&store),
            cwd: None,
            spawn_images: false,
            links_expanded: true,
        },
    );
    assert!(rendered.images.is_empty());
    assert!(
        text(&rendered.lines)
            .iter()
            .any(|line| line.contains("<svg")),
        "未就绪时保留代码块"
    );
}

fn joined(source: &str, views: RenderViews<'_>) -> String {
    text(&render(source, 40, views).lines).join("\n")
}

/// 闭合的流程图画成字符图；类图仍是代码块。
#[test]
fn mermaid_fence_draws_flowchart() {
    let source = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n";
    let picture = joined(source, RenderViews::default());
    assert!(
        picture.contains("Start") && picture.contains("End"),
        "{picture}"
    );
    assert!(picture.contains('▼'), "{picture}");
    assert!(!picture.contains("flowchart"), "{picture}");

    let class = joined(
        "```mermaid\nclassDiagram\n  Animal <|-- Duck\n```\n",
        RenderViews::default(),
    );
    assert!(class.contains("classDiagram"), "{class}");
}

/// 流式输出里未闭合的围栏保持源码；收尾后即使缺闭合围栏也尝试画。
#[test]
fn streaming_mermaid_stays_code_until_the_fence_closes() {
    let open = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n";
    let streaming = joined(
        open,
        RenderViews {
            streaming: true,
            ..RenderViews::default()
        },
    );
    assert!(streaming.contains("flowchart"), "{streaming}");

    let closed = "```mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n后面还有字";
    let live = joined(
        closed,
        RenderViews {
            streaming: true,
            ..RenderViews::default()
        },
    );
    assert!(
        live.contains("Start") && !live.contains("flowchart"),
        "{live}"
    );

    let settled = joined(open, RenderViews::default());
    assert!(
        settled.contains("Start") && !settled.contains("flowchart"),
        "{settled}"
    );

    // CRLF 行尾与闭围栏后的尾随空白都算闭合。
    let crlf = "```mermaid\r\nflowchart TD\r\n  A[Start] --> B[End]\r\n```\r\n";
    let live_crlf = joined(
        crlf,
        RenderViews {
            streaming: true,
            ..RenderViews::default()
        },
    );
    assert!(
        live_crlf.contains("Start") && !live_crlf.contains("flowchart"),
        "{live_crlf}"
    );

    // ``` 内容行在 ```` 块里不算闭合。
    let nested = "````mermaid\nflowchart TD\n  A[Start] --> B[End]\n```\n";
    let nested_live = joined(
        nested,
        RenderViews {
            streaming: true,
            ..RenderViews::default()
        },
    );
    assert!(nested_live.contains("flowchart"), "{nested_live}");
}

/// 引用和列表里没有独立的图区，围栏保持代码块。
#[test]
fn mermaid_in_quote_or_list_stays_code() {
    let quoted = joined(
        "> ```mermaid\n> flowchart TD\n>   A[Start] --> B[End]\n> ```\n",
        RenderViews::default(),
    );
    assert!(quoted.contains("flowchart"), "{quoted}");

    let listed = joined(
        "- ```mermaid\n  flowchart TD\n  A[Start] --> B[End]\n  ```\n",
        RenderViews::default(),
    );
    assert!(listed.contains("flowchart"), "{listed}");
}

/// 带 label 的本地路径链接文内已表达引用，不进附录；URL 与 file:// 保留，
/// file:// 与空标签的裸路径按 cwd 相对化（与工具标题同口径）。
#[test]
fn reference_links_filter_local_paths_and_relativize() {
    let rendered = render(
        "改了 [ModelCompat](/repo/src/config/model.rs)、[](/repo/src/lib.rs)，\
         参考 [文档](https://example.com/spec) 与 [旧版](file:///repo/OLD.md)。",
        77,
        RenderViews {
            cwd: Some(std::path::Path::new("/repo")),
            ..RenderViews::default()
        },
    );
    let out = text(&rendered.lines);
    let appendix: Vec<&String> = out
        .iter()
        .skip_while(|line| !line.contains("参考链接："))
        .collect();
    assert_eq!(appendix.len(), 4, "附录头部 + 3 条：{out:?}");
    assert!(appendix[1].contains("src/lib.rs"), "{out:?}");
    assert!(appendix[2].contains("https://example.com/spec"), "{out:?}");
    assert!(appendix[3].contains("OLD.md"), "{out:?}");
    assert!(
        appendix.iter().all(|line| !line.contains("/repo")),
        "本地路径必须相对化、被过滤的 ModelCompat 不得出现：{out:?}"
    );
    assert!(rendered.links_button.is_none(), "{out:?}");
}

/// 附录超过上限时只留前几条，其余收进「▸ 还有 N 条」入口；展开后出「▾ 收起」。
/// 入口的行号要落在渲染出的那一行上（含表格入口插入造成的偏移）。
#[test]
fn reference_links_fold_beyond_limit_with_clickable_entry() {
    let source = format!(
        "{NUMERIC_TABLE}\n\n{links}",
        links = (1..=8)
            .map(|i| format!("[l{i}](https://e{i}.example.com)"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    for expanded in [false, true] {
        let rendered = render(
            &source,
            77,
            RenderViews {
                graphics: graphics(),
                links_expanded: expanded,
                ..RenderViews::default()
            },
        );
        let out = text(&rendered.lines);
        let button = rendered.links_button.expect("8 条链接必有折叠入口");
        let entry = &out[button.row];
        let refs = out.iter().filter(|line| line.contains("https://e")).count();
        if expanded {
            assert_eq!(refs, 8, "{out:?}");
            assert_eq!(entry.trim(), "▾ 收起链接", "{out:?}");
        } else {
            assert_eq!(refs, 5, "{out:?}");
            assert_eq!(entry.trim(), "▸ 还有 3 条链接", "{out:?}");
        }
        // 命中的行/列区间就是入口文字本身：正文缩进之外从 x=2 起算。
        assert_eq!(button.x, 2);
        assert_eq!(button.w as usize, str_width(entry.trim()));
    }
}

/// 渲染行 → 源码行区间：标题、段落、紧凑列表、围栏代码块各归其块，空行无归属。
#[test]
fn source_map_tracks_blocks() {
    let theme = Theme::resolved(ColorLevel::TrueColor);
    let source = "# 标题\n\n段落一行。\n\n- 甲\n- 乙\n- 丙\n\n```rust\nlet x = 1;\n```\n";
    let width = 40;
    let (lines, map) = render_markdown_with_sources(source, width, &theme);
    // 输出与普通渲染完全一致。
    assert_eq!(lines, render_markdown(source, width, &theme));
    assert_eq!(map.len(), lines.len());
    // 宽度 0 的空行一律无归属，非空行必然有归属。
    for (line, entry) in lines.iter().zip(&map) {
        assert_eq!(line_width(line) == 0, entry.is_none(), "{line:?}");
    }
    let ranges: Vec<(usize, usize)> = map.into_iter().flatten().collect();
    // 块区间越过块尾空源行回退：末个列表项只覆盖自身一行。
    for expected in [(1, 1), (3, 3), (5, 5), (6, 6), (7, 7)] {
        assert!(ranges.contains(&expected), "缺 {expected:?}：{ranges:?}");
    }
    // 围栏代码块覆盖整个围栏（含 ``` 行）。
    let code = ranges.iter().copied().find(|range| range.1 >= 10);
    assert_eq!(code, Some((9, 11)), "{ranges:?}");
}

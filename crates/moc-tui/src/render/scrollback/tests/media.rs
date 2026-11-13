use super::*;

/// 回归：图片预筛必须与渲染期真正出图的条件一致。
///
/// 漏判不会导致画错，而是让懒块用折行近似顶替真实的占位行——块高差十几行，
/// 滚动范围与滚动条一起失真。围栏的四种写法都得出图。
#[test]
fn image_prefilter_matches_the_renderer() {
    for fence in ["```svg", "```SVG", "~~~svg", "``` svg", "~~~~svg"] {
        let source = format!("{fence}\n<svg width=\"10\" height=\"10\"/>\n```\n");
        assert!(may_contain_image(&source), "{fence} 应被预筛命中");
    }
    assert!(
        !may_contain_image("```rust\nfn main() {}\n```"),
        "普通代码块不该命中"
    );
    assert!(may_contain_image("前文\n\n![](./a.png)\n"), "markdown 图片");
    assert!(!may_contain_image("纯文本\n没有图也没有代码块"), "纯文本");
}

#[test]
fn mermaid_prefilter_matches_the_renderer() {
    for fence in ["```mermaid", "```Mermaid", "~~~mermaid", "``` mermaid"] {
        let source = format!("{fence}\nflowchart TD\n  A-->B\n```\n");
        assert!(may_contain_mermaid(&source), "{fence} 应被预筛命中");
    }
    assert!(
        !may_contain_mermaid("```rust\nfn main() {}\n```"),
        "普通代码块不该命中"
    );
    assert!(
        !may_contain_mermaid("```svg\n<svg/>\n```"),
        "SVG 围栏走图片预筛"
    );
}

/// 链接折叠入口的命中区：行号贴视口原点，列 = 正文缩进 + 行内偏移（前导两空格）。
#[test]
fn paint_registers_links_entry_hit_on_its_button_row() {
    let mut view = ScrollbackView::new();
    view.cache.push(CachedEntry {
        key: (0, 0),
        layout_epoch: 0,
        layout: TreeIndex::EMPTY,
        height: 3,
        lines: vec![
            Line::from("正文"),
            Line::from("  ▸ 还有 3 条链接"),
            Line::from("尾"),
        ],
        paged: None,
        footer: Vec::new(),
        line_start: 0,
        rendered: true,
        rendered_at: std::time::Instant::now(),
        render_cost: std::time::Duration::ZERO,
        tables: Vec::new(),
        links_button: Some(ButtonSpan {
            row: 1,
            x: 2,
            w: 14,
        }),
        images: Vec::new(),
    });
    let area = Rect::new(2, 5, 80, 10);
    let mut buffer = Buffer::empty(Rect::new(0, 0, 90, 20));
    let (mut hits, mut charts, mut links, mut slots, mut lines) =
        (Vec::new(), Vec::new(), Vec::new(), Vec::new(), Vec::new());
    view.paint(
        &mut buffer,
        area,
        0,
        false,
        Theme::global(),
        None,
        None,
        PaintSinks {
            hits: &mut hits,
            charts: &mut charts,
            links: &mut links,
            slots: &mut slots,
            lines: &mut lines,
        },
    );
    let prose_col = u16::try_from(PROSE_INDENT).expect("正文列应放得进 u16");
    assert_eq!(
        links,
        [crate::app::LinkHit {
            entry: 0,
            row: area.y + 1,
            x: area.x + prose_col + 2,
            w: 14,
        }]
    );
}

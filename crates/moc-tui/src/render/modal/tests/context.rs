use super::*;

/// 上下文窗：分块名称过长或包含宽字符时保持列宽对齐。
#[test]
fn context_modal_handles_wide_names() {
    let (events, _receiver) = unbounded_channel();
    let app = App::new(Theme::global(), events);
    let usage = ContextUsageResult {
        context_window: 100_000,
        total_tokens: 50_000,
        sections: vec![
            ContextUsageSection {
                name: "very_long_custom_section_name_overflow".into(),
                tokens: 30_000,
            },
            ContextUsageSection {
                name: "中文分块名称".into(),
                tokens: 20_000,
            },
        ],
    };

    let lines = super::super::context_lines(&usage, 60, app.theme);
    let texts: Vec<String> = lines.iter().map(line_text).collect();
    // 验证各分块行总宽度一致，列对齐未被顶破
    assert_eq!(line_width(&texts[2]), line_width(&texts[3]));
}

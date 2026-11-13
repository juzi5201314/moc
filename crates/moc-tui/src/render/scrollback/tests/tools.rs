use super::*;

/// 工具正文的简略预算：6 行内容 + 一行省略提示；完整档不截断。
#[test]
fn tool_brief_preview_marks_omission() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let result = (0..200)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.start_tool_block(ToolCallStart {
        id: "call_bash".into(),
        name: "bash".into(),
        parent_id: None,
        arguments: r#"{"command":"run"}"#.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_bash".into(),
        name: "bash".into(),
        parent_id: None,
        arguments: r#"{"command":"run"}"#.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_bash".into(),
            content: result,
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(10),
        diff: None,
    });
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    app.tool_detail = DetailLevel::Brief;
    let brief = render_entry(0, &app.blocks[0], &app, width).lines;
    // 标题（命令放得下，进标题）+ 结果尾 4 + 省略提示 1 + 留白 1
    assert_eq!(brief.len(), 7, "简略档行数固定");
    let brief_text = text_of(&brief);
    assert!(
        brief_text.contains("⋯ 还有 196 行"),
        "略去行数按源行计：{brief_text}"
    );
    assert!(
        brief_text.contains("line-196") && brief_text.contains("line-199"),
        "bash 只留尾部最新四行：{brief_text}"
    );
    assert!(
        !brief_text.contains("line-195") && !brief_text.contains("line-0"),
        "更早的输出被省略：{brief_text}"
    );
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        brief.len(),
        "简略档估算与渲染同源"
    );

    app.tool_detail = DetailLevel::Full;
    let full = render_entry(0, &app.blocks[0], &app, width).lines;
    assert_eq!(
        full.len(),
        200 + 2,
        "完整档 = 标题 + 全部 200 行 + 留白（命令已在标题）"
    );
    assert!(!text_of(&full).contains("还有"), "完整档不出现省略提示");
    assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());

    app.tool_detail = DetailLevel::Hidden;
    let hidden = render_entry(0, &app.blocks[0], &app, width).lines;
    assert_eq!(hidden.len(), 1, "隐藏档只占标题一行");
    assert!(!text_of(&hidden).contains("line-"), "隐藏档无正文");
    assert_eq!(estimate_entry_height_at(&app, 0, width), hidden.len());
}

/// read 始终只显示摘要；失败原因仍可展开查看。
#[test]
fn read_summary_never_expands_but_errors_remain_visible() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let result = (0..200)
        .map(|index| format!("line-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    let read_arguments = r#"{"path":"crates/moc-tui/src/app.rs","offset":1,"limit":200}"#;
    app.start_tool_block(ToolCallStart {
        id: "call_read".into(),
        name: "read".into(),
        parent_id: None,
        arguments: read_arguments.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_read".into(),
        name: "read".into(),
        parent_id: None,
        arguments: read_arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_read".into(),
            content: result.clone(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    assert!(!app.blocks[0].block.foldable());

    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        assert_eq!(
            lines.len(),
            if level == DetailLevel::Hidden { 1 } else { 2 }
        );
        let text = lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>();
        assert!(
            text.contains("crates/moc-tui/src/app.rs:1-200"),
            "摘要带行号范围：{text}"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    // 失败时正文是错误原因，仍可展开。
    app.finish_tool_block(ToolCallFinish {
        id: "call_read".into(),
        name: "read".into(),
        parent_id: None,
        arguments: String::new(),
        status: ToolStatus::Error,
        result: ToolResult {
            tool_call_id: "call_read".into(),
            content: "读取失败：权限不足".into(),
            is_error: true,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    app.tool_detail = DetailLevel::Brief;
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(
        text.contains("读取失败：权限不足"),
        "失败原因必须可见：{text}"
    );
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
}

/// grep 始终只报计数和警告，不显示匹配正文。
#[test]
fn grep_summary_reports_counts_and_warnings() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let arguments = r#"{"pattern":"alpha"}"#;
    let result = "src/a.rs:1:alpha\nsrc/a.rs-2-2026-10-19 12:34:56\nsrc/b.rs:3:alpha beta\n[1 files, 2 matches shown; limit reached]";
    app.start_tool_block(ToolCallStart {
        id: "call_grep".into(),
        name: "grep".into(),
        parent_id: None,
        arguments: arguments.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_grep".into(),
        name: "grep".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_grep".into(),
            content: result.into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        assert!(text_of(&lines).contains("limit reached"));
        assert!(!plain(&lines[0]).contains("有提示"));
        let title = plain(&lines[0]);
        assert!(
            title.contains("2 matches · 2 files"),
            "{level:?} 档计数进标题尾列：{title}"
        );
        assert!(
            !text_of(&lines).contains("alpha beta"),
            "{level:?} 档不显示匹配正文"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    // 零匹配同样给出计数行。
    app.finish_tool_block(ToolCallFinish {
        id: "call_grep".into(),
        name: "grep".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_grep".into(),
            content: "No matches found".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    app.tool_detail = DetailLevel::Brief;
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    assert!(plain(&lines[0]).contains("0 matches · 0 files"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
}

/// find 始终只报路径数和扫描警告。
#[test]
fn find_summary_reports_file_count_and_warnings() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let arguments = r#"{"glob":["**/*.rs"]}"#;
    let result = "src/a.rs\nsrc/b.rs\nsrc/[weird].rs\n[limit 200 reached; narrow the glob]\n[Scan incomplete (time limit, scan-entry limit)]";
    app.start_tool_block(ToolCallStart {
        id: "call_find".into(),
        name: "find".into(),
        parent_id: None,
        arguments: arguments.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_find".into(),
        name: "find".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_find".into(),
            content: result.into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        assert!(text_of(&lines).contains("limit 200 reached"));
        assert!(text_of(&lines).contains("Scan incomplete"));
        let title = plain(&lines[0]);
        assert!(
            title.contains("3 files"),
            "{level:?} 档计数进标题尾列：{title}"
        );
        assert!(
            !text_of(&lines).contains("src/a.rs"),
            "{level:?} 档不显示路径"
        );
        assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
    }

    app.finish_tool_block(ToolCallFinish {
        id: "call_find".into(),
        name: "find".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_find".into(),
            content: "No files found matching pattern".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    app.tool_detail = DetailLevel::Brief;
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    assert!(text_of(&lines).contains("0 files"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
}

/// bash 简略档 = description 标题 + 单行命令 + 末尾 4 行输出。
#[test]
fn bash_brief_uses_description_and_single_line_command() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let arguments =
        r#"{"description":"Search the renderer","command":"rg -n foo\n  crates/moc-tui/src"}"#;
    let result = (0..10)
        .map(|index| format!("out-{index}"))
        .collect::<Vec<_>>()
        .join("\n");
    app.start_tool_block(ToolCallStart {
        id: "call_bash".into(),
        name: "bash".into(),
        parent_id: None,
        arguments: arguments.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_bash".into(),
        name: "bash".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_bash".into(),
            content: result.clone(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    app.tool_detail = DetailLevel::Brief;
    let brief = render_entry(0, &app.blocks[0], &app, width).lines;
    let brief_text = text_of(&brief);
    assert_eq!(brief.len(), 8, "标题 + 命令 + 4 行输出 + 省略 + 留白");
    assert!(
        brief_text.contains("Search the renderer"),
        "标题取 description"
    );
    assert!(
        brief_text.contains("$ rg -n foo crates/moc-tui/src"),
        "命令折成单行：{brief_text}"
    );
    assert!(
        brief_text.contains("out-6") && brief_text.contains("out-9"),
        "只留末尾 4 行：{brief_text}"
    );
    assert!(!brief_text.contains("out-5"), "更早输出被省略");
    assert!(brief_text.contains("⋯ 还有 6 行"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), brief.len());

    app.tool_detail = DetailLevel::Hidden;
    let hidden = render_entry(0, &app.blocks[0], &app, width).lines;
    let hidden_text = text_of(&hidden);
    assert_eq!(hidden.len(), 1, "隐藏档只占标题一行");
    assert!(hidden_text.contains("Search the renderer"));
    assert!(!hidden_text.contains("rg -n"), "隐藏档不显示命令与输出");
    assert_eq!(estimate_entry_height_at(&app, 0, width), hidden.len());

    app.tool_detail = DetailLevel::Full;
    let full = render_entry(0, &app.blocks[0], &app, width).lines;
    let full_text = text_of(&full);
    assert!(
        full_text.contains("crates/moc-tui/src"),
        "完整档显示命令全文"
    );
    assert!(full_text.contains("out-0") && full_text.contains("out-9"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());
}

/// grep 结果计数：上下文行与提示行都不算匹配，旧的一行式与新的分组式都要认。
#[test]
fn grep_counts_match_rows_and_ignore_context() {
    // 一行式（历史会话回放仍然是这个形态）。
    let legacy = "src/a.rs:1:alpha\nsrc/a.rs-2-2026-10-19 12:34:56\nsrc/b.rs:3:beta\n[1 files, 2 matches shown; limit reached]";
    assert_eq!(grep_counts(legacy), (2, 2));
    assert_eq!(grep_counts("No matches found"), (0, 0));
    assert_eq!(grep_counts("src/a.rs:1:only"), (1, 1));
    // 分组式：标题行加缩进行，正文里的 12:34:56 不能被当成命中。
    let grouped = "src/a.rs\n  1:alpha\n  2-2026-10-19 12:34:56\n  3:beta\nsrc/b.rs:4:gamma\n[1 files, 3 matches shown; limit reached]";
    assert_eq!(grep_counts(grouped), (3, 2));
}

/// find 计数跳过 core 追加的提示行，但不误伤 `[` 开头的真实路径。
#[test]
fn find_count_skips_core_notices() {
    let result = "src/a.rs\nsrc/[weird].rs\n[limit 200 reached; narrow the glob]\n[Scan incomplete (time limit)]";
    assert_eq!(find_file_count(result), 2);
    assert_eq!(find_file_count("No files found matching pattern"), 0);
}

/// read 标题行号：分页 footer 的真实交付范围优先，缺结果时按 offset/limit 推算。
#[test]
fn read_range_prefers_delivered_page() {
    use moc_protocol::methods::ToolStatus;

    let theme = Theme::resolved(ColorLevel::TrueColor);
    let block = |result: Option<&str>| ToolCallBlock {
        id: "call_read".into(),
        batch_id: None,
        parent_id: None,
        name: "read".into(),
        arguments: "{}".into(),
        status: Some(ToolStatus::Completed),
        result: result.map(str::to_owned),
        diff: None,
        duration_ms: None,
        running: false,
        progress: None,
        images: Vec::new(),
        job_id: None,
        live_output: None,
    };
    let args: Value = serde_json::json!({"offset": 1, "limit": 300, "path": "a.rs"});
    let range_of = |spans: &[Span]| {
        spans
            .iter()
            .find(|span| span.content.starts_with(':'))
            .map(|span| span.content.to_string())
    };

    // 字节预算截到 210 时按 footer 报，而不是按请求的 limit 报 300。
    let spans = summary_spans(
        &block(Some(
            "[a.rs#0F00]\n1│x\n[Showing lines 1-210 of 909. Use offset=211 to continue.]",
        )),
        &args,
        &theme,
        "",
    );
    assert_eq!(range_of(&spans).as_deref(), Some(":1-210"));
    // 没有结果时按请求范围推算；limit 含端点，末行是 offset+limit-1。
    let spans = summary_spans(&block(None), &args, &theme, "");
    assert_eq!(range_of(&spans).as_deref(), Some(":1-300"));
    let args: Value = serde_json::json!({"offset": 546, "limit": 125, "path": "a.rs"});
    let spans = summary_spans(&block(None), &args, &theme, "");
    assert_eq!(range_of(&spans).as_deref(), Some(":546-670"));
    // 只有 offset 时是开区间。
    let args: Value = serde_json::json!({"offset": 546, "path": "a.rs"});
    let spans = summary_spans(&block(None), &args, &theme, "");
    assert_eq!(range_of(&spans).as_deref(), Some(":546-"));
}

/// ask 转写卡渲染：每问一行、key 对齐、未回答翻译、长答案单行截断、
/// 简略档行数上限 + 省略提示、估算与渲染同源。
#[test]
fn ask_tool_card_styles_answers_and_clamps_rows() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    // 8 问超过简略档 6 行上限；q6 是巨长答案，验证单行截断不撑行。
    let long_answer = "很".repeat(200);
    let questions = (0..8)
        .map(|index| {
            format!(
                r#"{{"id":"q{index}","question":"问题{index}?","header":"标题{index}","options":[]}}"#
            )
        })
        .collect::<Vec<_>>()
        .join(",");
    let arguments = format!(r#"{{"questions":[{questions}]}}"#);
    let result = [
        "User answers:".to_owned(),
        "q0: 墨黑".to_owned(),
        "q1: The user did not answer.".to_owned(),
        "q2: A, B (自定义)".to_owned(),
        "q3: 三".to_owned(),
        "q4: 四".to_owned(),
        "q5: 五".to_owned(),
        format!("q6: {long_answer}"),
        "q7: 七".to_owned(),
    ]
    .join("\n");
    app.start_tool_block(ToolCallStart {
        id: "call_ask".into(),
        name: "ask".into(),
        parent_id: None,
        arguments: arguments.clone(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_ask".into(),
        name: "ask".into(),
        parent_id: None,
        arguments,
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_ask".into(),
            content: result,
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(5),
        diff: None,
    });
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };

    app.tool_detail = DetailLevel::Full;
    let full = render_entry(0, &app.blocks[0], &app, width).lines;
    let full_text = text_of(&full);
    assert!(!full_text.contains("User answers:"), "协议头不进转写");
    assert!(!full_text.contains("The user did not answer"));
    assert!(full_text.contains("标题0"), "key 用 header 不用 id");
    assert!(!full_text.contains("q0"), "裸 id 不出现在卡片里");
    assert!(
        full_text.contains("标题1: 未回答"),
        "未回答标记:{full_text}"
    );
    assert!(full_text.contains("标题2: A, B (自定义)"));
    for line in &full {
        assert!(
            line.width() <= usize::from(width),
            "巨长答案也不撑行:{}",
            line.width()
        );
    }
    let answer_row = full
        .iter()
        .find(|line| text_of(std::slice::from_ref(line)).contains("标题6"))
        .expect("巨长答案行");
    assert!(
        text_of(std::slice::from_ref(answer_row)).contains('…'),
        "巨长答案截断加省略号"
    );
    assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());

    app.tool_detail = DetailLevel::Brief;
    let brief = render_entry(0, &app.blocks[0], &app, width).lines;
    let brief_text = text_of(&brief);
    assert!(brief_text.contains("标题5"), "简略档保留前 6 行答案");
    assert!(
        !brief_text.contains("标题6") && !brief_text.contains("标题7"),
        "超出简略档的答案行被省略"
    );
    assert!(brief_text.contains("⋯ 还有 2 行"), "省略提示:{brief_text}");
    assert_eq!(estimate_entry_height_at(&app, 0, width), brief.len());
}

/// execute：简略档只有标题行（返回值 JSON 不倒出来）；失败时简略档显示错误文本。
#[test]
fn execute_brief_shows_title_only_and_errors() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    run_tool(
        &mut app,
        "rc",
        "execute",
        r#"{"description":"核对","code":"await tools.bash({command:'ls'})"}"#,
        ToolStatus::Completed,
        r#"[{"status":"fulfilled","value":{"content":"ok"}}]"#,
    );
    let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
    assert_eq!(lines.len(), 2, "简略档 = 标题 + 留白");
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect();
    assert!(
        !text.contains("fulfilled") && !text.contains("status"),
        "{text}"
    );
    assert_eq!(estimate_entry_height_at(&app, 0, 80), lines.len());

    run_tool(
        &mut app,
        "rc_bad",
        "execute",
        r#"{"code":"throw 1"}"#,
        ToolStatus::Error,
        "脚本执行失败：boom",
    );
    let lines = render_entry(1, &app.blocks[1], &app, 80).lines;
    let text: String = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains("脚本执行失败：boom"), "失败文本可见：{text}");
    assert_eq!(estimate_entry_height_at(&app, 1, 80), lines.len());
}

/// grep 简略档：计数内联进标题尾列，块只剩标题行和空行。
#[test]
fn grep_brief_inlines_count_into_title_tail() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    run_tool(
        &mut app,
        "g",
        "grep",
        r#"{"pattern":"x"}"#,
        ToolStatus::Completed,
        "src/a.rs:1:x\nsrc/b.rs:2:x",
    );
    let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
    assert_eq!(lines.len(), 2, "标题 + 空行");
    let title = plain(&lines[0]);
    assert!(title.contains("matches ·"), "计数进标题尾列：{title}");
    assert!(plain(&lines[1]).is_empty(), "第二行是空行");
    assert_eq!(estimate_entry_height_at(&app, 0, 80), lines.len());
}

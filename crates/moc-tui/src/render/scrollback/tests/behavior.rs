use super::*;

/// 绘制登记：表格入口的列区间等于按钮宽度；切换后占位区落在正文列上。
#[test]
fn paint_registers_entry_hits_and_bitmap_slots() {
    use crate::render::graphics::{CellPixels, GraphicsProto, GraphicsSupport};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.graphics = Some(GraphicsSupport {
        proto: GraphicsProto::Kitty,
        cell: CellPixels { w: 8, h: 18 },
    });
    app.push_block(Block::Assistant {
        // 两个同量纲、同单位的测量列 → 分组条形图（图例 + 2 类别 × 2 系列 + 轴）
        text: "| 项 | 读 | 写 |\n|---|---|---|\n| a | 10 | 20 |\n| b | 30 | 40 |".into(),
    });

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("测试终端");
    let mut view = ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("首帧绘制成功");

    let hit = *app.chart_hits.first().expect("表格下应有入口命中");
    assert_eq!(hit.entry, 0);
    assert_eq!(hit.table, 0);
    assert_eq!(hit.w as usize, str_width("▸ 可视化图表"));
    assert_eq!(
        hit.x,
        app.scrollback_area.x + super::super::PROSE_INDENT as u16,
        "入口与正文左缘对齐"
    );
    assert!(app.image_slots.is_empty(), "表格形态没有位图槽位");

    app.toggle_table_chart(0, 0);
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("切换后绘制成功");
    let hit = *app.chart_hits.first().expect("展开后仍有入口");
    assert_eq!(hit.w as usize, str_width("▾ 隐藏图表"), "按钮变成隐藏");
    let slot = app.image_slots.first().expect("图表应登记位图槽位").clone();
    let kind = app.blocks[0]
        .table_chart(0)
        .map(|state| state.spec.kind)
        .expect("图表态仍在");
    assert_eq!(slot.cols, 90, "正文可用 91 列，但 720px 上限收窄到 90");
    assert_eq!(
        slot.x,
        app.scrollback_area.x + super::super::PROSE_INDENT as u16
    );
    assert_eq!(
        kind,
        moc_core::chart::ChartKind::Grouped,
        "两个同量纲测量列应画成分组条形图"
    );
    assert_eq!(slot.rows, 6, "图例 + 2 类别 × 2 系列 + 轴");
    assert_eq!(slot.y, hit.row + 2, "图表在入口与一行空白之后");
    assert!(
        slot.y + slot.rows <= app.scrollback_area.y + app.scrollback_area.height,
        "槽位必须整块落在视口内"
    );
}

/// diff 简略档停在 6 行并补省略提示；完整档给出行数。
#[test]
fn brief_diff_stops_at_six_rows() {
    use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let hunk = |start: u32, count: usize| DiffHunk {
        old_start: start,
        new_start: start,
        lines: (0..count)
            .map(|offset| DiffLine {
                kind: DiffLineKind::Insert,
                old_no: None,
                new_no: Some(start + u32::try_from(offset).expect("测试行号不会溢出 u32")),
                text: format!(
                    "新增第 {} 行",
                    start + u32::try_from(offset).expect("测试行号不会溢出 u32")
                ),
            })
            .collect(),
    };
    let diff = ToolDiff {
        files: vec![ToolFileDiff {
            path: "src/lib.rs".into(),
            hunks: vec![hunk(1, 3), hunk(20, 3), hunk(40, 1)],
        }],
        truncated: false,
    };
    let total = crate::render::diff::row_count(&diff);
    assert_eq!(total, 9, "3+3+1 行内容 + 2 条 hunk 分隔");
    app.start_tool_block(ToolCallStart {
        id: "call_edit".into(),
        name: "edit".into(),
        parent_id: None,
        arguments: r#"{"path":"src/lib.rs"}"#.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_edit".into(),
        name: "edit".into(),
        parent_id: None,
        arguments: r#"{"path":"src/lib.rs"}"#.into(),
        status: moc_protocol::methods::ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: "call_edit".into(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(1),
        diff: Some(diff),
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
    // 标题 + 6 行 diff + 省略提示 + 留白
    assert_eq!(brief.len(), 9, "简略档 diff 限 6 行");
    assert_eq!(
        text_of(&brief).matches("还有").count(),
        1,
        "省略提示只出现一次"
    );
    assert!(
        text_of(&brief).contains("⋯ 还有 3 行"),
        "{}",
        text_of(&brief)
    );
    assert_eq!(estimate_entry_height_at(&app, 0, width), brief.len());
    app.tool_detail = DetailLevel::Full;
    let full = render_entry(0, &app.blocks[0], &app, width).lines;
    assert_eq!(full.len(), total + 2, "完整档 = 全部 diff 行 + 留白 + 标题");
    assert!(!text_of(&full).contains("还有"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), full.len());
}

/// 标题计数只统计真实改动；空 diff 不显示 [+0/-0]。
#[test]
fn edit_diff_header_hides_zero_counts() {
    use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let changed = ToolDiff {
        files: vec![ToolFileDiff {
            path: "src/lib.rs".into(),
            hunks: vec![DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![
                    DiffLine {
                        kind: DiffLineKind::Delete,
                        old_no: Some(1),
                        new_no: None,
                        text: "old".into(),
                    },
                    DiffLine {
                        kind: DiffLineKind::Insert,
                        old_no: None,
                        new_no: Some(1),
                        text: "new".into(),
                    },
                ],
            }],
        }],
        truncated: false,
    };
    let empty = ToolDiff {
        files: vec![ToolFileDiff {
            path: "src/empty.rs".into(),
            hunks: vec![DiffHunk {
                old_start: 1,
                new_start: 1,
                lines: vec![DiffLine {
                    kind: DiffLineKind::Equal,
                    old_no: Some(1),
                    new_no: Some(1),
                    text: "same".into(),
                }],
            }],
        }],
        truncated: false,
    };
    for (id, path, diff) in [
        ("call_change", "src/lib.rs", changed),
        ("call_empty", "src/empty.rs", empty),
    ] {
        let arguments = format!(r#"{{"path":"{path}"}}"#);
        app.start_tool_block(ToolCallStart {
            id: id.into(),
            name: "edit".into(),
            parent_id: None,
            arguments: arguments.clone(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: id.into(),
            name: "edit".into(),
            parent_id: None,
            arguments,
            status: moc_protocol::methods::ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: id.into(),
                content: String::new(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: Some(diff),
        });
    }
    let title = |index: usize| {
        render_entry(index, &app.blocks[index], &app, width).lines[0]
            .spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    assert!(title(0).contains("+1 -1"), "真实改动要有计数：{}", title(0));
    assert!(
        !title(1).contains("+") && !title(1).contains("-0"),
        "空 diff 不显示计数：{}",
        title(1)
    );
}

/// 截断 diff：标题带 …，正文提示只出现一次且不再说“改动过多”。
#[test]
fn truncated_diff_marks_partial_and_prints_notice_once() {
    use moc_protocol::methods::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 80u16;
    let file = |path: &str| ToolFileDiff {
        path: path.into(),
        hunks: vec![DiffHunk {
            old_start: 1,
            new_start: 1,
            lines: vec![DiffLine {
                kind: DiffLineKind::Insert,
                old_no: None,
                new_no: Some(1),
                text: "x".into(),
            }],
        }],
    };
    let arguments = r#"{"path":"a.rs"}"#.to_owned();
    app.start_tool_block(ToolCallStart {
        id: "call_truncated".into(),
        name: "edit".into(),
        parent_id: None,
        arguments: arguments.clone(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_truncated".into(),
        name: "edit".into(),
        parent_id: None,
        arguments,
        status: moc_protocol::methods::ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: "call_truncated".into(),
            content: String::new(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(1),
        diff: Some(ToolDiff {
            files: vec![file("a.rs"), file("b.rs")],
            truncated: true,
        }),
    });
    app.tool_detail = DetailLevel::Full;
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let title = lines[0]
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>();
    let body = text_of(&lines[1..]);
    assert!(title.contains("+2 -0 …"), "部分统计要带 …：{title}");
    assert_eq!(
        body.matches("其余改动超出").count(),
        1,
        "截断提示只出现一次：{body}"
    );
    assert!(!body.contains("改动过多"), "不应再出现误导性措辞：{body}");
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
}

/// ask 结果解析：header 优先 id 兜底、逐问「未回答」标记、整单取消与超时哨兵、
/// 单问裸答案、自定义答案换行并入上一条。
#[test]
fn ask_answer_pairs_parses_headers_answers_and_sentinels() {
    use moc_protocol::methods::ToolStatus;

    let args: Value = serde_json::json!({
        "questions": [
            {"id": "theme", "question": "主题?", "header": "主题", "options": []},
            {"id": "features", "question": "特性?", "options": []},
            {"id": "lang", "question": "语言?", "header": "  ", "options": []}
        ]
    });
    let block = |result: &str| ToolCallBlock {
        id: "call_ask".into(),
        batch_id: None,
        parent_id: None,
        name: "ask".into(),
        arguments: "{}".into(),
        status: Some(ToolStatus::Completed),
        result: Some(result.to_owned()),
        diff: None,
        duration_ms: None,
        running: false,
        progress: None,
        images: Vec::new(),
        job_id: None,
        live_output: None,
        preview: None,
    };

    let pairs = ask_answer_pairs(
        &block("User answers:\ntheme: 墨黑\nfeatures: The user did not answer.\nlang: Rust, 日语"),
        &args,
    );
    assert_eq!(
        pairs,
        vec![
            ("主题".to_owned(), "墨黑".to_owned(), false),
            ("features".to_owned(), "未回答".to_owned(), true),
            ("lang".to_owned(), "Rust, 日语".to_owned(), false),
        ],
        "header 优先、空 header 与缺 header 都回落到 id"
    );

    // 自定义答案里的换行并进上一条，不顶出新问。
    let pairs = ask_answer_pairs(
        &block("User answers:\ntheme: 墨黑\n底色加暗\nfeatures: x\nlang: Rust"),
        &args,
    );
    assert_eq!(pairs[0].1, "墨黑 底色加暗", "换行内容并入上一条答案");
    assert_eq!(pairs.len(), 3);

    // 单问：core 只写裸答案，没有 `id: ` 前缀。
    let single_args: Value = serde_json::json!({
        "questions": [{"id": "theme", "question": "主题?", "header": "主题", "options": []}]
    });
    let pairs = ask_answer_pairs(&block("墨黑配米色"), &single_args);
    assert_eq!(
        pairs,
        vec![("主题".to_owned(), "墨黑配米色".to_owned(), false)]
    );

    // 整单哨兵。
    assert_eq!(
        ask_answer_pairs(&block("The user did not answer."), &args),
        vec![(String::new(), "未回答".to_owned(), true)],
        "取消/跳过整单"
    );
    assert_eq!(
        ask_answer_pairs(
            &block("The user did not answer within the time limit; continue without an answer."),
            &args
        ),
        vec![(String::new(), "超时未回答".to_owned(), true)],
        "超时哨兵"
    );
}

/// 防复发守卫：所有块共用一套左缘网格，且任何行都不超出块宽。
///
/// 标记（`>`、`◆`、批次 bullet）落在 [`TITLE_INDENT`]，正文（用户、助手、
/// 工具输出、耗时尾注）落在 [`PROSE_INDENT`]；放不下的标题以 `…` 收尾，
/// bash 命令只出现一次。
#[test]
fn transcript_blocks_share_one_left_grid() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    let width = 60u16;
    app.push_block(Block::User {
        text: "你好".into(),
    });
    app.push_block(Block::Assistant {
        text: "收到。".into(),
    });
    let command = "cargo test -p moc-tui --lib -- render::scrollback::tests --nocapture";
    run_tool(
        &mut app,
        "bash",
        "bash",
        &serde_json::json!({ "command": command }).to_string(),
        ToolStatus::Completed,
        "ok",
    );
    let pattern = "x".repeat(120);
    run_tool(
        &mut app,
        "grep",
        "grep",
        &serde_json::json!({ "pattern": pattern }).to_string(),
        ToolStatus::Completed,
        "src/a.rs:1:x",
    );
    app.register_tool_batch("batch".into(), ["r1".to_owned(), "r2".to_owned()]);
    for id in ["r1", "r2"] {
        run_tool(
            &mut app,
            id,
            "read",
            r#"{"path":"a.rs"}"#,
            ToolStatus::Completed,
            "body",
        );
    }

    for (index, entry) in app.blocks.iter().enumerate() {
        for line in render_entry(index, entry, &app, width).lines {
            assert!(
                line.width() <= usize::from(width),
                "第 {index} 块越界：{:?}",
                plain(&line)
            );
        }
    }
    let block = |index: usize| -> Vec<Line<'static>> {
        render_entry(index, &app.blocks[index], &app, width)
            .lines
            .into_iter()
            .filter(|line| !plain(line).trim().is_empty())
            .collect()
    };

    let user = block(0);
    for line in &user {
        assert_eq!(lead_col(line), TITLE_INDENT, "用户框线与块 bullet 同列");
    }
    assert!(
        plain(&user[1]).contains("❯ 你好"),
        "框内首行带提示符：{:?}",
        plain(&user[1])
    );
    assert_eq!(lead_col(&block(1)[0]), PROSE_INDENT, "助手正文");

    let bash = block(2);
    assert_eq!(lead_col(&bash[0]), TITLE_INDENT, "工具标题");
    for line in &bash[1..] {
        assert_eq!(lead_col(line), PROSE_INDENT, "工具正文：{:?}", plain(line));
    }
    let mentions = bash
        .iter()
        .filter(|line| plain(line).contains("cargo test"))
        .count();
    assert_eq!(mentions, 1, "bash 命令只出现一次");

    let grep = block(3);
    assert!(
        plain(&grep[0]).contains(crate::render::wrap::ELLIPSIS),
        "超长摘要截断加 …（右端是耗时）：{:?}",
        plain(&grep[0])
    );

    let batch = block(4);
    assert_eq!(
        lead_col(&batch[0]),
        TITLE_INDENT,
        "批次 bullet 与块 bullet 同列"
    );
    for line in &batch[1..] {
        assert_eq!(
            lead_col(line),
            TITLE_INDENT,
            "批次成员连接符落在 bullet 正下方：{:?}",
            plain(line)
        );
    }
}

/// 防复发守卫：块与块之间恰好一行留白——不叠成两行，也不贴在一起。
#[test]
fn blocks_are_separated_by_exactly_one_blank_row() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    app.push_block(Block::User {
        text: "第一问".into(),
    });
    app.push_block(Block::Assistant {
        text: "答复。".into(),
    });
    run_tool(
        &mut app,
        "read",
        "read",
        r#"{"path":"a.rs"}"#,
        ToolStatus::Completed,
        "body",
    );
    app.push_block(Block::Assistant {
        text: "读完了。".into(),
    });
    app.push_block(Block::User {
        text: "第二问".into(),
    });
    app.register_tool_batch("batch".into(), ["r1".to_owned(), "r2".to_owned()]);
    for id in ["r1", "r2"] {
        run_tool(
            &mut app,
            id,
            "read",
            r#"{"path":"b.rs"}"#,
            ToolStatus::Completed,
            "body",
        );
    }
    app.push_block(Block::Assistant {
        text: "收尾。".into(),
    });

    let rows: Vec<bool> = transcript(&app, 80)
        .iter()
        .map(|line| plain(line).trim().is_empty())
        .collect();
    assert!(!rows[0], "转写首行不留白");
    for (index, pair) in rows.windows(2).enumerate() {
        assert!(!(pair[0] && pair[1]), "第 {index} 行起出现连续两行留白");
    }
    // 工具块的尾随留白由 gap 决定（树尾才有），其余块恒以一行留白结尾。
    let mut tree = TreeIndex::default();
    tree.sync(&app.blocks);
    let mut batches = BatchIndex::default();
    batches.sync(&app.blocks, &mut Vec::new());
    for index in 0..app.blocks.len() {
        let lines = render_entry(index, &app.blocks[index], &app, 80).lines;
        let Some(last) = lines.last() else {
            continue;
        };
        if matches!(app.blocks[index].block, Block::Tool(_)) {
            assert_eq!(
                plain(last).is_empty(),
                block_layout(&batches, &tree, index).gap,
                "第 {index} 块尾随留白与 gap 规则不一致"
            );
        } else {
            assert!(plain(last).is_empty(), "第 {index} 块缺尾随留白");
        }
    }
}

/// 「内置工具优先」拒绝按建议展示：bullet 是 `→` 而非 `✘`，说明句去掉错误码前缀，
/// 建议行的工具名用工具色。
#[test]
fn prefer_builtin_rejection_renders_as_hint() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Full;
    run_tool(
        &mut app,
        "b1",
        "bash",
        r#"{"command":"sed -i 's/a/b/' src/a.rs"}"#,
        ToolStatus::Error,
        "[E_PREFER_BUILTIN] This bash command only reads, searches, lists, or edits workspace files; use built-in tools instead:\n- edit {\"path\":\"src/a.rs\"} (instead of sed -i: read the file, then edit the anchored lines)",
    );

    let rendered = render_entry(0, &app.blocks[0], &app, 140);
    let title = &rendered.lines[0];
    assert_eq!(title.spans[1].content, "→ ", "bullet 用建议符号：{title:?}");
    assert_eq!(
        title.spans[1].style.fg,
        Some(app.theme.accent_warning),
        "bullet 用警告色"
    );
    let explain = rendered
        .lines
        .iter()
        .map(plain)
        .find(|line| line.contains("use built-in tools instead"))
        .expect("说明句必须出现");
    assert!(
        !explain.contains("[E_PREFER_BUILTIN]"),
        "不展示错误码前缀：{explain:?}"
    );
    let suggestion = rendered
        .lines
        .iter()
        .find(|line| plain(line).contains("{\"path\":\"src/a.rs\"}"))
        .expect("建议行");
    let dash = suggestion
        .spans
        .iter()
        .position(|span| span.content.as_ref() == "- ")
        .expect("建议行带 - 前缀");
    assert_eq!(suggestion.spans[dash + 1].content.as_ref(), "edit");
    assert_eq!(
        suggestion.spans[dash + 1].style.fg,
        Some(app.theme.accent_tool),
        "工具名用工具色"
    );
    assert_eq!(
        estimate_entry_height_at(&app, 0, 140),
        rendered.lines.len(),
        "估高与渲染一致"
    );
}

#[test]
fn skill_descriptions_stay_on_one_line_in_single_and_grouped_calls() {
    use moc_protocol::methods::ToolStatus;
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Hidden;
    run_tool(
        &mut app,
        "single",
        "skill",
        r#"{"name":"review"}"#,
        ToolStatus::Completed,
        "Skill description: 检查代码与复用边界\n正文不应显示",
    );
    let full = render_entry(0, &app.blocks[0], &app, 80).lines;
    assert_eq!(full.len(), 1);
    assert!(plain(&full[0]).contains("review · 检查代码与复用边界 · 1.2s"));
    assert!(
        full[0]
            .spans
            .iter()
            .any(|span| span.content == "review" && span.style.fg == Some(app.theme.text_secondary)),
        "skill 名用 text_secondary：{:?}",
        full[0].spans
    );
    assert!(
        full[0]
            .spans
            .iter()
            .any(|span| span.content == "检查代码与复用边界"
                && span.style.fg == Some(app.theme.gray)),
        "skill 描述用 gray：{:?}",
        full[0].spans
    );
    assert!(
        full[0]
            .spans
            .iter()
            .filter(|span| span.content == crate::render::labels::SEP)
            .all(|span| span.style.fg == Some(app.theme.gray_dim)),
        "分隔符用 gray_dim：{:?}",
        full[0].spans
    );
    for width in [10, 11, 12, 24] {
        let lines = render_entry(0, &app.blocks[0], &app, width).lines;
        assert_eq!(lines.len(), 1);
        assert!(lines[0].width() <= usize::from(width));
        if width == 24 {
            assert!(plain(&lines[0]).contains("..."));
        }
    }
    app.register_tool_batch("skills".into(), ["first".into(), "second".into()]);
    for id in ["first", "second"] {
        run_tool(
            &mut app,
            id,
            "skill",
            r#"{"name":"review"}"#,
            ToolStatus::Completed,
            "Skill description: 检查代码与复用边界\n正文不应显示",
        );
    }
    let lines = render_entry(1, &app.blocks[1], &app, 28).lines;
    assert_eq!(lines.len(), 3);
    for line in &lines[1..] {
        assert!(plain(line).contains("review · 检查"));
        assert!(plain(line).ends_with("..."));
        assert!(line.width() <= 28);
    }
}

#[test]
fn fixed_summaries_always_show_notices_without_raw_body() {
    use moc_protocol::methods::ToolStatus;
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let notice = "[… 2048 bytes omitted; full output: artifact://large; read, grep, and bash accept this URI]";
    for name in [
        "read",
        "grep",
        "find",
        "web_fetch",
        "skill",
        "clm_context",
        "stop_agent",
    ] {
        run_tool(
            &mut app,
            name,
            name,
            r#"{"path":"a.rs"}"#,
            ToolStatus::Completed,
            &format!("secret-body\n{notice}"),
        );
    }
    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        for index in 0..app.blocks.len() {
            app.blocks[index].clear_level_override();
            let rendered = render_entry(index, &app.blocks[index], &app, 100);
            let text = rendered
                .lines
                .iter()
                .map(plain_text)
                .collect::<Vec<_>>()
                .join("\n");
            assert!(!text.contains("secret-body"), "{text}");
            assert!(text.contains(notice), "{text}");
            assert!(!plain(&rendered.lines[0]).contains("有提示"));
            assert_eq!(estimate_entry_height_at(&app, index, 100), rendered.height);
        }
    }
}

#[test]
fn fixed_fetch_summary_preserves_page_metadata_and_failures() {
    use moc_protocol::methods::ToolStatus;
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let result = "Web fetch summary: [{\"url\":\"https://a.test\",\"title\":\"Rust 文档\",\"tokens\":1234,\"truncated\":true},{\"url\":\"https://b.test\",\"error\":\"HTTP 403\"}]\n\nsecret-body";
    run_tool(
        &mut app,
        "fetch",
        "web_fetch",
        r#"{"urls":["https://a.test","https://b.test"]}"#,
        ToolStatus::Completed,
        result,
    );
    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        let rendered = render_entry(0, &app.blocks[0], &app, 80);
        let text = rendered
            .lines
            .iter()
            .map(plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(text.contains("Rust 文档"), "{text}");
        assert!(text.contains("约 1234 tokens"), "{text}");
        assert!(text.contains("内容已截断"), "{text}");
        assert!(text.contains("HTTP 403"), "{text}");
        assert!(!text.contains("secret-body"), "{text}");
        assert_eq!(estimate_entry_height_at(&app, 0, 80), rendered.height);
    }
}

/// display_path：cwd 内相对化、等于 cwd 显示 `.`、前缀必须落在 `/` 边界、
/// `$HOME` 折叠 `~`、相对路径与空 cwd 原样。
#[test]
fn display_path_relativizes_only_on_slash_boundary() {
    use crate::render::display_path;

    let home = std::env::var("HOME").expect("测试环境有 HOME");
    let cases: Vec<(&str, &str, &str)> = vec![
        ("/repo/src/a.rs", "/repo", "src/a.rs"),
        ("/repo", "/repo", "."),
        ("/repo2/a.rs", "/repo", "/repo2/a.rs"),
        ("src/a.rs", "/repo", "src/a.rs"),
        ("/repo/a.rs", "", "/repo/a.rs"),
        ("/repo/sub/a.rs", "/repo/", "sub/a.rs"),
    ];
    for (path, cwd, want) in cases {
        assert_eq!(
            &*display_path(path, cwd),
            want,
            "display_path({path:?}, {cwd:?})"
        );
    }
    let under_home = format!("{home}/x.rs");
    assert_eq!(&*display_path(&under_home, "/elsewhere"), "~/x.rs");
}

/// 粘性折叠头：盖住顶行的可折叠块报出标题行；标题恰好在顶或非折叠块不钉。
#[test]
fn sticky_title_picks_the_spanning_foldable_block() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.thinking_detail = DetailLevel::Full;
    app.push_block(Block::Thinking {
        text: "一段很长的思考\n".repeat(40),
        reasoning_tokens: None,
        duration_ms: None,
    });
    app.push_block(Block::User {
        text: "尾巴".into(),
    });
    let mut view = ScrollbackView::new();
    view.sync(&app, 80);
    // 先滚到顶让思考块完成真实渲染，再取它的实际高度。
    view.ensure_visible(&app, 20, usize::MAX);
    let thinking_end = view.cache[0].height;
    assert!(thinking_end > 4, "思考块要高过几行");

    let (index, title) = view
        .sticky_title(&app, 3)
        .expect("盖住顶行的思考块应报出标题");
    assert_eq!(index, 0);
    let text: String = title
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    assert!(text.contains('◆'), "钉住的是标题行：{text}");

    assert!(
        view.sticky_title(&app, 0).is_none(),
        "标题恰好在顶时自然绘制"
    );
    assert!(
        view.sticky_title(&app, thinking_end).is_none(),
        "盖住顶行的用户消息不可折叠"
    );
    assert!(
        view.sticky_title(&app, thinking_end + 1).is_none(),
        "顶行越过用户消息标题也只是普通滚动"
    );
}

/// 整帧：完整展开的思考块滚进正文后，标题行钉在转写区首行并仍可点击。
#[test]
fn expanded_block_title_pins_to_the_top_row() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.thinking_detail = DetailLevel::Full;
    app.push_block(Block::Thinking {
        text: "正文\n".repeat(200),
        reasoning_tokens: None,
        duration_ms: None,
    });
    // 停在块尾：标题早已滚出视口。
    app.scroll_offset = 0;
    let mut view = ScrollbackView::new();
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).expect("构造测试终端");
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .expect("绘制");

    let top = app.scrollback_area.y;
    let mut row = String::new();
    for x in app.scrollback_area.x..app.scrollback_area.right() {
        row.push_str(
            terminal
                .backend()
                .buffer()
                .cell((x, top))
                .expect("cell")
                .symbol(),
        );
    }
    assert!(row.contains('◆'), "标题应钉在转写区首行：{row}");
    assert!(
        app.mouse_hits
            .iter()
            .any(|&(index, start, _)| index == 0 && start == top),
        "钉住的标题行仍登记折叠命中区"
    );
}

use super::*;

#[test]
fn groups_same_name_tools_from_one_batch() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.register_tool_batch("batch".into(), ["call_a".to_owned(), "call_b".to_owned()]);
    app.start_tool_block(ToolCallStart {
        id: "call_a".into(),
        name: "read".into(),
        parent_id: None,
        arguments: r#"{"path":"a.rs"}"#.into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "call_b".into(),
        name: "read".into(),
        parent_id: None,
        arguments: r#"{"path":"b.rs"}"#.into(),
    });

    let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("read (2)"));
    assert!(text.contains("a.rs"));
    assert!(text.contains("b.rs"));
    for _ in 0..8 {
        app.blocks[0].touch();
    }
    let mut view = ScrollbackView::new();
    view.sync(&app, 80);
    let before = view.cache[0].key;
    app.blocks[1].touch();
    view.sync(&app, 80);
    assert_ne!(
        view.cache[0].key, before,
        "较低版本成员变化也必须使整批失效"
    );
    assert_eq!(render_entry(1, &app.blocks[1], &app, 80).lines.len(), 0);
}

/// 批次按（批次, 工具名）分组：同批的异名调用各自是单成员组，退回单块
/// 渲染——标题行内联摘要，不出现 `(1)` 组头与 `└─` 成员行。
#[test]
fn different_name_calls_in_one_batch_render_as_singles() {
    use moc_protocol::methods::ToolStatus;

    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.register_tool_batch("batch".into(), ["call_a".to_owned(), "call_b".to_owned()]);
    run_tool(
        &mut app,
        "call_a",
        "read",
        r#"{"path":"a.rs"}"#,
        ToolStatus::Completed,
        "body",
    );
    run_tool(
        &mut app,
        "call_b",
        "todo",
        r#"{"op":"init","list":[{"phase":"阶段","items":["任务"]}]}"#,
        ToolStatus::Completed,
        "阶段\n  [x] 任务\n",
    );

    let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
    let head = plain(&lines[0]);
    assert!(head.starts_with(" ◆ read "), "单块标题行内联摘要：{head}");
    assert!(
        head.contains("a.rs") && !head.contains('('),
        "无 (1) 组头：{head}"
    );
    assert!(
        !lines
            .iter()
            .skip(1)
            .map(plain)
            .any(|row| row.contains("─ ")),
        "无成员连线行：{lines:?}"
    );
    assert!(
        !render_entry(1, &app.blocks[1], &app, 80).lines.is_empty(),
        "第二个调用自己产出块"
    );
}

#[test]
fn nested_tool_call_renders_like_a_normal_one() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.start_tool_block(ToolCallStart {
        id: "call_run".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: r#"{"code":"await tools.grep({pattern:'artifact'})"}"#.into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "call_run:0".into(),
        name: "grep".into(),
        parent_id: Some("call_run".into()),
        arguments: r#"{"pattern":"artifact","path":"artifact://ses_1/job.log"}"#.into(),
    });

    let lines = render_entry(1, &app.blocks[1], &app, 80).lines;
    let title = lines.first().map(|line| plain(line)).unwrap_or_default();
    assert!(
        title.starts_with(" ├─ ") || title.starts_with(" └─ "),
        "嵌套调用带连线前缀：{title:?}"
    );
    assert!(!title.contains('◆'), "子调用不画 bullet：{title}");
    assert!(
        title.contains("grep artifact"),
        "嵌套调用的摘要同样取参数里的模式：{title}"
    );
}

/// 回归：批次里失败的成员必须可见，不能被计入完成。
#[test]
fn batch_marks_failed_members_and_counts_only_completed() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    app.register_tool_batch("batch".into(), ["g1".to_owned(), "g2".to_owned()]);
    run_tool(
        &mut app,
        "g1",
        "grep",
        r#"{"pattern":"ok"}"#,
        ToolStatus::Completed,
        "src/a.rs:1:ok",
    );
    run_tool(
        &mut app,
        "g2",
        "grep",
        r#"{"pattern":"bad"}"#,
        ToolStatus::Error,
        "regex parse error",
    );

    let lines: Vec<String> = render_entry(0, &app.blocks[0], &app, 80)
        .lines
        .iter()
        .map(plain)
        .collect();
    let failed = lines
        .iter()
        .find(|line| line.contains("bad"))
        .expect("失败成员行");
    assert!(failed.contains('✘'), "失败成员带 ✘：{failed:?}");
    // 统计进了组头右端尾列，不再有独立的统计行。
    let header = lines.first().expect("组头行");
    assert!(header.contains("1/2"), "只计成功：{header:?}");
    assert!(
        header.contains(crate::render::labels::status::FAILED),
        "组头点明失败：{header:?}"
    );
    assert!(
        lines[1..].iter().all(|line| !line.contains("/2")),
        "组尾不再有统计行：{lines:?}"
    );
    assert_eq!(
        estimate_entry_height_at(&app, 0, 80),
        lines.len(),
        "估高与渲染一致"
    );
}

/// execute 完整档：行号槽脚本 + JSON 树；多行字符串标 `(N lines)`。
#[test]
fn execute_full_renders_script_gutter_and_json_tree() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Full;
    run_tool(
        &mut app,
        "rc",
        "execute",
        r#"{"description":"核对","code":"const a = 1;\nawait tools.bash({command:'ls'})"}"#,
        ToolStatus::Completed,
        r#"[{"status":"fulfilled","value":{"content":" M crates/moc-tui/src/render/scrollback.rs 很长很长很长的第一行\n M b.rs","isError":false,"structuredContent":null}}]"#,
    );
    let lines = render_entry(0, &app.blocks[0], &app, 80).lines;
    let rows: Vec<String> = lines.iter().map(plain).collect();
    for row in &rows {
        eprintln!("{row}");
    }
    assert!(
        rows.iter().any(|row| row.contains("1 │ const a = 1;")),
        "脚本行号槽：{rows:?}"
    );
    assert!(
        rows.iter()
            .any(|row| row.contains("status:") && row.contains("fulfilled")),
        "JSON 树成员行：{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("(2 lines)")),
        "多行字符串标注：{rows:?}"
    );
    assert!(
        rows.iter().any(|row| row.contains("structuredContent:")),
        "null 标量成员：{rows:?}"
    );
    assert_eq!(estimate_entry_height_at(&app, 0, 80), lines.len());

    // 窄宽度下多行字符串先扣 ` (N lines)` 后缀再截首行，后缀不被裁掉。
    let narrow: Vec<String> = render_entry(0, &app.blocks[0], &app, 44)
        .lines
        .iter()
        .map(plain)
        .collect();
    assert!(
        narrow.iter().any(|row| row.contains("(2 lines)")),
        "窄宽度仍显示 (N lines)：{narrow:?}"
    );
}

/// 嵌套连线：父已结束时首个子块 `├─`、末个 `└─`；父还在跑时末个仍 `├─`。
/// 子调用不画 bullet，连接符本身就是标记；空行只出现在树尾。
#[test]
fn nested_children_draw_tree_connectors() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    // 两个子调用用不同名字：同名相邻子调用会合并成组，测不到各自的连接符。
    let spawn_children = |app: &mut App| {
        for (id, name, arguments) in [
            ("c1", "bash", r#"{"command":"echo c1"}"#),
            ("c2", "grep", r#"{"pattern":"x"}"#),
        ] {
            app.start_tool_block(ToolCallStart {
                id: id.into(),
                name: name.into(),
                parent_id: Some("p".into()),
                arguments: arguments.into(),
            });
            app.finish_tool_block(ToolCallFinish {
                id: id.into(),
                name: name.into(),
                parent_id: Some("p".into()),
                arguments: String::new(),
                status: ToolStatus::Completed,
                result: moc_protocol::ToolResult {
                    tool_call_id: id.into(),
                    content: "out".into(),
                    is_error: false,
                    images: Vec::new(),
                },
                duration_ms: Some(1),
                diff: None,
            });
        }
    };
    // 父仍在运行：两个子块都画 ├─（更多子调用还会到来）。
    app.start_tool_block(ToolCallStart {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: r#"{"description":"父"}"#.into(),
    });
    spawn_children(&mut app);
    fn title_of(app: &App, index: usize) -> String {
        render_entry(index, &app.blocks[index], app, 80)
            .lines
            .into_iter()
            .next()
            .map(|line| plain(&line))
            .unwrap_or_default()
    }
    assert!(
        title_of(&app, 1).starts_with(" ├─ "),
        "运行中：{:?}",
        title_of(&app, 1)
    );
    assert!(
        title_of(&app, 2).starts_with(" ├─ "),
        "运行中末块仍 ├─：{:?}",
        title_of(&app, 2)
    );
    assert!(
        !title_of(&app, 1).contains('◆'),
        "子调用不画 bullet：{:?}",
        title_of(&app, 1)
    );

    // 父结束：末个子块翻成 └─ 并留下一行纯空行；中间的子块不留白。
    app.finish_tool_block(ToolCallFinish {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: String::new(),
        status: ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: "p".into(),
            content: "[]".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(9),
        diff: None,
    });
    assert!(
        title_of(&app, 1).starts_with(" ├─ "),
        "首块仍 ├─：{:?}",
        title_of(&app, 1)
    );
    assert!(
        title_of(&app, 2).starts_with(" └─ "),
        "末块 └─：{:?}",
        title_of(&app, 2)
    );
    let child1 = render_entry(1, &app.blocks[1], &app, 80).lines;
    let tail = child1.last().map(plain).unwrap_or_default();
    assert!(!tail.trim().is_empty(), "非末兄弟不留尾随空行：{tail:?}");
    let child2 = render_entry(2, &app.blocks[2], &app, 80).lines;
    assert_eq!(
        plain(child2.last().expect("末块末行")),
        "",
        "树尾空行不带连线前缀"
    );
    for index in 0..app.blocks.len() {
        assert_eq!(
            estimate_entry_height_at(&app, index, 80),
            render_entry(index, &app.blocks[index], &app, 80)
                .lines
                .len(),
            "第 {index} 块估高与渲染一致"
        );
    }
}

/// 嵌套树的留白层级：父块运行中整棵树没有空行（父标题紧接第一个子行）；
/// 父块结束后只有最末后代带一行完全为空的空行。第一个子调用到达会让
/// 父块的 gap 翻转，缓存里的旧版式必须被重渲染掉。
#[test]
fn nested_tree_leaves_blank_only_after_the_last_descendant() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    let width = 80u16;
    let finish = |app: &mut App, id: &str, name: &str, parent: Option<&str>, content: &str| {
        app.finish_tool_block(ToolCallFinish {
            id: id.into(),
            name: name.into(),
            parent_id: parent.map(str::to_owned),
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: id.into(),
                content: content.into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: None,
        });
    };
    app.start_tool_block(ToolCallStart {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: r#"{"description":"父"}"#.into(),
    });
    // 尚无子调用：顶层单块照常带一行尾随留白。
    let mut view = ScrollbackView::new();
    view.sync(&app, width);
    let tail = plain(view.cache[0].lines.last().expect("父块末行"));
    assert!(tail.is_empty(), "无子调用时父块照常留白：{tail:?}");

    // 两个不同名的子调用（不合并），父块仍在运行。
    app.start_tool_block(ToolCallStart {
        id: "c1".into(),
        name: "bash".into(),
        parent_id: Some("p".into()),
        arguments: r#"{"command":"echo hi"}"#.into(),
    });
    finish(&mut app, "c1", "bash", Some("p"), "out");
    app.start_tool_block(ToolCallStart {
        id: "c2".into(),
        name: "grep".into(),
        parent_id: Some("p".into()),
        arguments: r#"{"pattern":"x"}"#.into(),
    });
    finish(&mut app, "c2", "grep", Some("p"), "src/a.rs:1:x");

    // 第一个子调用到达时父块的 gap 已翻转：缓存里是重渲染后的新版式。
    view.sync(&app, width);
    let parent = &view.cache[0].lines;
    assert!(
        parent.iter().all(|line| !plain(line).trim().is_empty()),
        "有子调用的父块不留尾随空行：{parent:?}"
    );
    let parent_last = render_entry(0, &app.blocks[0], &app, width)
        .lines
        .last()
        .map(plain)
        .unwrap_or_default();
    assert!(
        !parent_last.trim().is_empty(),
        "父块渲染结果没有空行：{parent_last:?}"
    );

    // 父块还在跑：整棵树没有任何空行。
    let rows = transcript(&app, width);
    assert!(
        rows.iter().all(|line| !plain(line).trim().is_empty()),
        "父运行中全树无空行：{rows:?}"
    );
    for index in 0..app.blocks.len() {
        assert_eq!(
            estimate_entry_height_at(&app, index, width),
            render_entry(index, &app.blocks[index], &app, width)
                .lines
                .len(),
            "第 {index} 块估高与渲染一致"
        );
    }

    // 父块结束：整棵树只剩一行空行，且完全为空、挂在最末子调用末尾。
    finish(&mut app, "p", "execute", None, "[]");
    let rows = transcript(&app, width);
    let blanks: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, line)| plain(line).is_empty())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        blanks.as_slice(),
        &[rows.len() - 1],
        "整棵树只有末尾一行空行：{blanks:?}"
    );
    let child2 = render_entry(2, &app.blocks[2], &app, width).lines;
    assert_eq!(
        plain(child2.last().expect("末子末行")),
        "",
        "树尾空行不带连线前缀"
    );
    let child1 = render_entry(1, &app.blocks[1], &app, width).lines;
    assert!(
        !plain(child1.last().expect("中间子末行")).trim().is_empty(),
        "中间子调用不留白"
    );
    for index in 0..app.blocks.len() {
        assert_eq!(
            estimate_entry_height_at(&app, index, width),
            render_entry(index, &app.blocks[index], &app, width)
                .lines
                .len(),
            "父结束后第 {index} 块估高与渲染一致"
        );
    }
}

/// 同一父调用下连续同名的子调用合并成组（组头 `read (N)`，成员画在下一层）；
/// 不同名打断成各自单块。父块结束后最末子调用翻成 `└─` 并带空行。
#[test]
fn nested_same_name_children_merge_into_one_group() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    let width = 80u16;
    app.start_tool_block(ToolCallStart {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: r#"{"description":"父"}"#.into(),
    });
    let children = [
        ("r1", "read", r#"{"path":"a.rs"}"#),
        ("r2", "read", r#"{"path":"b.rs"}"#),
        ("g", "grep", r#"{"pattern":"x"}"#),
        ("r3", "read", r#"{"path":"c.rs"}"#),
    ];
    for (id, name, arguments) in children {
        app.start_tool_block(ToolCallStart {
            id: id.into(),
            name: name.into(),
            parent_id: Some("p".into()),
            arguments: arguments.into(),
        });
        app.finish_tool_block(ToolCallFinish {
            id: id.into(),
            name: name.into(),
            parent_id: Some("p".into()),
            arguments: String::new(),
            status: ToolStatus::Completed,
            result: moc_protocol::ToolResult {
                tool_call_id: id.into(),
                content: "out".into(),
                is_error: false,
                images: Vec::new(),
            },
            duration_ms: Some(1),
            diff: None,
        });
    }

    // 前两个 read 合并成组：组头 `├─ read (2)`，成员挂在下一层。
    let group = render_entry(1, &app.blocks[1], &app, width).lines;
    let head = plain(&group[0]);
    assert!(
        head.starts_with(" ├─ ") && head.contains("read (2)"),
        "嵌套组头：{head}"
    );
    assert!(
        plain(&group[1]).starts_with(" │  ├─ ") && plain(&group[1]).contains("a.rs"),
        "组成员（非末位）：{:?}",
        plain(&group[1])
    );
    assert!(
        plain(&group[2]).starts_with(" │  └─ ") && plain(&group[2]).contains("b.rs"),
        "组成员（末位）：{:?}",
        plain(&group[2])
    );
    assert_eq!(
        render_entry(2, &app.blocks[2], &app, width).lines.len(),
        0,
        "非组首成员不产出任何行"
    );
    // 单独的同名单元不合并：grep 与最后一个 read 各自渲染。
    let grep = plain(
        render_entry(3, &app.blocks[3], &app, width)
            .lines
            .first()
            .expect("grep 标题"),
    );
    assert!(
        grep.starts_with(" ├─ ") && !grep.contains('('),
        "grep 单独渲染：{grep}"
    );
    let tail_read = render_entry(4, &app.blocks[4], &app, width).lines;
    assert!(
        plain(&tail_read[0]).starts_with(" ├─ "),
        "父运行中末子仍 ├─：{:?}",
        plain(&tail_read[0])
    );

    // 父块结束：最后一个 read 翻成 └─ 并带一行空行。
    app.finish_tool_block(ToolCallFinish {
        id: "p".into(),
        name: "execute".into(),
        parent_id: None,
        arguments: String::new(),
        status: ToolStatus::Completed,
        result: moc_protocol::ToolResult {
            tool_call_id: "p".into(),
            content: "[]".into(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(9),
        diff: None,
    });
    let tail_read = render_entry(4, &app.blocks[4], &app, width).lines;
    assert!(
        plain(&tail_read[0]).starts_with(" └─ "),
        "父结束末子 └─：{:?}",
        plain(&tail_read[0])
    );
    assert_eq!(plain(tail_read.last().expect("末行")), "", "树尾空行");
    for index in 0..app.blocks.len() {
        assert_eq!(
            estimate_entry_height_at(&app, index, width),
            render_entry(index, &app.blocks[index], &app, width)
                .lines
                .len(),
            "第 {index} 块估高与渲染一致"
        );
    }
}

/// 顶层批次：组头 `◆ name (N)`，成员 `├─` 落在 bullet 正下方；
/// 统计挪进组头右端且只在有失败或运行中时才出现。
#[test]
fn top_level_batch_header_carries_stats_only_when_needed() {
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.tool_detail = DetailLevel::Brief;
    let width = 80u16;
    app.register_tool_batch("b1".into(), ["r1".to_owned(), "r2".to_owned()]);
    for (id, path) in [("r1", "a.rs"), ("r2", "b.rs")] {
        run_tool(
            &mut app,
            id,
            "read",
            &format!(r#"{{"path":"{path}"}}"#),
            ToolStatus::Completed,
            "body",
        );
    }
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let head = plain(&lines[0]);
    assert!(head.starts_with(" ◆ read (2)"), "组头：{head}");
    assert!(
        !head.contains(crate::render::labels::status::DONE),
        "全部完成时组头不带统计：{head}"
    );
    assert!(
        plain(&lines[1]).starts_with(" ├─ "),
        "成员连接符在 bullet 正下方：{:?}",
        plain(&lines[1])
    );
    assert!(plain(&lines[2]).starts_with(" └─ "));
    assert_eq!(plain(lines.last().expect("末行")), "", "组尾空行");
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());

    // 有失败成员：组头右端出现 done/failed 统计。
    app.register_tool_batch("b2".into(), ["g1".to_owned(), "g2".to_owned()]);
    run_tool(
        &mut app,
        "g1",
        "grep",
        r#"{"pattern":"ok"}"#,
        ToolStatus::Completed,
        "src/a.rs:1:ok",
    );
    run_tool(
        &mut app,
        "g2",
        "grep",
        r#"{"pattern":"bad"}"#,
        ToolStatus::Error,
        "regex parse error",
    );
    let lines = render_entry(2, &app.blocks[2], &app, width).lines;
    let head = plain(&lines[0]);
    assert!(
        head.contains(crate::render::labels::status::DONE)
            && head.contains(crate::render::labels::status::FAILED),
        "有失败时组头带统计：{head}"
    );
    assert_eq!(estimate_entry_height_at(&app, 2, width), lines.len());
}

/// 简略档按整份清单保留首尾，不隐藏已完成任务；完整档不截断。
#[test]
fn todo_call_renders_snapshot_tree_per_level() {
    use moc_protocol::methods::ToolStatus;

    for count in [4, 5, 6, 10] {
        for single in [true, false] {
            let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
            let mut app = App::new(crate::theme::Theme::global(), events);
            let mut result = if single {
                "阶段\n".to_owned()
            } else {
                String::new()
            };
            for index in 0..count {
                if !single {
                    result.push_str(&format!("阶段{index}\n"));
                }
                result.push_str(&format!("  [x] 任务{index}\n"));
            }
            run_tool(
                &mut app,
                "todo",
                "todo",
                r#"{"op":"done"}"#,
                ToolStatus::Completed,
                &result,
            );
            for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
                app.tool_detail = level;
                let rendered = render_entry(0, &app.blocks[0], &app, 80);
                let text: String = rendered.lines.iter().map(plain).collect();
                for index in 0..count {
                    let visible = level == DetailLevel::Full
                        || (level == DetailLevel::Brief
                            && (count <= 5 || index < 3 || index >= count - 2));
                    assert_eq!(
                        text.contains(&format!("任务{index}")),
                        visible,
                        "{level:?}: {text}"
                    );
                }
                assert_eq!(
                    text.contains("已省略"),
                    level == DetailLevel::Brief && count > 5
                );
                if level == DetailLevel::Hidden {
                    assert_eq!(rendered.height, 1);
                }
                assert_eq!(estimate_entry_height_at(&app, 0, 80), rendered.height);
            }
        }
    }
}

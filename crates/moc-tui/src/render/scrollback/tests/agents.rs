use super::*;

/// 接收侧 IRC 卡片：标题措辞、默认简略预览、点击后完整／隐藏与估高同源。
#[test]
fn agent_message_card_renders_irc_header_and_preview() {
    use moc_protocol::methods::{AgentMessageMeta, AgentMessageOutcome};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 60u16;
    let body = (0..6)
        .map(|index| format!("第 {index} 行消息"))
        .collect::<Vec<_>>()
        .join("\n");
    app.push_block(Block::AgentMessage {
        meta: AgentMessageMeta {
            from_id: "ses_child".into(),
            from_title: "探查者".into(),
            delivery: AgentMessageOutcome::Injected,
        },
        body,
    });
    let text_of = |lines: &[Line<'static>]| {
        lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .map(|span| span.content.as_ref())
            .collect::<String>()
    };
    let axis = || DetailLevel::Brief;

    // 默认档位（简略）：预览 3 行 + 省略提示。
    let preview = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = text_of(&preview);
    assert!(text.contains("IRC ◀ 探查者"), "标题必须标明来源：{text}");
    assert!(text.contains("injected"), "标题必须标出投递结果：{text}");
    assert!(text.contains("⋯ 还有 3 行"), "简略档只留预览行：{text}");
    assert!(text.contains("第 0 行消息") && text.contains("第 2 行消息"));
    assert!(!text.contains("第 3 行消息"));
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        preview.len(),
        "简略档估算与渲染同源"
    );

    // 点一次到完整：全文，无省略提示。
    app.blocks[0].toggle_level(axis());
    let expanded = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = text_of(&expanded);
    assert!(text.contains("第 5 行消息"));
    assert!(!text.contains("还有"));
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        expanded.len(),
        "完整档估算与渲染同源"
    );

    // 再点一次：回到当前层档位（简略），不留覆盖。
    app.blocks[0].toggle_level(axis());
    assert_eq!(
        app.blocks[0].level_override, None,
        "目标与层档位相同时不留覆盖"
    );
    let back = render_entry(0, &app.blocks[0], &app, width).lines;
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        back.len(),
        "回落简略档后估算与渲染同源"
    );

    // 隐藏档（IRC 卡片的层档位是简略，只能由覆盖或调用方给定）：只剩标题行。
    let hidden = render_entry_single(
        &app.blocks[0],
        &app,
        usize::from(width),
        DetailLevel::Hidden,
        true,
        TreeIndex::EMPTY,
    );
    assert_eq!(hidden.lines.len(), 2, "隐藏档只剩标题行与尾随留白");
}

/// 发送侧 IRC 卡片：广播回执逐行展开，摘要给出总量。
#[test]
fn message_agent_card_lists_receipt_rows() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 60u16;
    let arguments = r#"{"target":"all","message":"请复查一下改动"}"#;
    app.start_tool_block(ToolCallStart {
        id: "call_msg".into(),
        name: "message_agent".into(),
        parent_id: None,
        arguments: arguments.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_msg".into(),
        name: "message_agent".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_msg".into(),
            content: serde_json::json!({
                "receipts": [
                    {"agent_id": "ses_a", "title": "甲", "outcome": "injected"},
                    {"agent_id": "ses_b", "title": "乙", "outcome": "failed", "error": "收件箱已满"}
                ]
            })
            .to_string(),
            is_error: true,
            images: Vec::new(),
        },
        duration_ms: Some(3),
        diff: None,
    });
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("IRC"), "工具卡标题必须显示 IRC：{text}");
    assert!(text.contains("▶ all"));
    assert!(text.contains("broadcast"));
    assert!(text.contains("1 delivered") && text.contains("1 failed"));
    assert!(
        text.contains("请复查一下改动"),
        "正文必须带消息预览：{text}"
    );
    assert!(text.contains("甲 injected"));
    assert!(text.contains("乙 failed：收件箱已满"));
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
}

/// 等待卡片：运行中显示进度行与耗时，结束后改用结果快照。
#[test]
fn wait_agent_renders_progress_rows() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::{AgentStatus, ToolProgressRow, ToolStatus};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 60u16;
    let arguments = r#"{"agent_ids":["ses_a","ses_b"]}"#;
    app.start_tool_block(ToolCallStart {
        id: "call_wait".into(),
        name: "wait_agent".into(),
        parent_id: None,
        arguments: arguments.into(),
    });
    app.set_tool_progress(
        "call_wait",
        vec![
            ToolProgressRow {
                agent_id: "ses_a".into(),
                title: "甲".into(),
                status: AgentStatus::Running,
                elapsed_ms: 3_200,
                activity: Some("cargo test".into()),
            },
            ToolProgressRow {
                agent_id: "ses_b".into(),
                title: "乙".into(),
                status: AgentStatus::Completed,
                elapsed_ms: 0,
                activity: None,
            },
        ],
    );
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("等待 2 个代理"));
    assert!(
        text.contains("▸ 甲 · running · 3.2s · cargo test"),
        "运行行带耗时与活动摘要：{text}"
    );
    assert!(text.contains("◆ 乙 · done"));
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        lines.len(),
        "进度态估算与渲染同源"
    );

    app.finish_tool_block(ToolCallFinish {
        id: "call_wait".into(),
        name: "wait_agent".into(),
        parent_id: None,
        arguments: arguments.into(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_wait".into(),
            content: serde_json::json!({
                "agents": [
                    {"id": "ses_a", "title": "甲", "status": "completed"},
                    {"id": "ses_b", "title": "乙", "status": "running"}
                ],
                "settled": true,
                "messages_pending": false,
                "timed_out": false
            })
            .to_string(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(1_100),
        diff: None,
    });
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("◆ 甲 · done"), "结果行回落到快照：{text}");
    assert!(text.contains("▸ 乙 · running"));
    assert_eq!(
        estimate_entry_height_at(&app, 0, width),
        lines.len(),
        "结果态估算与渲染同源"
    );
}

/// sidekick 卡片：摘要带 mode 标注；落定后状态行带模型、结构化结果一行
/// 摘要与 open_issue 逐条列出，note/report 照常。
#[test]
fn sidekick_card_renders_model_result_and_mode() {
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let width = 60u16;
    app.start_tool_block(ToolCallStart {
        id: "call_sk".into(),
        name: "sidekick".into(),
        parent_id: None,
        arguments: r#"{"mode":"compact","message":"跑一轮验证"}"#.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_sk".into(),
        name: "sidekick".into(),
        parent_id: None,
        arguments: String::new(),
        status: ToolStatus::Completed,
        result: ToolResult {
            tool_call_id: "call_sk".into(),
            content: serde_json::json!({
                "agent_id": "ses_sk",
                "delivery": "compacted",
                "status": "completed",
                "model": "test/gpt-cheap:off",
                "result": {
                    "status": "done",
                    "changes": [{"path": "a.rs", "summary": "改"}],
                    "verification": [
                        {"command": "cargo test", "outcome": "passed"},
                        {"command": "lint", "outcome": "not_run"}
                    ],
                    "open_issues": ["还有一个待定项"]
                },
                "note": "switched model",
                "report": "验证通过"
            })
            .to_string(),
            is_error: false,
            images: Vec::new(),
        },
        duration_ms: Some(900),
        diff: None,
    });
    let lines = render_entry(0, &app.blocks[0], &app, width).lines;
    let text = lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(text.contains("跑一轮验证 · 压缩后"), "摘要带 mode：{text}");
    assert!(
        text.contains("◆ Sidekick · compacted · completed · test/gpt-cheap:off"),
        "{text}"
    );
    assert!(
        text.contains("结果 done · 改动 1 个文件 · 验证 1 通过 1 未跑"),
        "{text}"
    );
    assert!(text.contains("! 还有一个待定项"), "{text}");
    assert!(
        text.contains("switched model") && text.contains("验证通过"),
        "{text}"
    );
    assert_eq!(estimate_entry_height_at(&app, 0, width), lines.len());
}

#[test]
fn fixed_agent_previews_show_tasks_and_reports_not_receipts() {
    use moc_protocol::methods::ToolStatus;
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    let report =
        "**检查边界**\n\n- 核对错误路径\n- 核对取消路径\n- 核对缓存\n- 核对结果\nsecret-tail";
    let tasks = serde_json::json!({"tasks":[{"title":"解析器","task":report,"role":"scout"},{"title":"渲染器","task":"检查布局","role":"scout"}]}).to_string();
    run_tool(
        &mut app,
        "spawn",
        "spawn_agent",
        &tasks,
        ToolStatus::Completed,
        "secret-receipt",
    );
    run_tool(
        &mut app,
        "yield",
        "yield",
        &serde_json::json!({"report":report}).to_string(),
        ToolStatus::Completed,
        "secret-receipt",
    );
    for level in [DetailLevel::Hidden, DetailLevel::Brief, DetailLevel::Full] {
        app.tool_detail = level;
        let text = transcript(&app, 80)
            .iter()
            .map(plain_text)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            text.contains("解析器") && text.contains("渲染器") && text.contains("交付报告"),
            "{text}"
        );
        assert_eq!(
            text.contains("检查边界"),
            level != DetailLevel::Hidden,
            "{text}"
        );
        assert!(!text.contains("**"), "{text}");
        assert!(
            !text.contains("secret-tail") && !text.contains("secret-receipt"),
            "{text}"
        );
        for index in 0..app.blocks.len() {
            assert!(!app.blocks[index].block.foldable());
            assert_eq!(
                estimate_entry_height_at(&app, index, 80),
                render_entry(index, &app.blocks[index], &app, 80).height
            );
        }
    }
}

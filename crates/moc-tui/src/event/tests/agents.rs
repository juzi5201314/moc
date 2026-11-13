use super::*;

/// 接收侧 IRC 卡片与等待进度都必须落到主流。
#[test]
fn agent_message_update_reaches_current_transcript() {
    use crate::transcript::Block;
    use moc_protocol::methods::{
        AgentMessageMeta, AgentMessageOutcome, AgentStatus, ToolProgressRow,
    };

    let mut app = app();
    apply_update(
        &mut app,
        SessionUpdate::AgentMessage {
            meta: AgentMessageMeta {
                from_id: "ses_child".into(),
                from_title: "探查者".into(),
                delivery: AgentMessageOutcome::Injected,
            },
            body: "子代理汇报".into(),
        },
    );
    assert!(
        matches!(
            app.blocks.last().map(|entry| &entry.block),
            Some(Block::AgentMessage { body, .. }) if body == "子代理汇报"
        ),
        "代理消息必须成为转写卡片"
    );

    app.start_tool_block(ToolCallStart {
        id: "call_wait".into(),
        name: "wait_agent".into(),
        parent_id: None,
        arguments: "{}".into(),
    });
    apply_update(
        &mut app,
        SessionUpdate::ToolProgress {
            prompt_id: "prompt_1".into(),
            tool_call_id: "call_wait".into(),
            rows: vec![ToolProgressRow {
                agent_id: "ses_child".into(),
                title: "探查者".into(),
                status: AgentStatus::Running,
                elapsed_ms: 500,
                activity: None,
            }],
        },
    );
    let tool = app
        .tool_entry_mut("call_wait")
        .map(|entry| match &entry.block {
            Block::Tool(tool) => tool,
            _ => panic!("等待块应是工具块"),
        })
        .expect("等待块存在");
    assert_eq!(
        tool.progress.as_ref().map(Vec::len),
        Some(1),
        "进度行必须写进工具块"
    );
}

/// workflow run 面板：滚轮滚正文 viewport，不改 focus；键盘移动焦点重新显露。
#[tokio::test]
async fn workflow_run_wheel_scrolls_body_not_focus() {
    use crossterm::event::MouseEventKind::ScrollDown;
    let (core, connection, mut app) = setup_test_app().await;
    let role = |index: usize| moc_protocol::methods::WorkflowRoleState {
        role_id: format!("role-{index}"),
        name: Some(format!("角色 {index}")),
        fan_out: false,
        status: "running".into(),
        model: None,
        asks_in_flight: 0,
        asks_done: 0,
        spent_tokens: 0,
    };
    let state = crate::workflow::WorkflowRunState {
        run_id: "run-1".into(),
        title: "run".into(),
        status: moc_protocol::methods::WorkflowRunStatus::Running,
        plan: serde_json::json!({"roles": []}),
        phases: vec![],
        roles: (0..8).map(role).collect(),
        commands: vec![],
        reports: vec![],
        artifacts: vec![],
        holes: vec![],
        usage: serde_json::json!({}),
        log: (0..80).map(|i| format!("log {i}")).collect(),
        artifact_versions: Default::default(),
        artifact_view: None,
        focus: 0,
        body_scroll: 0,
        list_scroll: 0,
        scroll: Default::default(),
        pointer: None,
    };
    app.modal = Some(Modal::WorkflowRun {
        state: Box::new(state),
    });
    let _ = draw_frame(&mut app, 100, 24);
    let (_, row) = *app.modal_item_hits.first().expect("role 行有命中");
    for _ in 0..3 {
        handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, row.x, row.y));
    }
    let _ = draw_frame(&mut app, 100, 24);
    let Some(Modal::WorkflowRun { state }) = &app.modal else {
        panic!("run 面板应保持打开")
    };
    assert_eq!(state.focus, 0, "滚轮不能改焦点");
    assert!(state.body_scroll > 0, "正文视口已滚动");
    // 键盘移动焦点重新显露焦点行。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    let _ = draw_frame(&mut app, 100, 24);
    let Some(Modal::WorkflowRun { state }) = &app.modal else {
        panic!("run 面板应保持打开")
    };
    assert_eq!(state.focus, 1);
    assert!(
        app.modal_item_hits.iter().any(|(index, _)| *index == 1),
        "键盘导航后焦点行在视口内"
    );
    connection.shutdown().await;
    core.shutdown().await;
}

/// 父请求失败只撤临时预览，异步到达且已落盘的结果保持原顺序。
#[test]
fn parent_error_preserves_delivered_agent_results() {
    use crate::transcript::Block;
    use moc_protocol::methods::{AgentResultMeta, AgentStatus, AssistantDelta};

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::User {
        text: "父任务".into(),
    });
    app.start_generation();
    for id in ["first", "second"] {
        apply_update(
            &mut app,
            SessionUpdate::AssistantDelta {
                prompt_id: "prompt_parent".into(),
                delta: AssistantDelta::Text {
                    delta: "未提交的父回复".into(),
                },
            },
        );
        apply_update(
            &mut app,
            SessionUpdate::AgentResult {
                entry_id: format!("result_{id}"),
                at_ms: 42,
                meta: AgentResultMeta {
                    agent_id: id.into(),
                    title: id.into(),
                    status: AgentStatus::Completed,
                },
                body: format!("报告 {id}"),
            },
        );
    }
    apply_update(
        &mut app,
        SessionUpdate::Error {
            prompt_id: "prompt_parent".into(),
            message: "父请求失败".into(),
            retryable: true,
        },
    );

    assert!(matches!(&app.blocks[0].block, Block::User { text } if text == "父任务"));
    let results = app
        .blocks
        .iter()
        .filter_map(|entry| match &entry.block {
            Block::AgentResult { meta, body } => {
                Some((meta.agent_id.as_str(), body.as_str(), entry.at_ms))
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(
        results,
        [
            ("first", "报告 first", Some(42)),
            ("second", "报告 second", Some(42))
        ]
    );
    assert!(
        !app.blocks
            .iter()
            .any(|entry| matches!(entry.block, Block::Assistant { .. }))
    );
    assert!(matches!(
        app.blocks.last().expect("保留错误反馈").block,
        Block::Note {
            tone: crate::transcript::NoteTone::Error,
            ..
        }
    ));
}

/// `Ctrl+N`/`/agents` 唤出右侧子代理侧栏而非旧居中模态；名册为空时只提示。
#[tokio::test]
async fn agents_action_summons_sidebar_not_modal() {
    let (core, connection, mut app) = setup_test_app().await;

    // 名册为空：不强开面板，只给提示。
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('n'),
    );
    assert!(app.modal.is_none(), "Ctrl+N 不得打开模态");
    assert_eq!(app.subagent_sidebar, Some(true), "记录唤出意图");
    assert!(!app.subagent_sidebar_visible(), "空名册仍不可见");
    assert_eq!(app.notice.as_deref(), Some("当前没有子代理"));

    // 名册有代理：Ctrl+N 后立即可见，先前手动关掉的也能再唤出。
    app.notice = None;
    app.subagents.insert(
        "agent_1".into(),
        crate::subagent::SubagentView::new(moc_protocol::methods::AgentInfo {
            id: "agent_1".into(),
            parent_id: "ses_parent".into(),
            title: "任务".into(),
            model: "test/model".into(),
            cwd: "/tmp".into(),
            preset: None,
            status: moc_protocol::methods::AgentStatus::Running,
            created_at_ms: 0,
            updated_at_ms: 0,
            last_message: None,
        }),
    );
    app.subagent_sidebar = Some(false);
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('n'),
    );
    assert!(app.modal.is_none());
    assert!(app.subagent_sidebar_visible(), "Ctrl+N 唤出侧栏");

    // /agents 与 Ctrl+N 同路。
    app.subagent_sidebar = Some(false);
    execute_command(&mut app, &connection, "/agents".to_owned());
    assert!(app.modal.is_none());
    assert!(app.subagent_sidebar_visible(), "/agents 唤出侧栏");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 聚焦：点中侧栏卡片（`focus_agent`）把当前会话换成子代理，
/// 空输入的 `Esc` 返回并还原草稿。
#[tokio::test]
async fn focusing_a_subagent_switches_session_and_escape_returns() {
    let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
    let home = app.session.clone().expect("主会话");
    let child = new_child(&connection, "探查者").await;
    let view = agent_view(&child);
    app.subagents.insert(child.id.clone(), view.clone());
    app.prompt.insert_str_raw("未发送的草稿");

    focus_agent(&mut app, &connection, child.id.clone());
    assert!(app.focused.is_some(), "聚焦状态必须置位");
    assert_eq!(
        app.subagent_selected.as_deref(),
        Some(child.id.as_str()),
        "聚焦的子代理应记为选中项"
    );
    assert_eq!(
        app.agents_root_id().as_deref(),
        Some(home.id.as_str()),
        "代理面在被聚焦期间仍挂靠主会话"
    );

    let page = load_page(&connection, &child.id)
        .await
        .expect("加载子代理会话");
    apply_focus_loaded(&mut app, &connection, false, Ok(page));
    assert_eq!(
        app.session.as_ref().map(|session| session.id.as_str()),
        Some(child.id.as_str()),
        "当前会话应换成子代理"
    );

    // 空输入的 Esc 只返回，不打断子代理。
    app.prompt.clear();
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(app.focused.is_none(), "Esc 返回主会话");
    let page = load_page(&connection, &home.id).await.expect("加载主会话");
    apply_focus_loaded(&mut app, &connection, true, Ok(page));
    assert_eq!(
        app.session.as_ref().map(|session| session.id.as_str()),
        Some(home.id.as_str())
    );
    assert_eq!(app.prompt_text(), "未发送的草稿", "返回时还原主会话草稿");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 双击 `←` 返回主会话；单次按键仍是移动光标。
#[tokio::test]
async fn double_left_returns_from_focus() {
    let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
    let child = new_child(&connection, "探查者").await;
    let view = agent_view(&child);
    app.subagents.insert(child.id.clone(), view.clone());
    let home = app.session.clone().expect("主会话");
    app.focused = Some(crate::app::FocusedAgent {
        home,
        home_draft: String::new(),
        home_scroll: 0,
    });

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    assert!(app.focused.is_some(), "单次左移不返回");
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    assert!(app.focused.is_none(), "双击左移返回主会话");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 聚焦期间主会话发来的代理状态事件仍要更新名册，否则侧栏在聚焦时僵住。
#[tokio::test]
async fn agent_status_updates_reach_the_roster_while_focused() {
    let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
    let home = app.session.clone().expect("主会话");
    let child = new_child(&connection, "探查者").await;
    app.subagents.insert(child.id.clone(), agent_view(&child));
    app.session = Some(child.clone());
    app.focused = Some(crate::app::FocusedAgent {
        home: home.clone(),
        home_draft: String::new(),
        home_scroll: 0,
    });

    apply_update_and_refresh(
        &mut app,
        &connection,
        home.id.clone(),
        SessionUpdate::AgentStatusChanged {
            agent_id: child.id.clone(),
            status: moc_protocol::methods::AgentStatus::Completed,
            last_message: Some("done".into()),
        },
    );

    let view = app.subagents.get(&child.id).expect("名册里有该代理");
    assert_eq!(view.status_label(), crate::render::labels::status::DONE);
    assert_eq!(view.info.last_message.as_deref(), Some("done"));
    assert!(!view.running);

    connection.shutdown().await;
    core.shutdown().await;
}

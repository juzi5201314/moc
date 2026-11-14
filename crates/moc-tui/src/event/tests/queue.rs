use super::*;

#[tokio::test]
async fn closing_modal_resumes_queue_after_turn_finishes() {
    use moc_core::{Core, CoreConfig};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"队列已恢复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.toml"), format!(
        "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        upstream.uri(),
    )).unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, mut receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    let id = session.id.clone();
    app.session = Some(session);
    app.prompt_running = true;
    app.inflight_prompts.insert(id.clone());
    app.modal = Some(Modal::Help { scroll: 0 });
    app.prompt.insert_str_raw("弹窗关闭后继续执行");
    crate::queue::submit(&mut app, &connection, true);
    crate::interaction::handle_event(
        &mut app,
        &connection,
        AppEvent::PromptReturned {
            session: id.clone(),
            result: Ok(()),
        },
    );
    assert_eq!(app.queues[&id].len(), 1, "弹窗打开期间应保留队列");
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        loop {
            if let AppEvent::PromptReturned { result, .. } = receiver.recv().await.unwrap() {
                result.unwrap();
                break;
            }
        }
    })
    .await
    .expect("关闭弹窗应派发队列，而不是等待下一次用户输入");
    let loaded = connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: id,
            max_entries: None,
            read_only: false,
        })
        .await
        .unwrap();
    // 运行时提醒也占 transcript 槽位，比对问答文本时先跳过。
    let texts: Vec<String> = loaded
        .entries
        .iter()
        .filter(|entry| entry.reminder.is_none())
        .map(|entry| entry.message.text())
        .collect();
    assert_eq!(texts, vec!["弹窗关闭后继续执行", "队列已恢复"]);
    connection.shutdown().await;
    core.shutdown().await;
}

/// `/ask <问题>`：本次提示带问答模式覆盖排队，不改变会话模式；缺参数只提示。
#[tokio::test]
async fn ask_command_queues_a_read_only_prompt() {
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    let connection = crate::protocol::connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let mut app = App::new(Theme::global(), events);
    let session = connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .unwrap();
    let session_id = session.id.clone();
    app.session = Some(session);

    // 进行中的提示期间只排队：模式覆盖是提示级属性，无法注入正在飞行的提示。
    app.prompt_running = true;
    execute_command(&mut app, &connection, "/ask 有没有遗留的临时文件".into());
    let queued = app.queues.get(&session_id).expect("命令排进待发送队列");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0].mode, Some(moc_protocol::methods::PromptMode::Ask));
    assert_eq!(queued[0].draft.text, "有没有遗留的临时文件");
    assert_eq!(
        app.session.as_ref().map(|session| session.engine),
        Some(SessionEngine::Standard),
        "临时问答不得改写会话引擎"
    );

    execute_command(&mut app, &connection, "/ask".into());
    assert_eq!(app.notice.as_deref(), Some("用法：/ask <问题>"));
    assert_eq!(
        app.queues.get(&session_id).map(|queue| queue.len()),
        Some(1),
        "缺参数不得产生新队列项"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn interrupt_submit_still_cancels_immediately() {
    let (core, connection, mut app) = setup_test_app().await;
    let session = app.session.as_ref().expect("测试会话").id.clone();
    app.preferences.active_submit = crate::config::ActiveSubmit::Interrupt;
    app.prompt_running = true;
    app.prompt.insert_text("中断消息");
    crate::queue::submit(&mut app, &connection, false);
    assert_eq!(app.queues[&session].len(), 1);
    assert!(app.interrupt_pending.contains(&session));
    assert!(app.cancel_requested);
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn steer_returned_keeps_message_pending_until_applied() {
    use crate::transcript::Block;

    let (core, connection, mut app) = setup_test_app().await;
    let session = app.session.as_ref().expect("测试会话").id.clone();
    app.steers
        .entry(session.clone())
        .or_default()
        .push_back(crate::queue::PendingSteer {
            id: 1,
            draft: crate::widgets::prompt::PromptState::from_text("先保留原文件".into()),
            display_text: "先保留原文件".into(),
            echo_text: "先保留原文件".into(),
            core_id: None,
            withdrawal: None,
        });
    crate::interaction::handle_event(
        &mut app,
        &connection,
        AppEvent::SteerReturned {
            session: session.clone(),
            prompt_id: 1,
            result: Ok(moc_protocol::SteerResult {
                accepted: true,
                id: Some(5),
            }),
        },
    );
    assert!(
        app.blocks.is_empty(),
        "accepted 不等于已写入，不能提前写转写"
    );
    assert_eq!(app.steers[&session][0].core_id, Some(5));
    apply_update_and_refresh(
        &mut app,
        &connection,
        session.clone(),
        SessionUpdate::SteeringApplied {
            id: 5,
            text: "先保留原文件".into(),
        },
    );
    assert!(app.steers[&session].is_empty());
    assert!(matches!(
        &app.blocks[0].block,
        Block::User { text } if text == "先保留原文件"
    ));
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn withdrawn_steer_edit_restores_draft() {
    let (core, connection, mut app) = setup_test_app().await;
    let session = app.session.as_ref().expect("测试会话").id.clone();
    app.steers
        .entry(session.clone())
        .or_default()
        .push_back(crate::queue::PendingSteer {
            id: 1,
            draft: crate::widgets::prompt::PromptState::from_text("撤回的草稿".into()),
            display_text: "撤回的草稿".into(),
            echo_text: "撤回的草稿".into(),
            core_id: Some(5),
            withdrawal: Some(crate::queue::WithdrawAction::Edit),
        });
    crate::interaction::handle_event(
        &mut app,
        &connection,
        AppEvent::UnsteerReturned {
            session: session.clone(),
            prompt_id: 1,
            action: crate::queue::WithdrawAction::Edit,
            result: Ok(true),
        },
    );
    assert!(app.steers[&session].is_empty());
    assert_eq!(app.prompt_text(), "撤回的草稿");
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn pending_steer_blocks_dispatch_until_steering_applied() {
    use crate::transcript::Block;

    let (core, connection, mut app) = setup_test_app().await;
    let session = app.session.as_ref().expect("测试会话").id.clone();
    app.steers
        .entry(session.clone())
        .or_default()
        .push_back(crate::queue::PendingSteer {
            id: 1,
            draft: crate::widgets::prompt::PromptState::from_text("补充内容".into()),
            display_text: "补充内容".into(),
            echo_text: "补充内容".into(),
            core_id: Some(5),
            withdrawal: None,
        });
    app.queues
        .entry(session.clone())
        .or_default()
        .push_back(crate::queue::QueuedPrompt {
            id: 2,
            draft: crate::widgets::prompt::PromptState::from_text("排队内容".into()),
            mode: None,
        });
    crate::queue::dispatch(&mut app, &connection);
    assert_eq!(
        app.queues[&session].len(),
        1,
        "pending steering 必须挡住下一条提示"
    );
    apply_update_and_refresh(
        &mut app,
        &connection,
        session.clone(),
        SessionUpdate::SteeringApplied {
            id: 5,
            text: "补充内容".into(),
        },
    );
    let users: Vec<&str> = app
        .blocks
        .iter()
        .filter_map(|entry| match &entry.block {
            Block::User { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        users,
        vec!["补充内容", "排队内容"],
        "用户块必须按注入顺序落地"
    );
    assert!(app.queues[&session].is_empty());
    connection.shutdown().await;
    core.shutdown().await;
}

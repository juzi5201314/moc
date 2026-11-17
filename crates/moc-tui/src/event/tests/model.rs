use super::*;

#[tokio::test]
async fn read_only_model_picker_refuses_changes_before_requesting_core() {
    let (core, connection, mut app) = setup_test_app().await;
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;
    app.read_only = true;
    app.open_model_picker();
    let Some(Modal::Model { picker, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应打开");
    };
    assert_eq!(
        picker.error.as_deref(),
        Some(rust_i18n::t!("tui.status.read_only").as_ref())
    );
    let session = app.session.clone();
    super::super::model::request_session_model(
        &mut app,
        &connection,
        session.as_ref().map(|session| session.id.clone()),
        "missing/model".into(),
    );
    super::super::model::cycle_thinking_level(&mut app, &connection);
    super::super::model::set_fast_mode(&mut app, &connection, true);
    assert!(app.inflight_session_ops.is_empty());
    assert_eq!(
        app.session.as_ref().map(|session| &session.model),
        session.as_ref().map(|session| &session.model)
    );
    let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
        panic!("只读选择器保持打开");
    };
    assert!(!picker.loading);
    assert!(detail.pending.is_none());
    assert_eq!(
        picker.error.as_deref(),
        Some(rust_i18n::t!("tui.status.read_only").as_ref())
    );
    core.shutdown().await;
}

/// 模型选择器回报容量缺口时渲染压缩确认卡片，确认后先切换再由新模型压缩。
#[tokio::test]
async fn model_picker_asks_before_compacting() {
    use moc_core::{Core, CoreConfig};
    use moc_protocol::{PromptParams, SessionPrompt};
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(
                    "data: {\"id\":\"reply\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"答复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                    "text/event-stream",
                )
            } else {
                ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id": "summary",
                    "choices": [{
                        "index": 0,
                        "message": {"role": "assistant", "content": "检查点"},
                        "finish_reason": "stop"
                    }]
                }))
            }
        })
        .mount(&upstream)
        .await;
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.toml"), format!(
        "language='zh'\n[models.roles]\ndefault='test/model:off'\n[providers.test]\napi='openai_chat'\nbase_url='{}'\n[providers.test.models.model]\ncontext_window=100000\n[providers.test.models.tiny]\ncontext_window=32000\nmax_output_tokens=4096\n[compaction]\nmethod='moc'\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
        upstream.uri(),
    )).unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, mut receiver) = unbounded_channel();
    let mut connection = connect_embedded(core.clone(), events.clone())
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
    app.models = connection
        .client
        .call::<ConfigGet>(ConfigGetParams {})
        .await
        .unwrap()
        .models;
    // 历史超过目标窗口，但系统前缀和摘要请求仍能放下。
    connection
        .client
        .call::<SessionPrompt>(PromptParams {
            session_id: id,
            input: vec![InputPart::Text {
                text: "上下文填充".repeat(8000),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
        })
        .await
        .unwrap();

    app.open_model_picker();
    let Some(Modal::Model {
        entries, picker, ..
    }) = app.modal.as_mut()
    else {
        panic!("模型选择器应打开");
    };
    // 候选首位是 Fusion 特殊项：按 selector 找 tiny 的条目下标。
    picker.selected = entries
        .iter()
        .position(|entry| entry.selector == "test/tiny")
        .expect("候选里有 tiny");
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);

    let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(event @ AppEvent::ModelSwitchConfirm { .. }) = receiver.recv().await {
                break event;
            }
        }
    })
    .await
    .expect("选择模型后应回报容量缺口");
    let AppEvent::ModelSwitchConfirm { overflow, .. } = &event else {
        unreachable!("循环只返回确认事件");
    };
    assert_eq!(overflow.context_window, 32000);
    crate::interaction::handle_event(&mut app, &connection, event);
    assert!(
        matches!(app.modal, Some(Modal::SwitchModel { option: 0, .. })),
        "确认卡片应打开并默认停在切换项"
    );

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).unwrap();
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut app, &mut view))
        .unwrap();
    let surface = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    // 宽字符在缓冲区里占两个单元，拼接后每个汉字后面留一个占位空格。
    let compact: String = surface.chars().filter(|ch| !ch.is_whitespace()).collect();
    assert!(compact.contains("超过test/tiny的窗口"), "{surface}");

    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let Some(event @ AppEvent::SessionControlSettled { .. }) = receiver.recv().await {
                break event;
            }
        }
    })
    .await
    .expect("确认后应回报切换结果");
    let AppEvent::SessionControlSettled { result, .. } = &event else {
        unreachable!("循环只返回收尾事件");
    };
    assert_eq!(result.as_ref().ok().cloned().flatten(), None, "{result:?}");
    crate::interaction::handle_event(&mut app, &connection, event);
    assert!(app.modal.is_none(), "切换完成应关闭确认卡片");
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        while app.session.as_ref().unwrap().model != "test/tiny:off" {
            let update = connection.updates.recv().await.unwrap();
            if let crate::protocol::UiUpdate::Session(update) = update {
                apply_update(&mut app, update.update);
            }
        }
    })
    .await
    .expect("模型变化通知应写回会话模型");
    connection.shutdown().await;
    core.shutdown().await;
}

/// 压缩在飞时发出的消息排队等待，切换收尾后打到新模型，而不是撞出 `-32011`。
#[tokio::test]
async fn model_switch_confirmation_queues_messages_until_switch_settles() {
    let mut h = SlowCompactionHarness::new().await;
    h.app.open_model_picker();
    let Some(Modal::Model {
        entries, picker, ..
    }) = h.app.modal.as_mut()
    else {
        panic!("模型选择器应打开");
    };
    // 候选首位是 Fusion 特殊项：按 selector 找 tiny 的条目下标。
    picker.selected = entries
        .iter()
        .position(|entry| entry.selector == "test/tiny")
        .expect("候选里有 tiny");
    handle_modal_key(
        &mut h.app,
        &h.connection,
        KeyModifiers::NONE,
        KeyCode::Enter,
    );

    let event = h
        .next_event(|event| matches!(event, AppEvent::ModelSwitchConfirm { .. }))
        .await;
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert!(matches!(
        h.app.modal,
        Some(Modal::SwitchModel { option: 0, .. })
    ));
    handle_modal_key(
        &mut h.app,
        &h.connection,
        KeyModifiers::NONE,
        KeyCode::Enter,
    );
    assert!(
        h.app.inflight_session_ops.contains(&h.session),
        "切换压缩在飞时应标记会话忙"
    );

    // 关掉卡片再发消息：会话仍忙，消息必须留在队列里。
    handle_modal_key(&mut h.app, &h.connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(h.app.modal.is_none(), "Esc 关闭确认卡片");
    assert!(
        h.submit_message("压缩期间的消息"),
        "压缩在飞时消息应排队而不是报错"
    );
    assert!(
        !h.transcript_contains("压缩期间的消息").await,
        "压缩结束前不应发送该消息"
    );

    h.gate.open();
    let event = h.settle().await;
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert!(
        h.app
            .queues
            .get(&h.session)
            .is_none_or(|queue| queue.is_empty()),
        "收尾后应立即派发队列"
    );
    assert!(h.app.prompt_running && h.app.inflight_prompts.contains(&h.session));
    let event = h
        .next_event(|event| matches!(event, AppEvent::PromptReturned { .. }))
        .await;
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert!(!h.app.prompt_running);
    assert!(h.transcript_contains("压缩期间的消息").await);

    let requests = h.upstream.received_requests().await.unwrap();
    assert!(
        requests.iter().any(|request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            body["model"] == "tiny"
                && body["stream"] == true
                && body.to_string().contains("压缩期间的消息")
        }),
        "排队消息应打到切换后的模型"
    );
    h.shutdown().await;
}

/// `/compact` 在飞时同样让消息排队，压缩收尾后再派发。
#[tokio::test]
async fn manual_compaction_queues_messages_until_settled() {
    let mut h = SlowCompactionHarness::new().await;
    execute_command(&mut h.app, &h.connection, "/compact 保留当前任务".into());
    assert!(
        h.app.inflight_session_ops.contains(&h.session),
        "手动压缩在飞时应标记会话忙"
    );
    assert!(
        h.submit_message("压缩期间的消息"),
        "手动压缩在飞时消息应排队而不是报错"
    );

    h.gate.open();
    let event = h.settle().await;
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert_eq!(h.app.notice.as_deref(), Some("上下文压缩已完成"));
    assert!(h.app.prompt_running && h.app.inflight_prompts.contains(&h.session));
    let event = h
        .next_event(|event| matches!(event, AppEvent::PromptReturned { .. }))
        .await;
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert!(h.transcript_contains("压缩期间的消息").await);
    h.shutdown().await;
}

/// 模型选择器：点详情卡思考等级行展开下拉；会话目标下点等级候选
/// 立即发起切换（选择器保持打开、进入 loading）。
#[tokio::test]
async fn model_picker_mouse_selects_row_then_level_applies_inline() {
    let (core, connection, mut app) = setup_test_app().await;
    let entry = |selector: &str| crate::modal::ModelEntry {
        selector: selector.into(),
        label: selector.into(),
        display_name: String::new(),
        levels: vec!["off".into(), "low".into(), "high".into()],
        context_window: None,
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    };
    app.modal = Some(Modal::Model {
        entries: vec![entry("test/a"), entry("test/b")],
        current: "test/a".into(),
        picker: crate::modal::PickerState::default(),
        level: crate::modal::LevelState::default(),
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    });
    let picker = |app: &App| match app.modal.as_ref() {
        Some(Modal::Model { picker, level, .. }) => {
            (picker.selected, picker.loading, level.manual.clone())
        }
        _ => panic!("模型选择器应保持打开"),
    };

    let _ = draw_frame(&mut app, 140, 30);
    let (_, row_b) = app.modal_item_hits[1];
    // 点未选中的行：只选中，未调整的模型使用默认等级。
    handle_mouse_with_connection(&mut app, &connection, left_click(row_b.x + 3, row_b.y));
    assert_eq!(picker(&app), (1, false, None));

    // 点详情卡 Level 行：展开等级下拉。
    let _ = draw_frame(&mut app, 140, 30);
    let (_, level_row) = *app
        .modal_detail_hits
        .iter()
        .find(|(row, _)| *row == crate::modal::DetailRow::Level)
        .expect("普通模型详情卡有思考等级行");
    handle_mouse_with_connection(
        &mut app,
        &connection,
        left_click(level_row.x + 1, level_row.y),
    );
    let _ = draw_frame(&mut app, 140, 30);
    let (_, low) = app.modal_menu_hits[1];

    // 点下拉候选 "low"：记下等级并立即发起切换（会话目标内联生效）。
    handle_mouse_with_connection(&mut app, &connection, left_click(low.x, low.y));
    assert_eq!(picker(&app), (1, true, Some("low".into())));
    let Some(Modal::Model { detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(detail.pending.as_deref(), Some("test/b:low"));

    connection.shutdown().await;
    core.shutdown().await;
}

/// popover 语义：点三块浮层之外一次即关并把点击交给被点位置；
/// 点触发标签只收起；下拉打开时点列表行先收起下拉再执行行点击。
#[tokio::test]
async fn model_picker_outside_click_closes_once_and_falls_through() {
    let (core, connection, mut app) = setup_test_app().await;
    let entry = |selector: &str| crate::modal::ModelEntry {
        selector: selector.into(),
        label: selector.into(),
        display_name: String::new(),
        levels: vec!["off".into(), "low".into(), "high".into()],
        context_window: None,
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    };
    let open_modal = || Modal::Model {
        entries: vec![entry("test/a"), entry("test/b")],
        current: "test/a".into(),
        picker: crate::modal::PickerState::default(),
        level: crate::modal::LevelState::default(),
        detail: crate::modal::DetailState {
            focus: Some(crate::modal::DetailRow::Level),
            menu: Some(crate::modal::MenuState {
                row: crate::modal::DetailRow::Level,
                selected: 0,
                offset: 0,
                scroll: Default::default(),
                query: String::new(),
            }),
            ..Default::default()
        },
        target: crate::modal::ModelTarget::Session,
    };
    // a) 详情焦点+下拉打开时，点浮层之外一次：选择器直接关闭。
    app.modal = Some(open_modal());
    let _ = draw_frame(&mut app, 140, 30);
    handle_mouse_with_connection(&mut app, &connection, left_click(5, 5));
    assert!(app.modal.is_none(), "点选择器外一次即关");

    // b) 点触发标签：收起且不会立刻重新打开。
    app.modal = Some(open_modal());
    let _ = draw_frame(&mut app, 140, 30);
    let tag = app.model_tag_area;
    assert!(!tag.is_empty(), "测试假定有触发标签");
    handle_mouse_with_connection(&mut app, &connection, left_click(tag.x, tag.y));
    assert!(app.modal.is_none(), "点触发标签只收起不 reopen");

    // c) 下拉打开时点另一个列表行：下拉收起、选中变成该行。
    app.modal = Some(open_modal());
    let _ = draw_frame(&mut app, 140, 30);
    let (_, row_b) = app.modal_item_hits[1];
    handle_mouse_with_connection(&mut app, &connection, left_click(row_b.x + 3, row_b.y));
    let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert!(detail.menu.is_none(), "点击后下拉已收起");
    assert_eq!(picker.selected, 1, "点击落到列表行");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 窄屏下详情卡盖在列表卡上：卡内非控制行的移动与点击不穿透到候选列表，
/// 列表高亮与详情焦点都不变。
#[tokio::test]
async fn model_picker_overlay_detail_blocks_mouse_passthrough() {
    let (core, connection, mut app) = setup_test_app().await;
    let entry = |selector: &str| crate::modal::ModelEntry {
        selector: selector.into(),
        label: selector.into(),
        display_name: String::new(),
        levels: vec!["off".into(), "low".into(), "high".into()],
        context_window: None,
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    };
    app.modal = Some(Modal::Model {
        entries: vec![entry("test/a"), entry("test/b")],
        current: "test/a".into(),
        picker: crate::modal::PickerState::default(),
        level: crate::modal::LevelState::default(),
        detail: crate::modal::DetailState {
            focus: Some(crate::modal::DetailRow::Level),
            ..Default::default()
        },
        target: crate::modal::ModelTarget::Session,
    });
    // 60 列放不下两块浮层：详情卡覆盖在列表卡上（因 focus 而绘制）。
    let _ = draw_frame(&mut app, 60, 24);
    let detail_area = app.modal_detail_area;
    assert!(
        !detail_area.is_empty() && !detail_area.intersection(app.modal_card_area).is_empty(),
        "窄屏下详情卡应覆盖列表卡"
    );
    // 详情卡内容首行是模型名（非控制行）：移上去什么都不发生。
    let point = (detail_area.x + 2, detail_area.y + 1);
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(crossterm::event::MouseEventKind::Moved, point.0, point.1),
    );
    let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(picker.selected, 0, "详情卡上的移动不改列表高亮");
    assert_eq!(
        detail.focus,
        Some(crate::modal::DetailRow::Level),
        "详情卡上的移动不丢焦点"
    );
    handle_mouse_with_connection(&mut app, &connection, left_click(point.0, point.1));
    let Some(Modal::Model { picker, detail, .. }) = app.modal.as_ref() else {
        panic!("模型选择器应保持打开");
    };
    assert_eq!(picker.selected, 0, "详情卡上的点击不选中列表行");
    assert_eq!(
        detail.focus,
        Some(crate::modal::DetailRow::Level),
        "详情卡上的点击不丢焦点"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

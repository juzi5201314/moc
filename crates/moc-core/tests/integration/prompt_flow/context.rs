use super::*;

#[tokio::test]
async fn hot_switch_persists_across_prompts_compaction_and_resume() {
    moc_core::i18n::init("zh");
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let model = body["model"].as_str().expect("请求带模型");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(
                    text_stream(match model {
                        "glm" => "glm response",
                        "ds" => "ds response",
                        other => panic!("unexpected model {other}"),
                    }),
                    "text/event-stream",
                )
            } else {
                assert_eq!(model, "ds", "热切换后的压缩必须使用当前会话模型");
                ResponseTemplate::new(200)
                    .set_body_json(complete(&support::soft_checkpoint("checkpoint")))
            }
        })
        .expect(4)
        .mount(&upstream)
        .await;

    let root = tempfile::tempdir().expect("temporary root");
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create workspace");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "language='zh'\n[models.roles]\ndefault='test/glm:off'\nexplore='test/glm:off'\nplan='test/glm:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [providers.test.models.ds]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [context_files]\nenabled=false\n[compaction]\nmethod='moc'\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    )
    .expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd,
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
        ptc_worker: None,
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::new((*core).clone());
    let connection = server.spawn(core_side);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "model-switch-test".into(),
        version: "0.1.0".into(),
        frontend: "test".into(),
    })
    .connect(frontend_side)
    .await
    .expect("connect client");

    let session = client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .expect("create session");
    assert_eq!(session.model, "test/glm:off");
    client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "first".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("prompt with default model");

    let switched = client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.id.clone(),
            selector: "test/ds:off".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .expect("switch session model");
    assert_eq!(switched.session.model, "test/ds:off");
    client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "second".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("prompt with switched model");
    client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session.id.clone(),
            instructions: None,
        })
        .await
        .expect("compact with switched model");

    client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: session.id.clone(),
        })
        .await
        .expect("unload session");
    let resumed = client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: session.id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("resume session");
    assert_eq!(resumed.session.model, "test/ds:off");
    client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "third".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("prompt after resume");

    let requests = upstream
        .received_requests()
        .await
        .expect("capture requests");
    let models: Vec<_> = requests
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("request body JSON")["model"]
                .as_str()
                .expect("request model")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["glm", "ds", "ds", "ds"]);

    server.shutdown();
    connection.wait().await;
    core.shutdown().await;
}

/// 目标模型装不下当前上下文时，`ask` 只回报缺口：不切换、不压缩、不推送模型变化。
#[tokio::test]
async fn model_switch_asks_before_compacting_when_the_target_window_is_too_small() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(complete(&support::soft_checkpoint("checkpoint")))
            }
        })
        .mount(&upstream)
        .await;

    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/glm:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.ds]\ncontext_window=200000\nmax_output_tokens=4096\n\
         [compaction]\nthreshold={mode=\"tokens\",value=1}\nmethod='moc'\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;

    let asked = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/ds:off".into(),
            overflow: SessionModelOverflowAction::Ask,
        })
        .await
        .expect("检查目标模型容量");
    let overflow = asked.overflow.expect("1 token 触发线必然被越过");
    assert!(overflow.tokens > overflow.threshold);
    assert!(overflow.tokens <= overflow.context_window);
    assert_eq!(overflow.context_window, 200_000);
    assert_eq!(overflow.threshold, 1);
    assert_eq!(overflow.target_tokens, 0);
    assert_eq!(asked.session.model, "test/glm:off", "ask 动作不切换模型");
    assert!(asked.compaction_error.is_none());

    // 目标就是当前模型：不检查容量，按原样回报。
    let same = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/glm:off".into(),
            overflow: SessionModelOverflowAction::Ask,
        })
        .await
        .expect("切换同一模型");
    assert!(same.overflow.is_none());
    assert_eq!(same.session.model, "test/glm:off");

    let updates = harness.updates.lock().clone();
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. })),
        "ask 动作没有切换模型：{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompacted { .. })),
        "ask 动作不做压缩：{updates:?}"
    );
    harness.close().await;
}

/// `compact` 动作先切换模型，再由新模型压缩上下文。
#[tokio::test]
async fn model_switch_compact_switches_first_and_compacts_with_the_target_model() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let model = body["model"].as_str().expect("请求带模型");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                assert_eq!(model, "small", "压缩必须由切换后的模型执行");
                let output_tokens = body["max_tokens"].as_u64().expect("摘要输出上限");
                assert!(
                    moc_core::compact::estimate_tokens(&body.to_string()) + output_tokens <= 32000,
                    "摘要请求连同输出预算必须放得下目标窗口"
                );
                ResponseTemplate::new(200)
                    .set_body_json(complete(&support::soft_checkpoint("checkpoint")))
            }
        })
        .mount(&upstream)
        .await;

    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/glm:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.small]\ncontext_window=32000\nmax_output_tokens=4096\n\
         [compaction]\nauto=false\nmethod='moc'\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, &"上下文填充".repeat(8000)).await;

    let switched = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/small:off".into(),
            overflow: SessionModelOverflowAction::Compact,
        })
        .await
        .expect("切换并压缩");
    assert!(
        switched.overflow.is_some(),
        "32000 窗口装不下四万 token 历史"
    );
    assert_eq!(switched.session.model, "test/small:off");
    assert!(
        switched.compaction_error.is_none(),
        "{:?}",
        switched.compaction_error
    );

    let updates = harness.updates.lock().clone();
    assert!(
        updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ModelChanged { model } if model == "test/small:off"
        )),
        "{updates:?}"
    );
    assert!(
        updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ContextCompacted { method, .. } if method == "soft"
        )),
        "{updates:?}"
    );

    harness.prompt(&session, "继续").await;
    assert_eq!(harness.load(&session).await.session.model, "test/small:off");
    harness.close().await;
}

/// 系统前缀都放不下时不发送摘要请求；已确认的模型切换保留，历史不得丢失。
#[tokio::test]
async fn model_switch_compact_rejects_impossible_windows_without_sending_summary() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
            assert_eq!(body["stream"], true, "不应向上游发送必然超窗的摘要请求");
            ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
        })
        .expect(1)
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/glm:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.tiny]\ncontext_window=1000\nmax_output_tokens=4096\n\
         [providers.test.models.small]\ncontext_window=2000\nmax_output_tokens=4096\n\
         [compaction]\nauto=false\nmethod='moc'\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "保留原始用户要求").await;
    let before = harness.load(&session).await.entries;
    for selector in ["test/tiny:off", "test/small:off"] {
        let switched = harness
            .client
            .call::<SessionSetModel>(SessionSetModelParams {
                session_id: session.clone(),
                selector: selector.into(),
                overflow: SessionModelOverflowAction::Compact,
            })
            .await
            .unwrap();
        assert_eq!(switched.session.model, selector);
        assert!(switched.overflow.is_some());
        let error = switched.compaction_error.expect("窗口不足应明确报错");
        assert!(error.contains("input budget"), "{error}");
        assert_eq!(harness.load(&session).await.entries, before);
    }
    let updates = harness.updates.lock().clone();
    assert_eq!(
        updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::ContextCompactionFailed { .. }))
            .count(),
        2
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompacted { .. }))
    );
    harness.close().await;
}

/// 压缩失败时保留已经完成的切换，只回报失败原因。
#[tokio::test]
async fn model_switch_keeps_the_switch_when_compaction_fails() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                ResponseTemplate::new(401)
                    .set_body_json(serde_json::json!({"error":{"message":"summary denied"}}))
            }
        })
        .mount(&upstream)
        .await;

    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/glm:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.small]\ncontext_window=32000\nmax_output_tokens=4096\n\
         [compaction]\nmethod='moc'\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, &"上下文填充".repeat(8000)).await;

    let switched = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/small:off".into(),
            overflow: SessionModelOverflowAction::Compact,
        })
        .await
        .expect("切换并压缩");
    assert_eq!(
        switched.session.model, "test/small:off",
        "压缩失败不回退切换"
    );
    assert!(switched.overflow.is_some());
    let error = switched.compaction_error.expect("无可剪结果且摘要失败");
    assert!(
        error.contains("summary denied"),
        "应抵达上游拒绝路径：{error}"
    );

    let updates = harness.updates.lock().clone();
    assert!(
        updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ModelChanged { model } if model == "test/small:off"
        )),
        "{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompacted { .. })),
        "{updates:?}"
    );
    harness.close().await;
}

#[tokio::test]
async fn plan_approval_keeps_current_model_for_compaction_and_execution() {
    use serde_json::json;
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("请求 JSON");
            if body["stream"] != true {
                assert_eq!(body["model"], "executor", "压缩必须使用执行模型");
                ResponseTemplate::new(200)
                    .set_body_json(complete(&support::soft_checkpoint("保留计划和接口决策")))
            } else if body.to_string().contains("<active-plan") {
                assert_eq!(body["model"], "executor");
                ResponseTemplate::new(200)
                    .set_body_raw(text_stream("执行完成"), "text/event-stream")
            } else {
                assert_eq!(body["model"], "planner");
                ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![
                        (
                            "save",
                            "plan",
                            json!({"title":"执行方案", "markdown":"# 方案\n保留接口"}),
                        ),
                        ("exit", "exit_plan", json!({})),
                    ]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let h = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/planner:off",
        "test/gpt-test:off",
        "[providers.test.models.planner]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.executor]\ncontext_window=200000\nmax_output_tokens=4096\n\
         [compaction]\nmethod='moc'\nauto=false\n",
    )
    .await;
    for action in [PlanAction::Execute, PlanAction::ExecuteCompacted] {
        let id = h.session_plan(SessionEngine::Standard).await;
        h.prompt(&id, "设计方案").await;
        let plan = h.load(&id).await.plans.pop().expect("已写计划");
        let invalid = h
            .client
            .call::<moc_protocol::SessionSetModel>(moc_protocol::SessionSetModelParams {
                session_id: id.clone(),
                selector: "test/missing:off".into(),
                overflow: moc_protocol::SessionModelOverflowAction::Switch,
            })
            .await;
        assert!(invalid.is_err());
        let unchanged = h.load(&id).await;
        assert_eq!(unchanged.plans[0].status, PlanStatus::Submitted);
        assert!(unchanged.session.plan_active);
        assert_eq!(unchanged.session.model, "test/planner:off");
        h.client
            .call::<moc_protocol::SessionSetModel>(moc_protocol::SessionSetModelParams {
                session_id: id.clone(),
                selector: "test/executor:off".into(),
                overflow: moc_protocol::SessionModelOverflowAction::Switch,
            })
            .await
            .unwrap();
        h.updates.lock().clear();
        let approved = h
            .client
            .call::<SessionPlanApprove>(SessionPlanApproveParams {
                session_id: id.clone(),
                plan_id: plan.plan_id,
                action,
                execution_engine: None,
                execution_model: None,
            })
            .await
            .expect("指定执行模型审批");
        let loaded = h.load(&id).await;
        assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
        assert_eq!(loaded.session.engine, SessionEngine::Standard);
        assert_eq!(loaded.session.model, "test/executor:off");
        let changes: Vec<_> = h
            .updates
            .lock()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::PlanModeChanged { active } => Some(format!("plan:{active}")),
                SessionUpdate::ModelChanged { model } => Some(format!("model:{model}")),
                _ => None,
            })
            .collect();
        assert_eq!(changes, ["plan:false"]);
        h.client
            .call::<SessionUnload>(SessionUnloadParams {
                session_id: id.clone(),
            })
            .await
            .expect("卸载会话");
        assert_eq!(h.load(&id).await.session.model, "test/executor:off");
        assert_eq!(
            h.prompt(&id, approved.auto_prompt.as_deref().expect("自动执行提示"))
                .await
                .message
                .text(),
            "执行完成"
        );
    }
    let requests = upstream.received_requests().await.expect("请求记录");
    let summaries = requests
        .iter()
        .filter(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body).expect("请求 JSON")["stream"]
                != true
        })
        .count();
    assert_eq!(summaries, 1);
    h.close().await;
}

/// 运行时提醒按条目差分注入：一次提示内的后续请求不再重复注入没变的条目。
#[tokio::test]
async fn runtime_reminders_are_injected_once_per_change_not_per_request() {
    let upstream = MockServer::builder().start().await;
    // 第一轮让模型调一次工具，逼出同一次提示里的第二个请求。
    let responses = [
        support::tool_stream(vec![(
            "call_read",
            "read",
            serde_json::json!({"path": "a.txt"}),
        )]),
        text_stream("完成"),
    ];
    let index = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = Arc::clone(&index);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let at = counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(responses[at].clone(), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    std::fs::write(
        h.root.path().join("work").join("a.txt"),
        "内容\n".repeat(400),
    )
    .expect("create read target");
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "开始").await;

    let requests = upstream.received_requests().await.expect("请求记录");
    assert_eq!(requests.len(), 2, "一次工具调用换来两个模型请求");
    let reminder_texts = |request: &wiremock::Request| -> Vec<String> {
        let body: serde_json::Value = serde_json::from_slice(&request.body).expect("请求 JSON");
        body["messages"]
            .as_array()
            .expect("消息数组")
            .iter()
            .filter_map(|message| message["content"].as_str())
            .filter(|text| text.starts_with("<system-reminder>"))
            .map(str::to_owned)
            .collect()
    };
    assert_eq!(
        reminder_texts(&requests[0]).len(),
        1,
        "首个请求注入一条合并的提醒消息"
    );
    assert_eq!(
        reminder_texts(&requests[1]).len(),
        1,
        "提醒已落库，第二个请求只应看到历史里的那一份，不得重复注入：{:?}",
        reminder_texts(&requests[1])
    );

    // 提醒进入历史后不再变化，所以两个请求里的提醒正文相同。
    assert_eq!(reminder_texts(&requests[0]), reminder_texts(&requests[1]));

    // 会话重载后提醒仍然只有一份，且不被算作用户消息。
    let loaded = h.load(&id).await;
    let reminders: Vec<_> = loaded
        .entries
        .iter()
        .filter(|entry| entry.reminder.is_some())
        .collect();
    assert_eq!(reminders.len(), 1, "一次示意：提醒不该按请求数量累积");
    let users: Vec<_> = loaded
        .entries
        .iter()
        .filter(|entry| entry.message.role == moc_protocol::Role::User && entry.reminder.is_none())
        .collect();
    assert_eq!(users.len(), 1, "提醒不得被算成用户消息");
    h.close().await;
}

#[tokio::test]
async fn web_widget_prompt_and_doc_follow_frontend_and_web_state() {
    use moc_protocol::{
        ResourceList, ResourceListParams, ResourceRead, ResourceReadParams, WebState, WebStateGet,
        WebStateGetParams, WebStateSave, WebStateSaveParams,
    };

    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("ok"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;

    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let connect_as = |frontend: &str| {
        let server = &h.server;
        let frontend = frontend.to_owned();
        async move {
            let (server_side, client_side) = transport::duplex(65536);
            let conn = server.spawn(server_side);
            let (client, _) = ClientBuilder::new(ClientInfo {
                name: "widget-test".into(),
                version: "1".into(),
                frontend,
            })
            .connect(client_side)
            .await
            .expect("connect");
            (client, conn)
        }
    };

    // 1. 纯 TUI 会话：系统提示与提醒不含 widget，tool://widget 不可列也不可读。
    let (tui_client, tui_conn) = connect_as("tui").await;
    let tui_session = tui_client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .expect("new tui session");
    let tui_tools = tui_client
        .call::<ResourceList>(ResourceListParams {
            session_id: tui_session.id.clone(),
            uri: "tool://".into(),
        })
        .await
        .expect("list tool:// in tui");
    assert!(!tui_tools.entries.iter().any(|entry| entry.uri == "tool://widget"));
    assert!(
        tui_client
            .call::<ResourceRead>(ResourceReadParams {
                session_id: tui_session.id.clone(),
                uri: "tool://widget".into(),
                max_bytes: None,
            })
            .await
            .is_err()
    );
    tui_client
        .call::<SessionPrompt>(PromptParams {
            session_id: tui_session.id.clone(),
            input: vec![InputPart::Text { text: "hi".into() }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("tui prompt");

    // 2. WebUI 默认开启 widgets：极短引导落在 system prompt（不进 runtime reminder），详细契约由 tool://widget 按需读取。
    let (web_client, web_conn) = connect_as("webui").await;
    let web_session = web_client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .expect("new webui session");
    let web_tools = web_client
        .call::<ResourceList>(ResourceListParams {
            session_id: web_session.id.clone(),
            uri: "tool://".into(),
        })
        .await
        .expect("list tool:// in webui");
    assert!(web_tools.entries.iter().any(|entry| entry.uri == "tool://widget"));
    let widget_doc = web_client
        .call::<ResourceRead>(ResourceReadParams {
            session_id: web_session.id.clone(),
            uri: "tool://widget".into(),
            max_bytes: None,
        })
        .await
        .expect("read tool://widget in webui");
    let widget_doc_text = widget_doc.content.as_deref().unwrap_or_default();
    assert!(widget_doc_text.contains("moc-theme-change"));
    assert!(widget_doc_text.contains("--bg"));
    web_client
        .call::<SessionPrompt>(PromptParams {
            session_id: web_session.id.clone(),
            input: vec![InputPart::Text { text: "hi".into() }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("webui prompt (widgets enabled)");

    // 3. 关闭 web.toml 的 widgets 后：系统提示与 tool://widget 均消失。
    let state_doc = web_client
        .call::<WebStateGet>(WebStateGetParams {})
        .await
        .expect("read web state");
    assert!(state_doc.state.widgets);
    let saved = web_client
        .call::<WebStateSave>(WebStateSaveParams {
            revision: state_doc.revision,
            state: WebState {
                widgets: false,
                ..state_doc.state
            },
        })
        .await
        .expect("disable web widgets");
    assert!(!saved.state.widgets);
    let web_tools_off = web_client
        .call::<ResourceList>(ResourceListParams {
            session_id: web_session.id.clone(),
            uri: "tool://".into(),
        })
        .await
        .expect("list tool:// when disabled");
    assert!(!web_tools_off.entries.iter().any(|entry| entry.uri == "tool://widget"));
    assert!(
        web_client
            .call::<ResourceRead>(ResourceReadParams {
                session_id: web_session.id.clone(),
                uri: "tool://widget".into(),
                max_bytes: None,
            })
            .await
            .is_err()
    );
    web_client
        .call::<SessionPrompt>(PromptParams {
            session_id: web_session.id.clone(),
            input: vec![InputPart::Text {
                text: "disabled".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("webui prompt (widgets disabled)");

    // 重新开启 widgets 后卸载，换 TUI 续聊同一会话：TUI 下不包含 widget 提示词。
    web_client
        .call::<WebStateSave>(WebStateSaveParams {
            revision: saved.revision,
            state: WebState {
                widgets: true,
                ..saved.state
            },
        })
        .await
        .expect("re-enable web widgets");
    web_client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: web_session.id.clone(),
        })
        .await
        .expect("unload webui session");
    web_conn.close();

    tui_client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: web_session.id.clone(),
            max_entries: None,
            omit_auxiliary: false,
            read_only: false,
        })
        .await
        .expect("load web session in tui");
    tui_client
        .call::<SessionPrompt>(PromptParams {
            session_id: web_session.id.clone(),
            input: vec![InputPart::Text {
                text: "follow up".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("tui follow up");
    tui_conn.close();

    let requests = upstream.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 4);
    let parse_messages = |req: &wiremock::Request| -> (String, String) {
        let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
        let messages = body["messages"].as_array().expect("messages array");
        let system = messages
            .iter()
            .filter(|m| m["role"] == "system")
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        let non_system = messages
            .iter()
            .filter(|m| m["role"] != "system")
            .filter_map(|m| m["content"].as_str())
            .collect::<Vec<_>>()
            .join("\n");
        (system, non_system)
    };

    // 请求 0：纯 TUI
    let (sys0, rest0) = parse_messages(&requests[0]);
    assert!(!sys0.contains("tool://widget"));
    assert!(!rest0.contains("tool://widget"));

    // 请求 1：WebUI（widgets 开启）— 简短声明在 system，详细契约不常驻，且不在 runtime reminder。
    let (sys1, rest1) = parse_messages(&requests[1]);
    assert!(sys1.contains("WebUI renders fenced `widget`"));
    assert!(sys1.contains("tool://widget"));
    assert!(!sys1.contains("moc-theme-change"));
    assert!(!rest1.contains("WebUI renders fenced"));
    assert!(!rest1.contains("tool://widget"));

    // 请求 2：WebUI（widgets 关闭）— system 与非 system 都不含 widget 提示词。
    let (sys2, rest2) = parse_messages(&requests[2]);
    assert!(!sys2.contains("WebUI renders fenced"));
    assert!(!sys2.contains("tool://widget"));
    assert!(!rest2.contains("WebUI renders fenced"));

    // 请求 3：切到 TUI 续聊 — system 不含 widget 提示词。
    let (sys3, rest3) = parse_messages(&requests[3]);
    assert!(!sys3.contains("WebUI renders fenced"));
    assert!(!sys3.contains("tool://widget"));
    assert!(!rest3.contains("Interactive web widgets are unavailable"));

    h.close().await;
}

/// MCP 不再推送一次性连接通知，改为基于状态指纹的基线/变更提醒：
/// 首轮全正常静默记基线，无变化重连不重复注入，空闲变更在下一次消息前注入，
/// 同一次提示多轮途中变更在下一模型请求当场注入。
#[tokio::test]
async fn mcp_state_reminder_injects_only_on_change_not_steady_or_reconnect() {
    use moc_protocol::{
        AskAnswer, AskResult, McpList, McpListParams, McpReconnect, McpReconnectParams,
        McpServerPhase, SessionRequestAsk,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mcp_phase = Arc::new(AtomicUsize::new(0));
    let mcp_phase_srv = Arc::clone(&mcp_phase);
    let mcp_server = MockServer::builder().start().await;
    Mock::given(method("GET"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(405))
        .mount(&mcp_server)
        .await;
    Mock::given(method("DELETE"))
        .and(path("/mcp"))
        .respond_with(ResponseTemplate::new(204))
        .mount(&mcp_server)
        .await;
    Mock::given(method("POST"))
        .and(path("/mcp"))
        .respond_with(move |req: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&req.body).unwrap_or_default();
            let id = body.get("id").cloned();
            let method_name = body["method"].as_str().unwrap_or_default();
            match method_name {
                "server/discover" => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })),
                "initialize" => ResponseTemplate::new(200)
                    .insert_header("Mcp-Session-Id", "mock-mcp-session")
                    .set_body_json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "protocolVersion": "2025-11-25",
                            "capabilities": { "tools": {} },
                            "serverInfo": { "name": "tavily", "version": "1.0.0" }
                        }
                    })),
                "notifications/initialized" => ResponseTemplate::new(202),
                "tools/list" => {
                    let tools = match mcp_phase_srv.load(Ordering::SeqCst) {
                        0 => serde_json::json!([
                            { "name": "search", "description": "Search web", "inputSchema": { "type": "object" } }
                        ]),
                        1 => serde_json::json!([
                            { "name": "search", "description": "Search web", "inputSchema": { "type": "object" } },
                            { "name": "extract", "description": "Extract page", "inputSchema": { "type": "object" } }
                        ]),
                        _ => serde_json::json!([
                            { "name": "search", "description": "Search web", "inputSchema": { "type": "object" } },
                            { "name": "map", "description": "Map site", "inputSchema": { "type": "object" } }
                        ]),
                    };
                    ResponseTemplate::new(200).set_body_json(serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": { "tools": tools }
                    }))
                }
                _ => ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "error": { "code": -32601, "message": "Method not found" }
                })),
            }
        })
        .mount(&mcp_server)
        .await;

    let llm_step = Arc::new(AtomicUsize::new(0));
    let llm_step_srv = Arc::clone(&llm_step);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            let step = llm_step_srv.fetch_add(1, Ordering::SeqCst);
            let stream = if step == 3 {
                // 第 4 条 prompt 的第 1 轮调用 ask，借工具执行空窗触发运行中 MCP 变更。
                support::tool_stream(vec![(
                    "call_ask",
                    "ask",
                    serde_json::json!({
                        "questions": [{
                            "id": "q1",
                            "question": "Proceed?",
                            "options": [{ "label": "Yes" }, { "label": "No" }]
                        }]
                    }),
                )])
            } else {
                text_stream("ok")
            };
            ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream")
        })
        .mount(&upstream)
        .await;

    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    std::fs::write(
        h.root.path().join("home").join("mcp.json"),
        serde_json::json!({
            "mcpServers": {
                "tavily": { "url": format!("{}/mcp", mcp_server.uri()) }
            }
        })
        .to_string(),
    )
    .expect("write mcp.json");

    // 单独的管理连接，用于重连 MCP 并轮询就绪；避免在反向请求回调内自调用重入。
    let (admin_server_side, admin_client_side) = moc_protocol::transport::duplex(65536);
    let admin_conn = h.server.spawn(admin_server_side);
    let (admin_client, _) = moc_protocol::ClientBuilder::new(moc_protocol::ClientInfo {
        name: "mcp-admin".into(),
        version: "1".into(),
        frontend: "test".into(),
    })
    .connect(admin_client_side)
    .await
    .expect("connect admin client");

    let wait_connected = |client: moc_protocol::ClientHandle, expected_tools: usize| async move {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        loop {
            let snapshot = client
                .call::<McpList>(McpListParams::default())
                .await
                .expect("mcp/list");
            if snapshot.servers.iter().any(|server| {
                server.name == "tavily"
                    && server.phase == McpServerPhase::Connected
                    && server.tool_count == expected_tools
            }) {
                return;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "等待 MCP 连接就绪超时：{snapshot:?}"
            );
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };

    // 主测试连接：支持 ask，并在收到 ask 时切换 MCP 工具名（数量同为 2，但工具名变了）。
    let (server_side, client_side) = moc_protocol::transport::duplex(65536);
    let conn = h.server.spawn(server_side);
    let ask_phase = Arc::clone(&mcp_phase);
    let ask_admin = admin_client.clone();
    let (client, _) = moc_protocol::ClientBuilder::new(moc_protocol::ClientInfo {
        name: "mcp-test".into(),
        version: "1".into(),
        frontend: "test".into(),
    })
    .capabilities(moc_protocol::ClientCapabilities {
        permission_requests: true,
        ask_requests: true,
    })
    .on_request::<SessionRequestAsk, _, _>(move |_params, _ctx| {
        let phase = Arc::clone(&ask_phase);
        let admin = ask_admin.clone();
        async move {
            phase.store(2, Ordering::SeqCst);
            admin
                .call::<McpReconnect>(McpReconnectParams {
                    server: Some("tavily".into()),
                    ..Default::default()
                })
                .await
                .expect("mid-prompt mcp reconnect");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                let snapshot = admin
                    .call::<McpList>(McpListParams::default())
                    .await
                    .expect("mcp/list");
                if snapshot
                    .servers
                    .iter()
                    .any(|s| s.name == "tavily" && s.phase == McpServerPhase::Connected)
                {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
            Ok(AskResult {
                answers: vec![AskAnswer {
                    id: "q1".into(),
                    selected: vec!["Yes".into()],
                    custom: None,
                }],
            })
        }
    })
    .connect(client_side)
    .await
    .expect("connect ask client");

    // 预热并确认首轮开始前 tavily 已就绪（1 tool）。
    wait_connected(admin_client.clone(), 1).await;

    let session = client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            engine: SessionEngine::Standard,
            plan_active: false,
            fast_mode: false,
        })
        .await
        .expect("new session");

    let send = |text: &str| {
        let client = client.clone();
        let session_id = session.id.clone();
        let text = text.to_owned();
        async move {
            client
                .call::<SessionPrompt>(PromptParams {
                    session_id,
                    input: vec![InputPart::Text { text }],
                    mode: None,
                    agent_message: None,
                    goal_round: None,
                    max_turns: None,
                })
                .await
                .expect("prompt")
        }
    };

    // 1. 首轮：tavily 已正常连接（1 tool）→ 系统提示含 tavily，但 runtime reminder 静默不注入。
    send("turn 1").await;

    // 2. 重连但工具集未变（仍为 [search]）→ 不应重复注入。
    admin_client
        .call::<McpReconnect>(McpReconnectParams {
            server: Some("tavily".into()),
            ..Default::default()
        })
        .await
        .expect("reconnect steady");
    wait_connected(admin_client.clone(), 1).await;
    send("turn 2").await;

    // 3. 会话空闲期间工具增加为 2 个（[search, extract]）→ 下一次消息前自动注入一次。
    mcp_phase.store(1, Ordering::SeqCst);
    admin_client
        .call::<McpReconnect>(McpReconnectParams {
            server: Some("tavily".into()),
            ..Default::default()
        })
        .await
        .expect("reconnect phase 1");
    wait_connected(admin_client.clone(), 2).await;
    send("turn 3").await;

    // 4. 运行中（ask 工具执行期间）工具名变更（[search, map]）→ 同条提示第 2 轮当场注入一次。
    send("turn 4").await;

    // 5. 后续未再变更 → 不再重复注入。
    send("turn 5").await;
    assert_eq!(mcp_phase.load(Ordering::SeqCst), 2, "ask 回调必须已触发运行中重连");

    conn.close();
    admin_conn.close();

    let requests = upstream.received_requests().await.expect("requests");
    assert_eq!(requests.len(), 6, "5 条 prompt（其中第 4 条含 1 次工具调用）共 6 个请求");

    let mcp_reminders_in = |req: &wiremock::Request| -> Vec<String> {
        let body: serde_json::Value = serde_json::from_slice(&req.body).expect("json body");
        body["messages"]
            .as_array()
            .expect("messages")
            .iter()
            .filter(|m| m["role"] != "system")
            .filter_map(|m| m["content"].as_str())
            .filter(|text| {
                text.contains("MCP servers updated")
                    || text.contains("MCP server status")
                    || text.contains("Connected (")
            })
            .map(str::to_owned)
            .collect()
    };

    // 请求 0（首轮正常）：0 条 MCP 运行时注入。
    let sys0 = serde_json::from_slice::<serde_json::Value>(&requests[0].body).unwrap()["messages"]
        [0]["content"]
        .as_str()
        .unwrap_or_default()
        .to_owned();
    assert!(sys0.contains("<mcp-servers>\ntavily"));
    assert!(
        mcp_reminders_in(&requests[0]).is_empty(),
        "首轮正常连接不应注入 MCP 提醒：{:?}",
        mcp_reminders_in(&requests[0])
    );

    // 请求 1（无变更重连后）：依然 0 条。
    assert!(
        mcp_reminders_in(&requests[1]).is_empty(),
        "无变更重连后不应重复注入：{:?}",
        mcp_reminders_in(&requests[1])
    );

    // 请求 2（空闲期间 1→2 tools）：历史里新增 1 条。
    let r2 = mcp_reminders_in(&requests[2]);
    assert_eq!(r2.len(), 1, "空闲期间变更应在下次消息前注入一次：{r2:?}");
    assert!(r2[0].contains("tavily: connected (2 tools)"));

    // 请求 3（turn 4 第 1 轮，尚未变更）：仍为 1 条。
    assert_eq!(mcp_reminders_in(&requests[3]).len(), 1);

    // 请求 4（turn 4 第 2 轮，ask 期间工具名变更）：当场新增至 2 条。
    let r4 = mcp_reminders_in(&requests[4]);
    assert_eq!(r4.len(), 2, "同次提示途中工具名变更应立即注入：{r4:?}");
    assert!(r4[1].contains("MCP servers updated"));

    // 请求 5（turn 5 未变更）：仍保持 2 条，不再重复。
    assert_eq!(mcp_reminders_in(&requests[5]).len(), 2);

    h.close().await;
}

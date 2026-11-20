use super::*;

#[tokio::test]
async fn plain_text_turn_persists_authoritative_message_over_rpc() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200)
            .set_body_raw(
                concat!(
                    "data: {\"id\":\"chatcmpl-test\",\"model\":\"model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Hello from core.\"},\"finish_reason\":null}]}\n\n",
                    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":4,\"total_tokens\":16}}\n\n",
                    "data: [DONE]\n\n",
                ),
                "text/event-stream",
            ))
        .mount(&upstream)
        .await;

    let root = tempfile::tempdir().expect("temporary project root");
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create workspace");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "language='zh'\n[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    ).expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd: cwd.clone(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
        ptc_worker: None,
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let update_sink = Arc::clone(&updates);
    let (client, handshake) = ClientBuilder::new(ClientInfo {
        name: "moc-core-test".into(),
        version: "0.1.0".into(),
        frontend: "test".into(),
    })
    .capabilities(ClientCapabilities {
        permission_requests: true,
        ask_requests: true,
    })
    .on_notification::<SessionUpdateMethod, _, _>(move |params, _ctx| {
        update_sink.lock().push(params.update);
        std::future::ready(())
    })
    .connect(frontend_side)
    .await
    .expect("connect embedded protocol client");

    assert_eq!(handshake.server.name, "moc-core");
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
    let result = client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "Say hello".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        })
        .await
        .expect("run model prompt");

    assert_eq!(result.prompt_id, "prompt_1");
    assert_eq!(result.message.text(), "Hello from core.");
    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: session.id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("load durable transcript");
    assert_eq!(
        loaded.entries.last().expect("assistant entry").message,
        result.message
    );

    {
        let updates = updates.lock();
        let finished = updates
            .iter()
            .find_map(|update| match update {
                SessionUpdate::PromptFinished { message, .. } => Some(message),
                _ => None,
            })
            .expect("prompt_finished update");
        assert_eq!(finished, &result.message);
        // 提示开始会广播一次写租约，提示结束后租约仍由本进程持有。
        assert!(updates.iter().any(|update| matches!(
            update,
            SessionUpdate::LockChanged { lock: Some(lock) } if lock.pid == std::process::id()
        )));
    }
    assert_eq!(
        upstream
            .received_requests()
            .await
            .expect("request capture")
            .len(),
        1
    );

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}

/// 在飞提示时 `session/unload` 必须拒绝，且不得摘除提示正在使用的运行态。
#[tokio::test]
async fn unload_is_refused_while_a_prompt_is_in_flight() {
    let upstream = MockServer::builder().start().await;
    // 门控掐住提示响应：请求已登记、提示在飞，不靠延迟猜窗口。
    // 注意不能轮询 `received_requests` 做同步——respond 在 wiremock 的
    // state 写锁内执行，Gate 阻塞期间读锁拿不到，测试会自己挂起。
    let gate = support::Gate::new();
    let held = gate.clone();
    let arrived = std::sync::Arc::new(tokio::sync::Notify::new());
    let hit = arrived.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            hit.notify_one();
            held.wait();
            ResponseTemplate::new(200).set_body_raw(text_stream("进行中"), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    let client = h.client.clone();
    let request_id = id.clone();
    let pending = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: request_id,
                input: vec![InputPart::Text {
                    text: "开始".into(),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
                max_turns: None,
            })
            .await
    });
    // 上游已收到请求且响应被掐住：提示锁与运行态都在飞。
    tokio::time::timeout(std::time::Duration::from_secs(30), arrived.notified())
        .await
        .expect("上游必须收到提示请求");
    let refused = h
        .client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: id.clone(),
        })
        .await;
    let error = match refused {
        Ok(_) => panic!("在飞提示时不得卸载会话"),
        Err(error) => error,
    };
    assert!(error.to_string().contains("SESSION_BUSY"), "{error}");
    gate.open();
    assert_eq!(
        pending.await.unwrap().expect("提示正常完成").message.text(),
        "进行中"
    );
    h.client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: id.clone(),
        })
        .await
        .expect("提示结束后可卸载");
    h.close().await;
}

use super::*;

/// 上游暂停在参数中间时必须已有预览，且尚未创建文件或开始执行。
#[tokio::test]
async fn tool_preview_arrives_before_response_finishes() {
    use serde_json::json;
    use tokio::io::AsyncWriteExt;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (release, gate) = tokio::sync::oneshot::channel();
    let frame = |delta: serde_json::Value, reason: serde_json::Value| {
        format!(
            "data: {}\n\n",
            json!({
                "id":"preview-response", "choices":[{"index":0,"delta":delta,"finish_reason":reason}]
            })
        )
    };
    let prefix = frame(
        json!({"tool_calls":[{"index":3,"id":"call_preview","type":"function","function":{"name":"write","arguments":"{\"path\":\"preview.txt\",\"content\":\"生成中"}}]}),
        json!(null),
    );
    let suffix = frame(
        json!({"tool_calls":[{"index":3,"function":{"arguments":"，完成\"}"}}]}),
        json!(null),
    ) + &frame(json!({}), json!("tool_calls"))
        + "data: [DONE]\n\n";
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        support::steering::request(&mut socket).await;
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{prefix}", prefix.len()+suffix.len()).as_bytes()).await.unwrap();
        gate.await.unwrap();
        socket.write_all(suffix.as_bytes()).await.unwrap();
        drop(socket);
        let (mut socket, _) = listener.accept().await.unwrap();
        support::steering::request(&mut socket).await;
        let body = text_stream("完成");
        socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
    });
    let h = Harness::new(&url, "openai_chat", "[compaction]\nenabled=false").await;
    let session_id = h.session(SessionEngine::Standard).await;
    let mut events = h.events.subscribe();
    let client = h.client.clone();
    let task = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id,
                input: vec![InputPart::Text {
                    text: "写入测试文件".into(),
                }],
                mode: None,
                agent_message: None,
                goal_round: None,
            })
            .await
            .unwrap()
    });
    let preview_id = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            let event = events.recv().await.unwrap();
            assert!(!matches!(event, SessionUpdate::ToolStarted { .. }));
            if let SessionUpdate::ToolPreview {
                preview_id,
                preview,
                ..
            } = event
            {
                if preview.body == "生成中" {
                    break preview_id;
                }
            }
        }
    })
    .await
    .expect("响应未结束时应收到解码后的预览");
    let file = h.root.path().join("work/preview.txt");
    assert!(!file.exists(), "生成预览不能提前执行写入");
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read_to_string(file).unwrap(), "生成中，完成");
    let updates = h.updates.lock().clone();
    let resolved = updates.iter().position(|event| matches!(event,
        SessionUpdate::ToolPreviewsResolved { calls, .. } if calls.iter().any(|call| call.preview_id == preview_id && call.tool_call_id == "call_preview")
    )).unwrap();
    let started = updates.iter().position(|event| matches!(event, SessionUpdate::ToolStarted { tool_call_id, .. } if tool_call_id == "call_preview")).unwrap();
    assert!(resolved < started);
    server.await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn upstream_model_and_effort_rewrites_are_reported_over_rpc() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            concat!(
                "data: {\"id\":\"chatcmpl-rewrite\",\"model\":\"model-lite\",\"reasoning_effort\":\"low\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"hi\"},\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":1,\"total_tokens\":6}}\n\n",
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
            "[models.roles]\ndefault='test/model:high'\nexplore='test/model:high'\nplan='test/model:high'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\nreasoning=true\n\
             [session]\ndir={:?}\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    )
    .expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd: cwd.clone(),
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
    let (client, _handshake) = ClientBuilder::new(ClientInfo {
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
    client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "Say hello".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
        })
        .await
        .expect("run model prompt");

    let fields = updates
        .lock()
        .iter()
        .find_map(|update| match update {
            SessionUpdate::UpstreamMismatch { fields, .. } => Some(fields.clone()),
            _ => None,
        })
        .expect("上游改写必须上报 UpstreamMismatch");
    assert!(
        fields
            .iter()
            .any(|field| field.field == UpstreamField::Model
                && field.requested == "model"
                && field.reported == "model-lite"),
        "{fields:?}"
    );
    assert!(
        fields
            .iter()
            .any(|field| field.field == UpstreamField::Effort
                && field.requested == "high"
                && field.reported == "low"),
        "{fields:?}"
    );

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}

/// 流式请求把首 token 等待记进 usage：TTFT 包含上游延迟，生成时间 = 总用时 − TTFT，
/// 且这条记录随 usage entry 落盘，恢复会话后仍能拆分。
#[tokio::test]
async fn usage_records_first_token_wait_for_streaming_calls() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|_request: &wiremock::Request| {
            ResponseTemplate::new(200)
                .set_body_raw(text_stream("答复"), "text/event-stream")
                .set_delay(std::time::Duration::from_millis(60))
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let session = h.session(SessionEngine::Standard).await;
    h.prompt(&session, "你好").await;

    let usage = h
        .client
        .call::<SessionUsage>(SessionUsageParams {
            session_id: session.clone(),
        })
        .await
        .expect("读取会话用量");
    assert_eq!(usage.total.calls, 1);
    assert_eq!(usage.total.ttft_calls, 1, "流式请求必须记录首 token");
    assert!(
        usage.total.ttft_ms >= 40,
        "首 token 等待必须包含上游延迟：{}",
        usage.total.ttft_ms
    );
    assert_eq!(
        usage.total.gen_ms,
        usage.total.active_ms - usage.total.ttft_ms,
        "生成时间 = 总用时 − 首 token 等待"
    );

    // 拆分数落在 usage entry 里，随 JSONL 保存。
    let ttft = support::transcript_entries(&h, &session)
        .await
        .iter()
        .find_map(|entry| match entry {
            moc_core::session::entries::Entry::Usage { ttft_ms, .. } => Some(*ttft_ms),
            _ => None,
        })
        .expect("usage entry 已落盘");
    assert!(ttft.is_some(), "首 token 等待必须随 usage entry 持久化");
    h.close().await;
}

//! 固定上游响应下验证 core 的 JSON-RPC 提示闭环。

#[path = "support/content.rs"]
mod content;
mod support;

use support::Harness;

fn text_stream(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "response",
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "delta": {"role": "assistant", "content": text},
                "finish_reason": "stop"
            }]
        })
    )
}

fn complete(text: &str) -> serde_json::Value {
    serde_json::json!({
        "id": "summary",
        "model": "gpt-test",
        "choices": [{
            "index": 0,
            "message": {"role": "assistant", "content": text},
            "finish_reason": "stop"
        }]
    })
}
use parking_lot::Mutex;
use std::sync::Arc;

use moc_core::{Core, CoreConfig};
use moc_llm::Client;
use moc_protocol::methods::{
    ClientCapabilities, ClientInfo, ConfigAction, ConfigManage, ConfigReply, ConfigScope,
    InputPart, PlanAction, PlanStatus, PromptParams, SessionCompact, SessionCompactParams,
    SessionLoad, SessionLoadParams, SessionMode, SessionModelOverflowAction, SessionNew,
    SessionNewParams, SessionPlanApprove, SessionPlanApproveParams, SessionPrompt, SessionSetMode,
    SessionSetModeParams, SessionSetModel, SessionSetModelParams, SessionUnload,
    SessionUnloadParams, SessionUpdate, SessionUpdateMethod, UpstreamField,
};
use moc_protocol::{ClientBuilder, Server, ServerOptions, transport};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};
#[tokio::test]
async fn hot_switch_persists_across_prompts_compaction_and_resume() {
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
                ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
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
            "[models.roles]\ndefault='test/glm:off'\nexplore='test/glm:off'\nplan='test/glm:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [providers.test.models.ds]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n[workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n[compaction]\nmethod_order=['soft']\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    )
    .expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd,
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
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
            mode: SessionMode::Normal,
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
            session_id: session.id.clone(),
            max_entries: None,
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
                ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
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
         [compaction]\nthreshold={mode=\"tokens\",value=1}\nmethod_order=['soft']\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;

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
                ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
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
         [providers.test.models.small]\ncontext_window=2000\nmax_output_tokens=4096\n\
         [compaction]\nauto=false\nmethod_order=['soft']\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness.prompt(&session, &"上下文填充".repeat(2000)).await;

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
        "2000 窗口装不下万级 token 历史"
    );
    assert_eq!(switched.session.model, "test/small:off");
    assert!(switched.compaction_error.is_none());

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

/// 目标模型装得下当前上下文时不打扰用户，照常切换。
#[tokio::test]
async fn model_switch_reports_no_overflow_when_the_target_window_fits() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
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
         [providers.test.models.big]\ncontext_window=2000000\nmax_output_tokens=4096\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness.prompt(&session, "先垫一点历史").await;

    let switched = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/big:off".into(),
            overflow: SessionModelOverflowAction::Ask,
        })
        .await
        .expect("切换到宽窗口模型");
    assert!(switched.overflow.is_none(), "{:?}", switched.overflow);
    assert_eq!(switched.session.model, "test/big:off");
    assert!(switched.compaction_error.is_none());

    let updates = harness.updates.lock().clone();
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. })),
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
                ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
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
         [providers.test.models.small]\ncontext_window=2000\nmax_output_tokens=4096\n\
         [compaction]\nmethod_order=['shake']\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness.prompt(&session, &"上下文填充".repeat(2000)).await;

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
    let error = switched.compaction_error.expect("无工具输出可供修剪");
    assert!(error.contains("Shake"), "{error}");

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

/// `switch` 动作直接切换并回报缺口，交给后续请求的既有压缩逻辑。
#[tokio::test]
async fn model_switch_action_switch_reports_overflow_after_switching() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
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
         [providers.test.models.small]\ncontext_window=2000\nmax_output_tokens=4096\n\
         [compaction]\nmethod_order=['shake']\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness.prompt(&session, &"上下文填充".repeat(2000)).await;

    let switched = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/small:off".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .expect("直接切换");
    assert_eq!(switched.session.model, "test/small:off");
    let overflow = switched.overflow.expect("回报容量缺口供前端告警");
    assert!(overflow.tokens > overflow.context_window);
    assert!(switched.compaction_error.is_none(), "switch 动作不压缩");

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
            "[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    ).expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd: cwd.clone(),
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
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
            mode: SessionMode::Normal,
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
        })
        .await
        .expect("run model prompt");

    assert_eq!(result.prompt_id, "prompt_1");
    assert_eq!(result.message.text(), "Hello from core.");
    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id.clone(),
            max_entries: None,
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
        assert!(
            updates
                .iter()
                .any(|update| matches!(update, SessionUpdate::LockChanged { lock: None }))
        );
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

#[tokio::test]
async fn plan_tools_write_then_exit_emit_review_only_on_exit() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use moc_protocol::methods::{
        PlanAction, PlanStatus, SessionPlanApprove, SessionPlanApproveParams,
    };
    use serde_json::json;

    let plan_arguments = json!({
        "title": "Build a small feature",
        "markdown": "# Approved plan\n\n1. Inspect the workspace.\n2. Implement the change.",
    })
    .to_string();
    let plan_chunk = json!({
        "id": "chatcmpl-plan",
        "model": "model",
        "choices": [{
            "index": 0,
            "delta": {
                "role": "assistant",
                "tool_calls": [{
                    "index": 0,
                    "id": "call_plan",
                    "type": "function",
                    "function": {"name": "write_plan", "arguments": plan_arguments},
                }, {
                    "index": 1,
                    "id": "call_exit",
                    "type": "function",
                    "function": {"name": "exit_plan", "arguments": "{}"},
                }],
            },
            "finish_reason": null,
        }],
    });
    let plan_response = format!(
        "data: {plan_chunk}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    );
    let text_response = concat!(
        "data: {\"id\":\"chatcmpl-implementation\",\"model\":\"model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Implementation started.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let response_index = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let body = if response_counter.fetch_add(1, Ordering::SeqCst) == 0 {
                plan_response.clone()
            } else {
                text_response.to_owned()
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
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
            "[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    ).expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd,
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let update_sink = Arc::clone(&updates);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "moc-core-plan-test".into(),
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
            mode: SessionMode::Plan,
        })
        .await
        .expect("create plan session");
    let submitted = client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "Design the change".into(),
            }],
            mode: None,
            agent_message: None,
        })
        .await
        .expect("submit plan via model tool call");
    assert_eq!(submitted.finish_reason, moc_llm::FinishReason::Stop);
    assert!(
        submitted
            .message
            .tool_calls()
            .any(|call| call.name == "write_plan")
    );
    let notifications = updates.lock();
    let exit_started = notifications
        .iter()
        .position(|update| {
            matches!(update,
                SessionUpdate::ToolStarted { name, .. } if name == "exit_plan"
            )
        })
        .expect("退出计划工具开始");
    let reviewed = notifications
        .iter()
        .position(|update| matches!(update, SessionUpdate::PlanSubmitted { .. }))
        .expect("计划审阅通知");
    assert!(reviewed > exit_started, "写入工具不得提前触发审阅");
    drop(notifications);
    assert!(
        submitted
            .message
            .tool_calls()
            .any(|call| call.name == "exit_plan")
    );
    assert!(
        updates
            .lock()
            .iter()
            .any(|update| matches!(update, SessionUpdate::PlanSubmitted { .. }))
    );
    let parent = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id.clone(),
            max_entries: None,
        })
        .await
        .expect("load submitted plan");
    let plan = parent
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("submitted plan projection");
    assert_eq!(plan.path, format!("local://{}.md", plan.plan_id));
    let paths = moc_core::session::store::SessionStore::new(&sessions).paths_for(
        std::path::Path::new(&parent.session.cwd),
        &session.id,
        parent.session.created_at_ms,
    );
    assert_eq!(
        std::fs::read_to_string(paths.plans().join(format!("{}.md", plan.plan_id)))
            .expect("实体计划文件"),
        plan.markdown
    );
    assert_eq!(parent.session.mode, SessionMode::Plan);
    assert!(!plan.path.contains("/sessions/"));
    let approved = client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id,
            plan_id: plan.plan_id.clone(),
            action: PlanAction::ExecuteNewSession,
            execution_model: None,
        })
        .await
        .expect("approve into new session");
    assert_eq!(approved.mode, SessionMode::Normal);
    assert_eq!(
        approved.auto_prompt.as_deref(),
        Some("Execute the approved plan.")
    );
    assert_eq!(
        approved.plan_markdown.as_deref(),
        Some(plan.markdown.as_str())
    );

    let child = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: approved.session_id.clone(),
            max_entries: None,
        })
        .await
        .expect("load child session");
    assert_eq!(child.session.mode, SessionMode::Normal);
    assert_eq!(child.entries.len(), 1);
    assert!(child.entries[0].message.text().contains("<active-plan"));
    let implementation = client
        .call::<SessionPrompt>(PromptParams {
            session_id: approved.session_id,
            input: vec![InputPart::Text {
                text: approved.auto_prompt.unwrap(),
            }],
            mode: None,
            agent_message: None,
        })
        .await
        .expect("start approved implementation");
    assert_eq!(implementation.message.text(), "Implementation started.");

    let requests = upstream
        .received_requests()
        .await
        .expect("capture upstream requests");
    assert_eq!(requests.len(), 2);
    let plan_request: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("plan request JSON");
    let plan_tools = plan_request["tools"].as_array().expect("plan tool list");
    assert!(
        plan_tools
            .iter()
            .any(|tool| tool["function"]["name"] == "write_plan")
    );
    assert!(
        !plan_tools
            .iter()
            .any(|tool| tool["function"]["name"] == "edit")
    );
    assert!(
        !plan_tools
            .iter()
            .any(|tool| tool["function"]["name"] == "write")
    );
    let implementation_request: serde_json::Value =
        serde_json::from_slice(&requests[1].body).expect("implementation request JSON");
    assert!(
        implementation_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.contains("<active-plan")))
    );
    assert_eq!(
        implementation_request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["content"]
                .as_str()
                .is_some_and(|text| text.matches("<active-plan").count() > 0))
            .count(),
        1
    );

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}

#[tokio::test]
async fn model_read_then_edit_updates_the_workspace_and_replays_tool_results() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use moc_core::hashline::{assign_hashes, encode_anchor};
    use serde_json::{Value, json};

    let first_line = "old value".to_owned();
    let second_line = "second value".to_owned();
    let anchors = assign_hashes(&[first_line.clone(), second_line.clone()], &[])
        .expect("allocate stable test anchors");
    let from = encode_anchor(anchors[0]).expect("encode first anchor");
    let to = encode_anchor(anchors[1]).expect("encode second anchor");
    let read_arguments = json!({"path": "target.txt"});
    let edit_arguments = json!({
        "path": "target.txt",
        "edits": [[from, to, "new value"]],
    });
    let tool_response = |call_id: &str, name: &str, arguments: &Value| {
        let first = json!({
            "id": format!("chatcmpl-{call_id}"),
            "model": "model",
            "choices": [{"index": 0, "delta": {
                "role": "assistant",
                "tool_calls": [{"index": 0, "id": call_id, "type": "function", "function": {
                    "name": name,
                    "arguments": arguments.to_string(),
                }}],
            }, "finish_reason": null}],
        });
        format!(
            "data: {first}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
        )
    };
    let responses = [
        tool_response("call_read", "read", &read_arguments),
        tool_response("call_edit", "edit", &edit_arguments),
        concat!(
            "data: {\"id\":\"chatcmpl-done\",\"model\":\"model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"Updated target.txt.\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ).to_owned(),
    ];
    let response_index = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = response_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(responses[index].clone(), "text/event-stream")
        })
        .mount(&upstream)
        .await;

    let root = tempfile::tempdir().expect("temporary project root");
    let home = root.path().join("home");
    let cwd = root.path().join("workspace");
    let sessions = root.path().join("sessions");
    std::fs::create_dir_all(&home).expect("create home");
    std::fs::create_dir_all(&cwd).expect("create workspace");
    std::fs::write(cwd.join("target.txt"), "old value\nsecond value\n")
        .expect("create edit target");
    std::fs::write(
        home.join("config.toml"),
        format!(
            "[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    ).expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd: cwd.clone(),
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let update_sink = Arc::clone(&updates);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "moc-core-tool-test".into(),
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
            mode: SessionMode::Normal,
        })
        .await
        .expect("create session");
    let result = client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "Read and update target.txt".into(),
            }],
            mode: None,
            agent_message: None,
        })
        .await
        .expect("run read-edit-tool prompt");

    assert_eq!(result.message.text(), "Updated target.txt.");
    assert_eq!(
        std::fs::read_to_string(cwd.join("target.txt")).unwrap(),
        "new value\n"
    );
    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id,
            max_entries: None,
        })
        .await
        .expect("load persisted tool history");
    let results: Vec<_> = loaded
        .entries
        .iter()
        .filter(|entry| entry.message.role == moc_llm::Role::Tool)
        .collect();
    assert_eq!(results.len(), 2);
    let read_output = results[0]
        .message
        .content
        .iter()
        .find_map(|part| match part {
            moc_llm::ContentPart::ToolResult(result) => Some(result.content.as_str()),
            _ => None,
        })
        .expect("read tool result content");
    let edit_output = results[1]
        .message
        .content
        .iter()
        .find_map(|part| match part {
            moc_llm::ContentPart::ToolResult(result) => Some(result.content.as_str()),
            _ => None,
        })
        .expect("edit tool result content");
    assert!(read_output.contains(&format!("{from}│old value")));
    assert!(edit_output.contains("+ ") && edit_output.contains("new value"));
    {
        let updates = updates.lock();
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(update, SessionUpdate::ToolFinished { .. }))
                .count(),
            2
        );
        assert!(updates.iter().any(|update| matches!(
        update,
        SessionUpdate::ToolFinished { tool_call_id, status: moc_protocol::methods::ToolStatus::Completed, .. }

            if tool_call_id == "call_edit"
    )));
    }

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}
#[tokio::test]
async fn plan_mode_reports_shell_workspace_changes_in_the_next_request() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    let first = json!({
        "id": "chatcmpl-plan-bash",
        "model": "model",
        "choices": [{"index": 0, "delta": {
            "role": "assistant",
            "tool_calls": [{"index": 0, "id": "call_bash", "type": "function", "function": {
                "name": "bash",
                "arguments": "{\"command\":\"printf changed > violation.txt\"}",
            }}],
        }, "finish_reason": null}],
    });
    let response = format!(
        "data: {first}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    );
    let final_response = concat!(
        "data: {\"id\":\"chatcmpl-violation\",\"model\":\"model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"I will stop.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let responses = [response, final_response.to_owned()];
    let response_index = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = response_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(responses[index].clone(), "text/event-stream")
        })
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
            "[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [workspace]\nwatch=true\nmax_scan_entries=1000\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n",
            upstream.uri(),
            sessions.to_string_lossy(),
        ),
    ).expect("write config");

    let core = Core::new(CoreConfig {
        home,
        cwd: cwd.clone(),
        overlays: Vec::new(),
        session_dir: None,
        client: Client::new(),
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let update_sink = Arc::clone(&updates);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "moc-core-mode-test".into(),
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
            mode: SessionMode::Plan,
        })
        .await
        .expect("create plan session");
    let result = client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "Inspect without changing files".into(),
            }],
            mode: None,
            agent_message: None,
        })
        .await
        .expect("run read-only plan prompt");
    assert_eq!(result.message.text(), "I will stop.");
    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id,
            max_entries: None,
        })
        .await
        .expect("load plan session after shell");
    let shell_result = loaded
        .entries
        .iter()
        .find_map(|entry| {
            entry.message.content.iter().find_map(|part| match part {
                moc_llm::ContentPart::ToolResult(result) if result.tool_call_id == "call_bash" => {
                    Some(result.content.clone())
                }
                _ => None,
            })
        })
        .expect("persisted shell result");
    assert!(
        cwd.join("violation.txt").is_file(),
        "bash tool result: {shell_result}"
    );
    assert_eq!(
        std::fs::read_to_string(cwd.join("violation.txt")).unwrap(),
        "changed"
    );

    let violation = updates.lock().iter().any(|update| matches!(
        update,
        SessionUpdate::WorkspaceChanged { violation: true, changes }
            if changes.iter().any(|change| change.path == "violation.txt" && change.kind == "added")
    ));
    assert!(violation, "plan-mode writes must be reported as violations");
    let requests = upstream
        .received_requests()
        .await
        .expect("capture model requests");
    assert_eq!(requests.len(), 2);
    let next_request: serde_json::Value =
        serde_json::from_slice(&requests[1].body).expect("next request JSON");
    let reminder = next_request["messages"].as_array().unwrap().last().unwrap();
    assert_eq!(reminder["role"], "user");
    let text = content::content_text(&reminder["content"]);
    assert!(
        text.contains("modified the workspace in read-only mode"),
        "{text}"
    );
    assert!(text.contains("violation.txt"), "{text}");

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}

#[tokio::test]
async fn prompt_keeps_arguments_when_tool_name_precedes_call_id() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    // 网关把工具名放进先到的 chunk、`id` 放到后续 chunk：参数增量必须归到真实
    // 调用 ID 上；若 `ToolCallStart` 带着空 ID 提前发射，前端会把参数落到空 ID
    // 上，工具块只剩动作没有命令。
    let mut named = json!({
        "id": "chatcmpl-name-first",
        "model": "model",
        "choices": [{"index": 0, "delta": {"role": "assistant", "tool_calls": [{
            "index": 0,
            "type": "function",
            "function": {"name": "bash"},
        }]}, "finish_reason": null}],
    });
    named["choices"][0]["delta"]["tool_calls"][0]
        .as_object_mut()
        .expect("tool call 是对象")
        .remove("id");
    let with_id = json!({
        "choices": [{"index": 0, "delta": {"tool_calls": [{
            "index": 0,
            "id": "call_name_first",
            "type": "function",
            "function": {"arguments": "{\"command\":\"printf name-first\"}"},
        }]}, "finish_reason": null}],
    });
    let finalized = json!({
        "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
    });
    let follow_up = concat!(
        "data: {\"id\":\"chatcmpl-name-first-done\",\"model\":\"model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"done\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let response =
        format!("data: {named}\n\ndata: {with_id}\n\ndata: {finalized}\n\ndata: [DONE]\n\n");
    let responses = [response, follow_up.to_owned()];
    let response_index = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = response_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(responses[index].clone(), "text/event-stream")
        })
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
            "[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [workspace]\nwatch=true\nmax_scan_entries=1000\n\
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
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let update_sink = Arc::clone(&updates);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "moc-core-name-first-test".into(),
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
            mode: SessionMode::Normal,
        })
        .await
        .expect("create session");
    let result = client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "运行 name-first".into(),
            }],
            mode: None,
            agent_message: None,
        })
        .await
        .expect("run prompt");
    assert_eq!(result.message.text(), "done");

    let updates = updates.lock();
    let mut deltas: Vec<&str> = Vec::new();
    let mut empty_ids: Vec<String> = Vec::new();
    for update in updates.iter() {
        let SessionUpdate::AssistantDelta { delta, .. } = update else {
            continue;
        };
        match delta {
            moc_protocol::methods::AssistantDelta::ToolCallStarted { tool_call_id, .. } => {
                if tool_call_id.is_empty() {
                    empty_ids.push("ToolCallStarted".into());
                }
            }
            moc_protocol::methods::AssistantDelta::ToolCallArgs {
                tool_call_id,
                delta,
            } => {
                if tool_call_id.is_empty() {
                    empty_ids.push(format!("ToolCallArgs({delta})"));
                }
                deltas.push(delta);
            }
            _ => {}
        }
    }
    assert!(
        empty_ids.is_empty(),
        "工具增量不得落到空调用 ID 上：{empty_ids:?}"
    );
    assert_eq!(deltas.len(), 1, "参数增量数量：{deltas:?}");
    let arguments = deltas.concat();
    let arguments: serde_json::Value =
        serde_json::from_str(&arguments).expect("参数增量拼接后是合法 JSON");
    assert_eq!(arguments["command"], "printf name-first");
    drop(updates);

    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id,
            max_entries: None,
        })
        .await
        .expect("load session");
    let persisted = loaded
        .entries
        .iter()
        .find_map(|entry| {
            entry
                .message
                .tool_calls()
                .find(|call| call.id == "call_name_first")
                .cloned()
        })
        .expect("持久化的工具调用");
    assert_eq!(persisted.name, "bash");
    assert_eq!(persisted.arguments, "{\"command\":\"printf name-first\"}");

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}

#[tokio::test]
async fn transient_prompt_mode_is_read_only_and_not_persisted() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    // 上游先让我调用 bash 写文件，再收尾：用来观察本次提示是否被按只读模式约束。
    let first = json!({
        "id": "chatcmpl-ask-bash",
        "model": "model",
        "choices": [{"index": 0, "delta": {
            "role": "assistant",
            "tool_calls": [{"index": 0, "id": "call_bash", "type": "function", "function": {
                "name": "bash",
                "arguments": "{\"command\":\"printf changed > violation.txt\"}",
            }}],
        }, "finish_reason": null}],
    });
    let response = format!(
        "data: {first}\n\ndata: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\ndata: [DONE]\n\n"
    );
    let final_response = concat!(
        "data: {\"id\":\"chatcmpl-ask-done\",\"model\":\"model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"I will stop.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: [DONE]\n\n",
    );
    let responses = [response, final_response.to_owned()];
    let response_index = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = response_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(responses[index].clone(), "text/event-stream")
        })
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
            "[models.roles]\ndefault='test/model:off'\nexplore='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\n\
             [workspace]\nwatch=true\nmax_scan_entries=1000\n\
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
    })
    .expect("construct core");
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let updates = Arc::new(Mutex::new(Vec::new()));
    let update_sink = Arc::clone(&updates);
    let (client, _) = ClientBuilder::new(ClientInfo {
        name: "moc-core-ask-once-test".into(),
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
            mode: SessionMode::Normal,
        })
        .await
        .expect("create normal session");
    let result = client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.id.clone(),
            input: vec![InputPart::Text {
                text: "看一下有没有遗留文件".into(),
            }],
            mode: Some(SessionMode::Ask),
            agent_message: None,
        })
        .await
        .expect("run ask-once prompt");
    assert_eq!(result.message.text(), "I will stop.");

    // 本次提示按只读约束：写操作被执行但必须被标记为违规，并注入到下一次请求的提醒里。
    let violation = updates.lock().iter().any(|update| matches!(
        update,
        SessionUpdate::WorkspaceChanged { violation: true, changes }
            if changes.iter().any(|change| change.path == "violation.txt" && change.kind == "added")
    ));
    assert!(violation, "提示级问答模式必须按只读判定工作区改动");

    let requests = upstream
        .received_requests()
        .await
        .expect("capture model requests");
    let first_request: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("first request JSON");
    let messages = first_request["messages"]
        .as_array()
        .expect("request messages");
    assert!(
        messages.iter().any(|message| {
            content::content_text(&message["content"]).contains("Ask mode is active")
        }),
        "mode contract missing from request messages"
    );

    // 覆盖不落地：既没有 ModeChanged 通知，重载后会话模式仍是 Normal。
    assert!(
        !updates
            .lock()
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModeChanged { .. })),
        "提示级覆盖不得广播为会话模式变更"
    );
    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id,
            max_entries: None,
        })
        .await
        .expect("load session after ask-once prompt");
    assert_eq!(loaded.session.mode, SessionMode::Normal);

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
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
             [workspace]\nwatch=false\n\
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
            mode: SessionMode::Normal,
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

/// 进入计划模式切到 `plan` 角色模型，退出（含会话恢复后）恢复进入前的会话模型。
#[tokio::test]
async fn plan_mode_switches_to_the_plan_role_model_and_restores_it() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let model = body["model"].as_str().expect("请求带模型");
            ResponseTemplate::new(200).set_body_raw(
                text_stream(match model {
                    "glm" => "glm response",
                    "ds" => "ds response",
                    other => panic!("unexpected model {other}"),
                }),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/ds:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.ds]\ncontext_window=100000\nmax_output_tokens=4096\n",
    )
    .await;

    let session = harness
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            mode: SessionMode::Normal,
        })
        .await
        .expect("create session");
    assert_eq!(session.model, "test/glm:off");
    harness.prompt(&session.id, "first").await;

    let planned = harness
        .client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: session.id.clone(),
            mode: SessionMode::Plan,
        })
        .await
        .expect("enter plan mode");
    assert_eq!(planned.mode, SessionMode::Plan);
    assert_eq!(planned.model, "test/ds:off", "进入计划模式要切到计划模型");
    harness.prompt(&session.id, "plan").await;

    // 卸载再恢复：计划模式与计划模型都从 transcript 复原。
    harness
        .client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: session.id.clone(),
        })
        .await
        .expect("unload session");
    let resumed = harness.load(&session.id).await;
    assert_eq!(resumed.session.mode, SessionMode::Plan);
    assert_eq!(resumed.session.model, "test/ds:off");

    let normal = harness
        .client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: session.id.clone(),
            mode: SessionMode::Normal,
        })
        .await
        .expect("leave plan mode");
    assert_eq!(
        normal.model, "test/glm:off",
        "退出计划模式要恢复进入前的模型"
    );
    harness.prompt(&session.id, "implement").await;

    // 以计划模式直接创建：立即使用计划模型，退出后回到创建时指定的模型。
    let created = harness
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            mode: SessionMode::Plan,
        })
        .await
        .expect("create plan session");
    assert_eq!(created.model, "test/ds:off");
    let exited = harness
        .client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: created.id.clone(),
            mode: SessionMode::Normal,
        })
        .await
        .expect("leave plan mode");
    assert_eq!(exited.model, "test/glm:off");

    let updates = harness.updates.lock().clone();
    for model in ["test/ds:off", "test/glm:off"] {
        assert!(
            updates.contains(&SessionUpdate::ModelChanged {
                model: model.into()
            }),
            "缺少 {model} 的模型变化通知：{updates:?}"
        );
    }
    let models: Vec<String> = upstream
        .received_requests()
        .await
        .expect("capture requests")
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("请求体 JSON")["model"]
                .as_str()
                .expect("请求模型")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["glm", "ds", "glm"]);
    harness.close().await;
}

/// `plan = "none"`：进入与退出计划模式都不切换模型，也不推送模型变化。
#[tokio::test]
async fn plan_role_none_keeps_the_session_model_across_mode_changes() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(text_stream("plain response"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "none",
        "",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness.prompt(&session, "first").await;
    let planned = harness
        .client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: session.clone(),
            mode: SessionMode::Plan,
        })
        .await
        .expect("enter plan mode");
    assert_eq!(planned.model, "test/gpt-test:off");
    harness.prompt(&session, "plan").await;
    let normal = harness
        .client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: session,
            mode: SessionMode::Normal,
        })
        .await
        .expect("leave plan mode");
    assert_eq!(normal.model, "test/gpt-test:off");
    assert!(
        !harness
            .updates
            .lock()
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. })),
        "none 不应产生模型变化通知"
    );
    let models: Vec<String> = upstream
        .received_requests()
        .await
        .expect("capture requests")
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("请求体 JSON")["model"]
                .as_str()
                .expect("请求模型")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["gpt-test", "gpt-test"]);
    harness.close().await;
}

/// 未选择执行模型时使用审批时的会话模型，不恢复进入计划前的模型。
#[tokio::test]
async fn plan_approval_defaults_to_current_session_model() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    let plan_arguments = json!({
        "title": "Build a small feature",
        "markdown": "# Approved plan\n\n1. Inspect the workspace.\n2. Implement the change.",
    });
    let responses = [
        support::tool_stream(vec![
            ("call_plan", "write_plan", plan_arguments),
            ("call_exit", "exit_plan", json!({})),
        ]),
        text_stream("Implementation started."),
    ];
    let response_index = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&response_index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let index = response_counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(responses[index].clone(), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/ds:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.ds]\ncontext_window=100000\nmax_output_tokens=4096\n",
    )
    .await;

    let session = harness
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            mode: SessionMode::Plan,
        })
        .await
        .expect("create plan session");
    assert_eq!(session.model, "test/ds:off");
    harness.prompt(&session.id, "Design the change").await;

    let loaded = harness.load(&session.id).await;
    let plan = loaded
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("提交的计划");
    let approved = harness
        .client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id.clone(),
            plan_id: plan.plan_id.clone(),
            action: PlanAction::Execute,
            execution_model: None,
        })
        .await
        .expect("approve plan in place");
    assert_eq!(approved.mode, SessionMode::Normal);
    assert_eq!(
        approved.auto_prompt.as_deref(),
        Some("Execute the approved plan.")
    );
    let reloaded = harness.load(&session.id).await;
    assert_eq!(reloaded.session.mode, SessionMode::Normal);
    assert_eq!(reloaded.session.model, "test/ds:off");
    harness
        .prompt(&session.id, "Execute the approved plan.")
        .await;
    let models: Vec<String> = upstream
        .received_requests()
        .await
        .expect("capture requests")
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("请求体 JSON")["model"]
                .as_str()
                .expect("请求模型")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["ds", "ds"]);
    harness.close().await;
}

#[tokio::test]
async fn plan_approval_uses_selected_execution_model_for_compaction_and_execution() {
    use serde_json::json;
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("请求 JSON");
            if body["stream"] != true {
                assert_eq!(body["model"], "executor", "压缩必须使用执行模型");
                ResponseTemplate::new(200).set_body_json(complete("保留计划和接口决策"))
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
                            "write_plan",
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
        "test/gpt-test:off",
        "test/planner:off",
        "[providers.test.models.planner]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.executor]\ncontext_window=200000\nmax_output_tokens=4096\n\
         [compaction]\nmethod_order=['soft']\nauto=false\n",
    )
    .await;
    for action in [PlanAction::Execute, PlanAction::ExecuteCompacted] {
        let id = h.session(SessionMode::Plan).await;
        h.prompt(&id, "设计方案").await;
        let plan = h.load(&id).await.plans.pop().expect("已写计划");
        let invalid = h
            .client
            .call::<SessionPlanApprove>(SessionPlanApproveParams {
                session_id: id.clone(),
                plan_id: plan.plan_id.clone(),
                action,
                execution_model: Some("test/missing:off".into()),
            })
            .await;
        assert!(invalid.is_err());
        let unchanged = h.load(&id).await;
        assert_eq!(unchanged.plans[0].status, PlanStatus::Submitted);
        assert_eq!(unchanged.session.mode, SessionMode::Plan);
        assert_eq!(unchanged.session.model, "test/planner:off");
        h.updates.lock().clear();
        let approved = h
            .client
            .call::<SessionPlanApprove>(SessionPlanApproveParams {
                session_id: id.clone(),
                plan_id: plan.plan_id,
                action,
                execution_model: Some("test/executor:off".into()),
            })
            .await
            .expect("指定执行模型审批");
        let loaded = h.load(&id).await;
        assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
        assert_eq!(loaded.session.mode, SessionMode::Normal);
        assert_eq!(loaded.session.model, "test/executor:off");
        let changes: Vec<_> = h
            .updates
            .lock()
            .iter()
            .filter_map(|update| match update {
                SessionUpdate::ModeChanged { mode } => Some(format!("mode:{mode:?}")),
                SessionUpdate::ModelChanged { model } => Some(format!("model:{model}")),
                _ => None,
            })
            .collect();
        assert_eq!(changes, ["mode:Normal", "model:test/executor:off"]);
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

#[tokio::test]
async fn plan_new_session_uses_selected_execution_model() {
    use serde_json::json;
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("请求 JSON");
            let response = if body["model"] == "executor" {
                assert!(body.to_string().contains("<active-plan"));
                text_stream("子会话执行")
            } else {
                support::tool_stream(vec![
                    (
                        "save",
                        "write_plan",
                        json!({"title":"子会话方案", "markdown":"# 方案\n实现接口"}),
                    ),
                    ("exit", "exit_plan", json!({})),
                ])
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "test/planner:off",
        "[providers.test.models.planner]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.executor]\ncontext_window=200000\nmax_output_tokens=4096\n",
    )
    .await;
    let id = h.session(SessionMode::Plan).await;
    h.prompt(&id, "设计子会话方案").await;
    let plan = h.load(&id).await.plans.pop().expect("待审计划");
    let approved = h
        .client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: id.clone(),
            plan_id: plan.plan_id.clone(),
            action: PlanAction::ExecuteNewSession,
            execution_model: Some("test/executor:off".into()),
        })
        .await
        .expect("新会话审批");
    assert_ne!(approved.session_id, id);
    let child = h.load(&approved.session_id).await;
    assert_eq!(child.session.mode, SessionMode::Normal);
    assert_eq!(child.session.model, "test/executor:off");
    assert_eq!(child.plans[0].status, PlanStatus::Approved);
    assert_eq!(child.plans[0].path, format!("local://{}.md", plan.plan_id));
    assert!(child.entries[0].message.text().contains("<active-plan"));
    let parent = h.load(&id).await;
    assert_eq!(parent.session.mode, SessionMode::Plan);
    assert_eq!(parent.session.model, "test/planner:off");
    assert_eq!(
        h.prompt(
            &approved.session_id,
            approved.auto_prompt.as_deref().expect("执行提示")
        )
        .await
        .message
        .text(),
        "子会话执行"
    );
    assert_eq!(parent.plans[0].status, PlanStatus::Approved);
    assert_eq!(h.load(&id).await.entries, parent.entries);
    h.close().await;
}

#[tokio::test]
async fn plan_tools_reject_invalid_payloads_and_keep_round_open() {
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    let upstream = MockServer::builder().start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let seen = calls.clone();
    Mock::given(method("POST"))
        .respond_with(move |_request: &wiremock::Request| {
            let tool = match seen.fetch_add(1, Ordering::SeqCst) {
                0 => ("missing", "exit_plan", json!({})),
                1 => (
                    "invalid_write",
                    "write_plan",
                    json!({"title":"方案", "markdown":"# 方案", "extra":true}),
                ),
                2 => (
                    "save",
                    "write_plan",
                    json!({"title":"方案", "markdown":"# 方案"}),
                ),
                3 => ("invalid_exit", "exit_plan", json!({"extra":true})),
                4 => ("exit", "exit_plan", json!({})),
                _ => panic!("成功退出计划后不得继续调用上游"),
            };
            ResponseTemplate::new(200)
                .set_body_raw(support::tool_stream(vec![tool]), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionMode::Plan).await;
    h.prompt(&id, "保存和审阅计划").await;
    let loaded = h.load(&id).await;
    assert_eq!(loaded.session.mode, SessionMode::Plan);
    assert_eq!(loaded.plans.len(), 1);
    assert_eq!(loaded.plans[0].status, PlanStatus::Submitted);
    let errors: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| &entry.message.content)
        .filter_map(|part| match part {
            moc_llm::ContentPart::ToolResult(result) if result.is_error => {
                Some((&*result.tool_call_id, &*result.content))
            }
            _ => None,
        })
        .collect();
    assert_eq!(errors.len(), 3);
    assert_eq!(errors[0].0, "missing");
    assert!(errors[0].1.contains("E_PLAN_NOT_FOUND"));
    assert_eq!(errors[1].0, "invalid_write");
    assert!(errors[1].1.contains("E_BAD_PAYLOAD"));
    assert_eq!(errors[2].0, "invalid_exit");
    assert!(errors[2].1.contains("E_BAD_PAYLOAD"));
    assert_eq!(calls.load(Ordering::SeqCst), 5);
    h.close().await;
}

/// 在飞提示时 `session/unload` 必须拒绝，且不得摘除提示正在使用的运行态。
#[tokio::test]
async fn unload_is_refused_while_a_prompt_is_in_flight() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(400))
                .set_body_raw(text_stream("进行中"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
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
            })
            .await
    });
    // 上游已收到请求且响应被延迟：提示锁与运行态都在飞。
    let mut waited = 0;
    while upstream
        .received_requests()
        .await
        .map_or(0, |requests| requests.len())
        == 0
    {
        assert!(waited < 200, "上游必须收到提示请求");
        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        waited += 1;
    }
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

/// 多轮提示每轮都从当前会话状态重建请求投影，不得重复或丢失已提交的消息。
#[tokio::test]
async fn tool_rounds_rebuild_the_request_projection_from_current_state() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let upstream = MockServer::builder().start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let response = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                support::tool_stream(vec![(
                    "todo_1",
                    "todo",
                    serde_json::json!({"op":"add","text":"第一轮任务"}),
                )])
            } else {
                text_stream("收尾完成")
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
    assert_eq!(h.prompt(&id, "执行两轮").await.message.text(), "收尾完成");
    let requests = upstream.received_requests().await.expect("请求记录");
    assert_eq!(requests.len(), 2);
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    let rendered = second.to_string();
    assert_eq!(
        rendered.matches("执行两轮").count(),
        1,
        "用户消息必须恰好出现一次：{rendered}"
    );
    assert_eq!(
        rendered.matches("todo_1").count(),
        2,
        "工具调用与结果各出现一次"
    );
    assert_eq!(second["messages"][0]["role"], "system");
    h.close().await;
}

/// 保存配置后既有会话必须重新读取指令文件，而不是沿用首次提示的缓存。
#[tokio::test]
async fn config_save_invalidates_the_session_instruction_cache() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_raw(text_stream("好的"), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    std::fs::write(
        h.root.path().join("work/AGENTS.md"),
        "配置后指令：始终使用中文回答\n",
    )
    .expect("写指令文件");
    let id = h.session(SessionMode::Normal).await;
    h.prompt(&id, "第一次").await;
    let cwd = h.root.path().join("work").to_string_lossy().into_owned();
    let reply = h
        .client
        .call::<ConfigManage>(ConfigAction::Read {
            scope: ConfigScope::Global,
            cwd: cwd.clone(),
        })
        .await
        .expect("读取配置");
    let ConfigReply::Settings(document) = reply else {
        panic!("设置文档");
    };
    let mut edited = document.document.clone();
    edited["context_files"] = serde_json::json!({"enabled": true});
    h.client
        .call::<ConfigManage>(ConfigAction::Save {
            scope: ConfigScope::Global,
            cwd,
            document: edited,
            revision: document.revision,
        })
        .await
        .expect("保存配置");
    h.prompt(&id, "第二次").await;
    let requests = upstream.received_requests().await.expect("请求记录");
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        !first.to_string().contains("配置后指令"),
        "指令文件未启用时不得进入系统提示"
    );
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(
        second.to_string().contains("配置后指令"),
        "保存配置后既有会话必须重新读取指令文件"
    );
    h.close().await;
}

/// Anthropic 协议下的缓存断点落位：静态前缀末块与系统边界各一个断点，尾部断点落在
/// 最后一条会进入历史的用户消息上，而本轮才出现的提醒块不得落位（它永远不命中）。
#[tokio::test]
async fn anthropic_requests_put_cache_breakpoints_on_static_prefix_and_stable_tail() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_raw(
                support::anthropic_text_stream("缓存冒烟完成"),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "anthropic_messages", "").await;
    // 计划模式：本轮带模式契约（尾部提醒），断点必须避开它。
    let id = h.session(SessionMode::Plan).await;
    h.prompt(&id, "缓存冒烟").await;

    let requests = upstream.received_requests().await.expect("请求记录");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("请求 JSON");

    let system = body["system"].as_array().expect("系统提示为内容块数组");
    let cached: Vec<usize> = system
        .iter()
        .enumerate()
        .filter(|(_, block)| block.get("cache_control").is_some())
        .map(|(index, _)| index)
        .collect();
    assert_eq!(
        cached,
        vec![2, system.len() - 1],
        "断点应落在静态前缀末块与系统边界块：{system:?}"
    );
    assert!(
        system[0]["text"]
            .as_str()
            .expect("首块文本")
            .starts_with("You are a precise software-engineering agent"),
        "{system:?}"
    );
    assert!(
        system[2]["text"]
            .as_str()
            .expect("静态末块")
            .contains("run_code"),
        "{system:?}"
    );
    assert!(
        system
            .last()
            .and_then(|block| block["text"].as_str())
            .expect("系统末块")
            .starts_with("<env>"),
        "{system:?}"
    );

    // 用户消息与尾部提醒在 Anthropic 上合并为同一条线上消息：断点必须落在用户
    // 这一块上，本轮才出现的提醒块不能落位。
    let messages = body["messages"].as_array().expect("线上消息");
    assert_eq!(messages.len(), 1, "{messages:?}");
    let content = messages[0]["content"].as_array().expect("消息内容块");
    assert_eq!(content.len(), 2, "{content:?}");
    assert!(
        content[0]["text"]
            .as_str()
            .expect("用户文本")
            .contains("缓存冒烟")
    );
    assert!(
        content[0].get("cache_control").is_some(),
        "尾断点落在最后一条会持久化的消息上"
    );
    assert!(
        content[1]["text"]
            .as_str()
            .expect("提醒文本")
            .contains("<system-reminder>")
    );
    assert!(
        content[1].get("cache_control").is_none(),
        "提醒不会进入历史，断点落在它上面永远不会命中"
    );
    h.close().await;
}

/// 指令角色切到 user：规定性片段作为首条 user 消息下发，描述性片段留在顶层 system；
/// 两条指令消息各带自己的缓存断点，正文与默认配置逐字相同。
#[tokio::test]
async fn user_role_instructions_ride_in_a_user_message_with_their_own_breakpoint() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_raw(
                support::anthropic_text_stream("切换完成"),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "anthropic_messages",
        "[providers.test.models.gpt-test.compat]\ninstruction_role='user'",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
    h.prompt(&id, "切换冒烟").await;

    let requests = upstream.received_requests().await.expect("请求记录");
    assert_eq!(requests.len(), 1);
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).expect("请求 JSON");

    // 顶层 system 只剩描述性片段：身份与环境；工具政策不得留在其中。
    let system = body["system"].as_array().expect("系统提示为内容块数组");
    assert!(
        system[0]["text"]
            .as_str()
            .expect("首块文本")
            .starts_with("You are a precise software-engineering agent"),
        "{system:?}"
    );
    assert!(
        system
            .last()
            .and_then(|block| block["text"].as_str())
            .expect("系统末块")
            .starts_with("<env>"),
        "{system:?}"
    );
    assert!(
        system
            .last()
            .expect("系统末块")
            .get("cache_control")
            .is_some(),
        "系统边界断点仍落在顶层 system 的末块上：{system:?}"
    );

    // 首条 user 消息承载工具政策与 PTC 运行时，静态前缀断点落在 PTC 块上；本轮的真实
    // 用户消息与它合并成同一条线上消息，并带走尾部断点。
    let messages = body["messages"].as_array().expect("线上消息");
    assert_eq!(messages.len(), 1, "{messages:?}");
    assert_eq!(messages[0]["role"], "user");
    let content = messages[0]["content"].as_array().expect("消息内容块");
    let texts: Vec<&str> = content
        .iter()
        .map(|block| block["text"].as_str().expect("文本块"))
        .collect();
    let policy = texts[0];
    assert!(
        policy.starts_with("Use read for files and directories"),
        "{policy}"
    );
    assert!(policy.contains("Do not use bash"), "{policy}");
    assert!(texts[1].contains("run_code"), "{texts:?}");
    assert!(
        content[1].get("cache_control").is_some(),
        "user 组静态前缀断点落在 PTC 块上：{content:?}"
    );
    let task = texts
        .iter()
        .position(|text| *text == "切换冒烟")
        .expect("本轮用户消息与指令合并成同一条线上消息");
    assert!(
        content[task].get("cache_control").is_some(),
        "尾断点落在最后一条会持久化的消息上：{content:?}"
    );
    for extra in &content[task + 1..] {
        assert!(
            extra.get("cache_control").is_none(),
            "本轮提醒不落断点：{content:?}"
        );
    }
    h.close().await;
}

use super::*;

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
                    "function": {"name": "plan", "arguments": plan_arguments},
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
        ptc_worker: None,
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
            engine: SessionEngine::Standard,
            plan_active: true,
            fast_mode: false,
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
            goal_round: None,
        })
        .await
        .expect("submit plan via model tool call");
    assert_eq!(submitted.finish_reason, moc_llm::FinishReason::Stop);
    assert!(
        submitted
            .message
            .tool_calls()
            .any(|call| call.name == "plan")
    );
    let notifications = updates.lock().clone();
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
            omit_auxiliary: false,
            session_id: session.id.clone(),
            max_entries: None,
            read_only: false,
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
    assert!(parent.session.plan_active);
    assert!(!plan.path.contains("/sessions/"));
    let approved = client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id,
            plan_id: plan.plan_id.clone(),
            action: PlanAction::ExecuteNewSession,
            execution_engine: None,
        })
        .await
        .expect("approve into new session");
    assert_eq!(approved.engine, SessionEngine::Standard);
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
            omit_auxiliary: false,
            session_id: approved.session_id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("load child session");
    assert_eq!(child.session.engine, SessionEngine::Standard);
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
            goal_round: None,
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
            .any(|tool| tool["function"]["name"] == "plan")
    );
    // 模式不裁剪声明面：写工具 schema 仍在表中，执行时才被 plan 模式拒绝。
    assert!(
        plan_tools
            .iter()
            .any(|tool| tool["function"]["name"] == "edit")
    );
    assert!(
        plan_tools
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

    use moc_core::hashline::{content_hash, tag_of, tag_text};
    use serde_json::{Value, json};

    let tag = tag_text(tag_of(content_hash(&[
        "old value".to_owned(),
        "second value".to_owned(),
    ])));
    let read_arguments = json!({"path": "target.txt"});
    let edit_arguments = json!({
        "path": "target.txt",
        "tag": tag,
        "edits": [{"op": "replace", "from": 1, "to": 2, "text": "new value"}],
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
        ptc_worker: None,
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
                text: "Read and update target.txt".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
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
            omit_auxiliary: false,
            session_id: session.id,
            max_entries: None,
            read_only: false,
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
    assert!(read_output.contains("1│old value") && read_output.contains(&format!("#{tag}]")));
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
            "function": {"arguments": "{\"description\":\"Print the name-first marker\",\"command\":\"printf name-first\"}"},
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
        ptc_worker: None,
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
                text: "运行 name-first".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
        })
        .await
        .expect("run prompt");
    assert_eq!(result.message.text(), "done");

    let updates = updates.lock().clone();
    // 前端只从 `tool_started` 拿参数：名字与 ID 分片到达的网关下，
    // 这里必须是真实参数，而不是空 ID 上攒下来的半截增量。
    let (name, arguments) = updates
        .iter()
        .find_map(|update| match update {
            SessionUpdate::ToolStarted {
                name, arguments, ..
            } => Some((name.clone(), arguments.clone())),
            _ => None,
        })
        .expect("收到 tool_started");
    assert_eq!(name, "bash");
    let arguments: serde_json::Value =
        serde_json::from_str(&arguments).expect("启动更新里的参数是合法 JSON");
    assert_eq!(arguments["command"], "printf name-first");

    let loaded = client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: session.id,
            max_entries: None,
            read_only: false,
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
    assert_eq!(
        persisted.arguments,
        "{\"description\":\"Print the name-first marker\",\"command\":\"printf name-first\"}"
    );

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
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
                    "plan",
                    json!({"title":"方案", "markdown":"# 方案", "extra":true}),
                ),
                2 => ("save", "plan", json!({"title":"方案", "markdown":"# 方案"})),
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
    let id = h.session_plan(SessionEngine::Standard).await;
    h.prompt(&id, "保存和审阅计划").await;
    let loaded = h.load(&id).await;
    assert!(loaded.session.plan_active);
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
    let id = h.session(SessionEngine::Standard).await;
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

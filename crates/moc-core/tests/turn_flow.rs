//! 固定上游响应下验证 core 的 JSON-RPC 轮次闭环。

use parking_lot::Mutex;
use std::sync::Arc;

use moc_core::{Core, CoreConfig};
use moc_llm::Client;
use moc_protocol::methods::{
    ClientCapabilities, ClientInfo, InputPart, PromptParams, SessionLoad, SessionLoadParams,
    SessionMode, SessionNew, SessionNewParams, SessionPrompt, SessionUpdate, SessionUpdateMethod,
};
use moc_protocol::{ClientBuilder, Server, ServerOptions, transport};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn plain_text_turn_persists_authoritative_message_over_rpc() {
    let upstream = MockServer::start().await;
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
            "[models.roles]\ndefault='test/model:off'\nsmol='test/model:off'\nslow='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\nlocking=false\n\
             [workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n\
             [limits]\nmax_steps_per_turn=4\n",
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
        })
        .await
        .expect("run model turn");

    assert_eq!(result.turn_id, "turn_1");
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
                SessionUpdate::TurnFinished { message, .. } => Some(message),
                _ => None,
            })
            .expect("turn_finished update");
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
async fn plan_submission_and_new_session_approval_preserve_one_plan_injection() {
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
                    "function": {"name": "submit_plan", "arguments": plan_arguments},
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
    let upstream = MockServer::start().await;
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
            "[models.roles]\ndefault='test/model:off'\nsmol='test/model:off'\nslow='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\nlocking=false\n\
             [workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n\
             [limits]\nmax_steps_per_turn=4\n",
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
        })
        .await
        .expect("submit plan via model tool call");
    assert_eq!(submitted.finish_reason, moc_llm::FinishReason::Stop);
    assert!(
        submitted
            .message
            .tool_calls()
            .any(|call| call.name == "submit_plan")
    );
    assert!(updates.lock().iter().any(|update| matches!(
        update,
        SessionUpdate::PlanSubmitted { title, .. } if title == "Build a small feature"
    )));

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
    assert!(std::path::Path::new(&plan.path).is_file());
    let approved = client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id,
            plan_id: plan.plan_id.clone(),
            action: PlanAction::ExecuteNewSession,
        })
        .await
        .expect("approve into new session");
    assert_eq!(approved.mode, SessionMode::Normal);
    assert_eq!(approved.auto_prompt.as_deref(), Some("开始执行计划。"));
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
            .any(|tool| tool["function"]["name"] == "submit_plan")
    );
    assert!(
        !plan_tools
            .iter()
            .any(|tool| matches!(tool["function"]["name"].as_str(), Some("edit" | "write")))
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
    let upstream = MockServer::start().await;
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
            "[models.roles]\ndefault='test/model:off'\nsmol='test/model:off'\nslow='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\nlocking=false\n\
             [workspace]\nwatch=false\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n\
             [limits]\nmax_steps_per_turn=4\n",
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
        })
        .await
        .expect("run read-edit-tool turn");

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
    let upstream = MockServer::start().await;
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
            "[models.roles]\ndefault='test/model:off'\nsmol='test/model:off'\nslow='test/model:off'\nplan='test/model:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\napi_key=''\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [session]\ndir={:?}\nlocking=false\n\
             [workspace]\nwatch=true\nmax_scan_entries=1000\n\
             [context_files]\nenabled=false\n\
             [compaction]\nenabled=false\n\
             [limits]\nmax_steps_per_turn=4\n",
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
        })
        .await
        .expect("run read-only plan turn");
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
    let text = reminder["content"][0]["text"]
        .as_str()
        .expect("cached reminder text block");
    assert!(text.contains("只读模式下修改了工作区"), "{text}");
    assert!(text.contains("violation.txt"), "{text}");

    core.shutdown().await;
    drop(client);
    drop(connection);
    drop(server);
}

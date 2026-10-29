//! 真实协议连接与固定本地上游下的中断、压缩和计划状态转移。
#[path = "support/steering.rs"]
mod steering;
mod support;

use moc_protocol::{FinishReason, Role, methods::*};
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{Harness, complete, text_stream, tool_stream};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn strict_chat_provider_omits_unsupported_prompt_cache_key() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body.get("prompt_cache_key").is_some() {
                return ResponseTemplate::new(400).set_body_json(json!({
                    "error": {"message": "未知请求字段：prompt_cache_key", "code": "UNKNOWN_FIELD"}
                }));
            }
            ResponseTemplate::new(200).set_body_raw(text_stream("请求已接受"), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat",
        "[compaction]\nenabled=false\n[providers.test.models.gpt-test.compat]\nsupports_prompt_cache_key=false").await;
    let id = h.session(SessionEngine::Standard).await;
    assert_eq!(h.prompt(&id, "执行").await.message.text(), "请求已接受");
    h.close().await;
}

#[tokio::test]
async fn all_assistant_reasoning_constraint_repairs_text_only_history() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("已记录"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat",
        "[compaction]\nenabled=false\n[providers.test.models.gpt-test.compat]\necho_reasoning='never'\nrequires_reasoning_content_for_all_assistant_turns=true").await;
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "第一轮").await;
    h.prompt(&id, "继续").await;
    let requests = upstream.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let assistant = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "assistant")
        .unwrap();
    assert_eq!(assistant["content"], "已记录");
    assert_eq!(assistant["reasoning_content"], "");
    assert!(
        body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|message| message["role"] == "user")
            .all(|message| message.get("reasoning_content").is_none())
    );
    h.close().await;
}

#[tokio::test]
async fn cancel_persists_partial_output_and_repairs_unexecuted_calls() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (release, gate) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 65536];
        let mut used = 0;
        loop {
            used += socket.read(&mut buffer[used..]).await.unwrap();
            if buffer[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let data = format!(
            "data: {}\n\n",
            json!({"id":"partial","model":"gpt-test","choices":[{
                "index":0,"delta":{"role":"assistant","content":"已收到部分正文","tool_calls":[{
                    "index":0,"id":"unfinished","type":"function","function":{"name":"read","arguments":"{\"path\":\"pending.txt\"}"}
                }]},"finish_reason":null
            }]})
        );
        socket
            .write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes())
            .await
            .unwrap();
        let _ = gate.await;
    });
    let h = Harness::new(&url, "openai_chat", "[compaction]\nenabled=false").await;
    let id = h.session(SessionEngine::Standard).await;
    let mut events = h.events.subscribe();
    let client = h.client.clone();
    let sid = id.clone();
    let prompt = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text {
                    text: "开始".into(),
                }],
                mode: None,
                agent_message: None,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            // 同一帧里的正文与工具调用由累加器一并消化，正文增量到达前端即代表
            // 这次工具调用已经进入待修复集合。
            if matches!(
                events.recv().await.unwrap(),
                SessionUpdate::AssistantDelta {
                    delta: AssistantDelta::Text { .. },
                    ..
                }
            ) {
                break;
            }
        }
    })
    .await
    .expect("上游数据已到达前端才发送取消");
    assert!(
        h.client
            .call::<SessionCancel>(SessionCancelParams {
                session_id: id.clone()
            })
            .await
            .unwrap()
            .cancelled
    );
    let result = prompt.await.unwrap().unwrap();
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert_eq!(result.message.text(), "已收到部分正文");
    let loaded = h.load(&id).await;
    // [0] 用户消息、[1] 首轮运行时提醒、[2] 被中断的助手消息、[3] 未执行调用的占位结果。
    assert_eq!(loaded.entries[2].message, result.message);
    let repair = loaded.entries[3].message.tool_results().next().unwrap();
    assert_eq!(repair.tool_call_id, "unfinished");
    assert!(repair.is_error);
    assert_eq!(
        loaded.entries[3].tool.as_ref().unwrap().status,
        ToolStatus::Cancelled
    );
    let _ = release.send(());
    upstream.await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn overflow_summarizes_the_old_prefix_and_retries_with_smaller_input() {
    let upstream = MockServer::builder().start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = counter.clone();
    Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(move |request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["stream"] != true { return ResponseTemplate::new(200).set_body_json(complete(&support::soft_checkpoint("保留用户纠正、文件路径与尚未完成的目标。"))); }
        match seen.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(200).set_body_raw(text_stream("已记录"), "text/event-stream"),
            1 => ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"maximum context length exceeded","code":"context_length_exceeded","type":"invalid_request_error"}})),
            _ => ResponseTemplate::new(200).set_body_raw(text_stream("恢复成功"), "text/event-stream"),
        }
    }).mount(&upstream).await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nkeep_recent_tokens=100\nthreshold={mode='tokens',value=900000}",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, &"重要的历史文件和用户纠正。".repeat(2000))
        .await;
    assert_eq!(h.prompt(&id, "继续实现").await.message.text(), "恢复成功");
    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 4);
    let failed: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let retried: Value = serde_json::from_slice(&requests[3].body).unwrap();
    assert!(
        requests[3].body.len() < requests[1].body.len() / 2,
        "压缩请求大小 {} -> {}",
        requests[1].body.len(),
        requests[3].body.len()
    );
    assert!(!retried.to_string().contains("重要的历史文件和用户纠正。"));
    assert!(retried.to_string().contains("compaction-summary"));
    assert!(retried.to_string().contains("继续实现"));
    assert_eq!(failed["messages"][0], retried["messages"][0]);
    let updates = h.updates.lock().clone();
    let started = updates
        .iter()
        .position(|update| matches!(update, SessionUpdate::ContextCompactionStarted))
        .expect("摘要压缩应先报开始");
    let compacted = updates
        .iter()
        .position(|update| {
            matches!(update, SessionUpdate::ContextCompacted { method, tokens_before, tokens_after, full } if method == "soft" && *full && tokens_before > tokens_after)
        })
        .expect("摘要压缩应报完成");
    assert!(started < compacted, "开始必须早于完成：{updates:?}");
    h.close().await;
}

#[tokio::test]
async fn fatal_upstream_error_emits_error_and_releases_session_lock() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(400).set_body_json(
            json!({"error":{"message":"bad request from fixture","type":"invalid_request_error"}}),
        ))
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    let result = h
        .client
        .call::<SessionPrompt>(PromptParams {
            session_id: id.clone(),
            input: vec![InputPart::Text {
                text: "失败请求".into(),
            }],
            mode: None,
            agent_message: None,
        })
        .await;
    assert!(result.is_err());
    assert!(h.updates.lock().iter().any(|update| matches!(update, SessionUpdate::Error { message, retryable: false, .. } if message.contains("bad request"))));
    // 失败请求不释放写租约；租约随会话卸载或进程退出释放，后续操作仍可继续。
    assert!(
        h.load(&id)
            .await
            .session
            .lock
            .as_ref()
            .is_some_and(|lock| lock.pid == std::process::id())
    );
    h.client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: id,
            active: true,
        })
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn tool_batch_preserves_order_and_disabled_tools_are_not_executed() {
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let response = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_stream(vec![
                    ("one", "read", json!({"path":"one.txt"})),
                    ("two", "read", json!({"path":"two.txt"})),
                    ("three", "todo", json!({"op":"init","items":["不应创建"]})),
                ])
            } else {
                text_stream("批次完成")
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false\n[tools.todo]\nenabled=false",
    )
    .await;
    std::fs::write(h.root.path().join("work/one.txt"), "第一文件").unwrap();
    std::fs::write(h.root.path().join("work/two.txt"), "第二文件").unwrap();
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "并发读两个文件").await;
    let batch = h
        .updates
        .lock()
        .iter()
        .find_map(|update| match update {
            SessionUpdate::ToolBatchStarted { calls, .. } => Some(calls.clone()),
            _ => None,
        })
        .expect("工具批次事件");
    assert_eq!(batch.len(), 3);
    assert_eq!(batch[0].name, "read");
    assert_eq!(batch[2].name, "todo");
    let loaded = h.load(&id).await;
    let results: Vec<_> = loaded
        .entries
        .iter()
        .filter(|entry| entry.message.role == Role::Tool)
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    assert_eq!(
        results
            .iter()
            .map(|result| result.tool_call_id.as_str())
            .collect::<Vec<_>>(),
        ["one", "two", "three"]
    );
    assert!(results[0].content.contains("第一文件"));
    assert!(results[1].content.contains("第二文件"));
    assert!(results[2].is_error);
    assert!(loaded.todos.is_empty());
    let requests = upstream.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let ids: Vec<_> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|message| message["tool_call_id"].as_str())
        .collect();
    assert_eq!(ids, ["one", "two", "three"]);
    assert!(
        !body["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|tool| tool["function"]["name"] == "todo")
    );
    h.close().await;
}

#[tokio::test]
async fn execute_compacted_and_reject_keep_plan_state_consistent() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] != true {
                return ResponseTemplate::new(200).set_body_json(complete(
                    &support::soft_checkpoint("计划和关键决策：保留接口。"),
                ));
            }
            ResponseTemplate::new(200).set_body_raw(
                tool_stream(vec![
                    (
                        "plan",
                        "plan",
                        json!({"title":"实施计划","markdown":"# 保留接口\n1. 完成实现"}),
                    ),
                    ("exit", "exit_plan", json!({})),
                ]),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']",
    )
    .await;
    for action in [
        PlanAction::Execute,
        PlanAction::ExecuteCompacted,
        PlanAction::Reject,
    ] {
        let id = h.session_plan(SessionEngine::Standard).await;
        h.prompt(&id, "提交计划").await;
        let plan = h.load(&id).await.plans.pop().unwrap();
        let result = h
            .client
            .call::<SessionPlanApprove>(SessionPlanApproveParams {
                session_id: id.clone(),
                plan_id: plan.plan_id,
                action,
                execution_model: None,
                execution_engine: None,
            })
            .await
            .unwrap();
        let loaded = h.load(&id).await;
        if action == PlanAction::Reject {
            assert_eq!(loaded.plans[0].status, PlanStatus::Rejected);
            assert!(result.plan_active);
            assert!(result.auto_prompt.is_none());
        } else {
            assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
            assert_eq!(loaded.session.engine, SessionEngine::Standard);
            assert_eq!(
                result.auto_prompt.as_deref(),
                Some("Execute the approved plan.")
            );
        }
    }
    upstream.reset().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).expect("请求 JSON");
            if body["stream"] != true {
                ResponseTemplate::new(500)
                    .set_body_json(json!({"error": {"message": "upstream down"}}))
            } else {
                ResponseTemplate::new(200).set_body_raw(
                    tool_stream(vec![
                        (
                            "save",
                            "plan",
                            json!({"title":"失败后重试", "markdown":"# 保留计划"}),
                        ),
                        ("exit", "exit_plan", json!({})),
                    ]),
                    "text/event-stream",
                )
            }
        })
        .mount(&upstream)
        .await;
    let id = h.session_plan(SessionEngine::Standard).await;
    h.prompt(&id, "保存计划").await;
    let before = h.load(&id).await;
    let error = h
        .client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: id.clone(),
            plan_id: before.plans[0].plan_id.clone(),
            action: PlanAction::ExecuteCompacted,
            execution_model: None,
            execution_engine: None,
        })
        .await;
    assert!(error.is_err(), "空摘要不能批准计划");
    let after = h.load(&id).await;
    assert_eq!(after.plans[0].status, PlanStatus::Submitted);
    assert_eq!(after.session.engine, before.session.engine);
    assert_eq!(after.session.model, before.session.model);
    h.close().await;
}

#[tokio::test]
async fn disconnected_frontend_persists_partial_text_before_connection_finishes() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (release, gate) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut buffer = vec![0; 65536];
        let mut used = 0;
        loop {
            used += socket.read(&mut buffer[used..]).await.unwrap();
            if buffer[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
                break;
            }
        }
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n").await.unwrap();
        let data = format!(
            "data: {}\n\n",
            json!({"id":"partial","choices":[{"index":0,"delta":{"role":"assistant","content":"断线前正文"},"finish_reason":null}]})
        );
        socket
            .write_all(format!("{:x}\r\n{data}\r\n", data.len()).as_bytes())
            .await
            .unwrap();
        let _ = gate.await;
    });
    let h = Harness::new(&url, "openai_chat", "[compaction]\nenabled=false").await;
    let id = h.session(SessionEngine::Standard).await;
    let mut events = h.events.subscribe();
    let client = h.client.clone();
    let sid = id.clone();
    let pending = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text {
                    text: "开始".into(),
                }],
                mode: None,
                agent_message: None,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !matches!(
            events.recv().await.unwrap(),
            SessionUpdate::AssistantDelta {
                delta: AssistantDelta::Text { .. },
                ..
            }
        ) {}
    })
    .await
    .unwrap();
    h.client.close().await;
    assert!(pending.await.unwrap().is_err());
    h.connection.wait().await;
    h.core.shutdown().await;
    let store = moc_core::session::store::SessionStore::new(h.root.path().join("sessions"));
    let paths = store.find(&id).await.unwrap().unwrap();
    let entries = store.read_entries(&paths).await.unwrap();
    assert!(entries.iter().any(|entry| matches!(entry, moc_core::session::entries::Entry::Message { message, finish_reason: Some(FinishReason::Cancelled), .. } if message.text() == "断线前正文")));
    assert!(
        paths.lock_file(&id).exists(),
        "锁文件持久保留，持有状态由内核锁判定"
    );
    let _ = release.send(());
    upstream.await.unwrap();
    h.core.shutdown().await;
}

#[tokio::test]
async fn shell_jobs_stdin_and_pty_run_real_processes_and_reap_them() {
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let response = match seen.fetch_add(1, Ordering::SeqCst) {
                0 => tool_stream(vec![(
                    "background",
                    "bash",
                    json!({"description":"Hold a background shell open","command":"printf '%s' $$ > bg.pid; exec sleep 300","async":true}),
                )]),
                1 => {
                    let result = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|message| message["tool_call_id"] == "background")
                        .unwrap();
                    let content = support::content::content_text(&result["content"]);
                    let id = content
                        .split_once(" job ")
                        .expect("后台任务启动消息")
                        .1
                        .split_whitespace()
                        .next()
                        .unwrap();
                    tool_stream(vec![
                        ("kill", "job", json!({"op":"kill","job_id":id})),
                        (
                            "stdin",
                            "bash",
                            json!({"description":"Count stdin bytes","command":"wc -c","stdin":"a".repeat(200000)}),
                        ),
                        (
                            "pty",
                            "bash",
                            json!({"description":"Print the PTY marker","command":"printf 'terminal-ok'","pty":true,"timeout":5}),
                        ),
                    ])
                }
                _ => text_stream("完成"),
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
    h.prompt(&id, "执行任务").await;
    let loaded = h.load(&id).await;
    let results: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    assert!(results.iter().all(|result| !result.is_error), "{results:?}");
    assert!(
        results
            .iter()
            .find(|r| r.tool_call_id == "stdin")
            .unwrap()
            .content
            .contains("200000")
    );
    assert!(
        results
            .iter()
            .find(|r| r.tool_call_id == "pty")
            .unwrap()
            .content
            .contains("terminal-ok")
    );
    assert!(
        results
            .iter()
            .find(|r| r.tool_call_id == "kill")
            .unwrap()
            .content
            .contains("Killed")
    );
    h.close().await;
}

#[tokio::test]
async fn todo_rejects_duplicates_atomically_and_blocks_without_a_reason() {
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let response = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_stream(vec![
                    (
                        "init",
                        "todo",
                        json!({"op":"init","items":["first","second"]}),
                    ),
                    (
                        "bad",
                        "todo",
                        json!({"op":"append","phase":"任务","items":["first","third"]}),
                    ),
                    ("block", "todo", json!({"op":"block","task":"first"})),
                ])
            } else {
                text_stream("完成")
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
    h.prompt(&id, "管理任务").await;
    let loaded = h.load(&id).await;
    assert_eq!(loaded.todos[0].items.len(), 2);
    assert_eq!(loaded.todos[0].items[0].status, TodoStatus::Blocked);
    assert_eq!(loaded.todos[0].items[1].status, TodoStatus::InProgress);
    assert!(
        loaded
            .entries
            .iter()
            .flat_map(|e| e.message.tool_results())
            .find(|r| r.tool_call_id == "bad")
            .unwrap()
            .is_error
    );
    h.close().await;
}

fn responses_stream(text: &str) -> String {
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type":"response.created","response":{"id":"resp_test","model":"gpt-test"}}),
        json!({"type":"response.output_text.delta","output_index":0,"delta":text}),
        json!({"type":"response.completed","response":{"status":"completed","output":[{"type":"message"}]}})
    )
}

#[tokio::test]
async fn remote_compaction_replays_native_items_and_retained_messages_once() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(responses_stream("远程回复"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let mut output =
                vec![json!({"type":"compaction","id":"cmp_test","encrypted_content":"opaque"})];
            output.extend(
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["role"] != "system")
                    .cloned(),
            );
            ResponseTemplate::new(200).set_body_json(json!({"output":output}))
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_responses",
        "[compaction]\nmethod_order=['remote']\nthreshold={mode='tokens',value=900000}",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "历史消息只出现一次").await;
    let compacted = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "remote");
    h.client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    h.prompt(&id, "恢复并继续").await;
    let requests = upstream.received_requests().await.unwrap();
    let replay: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert_eq!(replay["input"][0]["encrypted_content"], "opaque");
    assert_eq!(
        replay["input"]
            .to_string()
            .matches("历史消息只出现一次")
            .count(),
        1
    );
    assert_eq!(replay["input"].to_string().matches("远程回复").count(), 1);
    assert!(replay["input"].to_string().contains("恢复并继续"));
    h.close().await;
}

#[tokio::test]
async fn unsupported_server_compaction_is_disabled_for_the_session() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(|request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body.get("context_management").is_some() {
            ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"unknown parameter context_management","type":"invalid_request_error"}}))
        } else { ResponseTemplate::new(200).set_body_raw(responses_stream("兼容成功"), "text/event-stream") }
    }).mount(&upstream).await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_responses",
        "[compaction]\nthreshold={mode='tokens',value=900000}",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    assert_eq!(h.prompt(&id, "第一次").await.message.text(), "兼容成功");
    h.prompt(&id, "第二次").await;
    let bodies: Vec<Value> = upstream
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 3);
    assert!(bodies[0].get("context_management").is_some());
    assert!(
        bodies[1..]
            .iter()
            .all(|body| body.get("context_management").is_none())
    );
    h.close().await;
}

#[tokio::test]
async fn disabled_auto_threshold_skips_automatic_and_server_side_compaction() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert!(
                body.get("context_management").is_none(),
                "auto=false 不得注入服务端压缩阈值"
            );
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(responses_stream("自动已关闭"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(json!({
                    "id": "resp_c",
                    "model": "gpt-test",
                    "status": "completed",
                    "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "checkpoint"}]}],
                    "usage": {"input_tokens": 10, "output_tokens": 2},
                }))
            }
        })
        .mount(&upstream)
        .await;
    // 一 token 阈值确保 auto=false 确实阻止按阈值压缩。
    let h = Harness::new(
        &upstream.uri(),
        "openai_responses",
        "[compaction]\nmethod_order=['soft']\nthreshold={mode='tokens',value=1}\nauto=false",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    let result = h.prompt(&id, "auto 关闭后不自动压缩").await;
    assert_eq!(result.finish_reason, FinishReason::Stop);
    assert!(
        !h.updates
            .lock()
            .iter()
            .any(|event| { matches!(event, SessionUpdate::ContextCompacted { .. }) })
    );
    let compacted = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert_eq!(compacted.method, "soft");
    h.close().await;
}

#[tokio::test]
async fn shake_shrinks_repeated_reads_and_keeps_tool_history_replayable() {
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let step = seen.fetch_add(1, Ordering::SeqCst);
            let response = if step.is_multiple_of(2) {
                tool_stream(vec![(
                    &format!("read{step}"),
                    "read",
                    json!({"path":"large.txt"}),
                )])
            } else {
                text_stream("已读")
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "[compaction]\nmethod_order=['shake']\nprotect_recent_tool_tokens=0\nthreshold={mode='tokens',value=900000}\n[compaction.shake]\nkeep_last_prompts=0\nsoft_trim_chars=64").await;
    std::fs::write(
        h.root.path().join("work/large.txt"),
        format!("{}\n", "内容".repeat(250)).repeat(10),
    )
    .unwrap();
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "第一次读取").await;
    h.prompt(&id, "再次读取").await;
    let compacted = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap();
    assert!(compacted.tokens_after < compacted.tokens_before / 2);
    let updates = h.updates.lock().clone();
    assert!(
        updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ContextCompacted { tokens_before, tokens_after, full: false, .. }
                if tokens_before > tokens_after
        )),
        "本地裁剪应标记为非全量压缩：{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompactionStarted)),
        "本地裁剪瞬时可成，不应出现等待模型中的提示：{updates:?}"
    );
    let loaded = h.load(&id).await;
    let outputs: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].content, "[superseded by a later read]");
    assert_eq!(outputs[1].content, "[tool output pruned]");
    h.prompt(&id, "验证回放").await;
    h.close().await;
}

#[tokio::test]
async fn background_pipes_and_pty_enforce_deadlines_and_reap_killed_jobs() {
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST")).respond_with(move |_: &wiremock::Request| {
        let response = match count.fetch_add(1, Ordering::SeqCst) {
            0 => tool_stream(vec![
                ("pipe_start", "bash", json!({"description":"Start a piped job","command":"printf pipe-ready; exec sleep 60","async":true,"timeout":1})),
                ("pty_start", "bash", json!({"description":"Start a PTY job","command":"printf pty-ready; exec sleep 60","async":true,"pty":true,"timeout":1})),
            ]),
            1 => tool_stream(vec![
                ("pipe_wait", "job", json!({"op":"wait","job_id":"job_1","timeout_secs":5})),
                ("pty_wait", "job", json!({"op":"wait","job_id":"job_2","timeout_secs":5})),
            ]),
            2 => tool_stream(vec![("pty_again", "bash", json!({"description":"Restart the PTY job","command":"exec sleep 60","async":true,"pty":true,"lifecycle":"persistent"}))]),
            3 => tool_stream(vec![("pty_kill", "job", json!({"op":"kill","job_id":"job_3"}))]),
            5 => tool_stream(vec![("after_resume", "bash", json!({"description":"Print the resume marker","command":"printf resumed"}))]),
            _ => text_stream("已回收"),
        };
        ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
    }).mount(&upstream).await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        h.prompt(&id, "执行后台生命周期验收"),
    )
    .await
    .unwrap();
    let loaded = h.load(&id).await;
    let results: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    let output = |id: &str| {
        results
            .iter()
            .find(|result| result.tool_call_id == id)
            .unwrap()
            .content
            .as_str()
    };
    assert!(output("pipe_wait").contains("TimedOut\njob finished\npipe-ready"));
    assert!(output("pty_wait").contains("TimedOut"));
    assert!(output("pty_wait").contains("pty-ready"));
    assert_eq!(output("pty_kill"), "Killed");
    assert!(results.iter().all(|result| !result.is_error));
    let original_uri = output("pipe_start")
        .split("log: ")
        .nth(1)
        .expect("job log URI");
    let file_name = original_uri
        .strip_prefix("artifact://jobs/")
        .expect("job log URI 形状");
    let original_log = h
        .root
        .path()
        .join("sessions")
        .read_dir()
        .expect("sessions")
        .filter_map(Result::ok)
        .find(|entry| entry.path().join(&id).is_dir())
        .expect("session sidecar")
        .path()
        .join(&id)
        .join("jobs")
        .join(file_name);
    let before = std::fs::read(&original_log).unwrap();
    h.client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    h.prompt(&id, "恢复后执行新命令").await;
    assert_eq!(std::fs::read(&original_log).unwrap(), before);
    assert_eq!(
        std::fs::read_to_string(original_log.with_file_name("job_4.log")).unwrap(),
        "resumed"
    );
    h.close().await;
}

#[cfg(unix)]
#[tokio::test]
async fn oversized_read_line_does_not_block_later_anchors_or_replace_symlinks() {
    let upstream = MockServer::builder().start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let response = match count.fetch_add(1, Ordering::SeqCst) {
                0 => tool_stream(vec![("read_link", "read", json!({"path":"alias.txt"}))]),
                1 => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let result = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|message| message["tool_call_id"] == "read_link")
                        .unwrap();
                    let output = support::content::content_text(&result["content"]);
                    assert!(output.contains("content omitted"));
                    let anchor = output
                        .lines()
                        .find_map(|line| line.strip_suffix("│short"))
                        .unwrap();
                    tool_stream(vec![(
                        "edit_link",
                        "edit",
                        json!({"path":"alias.txt","edits":[[anchor,anchor,"inserted"]]}),
                    )])
                }
                _ => text_stream("已编辑真实目标"),
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
    let target = h.root.path().join("work/target.txt");
    let link = h.root.path().join("work/alias.txt");
    let prefix = "x".repeat(60_000);
    std::fs::write(&target, format!("{prefix}\nshort\nlast\n")).unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "读取并编辑符号链接").await;
    assert!(link.is_symlink());
    assert_eq!(
        std::fs::read_to_string(target).unwrap(),
        format!("{prefix}\nshort\ninserted\nlast\n")
    );
    assert!(
        h.load(&id)
            .await
            .entries
            .iter()
            .flat_map(|entry| entry.message.tool_results())
            .all(|result| !result.is_error)
    );
    h.close().await;
}

#[tokio::test]
async fn disabled_auto_continue_finishes_after_compaction_without_a_stream_request() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_ne!(body["stream"], true);
            ResponseTemplate::new(200)
                .set_body_json(complete(&support::soft_checkpoint("checkpoint")))
        })
        .expect(1)
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nthreshold={mode='tokens',value=1}\nauto_continue=false",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    let result = h.prompt(&id, "保留用户任务，压缩后等待下一次输入").await;
    assert_eq!(
        result.finish_reason,
        FinishReason::Other("compacted".into())
    );
    assert!(h.updates.lock().iter().any(
        |event| matches!(event, SessionUpdate::ContextCompacted { method, .. } if method == "soft")
    ));
    h.close().await;
}

#[tokio::test]
async fn cancellation_during_compaction_finishes_without_waiting_for_the_upstream() {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (entered, waiting) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut length = None;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = Some(value.trim().parse::<usize>().unwrap());
            }
        }
        let mut body = vec![0; length.unwrap()];
        reader.read_exact(&mut body).await.unwrap();
        let request: Value = serde_json::from_slice(&body).unwrap();
        assert_ne!(request["stream"], true);
        assert!(
            request["messages"]
                .as_array()
                .unwrap()
                .iter()
                .any(|message| {
                    message["content"].as_str().is_some_and(|content| {
                        content.contains("You are compressing an agent conversation")
                    })
                })
        );
        entered.send(()).unwrap();
        let _ = gate.await;
    });
    let h = Harness::new(
        &url,
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nthreshold={mode='tokens',value=1}",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    let client = h.client.clone();
    let sid = id.clone();
    let prompt = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text {
                    text: "压缩时取消".into(),
                }],
                mode: None,
                agent_message: None,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .unwrap()
        .unwrap();
    assert!(
        h.client
            .call::<SessionCancel>(SessionCancelParams {
                session_id: id.clone()
            })
            .await
            .unwrap()
            .cancelled
    );
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), prompt)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    let loaded = h.load(&id).await;
    assert_eq!(loaded.entries[0].message.text(), "压缩时取消");
    // 取消不释放写租约。
    assert!(
        loaded
            .session
            .lock
            .as_ref()
            .is_some_and(|lock| lock.pid == std::process::id())
    );
    release.send(()).unwrap();
    upstream.await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn failed_manual_compaction_preserves_upstream_error_and_history() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("已记录"), "text/event-stream")
            } else {
                ResponseTemplate::new(503).set_body_json(json!({
                    "error": {"message": "压缩模型拒绝输入", "code": "invalid_request"}
                }))
            }
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "保留这个用户请求").await;
    let before = h.load(&id).await;
    let error = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("压缩模型拒绝输入"));
    let updates = h.updates.lock().clone();
    let started = updates
        .iter()
        .position(|update| matches!(update, SessionUpdate::ContextCompactionStarted))
        .expect("压缩开始事件");
    let failure = updates
        .iter()
        .position(|update| matches!(update, SessionUpdate::ContextCompactionFailed { .. }))
        .expect("压缩失败事件");
    assert!(started < failure, "开始必须早于失败：{updates:?}");
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompacted { .. })),
        "失败时不能报完成：{updates:?}"
    );
    let after = h.load(&id).await;
    assert_eq!(after.entries, before.entries);
    // 压缩失败同样不释放写租约。
    assert!(
        after
            .session
            .lock
            .as_ref()
            .is_some_and(|lock| lock.pid == std::process::id())
    );
    h.close().await;
}

/// 建立一个含可剪枝 `read` 工具结果的会话；`extra` 追加压缩配置。
async fn prunable_session(extra: &str, summary: &str) -> (Harness, String) {
    let workspace = tempfile::tempdir().expect("临时目录");
    let long_file = workspace.path().join("long.txt");
    let content = format!("{}\n{}", support::PRUNE_MARKER, "长输出".repeat(800));
    std::fs::write(&long_file, &content).expect("写长文件");
    let upstream = MockServer::builder().start().await;
    support::mount_read_round(&upstream, &long_file, summary).await;
    let h = Harness::new(&upstream.uri(), "openai_chat", extra).await;
    let id = h.session(SessionEngine::Standard).await;
    let loaded = h.prompt(&id, "读取长文件").await;
    assert_eq!(loaded.message.text(), "已读取");
    let texts = support::tool_result_texts(&h.load(&id).await);
    assert!(
        texts
            .iter()
            .any(|text| text.contains(support::PRUNE_MARKER)),
        "会话必须先落下可剪枝的工具结果"
    );
    (h, id)
}

const SHAKE_ONLY: &str = "[compaction]\nmethod_order=['shake']\nthreshold={mode='tokens',value=900000}\n\
    protect_recent_tool_tokens=0\n[compaction.shake]\nsoft_trim_chars=100\nhead_chars=10\ntail_chars=10\n\
    keep_last_prompts=0\n";

#[tokio::test]
async fn shake_archives_the_full_pre_compaction_history() {
    let (h, id) = prunable_session(SHAKE_ONLY, "压缩摘要").await;
    let compacted = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .expect("本地剪枝压缩");
    assert_eq!(compacted.method, "shake");

    // 整表 rewrite 之后会话里只剩剪枝结果。
    let texts = support::tool_result_texts(&h.load(&id).await);
    assert!(
        texts
            .iter()
            .all(|text| !text.contains(support::PRUNE_MARKER)),
        "压缩后 transcript 不应保留原始长输出"
    );
    assert!(
        texts
            .iter()
            .any(|text| text.contains("[tool output pruned]")),
        "shake 必须真的剪枝"
    );

    // 归档承诺给模型的是完整压缩前历史，不能是剪枝后的内容。
    let store = moc_core::session::store::SessionStore::new(h.root.path().join("sessions"));
    let paths = store.find(&id).await.unwrap().unwrap();
    let mut segments: Vec<_> = std::fs::read_dir(paths.compaction())
        .expect("归档目录")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "md"))
        .collect();
    assert!(!segments.is_empty(), "压缩必须写出历史归档");
    segments.sort();
    let mut archive = String::new();
    for segment in segments {
        archive.push_str(&std::fs::read_to_string(segment).expect("读取归档段"));
    }
    assert!(
        archive.contains(support::PRUNE_MARKER),
        "归档必须包含压缩前的完整工具输出"
    );
    assert!(
        !archive.contains("[tool output pruned]"),
        "归档不得记录剪枝后的占位内容"
    );
    h.close().await;
}

/// 裁剪没腾出空间时会继续走摘要；整条链只报一个终态，且按全量压缩呈现。
#[tokio::test]
async fn shake_without_headroom_rolls_into_soft_and_reports_one_terminal_event() {
    let workspace = tempfile::tempdir().expect("临时目录");
    let long_file = workspace.path().join("long.txt");
    let content = format!("{}\n{}", support::PRUNE_MARKER, "长输出".repeat(800));
    std::fs::write(&long_file, &content).expect("写长文件");
    let upstream = MockServer::builder().start().await;
    support::mount_read_round(&upstream, &long_file, &support::soft_checkpoint("压缩摘要")).await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nauto=false\nmethod_order=['shake','soft']\n\
         threshold={mode='tokens',value=1}\nprotect_recent_tool_tokens=0\n\
         [compaction.shake]\nkeep_last_prompts=0\nsoft_trim_chars=100\nhead_chars=10\ntail_chars=10",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    let loaded = h.prompt(&id, "读取长文件").await;
    assert_eq!(loaded.message.text(), "已读取");

    let compacted = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .expect("先裁剪后摘要");
    assert_eq!(compacted.method, "soft", "裁剪没腾出空间时应继续摘要");
    assert!(compacted.full);

    let updates = h.updates.lock().clone();
    let completed: Vec<_> = updates
        .iter()
        .filter_map(|update| match update {
            SessionUpdate::ContextCompacted { method, full, .. } => Some((method.as_str(), *full)),
            _ => None,
        })
        .collect();
    assert_eq!(
        completed,
        vec![("soft", true)],
        "一次压缩只报一个终态：{updates:?}"
    );
    assert_eq!(
        updates
            .iter()
            .filter(|update| matches!(update, SessionUpdate::ContextCompactionStarted))
            .count(),
        1,
        "开始事件只发一次：{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompactionFailed { .. })),
        "成功的压缩不应报失败：{updates:?}"
    );
    h.close().await;
}

/// 响应协议下的「工具调用 → 收尾」流。
fn responses_tool_stream(call_id: &str, name: &str, arguments: &Value) -> String {
    let arguments = arguments.to_string();
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type":"response.created","response":{"id":"resp_tool","model":"gpt-test"}}),
        json!({"type":"response.output_item.added","output_index":0,
               "item":{"type":"function_call","id":"fc_read","call_id":call_id,"name":name}}),
        json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":arguments}),
        json!({"type":"response.completed","response":{"id":"resp_tool","status":"completed",
               "output":[{"type":"function_call","call_id":call_id}],
               "usage":{"input_tokens":5,"output_tokens":1}}})
    )
}

/// shake 复用远端 checkpoint 时必须保留 `remote` 方法名，否则换模型不会清理原生载荷。
#[tokio::test]
async fn shake_keeps_the_reused_remote_checkpoint_provenance() {
    let workspace = tempfile::tempdir().expect("临时目录");
    let long_file = workspace.path().join("long.txt");
    std::fs::write(
        &long_file,
        format!("{}\n{}", support::PRUNE_MARKER, "长输出".repeat(800)),
    )
    .expect("写长文件");
    let upstream = MockServer::builder().start().await;
    let read_path = long_file.to_string_lossy().into_owned();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/v1/responses"))
        .respond_with(move |_: &wiremock::Request| {
            let response = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                responses_tool_stream("read_long", "read", &json!({"path": read_path.clone()}))
            } else {
                responses_stream("已读取")
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let mut output =
                vec![json!({"type":"compaction","id":"cmp_remote","encrypted_content":"opaque"})];
            output.extend(
                body["input"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|item| item["role"] != "system")
                    .cloned(),
            );
            ResponseTemplate::new(200).set_body_json(json!({"output":output}))
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_responses",
        "[compaction]\nmethod_order=['remote','shake']\nthreshold={mode='tokens',value=900000}\n\
         protect_recent_tool_tokens=0\n[compaction.shake]\nsoft_trim_chars=100\nhead_chars=10\n\
         tail_chars=10\nkeep_last_prompts=0\n",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "读取长文件").await;
    let remote = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .expect("远端压缩");
    assert_eq!(remote.method, "remote");

    // 远端端点随后失败：压缩链落到 shake，并复用远端 checkpoint 的摘要与原生载荷。
    upstream.reset().await;
    Mock::given(method("POST"))
        .and(path("/v1/responses/compact"))
        .respond_with(
            ResponseTemplate::new(500).set_body_json(json!({"error":{"message":"远端压缩不可用"}})),
        )
        .mount(&upstream)
        .await;
    let shaken = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .expect("shake 接管压缩");
    assert_eq!(
        shaken.method, "remote",
        "复用远端载荷的 checkpoint 不得改写成 shake"
    );
    let entries = support::transcript_entries(&h, &id).await;
    let methods: Vec<String> = entries
        .iter()
        .filter_map(|entry| match entry {
            moc_core::session::entries::Entry::Compaction { method, .. } => Some(method.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(methods, ["remote", "remote"]);
    let texts = support::tool_result_texts(&h.load(&id).await);
    assert!(
        texts
            .iter()
            .any(|text| text.contains("[tool output pruned]")),
        "shake 必须真的剪枝：{texts:?}"
    );
    h.close().await;
}

#[tokio::test]
async fn compaction_failure_keeps_entries_appended_by_the_failed_method() {
    let (h, id) = prunable_session(
        "[compaction]\nmethod_order=['soft','shake']\nthreshold={mode='tokens',value=900000}\n\
         protect_recent_tool_tokens=0\n[compaction.shake]\nsoft_trim_chars=100\nhead_chars=10\n\
         tail_chars=10\nkeep_last_prompts=0\n",
        "",
    )
    .await;
    // soft 以空摘要失败，但已经把本次调用计入 usage；随后的 shake 整表 rewrite 不得丢掉它。
    let compacted = h
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: id.clone(),
            instructions: None,
        })
        .await
        .expect("失败后由 shake 接管");
    assert_eq!(compacted.method, "shake");
    let usage = h
        .client
        .call::<SessionUsage>(SessionUsageParams {
            session_id: id.clone(),
        })
        .await
        .expect("会话用量");
    // soft 的空摘要失败发生在记录用量之后：整表 rewrite 不得丢掉期间追加的用量 entry。
    assert!(
        usage.total.calls >= 2,
        "整表 rewrite 不得减少已落盘的用量 entry：{}",
        usage.total.calls
    );
    let disk = support::transcript_entries(&h, &id)
        .await
        .iter()
        .filter(|entry| matches!(entry, moc_core::session::entries::Entry::Usage { .. }))
        .count();
    assert_eq!(
        disk, usage.total.calls as usize,
        "磁盘与内存的用量记录必须一致"
    );
    h.close().await;
}

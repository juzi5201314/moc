//! 真实协议连接与固定本地上游下的中断、压缩和计划状态转移。
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
    let id = h.session(SessionMode::Normal).await;
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
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                SessionUpdate::AssistantDelta {
                    delta: AssistantDelta::ToolCallArgs { .. },
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
    assert_eq!(loaded.entries[1].message, result.message);
    let repair = loaded.entries[2].message.tool_results().next().unwrap();
    assert_eq!(repair.tool_call_id, "unfinished");
    assert!(repair.is_error);
    assert_eq!(
        loaded.entries[2].tool.as_ref().unwrap().status,
        ToolStatus::Cancelled
    );
    let _ = release.send(());
    upstream.await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn overflow_summarizes_the_old_prefix_and_retries_with_smaller_input() {
    let upstream = MockServer::start().await;
    let counter = Arc::new(AtomicUsize::new(0));
    let seen = counter.clone();
    Mock::given(method("POST")).and(path("/v1/chat/completions")).respond_with(move |request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body["stream"] != true { return ResponseTemplate::new(200).set_body_json(complete("保留用户纠正、文件路径与尚未完成的目标。")); }
        match seen.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(200).set_body_raw(text_stream("已记录"), "text/event-stream"),
            1 => ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"maximum context length exceeded","code":"context_length_exceeded","type":"invalid_request_error"}})),
            _ => ResponseTemplate::new(200).set_body_raw(text_stream("恢复成功"), "text/event-stream"),
        }
    }).mount(&upstream).await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nkeep_recent_tokens=100\nthreshold_tokens=900000",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
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
    assert!(h.updates.lock().iter().any(|update| matches!(update, SessionUpdate::ContextCompacted { method, tokens_before, tokens_after } if method == "soft" && tokens_before > tokens_after)));
    h.close().await;
}

#[tokio::test]
async fn fatal_upstream_error_emits_error_and_releases_session_lock() {
    let upstream = MockServer::start().await;
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
    let id = h.session(SessionMode::Normal).await;
    let result = h
        .client
        .call::<SessionPrompt>(PromptParams {
            session_id: id.clone(),
            input: vec![InputPart::Text {
                text: "失败请求".into(),
            }],
        })
        .await;
    assert!(result.is_err());
    assert!(h.updates.lock().iter().any(|update| matches!(update, SessionUpdate::Error { message, retryable: false, .. } if message.contains("bad request"))));
    assert!(h.load(&id).await.session.lock.is_none());
    h.client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: id,
            mode: SessionMode::Ask,
        })
        .await
        .unwrap();
    h.close().await;
}

#[tokio::test]
async fn tool_batch_preserves_order_and_disabled_tools_are_not_executed() {
    let upstream = MockServer::start().await;
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
    let id = h.session(SessionMode::Normal).await;
    h.prompt(&id, "并发读两个文件").await;
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
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] != true {
                return ResponseTemplate::new(200)
                    .set_body_json(complete("计划和关键决策：保留接口。"));
            }
            ResponseTemplate::new(200).set_body_raw(
                tool_stream(vec![(
                    "plan",
                    "submit_plan",
                    json!({"title":"实施计划","markdown":"# 保留接口\n1. 完成实现"}),
                )]),
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
        let id = h.session(SessionMode::Plan).await;
        h.prompt(&id, "提交计划").await;
        let plan = h.load(&id).await.plans.pop().unwrap();
        let result = h
            .client
            .call::<SessionPlanApprove>(SessionPlanApproveParams {
                session_id: id.clone(),
                plan_id: plan.plan_id,
                action,
            })
            .await
            .unwrap();
        let loaded = h.load(&id).await;
        if action == PlanAction::Reject {
            assert_eq!(loaded.plans[0].status, PlanStatus::Rejected);
            assert_eq!(result.mode, SessionMode::Plan);
            assert!(result.auto_prompt.is_none());
        } else {
            assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
            assert_eq!(loaded.session.mode, SessionMode::Normal);
            assert_eq!(result.auto_prompt.as_deref(), Some("开始执行计划。"));
        }
    }
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
    let id = h.session(SessionMode::Normal).await;
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
    assert!(!paths.lock_file(&id).exists());
    let _ = release.send(());
    upstream.await.unwrap();
    h.core.shutdown().await;
}

#[tokio::test]
async fn shell_jobs_stdin_and_pty_run_real_processes_and_reap_them() {
    let upstream = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    let seen = count.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            let response = match seen.fetch_add(1, Ordering::SeqCst) {
                0 => tool_stream(vec![(
                    "background",
                    "bash",
                    json!({"command":"printf '%s' $$ > bg.pid; exec sleep 300","async":true}),
                )]),
                1 => {
                    let result = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|message| message["tool_call_id"] == "background")
                        .unwrap();
                    let content = result["content"][0]["text"].as_str().unwrap();
                    let id = content
                        .split("后台任务已启动：")
                        .nth(1)
                        .unwrap()
                        .split('，')
                        .next()
                        .unwrap();
                    tool_stream(vec![
                        ("kill", "job", json!({"op":"kill","job_id":id})),
                        (
                            "stdin",
                            "bash",
                            json!({"command":"wc -c","stdin":"a".repeat(200000)}),
                        ),
                        (
                            "pty",
                            "bash",
                            json!({"command":"printf 'terminal-ok'","pty":true,"timeout":5}),
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
    let id = h.session(SessionMode::Normal).await;
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
    let upstream = MockServer::start().await;
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
    let id = h.session(SessionMode::Normal).await;
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
    let upstream = MockServer::start().await;
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
        "[compaction]\nmethod_order=['remote']\nthreshold_tokens=900000",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
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
    let upstream = MockServer::start().await;
    Mock::given(method("POST")).and(path("/v1/responses")).respond_with(|request: &wiremock::Request| {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        if body.get("context_management").is_some() {
            ResponseTemplate::new(400).set_body_json(json!({"error":{"message":"unknown parameter context_management","type":"invalid_request_error"}}))
        } else { ResponseTemplate::new(200).set_body_raw(responses_stream("兼容成功"), "text/event-stream") }
    }).mount(&upstream).await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_responses",
        "[compaction]\nthreshold_tokens=900000",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
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
async fn shake_shrinks_repeated_reads_and_keeps_tool_history_replayable() {
    let upstream = MockServer::start().await;
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
    let h = Harness::new(&upstream.uri(), "openai_chat", "[compaction]\nmethod_order=['shake']\nprotect_recent_tool_tokens=0\nthreshold_tokens=900000\n[compaction.shake]\nkeep_last_turns=0\nsoft_trim_chars=64").await;
    std::fs::write(h.root.path().join("work/large.txt"), "内容".repeat(2500)).unwrap();
    let id = h.session(SessionMode::Normal).await;
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
    let loaded = h.load(&id).await;
    let outputs: Vec<_> = loaded
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    assert_eq!(outputs.len(), 2);
    assert_eq!(outputs[0].content, "[已被后续读取取代]");
    assert_eq!(outputs[1].content, "[工具输出已修剪]");
    h.prompt(&id, "验证回放").await;
    h.close().await;
}

#[tokio::test]
async fn background_pipes_and_pty_enforce_deadlines_and_reap_killed_jobs() {
    let upstream = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST")).respond_with(move |_: &wiremock::Request| {
        let response = match count.fetch_add(1, Ordering::SeqCst) {
            0 => tool_stream(vec![
                ("pipe_start", "bash", json!({"command":"printf pipe-ready; exec sleep 60","async":true,"timeout":1})),
                ("pty_start", "bash", json!({"command":"printf pty-ready; exec sleep 60","async":true,"pty":true,"timeout":1})),
            ]),
            1 => tool_stream(vec![
                ("pipe_wait", "job", json!({"op":"wait","job_id":"job_1","timeout_secs":5})),
                ("pty_wait", "job", json!({"op":"wait","job_id":"job_2","timeout_secs":5})),
            ]),
            2 => tool_stream(vec![("pty_again", "bash", json!({"command":"exec sleep 60","async":true,"pty":true,"timeout":0}))]),
            3 => tool_stream(vec![("pty_kill", "job", json!({"op":"kill","job_id":"job_3"}))]),
            5 => tool_stream(vec![("after_resume", "bash", json!({"command":"printf resumed"}))]),
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
    let id = h.session(SessionMode::Normal).await;
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
    assert!(output("pipe_wait").contains("TimedOut\npipe-ready"));
    assert!(output("pty_wait").contains("TimedOut"));
    assert!(output("pty_wait").contains("pty-ready"));
    assert_eq!(output("pty_kill"), "Killed");
    assert!(results.iter().all(|result| !result.is_error));
    let original_log =
        std::path::PathBuf::from(output("pipe_start").split("日志：").nth(1).unwrap());
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

#[tokio::test]
async fn oversized_read_line_does_not_block_later_anchors_or_replace_symlinks() {
    let upstream = MockServer::start().await;
    let count = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let response = match count.fetch_add(1, Ordering::SeqCst) {
                0 => tool_stream(vec![("read_link", "read", json!({"path":"alias.txt"}))]),
                1 => {
                    let body: Value = serde_json::from_slice(&request.body).unwrap();
                    let output = body["messages"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .find(|message| message["tool_call_id"] == "read_link")
                        .unwrap()["content"][0]["text"]
                        .as_str()
                        .unwrap();
                    assert!(output.contains("内容未展示"));
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
    let id = h.session(SessionMode::Normal).await;
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
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_ne!(body["stream"], true);
            ResponseTemplate::new(200).set_body_json(complete("checkpoint"))
        })
        .expect(1)
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nthreshold_tokens=1\nauto_continue=false",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
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
            request["messages"][0]["content"]
                .as_str()
                .unwrap()
                .contains("compressing an agent conversation")
        );
        entered.send(()).unwrap();
        let _ = gate.await;
    });
    let h = Harness::new(
        &url,
        "openai_chat",
        "[compaction]\nmethod_order=['soft']\nthreshold_tokens=1",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
    let client = h.client.clone();
    let sid = id.clone();
    let prompt = tokio::spawn(async move {
        client
            .call::<SessionPrompt>(PromptParams {
                session_id: sid,
                input: vec![InputPart::Text {
                    text: "压缩时取消".into(),
                }],
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
    assert!(loaded.session.lock.is_none());
    release.send(()).unwrap();
    upstream.await.unwrap();
    h.close().await;
}

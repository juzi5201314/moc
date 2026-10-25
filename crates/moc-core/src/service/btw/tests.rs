//! 旁路关键回归：同会话主任务隔离、预算、快照、取消和异常上游。

use super::*;
use crate::Core;
use crate::session::entries::SessionOrigin;
use moc_protocol::methods::{
    BtwCancelParams, BtwUpdateMethod, BtwUpdateParams, ClientInfo, SessionBtw, SessionBtwCancel,
    SessionMode, SessionPermission, SessionUpdateMethod,
};
use moc_protocol::{ClientBuilder, ClientHandle, Server, ServerConnection, transport};
use serde_json::json;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

struct Harness {
    _root: tempfile::TempDir,
    core: Arc<Core>,
    client: ClientHandle,
    server: Arc<Server<Core>>,
    connection: ServerConnection,
    record: Arc<crate::session::SessionRecord>,
    session_id: String,
    updates: tokio::sync::mpsc::UnboundedReceiver<BtwUpdateParams>,
    main_updates: Arc<parking_lot::Mutex<Vec<moc_protocol::methods::SessionUpdate>>>,
}

impl Harness {
    async fn new(url: &str, api: &str, extra: &str) -> Self {
        let root = tempfile::tempdir().expect("创建测试目录");
        let home = root.path().join("home");
        let cwd = root.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        std::fs::write(cwd.join("AGENTS.md"), "Keep the repository rule marker.").unwrap();
        std::fs::write(home.join("config.toml"), format!(
            "[models.roles]\ndefault='test/model:off'\n\
             [providers.test]\napi='{api}'\nbase_url='{url}/v1'\nmax_concurrency=1\nretry={{max_attempts=1}}\n\
             [providers.test.models.model]\ncontext_window=100000\nmax_output_tokens=1024\ninput=['text','image']\n\
             [workspace]\nwatch=false\n[system_prompt]\nappend='Keep the append marker.'\n{extra}\n"
        )).unwrap();
        let core = Core::new_with_sandbox_support(
            crate::CoreConfig {
                home,
                cwd: cwd.clone(),
                session_dir: Some(root.path().join("sessions")),
                ..Default::default()
            },
            crate::sandbox::SandboxSupport::full_only(),
        )
        .unwrap();
        // 绕过 session/new 的插件生命周期，只测试旁路是否自行启动宿主。
        let info = core
            .inner
            .sessions
            .create(
                &cwd,
                "test/model:off",
                SessionMode::Normal,
                SessionPermission::Full,
                None,
                SessionOrigin::root(),
                false,
            )
            .await
            .unwrap();
        let record = core.inner.sessions.get(&info.id).await.unwrap();
        let server = Server::new((*core).clone());
        let (server_side, client_side) = transport::duplex(65536);
        let connection = server.spawn(server_side);
        let (sender, updates) = tokio::sync::mpsc::unbounded_channel();
        let main_updates = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let main_sink = Arc::clone(&main_updates);
        let (client, _) = ClientBuilder::new(ClientInfo {
            name: "旁路测试".into(),
            version: "1".into(),
            frontend: "test".into(),
        })
        .on_notification::<BtwUpdateMethod, _, _>(move |params, _| {
            sender.send(params).unwrap();
            std::future::ready(())
        })
        .on_notification::<SessionUpdateMethod, _, _>(move |params, _| {
            main_sink.lock().push(params.update);
            std::future::ready(())
        })
        .connect(client_side)
        .await
        .unwrap();
        Self {
            _root: root,
            core,
            client,
            server,
            connection,
            record,
            session_id: info.id,
            updates,
            main_updates,
        }
    }

    fn params(&self, question: &str) -> BtwParams {
        BtwParams {
            session_id: self.session_id.clone(),
            request_id: 7,
            question: question.into(),
            history: Vec::new(),
        }
    }

    async fn append(&self, message: Message) {
        self.core
            .inner
            .sessions
            .append_message(&self.session_id, message, None, None)
            .await
            .unwrap();
    }

    async fn unchanged_snapshot(&self) -> (String, Option<Vec<u8>>) {
        let state = format!("{:?}", *self.record.state.lock().await);
        let disk = std::fs::read(&self.record.paths.transcript).ok();
        (state, disk)
    }

    async fn event(&mut self) -> BtwUpdate {
        let update = tokio::time::timeout(Duration::from_secs(3), self.updates.recv())
            .await
            .expect("旁路应及时推送更新")
            .expect("通知通道应保持连接");
        assert_eq!(update.session_id, self.session_id);
        assert_eq!(update.request_id, 7);
        update.update
    }

    async fn cancel(&self) {
        let result = self
            .client
            .call::<SessionBtwCancel>(BtwCancelParams {
                session_id: self.session_id.clone(),
                request_id: 7,
            })
            .await
            .unwrap();
        assert!(result.cancelled);
    }

    async fn close(self) {
        self.server.shutdown();
        self.connection.wait().await;
        self.core.shutdown().await;
    }
}

fn text_stream(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "id":"btw-response", "model":"model",
            "choices":[{"index":0,"delta":{"content":text},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":19,"completion_tokens":3,"total_tokens":22}
        })
    )
}

async fn mount_text(upstream: &MockServer) {
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("旁路回答"), "text/event-stream"),
        )
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn readonly_snapshot_works_while_main_lock_is_held_and_never_loads_hosts() {
    let upstream = MockServer::builder().start().await;
    let stream = format!(
        "data: {}\n\n{}",
        json!({
            "choices":[{"index":0,"delta":{"reasoning_content":"先看已有上下文"}}]
        }),
        text_stream("旁路回答")
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let mut h = Harness::new(&upstream.uri(), "openai_chat",
        "[providers.test.extra_body]\ntools=[{type='web_search'}]\ntool_choice='required'\ncontext_management=[{type='compaction'}]"
    ).await;
    h.append(Message::user_text("主任务记录")).await;
    h.append(Message {
        role: Role::Assistant,
        content: vec![ContentPart::ToolCall(moc_llm::ToolCall {
            id: "pending".into(),
            name: "write".into(),
            arguments: "{\"path\":\"never.txt\"}".into(),
        })],
    })
    .await;
    h.append(Message {
        role: Role::Tool,
        content: vec![ContentPart::ToolResult(moc_llm::ToolResult {
            tool_call_id: "older".into(),
            content: "已有工具证据".into(),
            is_error: false,
            images: Vec::new(),
        })],
    })
    .await;
    h.record
        .state
        .lock()
        .await
        .notices
        .push("不得消费的主提醒".into());
    let before = h.unchanged_snapshot().await;
    let lock = h.record.try_prompt_lock().expect("模拟主提示占用");
    let mut params = h.params("解释 @does-not-exist.png 的含义，不读取文件");
    params.history.push(BtwExchange {
        question: "前一旁路问题".into(),
        answer: "前一旁路回答".into(),
    });
    let result = h.client.call::<SessionBtw>(params).await.unwrap();
    assert_eq!(result.text, "旁路回答");
    assert_eq!(result.model, "test/model:off");
    assert_eq!(result.finish_reason, FinishReason::Stop);
    assert_eq!(result.history_turns_used, 1);
    assert_eq!(result.usage.input_tokens, 19);
    assert_eq!(h.unchanged_snapshot().await, before);
    assert!(h.core.inner.plugin_hosts.lock().await.is_empty());
    assert!(h.core.inner.mcp_managers.lock().await.is_empty());
    assert!(h.core.inner.skill_indexes.read().is_empty());
    assert!(h.main_updates.lock().is_empty());
    assert!(matches!(h.event().await, BtwUpdate::Started));
    assert!(matches!(h.event().await, BtwUpdate::Waiting));
    assert!(matches!(h.event().await, BtwUpdate::Generating));
    assert!(
        matches!(h.event().await, BtwUpdate::ReasoningDelta { text } if text == "先看已有上下文")
    );
    assert!(matches!(h.event().await, BtwUpdate::TextDelta { text } if text == "旁路回答"));
    let requests = upstream.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(body.get("tools").is_none());
    assert!(body.get("tool_choice").is_none());
    assert!(body.get("parallel_tool_calls").is_none());
    assert!(body.get("context_management").is_none());
    let messages = body["messages"].as_array().unwrap();
    assert!(
        messages
            .iter()
            .all(|m| m["role"] != "tool" && m.get("tool_calls").is_none())
    );
    let text = body["messages"].to_string();
    for expected in [
        "主任务记录",
        "historical-tool-call",
        "已有工具证据",
        "does-not-exist.png",
        "前一旁路问题",
        "前一旁路回答",
        "repository rule marker",
        "append marker",
        "No tools are available",
    ] {
        assert!(text.contains(expected), "请求应包含 {expected}：{text}");
    }
    for excluded in [
        "run_code works",
        "Standard tool batches",
        "<skills>",
        "<plugins>",
        "<mcp-servers>",
        "不得消费的主提醒",
    ] {
        assert!(!text.contains(excluded), "旁路不得注入 {excluded}");
    }
    drop(lock);
    h.close().await;
}

#[tokio::test]
async fn rejects_empty_or_excess_history_before_calling_provider() {
    let upstream = MockServer::builder().start().await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let before = h.unchanged_snapshot().await;
    for mut params in [h.params(" \n\t"), h.params("正常问题")] {
        if !params.question.trim().is_empty() {
            params.history = (0..=MAX_BTW_HISTORY_TURNS)
                .map(|_| BtwExchange {
                    question: "问".into(),
                    answer: "答".into(),
                })
                .collect();
        }
        let error = h.client.call::<SessionBtw>(params).await.unwrap_err();
        assert!(error.to_string().contains("不能为空") || error.to_string().contains("20"));
    }
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert_eq!(h.unchanged_snapshot().await, before);
    h.close().await;
}

#[tokio::test]
async fn budget_drops_whole_oldest_turns_and_never_compacts_main_context() {
    let upstream = MockServer::builder().start().await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let config = h.core.inner.config();
    let mut model = config.resolve_model("test/model:off").unwrap();
    let base = vec![Message::user_text("主上下文")];
    let history: Vec<_> = (0..MAX_BTW_HISTORY_TURNS)
        .map(|index| BtwExchange {
            question: format!("问题 {index}"),
            answer: format!("回答 {index}"),
        })
        .collect();
    let last = history.last().unwrap();
    let fixed = compact::messages_tokens(&base)
        + compact::message_tokens(&Message::user_text("新问题"))
        + u64::from(model.max_output_tokens);
    let last_tokens = compact::messages_tokens(&[
        Message::user_text(&last.question),
        Message::assistant_text(&last.answer),
    ]);
    model.context_window = fixed + last_tokens;
    let (messages, used) = fit_history(base.clone(), None, &history, "新问题", &model).unwrap();
    assert_eq!(used, 1);
    assert_eq!(
        messages.iter().map(Message::text).collect::<Vec<_>>(),
        ["主上下文", "问题 19", "回答 19", "新问题"]
    );
    let native = [json!({"type":"compaction","encrypted_content":"x".repeat(800)})];
    assert!(fit_history(base.clone(), Some(&native), &[], "新问题", &model).is_err());
    model.context_window = fixed - 1;
    assert!(
        fit_history(base, None, &history, "新问题", &model)
            .unwrap_err()
            .message
            .contains("不会压缩主会话")
    );
    // 真实请求的主视图溢出同样在发 HTTP 之前失败，且不更新状态。
    h.append(Message::user_text("长".repeat(100_000))).await;
    let before = h.unchanged_snapshot().await;
    let error = h
        .client
        .call::<SessionBtw>(h.params("简问"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("不会压缩主会话"));
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert_eq!(h.unchanged_snapshot().await, before);
    h.close().await;
}

#[tokio::test]
async fn clm_snapshot_uses_live_context_instead_of_raw_transcript() {
    let upstream = MockServer::builder().start().await;
    mount_text(&upstream).await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    h.append(Message::user_text("已由 CLM 替换的旧历史")).await;
    {
        let mut state = h.record.state.lock().await;
        state.mode = SessionMode::ClmExperimental;
        state.clm_context = Some("当前 CLM 工作视图".into());
    }
    let before = h.unchanged_snapshot().await;
    h.client
        .call::<SessionBtw>(h.params("当前在做什么"))
        .await
        .unwrap();
    let requests = upstream.received_requests().await.unwrap();
    let text = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(text.contains("当前 CLM 工作视图"));
    assert!(!text.contains("已由 CLM 替换的旧历史"));
    assert!(!text.contains("maintain it with `clm_context`"));
    assert_eq!(h.unchanged_snapshot().await, before);
    h.close().await;
}

#[tokio::test]
async fn responses_preserve_native_checkpoint_and_textualize_native_tools() {
    let upstream = MockServer::builder().start().await;
    let delta = |index, text| {
        format!(
            "event: response.output_text.delta\ndata: {}\n\n",
            json!({
                "type":"response.output_text.delta", "output_index":index, "delta":text
            })
        )
    };
    // 到达顺序与内容块顺序不同：最终结果必须使用累加器，不能重拼通知文本。
    let stream = format!(
        "{}{}{}event: response.completed\ndata: {}\n\n",
        delta(0, "原生"),
        delta(1, "回答"),
        delta(0, "旁路"),
        json!({
            "type":"response.completed", "response":{
                "id":"r", "model":"model", "status":"completed",
                "output":[{"type":"message","id":"m","role":"assistant","content":[{"type":"output_text","text":"原生旁路回答"}]}],
                "usage":{"input_tokens":9,"output_tokens":2,"total_tokens":11}
            }
        })
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_responses", "").await;
    h.append(Message::user_text("仍可见的主尾部")).await;
    let native = json!({"type":"compaction","encrypted_content":"opaque-checkpoint"});
    {
        let mut state = h.record.state.lock().await;
        state.summary = Some(("remote".into(), "不能替代原生数据的摘要".into()));
        state.native_input = Some(vec![
            native.clone(),
            json!({"type":"function_call","call_id":"old","name":"read","arguments":"{}"}),
            json!({"type":"function_call_output","call_id":"old","output":"原生工具证据"}),
        ]);
    }
    let before = h.unchanged_snapshot().await;
    let result = h
        .client
        .call::<SessionBtw>(h.params("解释已知证据"))
        .await
        .unwrap();
    assert_eq!(result.text, "原生旁路回答");
    let requests = upstream.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    let input = body["input"].as_array().unwrap();
    assert_eq!(input[0], native);
    assert!(
        input
            .iter()
            .all(|item| item["type"] != "function_call" && item["type"] != "function_call_output")
    );
    assert!(body["input"].to_string().contains("原生工具证据"));
    assert!(!body["input"].to_string().contains("不能替代原生数据的摘要"));
    assert_eq!(h.unchanged_snapshot().await, before);
    h.close().await;
}

#[tokio::test]
async fn tool_calls_fail_explicitly_without_execution_or_retry() {
    let upstream = MockServer::builder().start().await;
    let stream = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        json!({
            "choices":[{"index":0,"delta":{"tool_calls":[{
                "index":0,"id":"bad","type":"function","function":{"name":"write","arguments":"{\"path\":\"bad.txt\",\"content\":\"bad\"}"}
            }]},"finish_reason":"tool_calls"}]
        })
    );
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
        .expect(1)
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let before = h.unchanged_snapshot().await;
    let error = h
        .client
        .call::<SessionBtw>(h.params("写入文件"))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("旁路问答不能执行工具"));
    assert!(!h.core.inner.config().cwd.join("bad.txt").exists());
    assert_eq!(h.unchanged_snapshot().await, before);
    assert!(h.core.inner.plugin_hosts.lock().await.is_empty());
    h.close().await;
}

#[tokio::test]
async fn provider_wait_is_cancellable_without_releasing_the_main_permit() {
    let upstream = MockServer::builder().start().await;
    let mut h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let model = h
        .core
        .inner
        .config()
        .resolve_model("test/model:off")
        .unwrap();
    let permit = h
        .core
        .inner
        .provider_permit(&model, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let before = h.unchanged_snapshot().await;
    let params = h.params("等待中的旁路");
    let client = h.client.clone();
    let running = tokio::spawn(async move { client.call::<SessionBtw>(params).await.unwrap() });
    assert!(matches!(h.event().await, BtwUpdate::Started));
    assert!(matches!(h.event().await, BtwUpdate::Waiting));
    assert!(h.record.try_prompt_lock().is_ok(), "旁路不能占用主提示锁");
    assert!(
        h.record.state.try_lock().is_ok(),
        "等待供应商时不能持会话状态锁"
    );
    h.cancel().await;
    let result = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert!(result.text.is_empty());
    let gate = h
        .core
        .inner
        .provider_gates
        .read()
        .get("test")
        .unwrap()
        .clone();
    assert!(
        gate.try_acquire(1).is_none(),
        "旁路取消不能释放主请求的许可"
    );
    assert!(upstream.received_requests().await.unwrap().is_empty());
    assert_eq!(h.unchanged_snapshot().await, before);
    drop(permit);
    assert!(gate.try_acquire(1).is_some());
    h.close().await;
}

/// 读取一个完整 HTTP 请求，避免响应前仍有请求体滞留在 socket 上。
async fn read_request(socket: &mut tokio::net::TcpStream) {
    let mut bytes = Vec::new();
    loop {
        assert!(socket.read_buf(&mut bytes).await.unwrap() > 0);
        let Some(end) = bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .map(|index| index + 4)
        else {
            continue;
        };
        let headers = std::str::from_utf8(&bytes[..end]).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                let (key, value) = line.split_once(':')?;
                key.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse().unwrap())
            })
            .unwrap();
        if bytes.len() >= end + length {
            return;
        }
    }
}

#[tokio::test]
async fn stream_cancel_returns_partial_text_and_releases_provider_slot() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (release, gate) = tokio::sync::oneshot::channel();
    let upstream = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        read_request(&mut socket).await;
        socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 100000\r\nConnection: close\r\n\r\n").await.unwrap();
        let part = format!(
            "data: {}\n\n",
            json!({
                "choices":[{"index":0,"delta":{"content":"已经生成的部分"}}]
            })
        );
        socket.write_all(part.as_bytes()).await.unwrap();
        socket.flush().await.unwrap();
        let _ = gate.await;
    });
    let mut h = Harness::new(&url, "openai_chat", "").await;
    let before = h.unchanged_snapshot().await;
    let client = h.client.clone();
    let params = h.params("持续回答");
    let running = tokio::spawn(async move { client.call::<SessionBtw>(params).await.unwrap() });
    loop {
        if matches!(h.event().await, BtwUpdate::TextDelta { text } if text == "已经生成的部分")
        {
            break;
        }
    }
    h.cancel().await;
    let result = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.text, "已经生成的部分");
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert_eq!(h.unchanged_snapshot().await, before);
    assert!(
        h.core
            .inner
            .provider_gates
            .read()
            .get("test")
            .unwrap()
            .try_acquire(1)
            .is_some()
    );
    release.send(()).unwrap();
    upstream.await.unwrap();
    h.close().await;
}

#[tokio::test]
async fn same_session_second_btw_is_busy_and_wrong_cancel_is_ignored() {
    let upstream = MockServer::builder().start().await;
    let mut h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let model = h
        .core
        .inner
        .config()
        .resolve_model("test/model:off")
        .unwrap();
    // 占住唯一的供应商名额，让首个旁路停在 Waiting，便于观察并发门禁。
    let permit = h
        .core
        .inner
        .provider_permit(&model, &tokio_util::sync::CancellationToken::new())
        .await
        .unwrap();
    let params = h.params("第一条旁路");
    let client = h.client.clone();
    let running = tokio::spawn(async move { client.call::<SessionBtw>(params).await.unwrap() });
    assert!(matches!(h.event().await, BtwUpdate::Started));
    assert!(matches!(h.event().await, BtwUpdate::Waiting));

    let mut second = h.params("第二条旁路");
    second.request_id = 8;
    let error = h.client.call::<SessionBtw>(second).await.unwrap_err();
    assert_eq!(error.code(), Some(-32011), "同会话第二条旁路必须直接 busy");

    let wrong = h
        .client
        .call::<SessionBtwCancel>(BtwCancelParams {
            session_id: h.session_id.clone(),
            request_id: 99,
        })
        .await
        .unwrap();
    assert!(!wrong.cancelled, "身份不匹配的取消不能停掉在飞旁路");
    assert!(h.record.try_prompt_lock().is_ok(), "主提示锁仍未占用");

    h.cancel().await;
    let result = tokio::time::timeout(Duration::from_secs(3), running)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.finish_reason, FinishReason::Cancelled);
    assert!(upstream.received_requests().await.unwrap().is_empty());
    drop(permit);
    h.close().await;
}

#[tokio::test]
async fn stream_guard_discards_loop_without_touching_main_guard_or_retrying() {
    let upstream = MockServer::builder().start().await;
    let delta = json!({"choices":[{"index":0,"delta":{"content":"反复检查现有情况，然后再次确认当前结果。".repeat(150)}}]});
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            format!("data: {delta}\n\n{}", text_stream("不得保留")),
            "text/event-stream",
        ))
        .expect(1)
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let runtime = h.core.inner.runtime(&h.session_id).await;
    let guard = super::super::prompt::guard::PromptGuard::new(
        tokio_util::sync::CancellationToken::new(),
        None,
    );
    *runtime.guard.lock() = Some(Arc::clone(&guard));
    let before = h.unchanged_snapshot().await;
    let result = h
        .client
        .call::<SessionBtw>(h.params("产生循环"))
        .await
        .unwrap();
    assert_eq!(result.finish_reason, FinishReason::LoopDetected);
    assert!(result.text.is_empty());
    assert!(guard.violation().is_none());
    assert!(!guard.cancel.is_cancelled());
    assert_eq!(h.unchanged_snapshot().await, before);
    h.close().await;
}

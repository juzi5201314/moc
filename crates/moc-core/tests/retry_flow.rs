//! `/retry`：剪掉没跑完的尾巴，然后不追加任何用户消息地重发一次提示。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moc_protocol::methods::*;
use moc_protocol::{FinishReason, Role, RpcErrorCode};
use serde_json::Value;
use support::{Harness, text_stream};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// 一个「先吐一半、再挂住」的上游：客户端取消后接受下一条连接并正常应答。
///
/// 挂住是为了让取消发生在半截响应上——只有这样 core 才会把这一轮落库成待剪的
/// 失败回合。返回的计数是收到的请求数。
async fn partial_then_complete(
    partial: String,
    answer: &'static str,
) -> (String, Arc<AtomicUsize>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let requests = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&requests);
    tokio::spawn(async move {
        // 第一连接：写半截后挂住，等客户端取消断开。
        let (mut first, _) = listener.accept().await.unwrap();
        counter.fetch_add(1, Ordering::SeqCst);
        read_headers(&mut first).await;
        first
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        first
            .write_all(format!("{:x}\r\n{partial}\r\n", partial.len()).as_bytes())
            .await
            .unwrap();

        // 第二连接：正常答完。第一条连接保持打开——客户端取消时才不会先撞上读错误。
        let (mut second, _) = listener.accept().await.unwrap();
        counter.fetch_add(1, Ordering::SeqCst);
        read_headers(&mut second).await;
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
            )
            .await
            .unwrap();
        let body = text_stream(answer);
        second
            .write_all(format!("{:x}\r\n{body}\r\n0\r\n\r\n", body.len()).as_bytes())
            .await
            .unwrap();
        // 留住两条连接，避免客户端在提示结束前看到 EOF。
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        drop(first);
    });
    (url, requests)
}

async fn read_headers(socket: &mut tokio::net::TcpStream) {
    let mut buffer = vec![0; 65536];
    let mut used = 0;
    while !buffer[..used].windows(4).any(|bytes| bytes == b"\r\n\r\n") {
        let read = socket.read(&mut buffer[used..]).await.unwrap();
        assert!(read > 0, "对端在报文头完成前关闭了连接");
        used += read;
    }
    let head = String::from_utf8_lossy(&buffer[..used]);
    eprintln!(
        "DEBUG 上游收到：{}",
        head.lines().next().unwrap_or_default()
    );
}

/// 取消发生在半截响应上：这一轮落库成待剪的失败回合，重试要把它整条撤掉。
#[tokio::test]
async fn retry_drops_the_interrupted_turn_before_asking_again() {
    let partial = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "partial",
            "model": "gpt-test",
            "choices": [{"index": 0, "delta": {"role": "assistant", "content": "半截正文"}, "finish_reason": null}]
        })
    );
    let (url, requests) = partial_then_complete(partial, "续跑完成").await;
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
                mode: None,
                agent_message: None,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                SessionUpdate::AssistantDelta { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "半截正文到达前端才发送取消（请求数 {}）",
            requests.load(Ordering::SeqCst)
        )
    });
    h.client
        .call::<SessionCancel>(SessionCancelParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    let cancelled = prompt.await.unwrap().expect("取消收尾为正常返回");
    assert_eq!(cancelled.finish_reason, FinishReason::Cancelled);

    let result = h
        .client
        .call::<SessionRetry>(SessionRetryParams {
            session_id: id.clone(),
        })
        .await
        .expect("重试成功");
    assert_eq!(result.message.text(), "续跑完成");

    // 半截那一轮已经被剪掉：transcript 只剩原始用户消息、运行时提醒与重试的回复。
    let entries = h.load(&id).await.entries;
    let texts: Vec<String> = entries
        .iter()
        .filter(|entry| entry.reminder.is_none())
        .map(|entry| entry.message.text())
        .collect();
    assert_eq!(texts, vec!["开始".to_owned(), "续跑完成".to_owned()]);
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    // 前端据此撤掉那段已经不在会话里的预览。
    assert!(
        h.updates.lock().iter().any(|update| matches!(
            update,
            SessionUpdate::PromptRewound {
                replayed_tools: false
            }
        )),
        "剪掉失败回合后必须通知前端回退"
    );

    h.close().await;
}

/// 整批工具都没跑：删掉占位结果、保留 assistant 的工具调用，直接重放这批调用。
#[tokio::test]
async fn retry_replays_tools_that_never_ran_without_asking_the_model() {
    let partial = format!(
        "data: {}\n\n",
        serde_json::json!({
            "id": "partial",
            "model": "gpt-test",
            "choices": [{
                "index": 0,
                "delta": {
                    "role": "assistant",
                    "content": "先看看文件",
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_replay",
                        "type": "function",
                        "function": {"name": "read", "arguments": "{\"path\":\"pending.txt\"}"}
                    }]
                },
                "finish_reason": null
            }]
        })
    );
    let (url, requests) = partial_then_complete(partial, "工具跑完继续").await;
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
                mode: None,
                agent_message: None,
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if matches!(
                events.recv().await.unwrap(),
                SessionUpdate::AssistantDelta { .. }
            ) {
                break;
            }
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "工具调用到达前端才发送取消（请求数 {}）",
            requests.load(Ordering::SeqCst)
        )
    });
    h.client
        .call::<SessionCancel>(SessionCancelParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    prompt.await.unwrap().unwrap();

    let result = h
        .client
        .call::<SessionRetry>(SessionRetryParams {
            session_id: id.clone(),
        })
        .await
        .expect("重试成功");
    assert_eq!(result.message.text(), "工具跑完继续");

    // 只有两次模型请求：取消前那次，以及工具重放之后的收尾。重放本身不花模型调用。
    assert_eq!(requests.load(Ordering::SeqCst), 2);
    // assistant 消息仍在会话里，前端只能撤工具占位、不能撤它的正文。
    assert!(
        h.updates.lock().iter().any(|update| matches!(
            update,
            SessionUpdate::PromptRewound {
                replayed_tools: true
            }
        )),
        "重放工具时要说明回复仍然有效"
    );

    h.close().await;
}

#[tokio::test]
async fn retry_resends_context_without_appending_user_message() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("收到"), "text/event-stream"),
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
    h.prompt(&id, "第一问").await;

    let result = h
        .client
        .call::<SessionRetry>(SessionRetryParams {
            session_id: id.clone(),
        })
        .await
        .expect("重试成功");
    assert_eq!(result.session_id, id);
    assert_eq!(result.message.text(), "收到");

    // 重试不发新请求以外的任何写入：transcript 仍是「一问一答」。
    // 运行时提醒也是 user 角色，但它属于模型内部状态，不算用户消息。
    let entries = h.load(&id).await.entries;
    let user_texts: Vec<String> = entries
        .iter()
        .filter(|entry| entry.message.role == Role::User && entry.reminder.is_none())
        .map(|entry| entry.message.text())
        .collect();
    assert_eq!(
        user_texts,
        vec!["第一问".to_owned()],
        "重试不得追加用户消息"
    );

    let requests = upstream.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2, "重试就是一次新的模型请求");
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let conversation: Vec<&Value> = body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|message| {
            // 沙盒契约等运行时提醒是每轮固定追加的尾巴，不属于对话内容。
            message["role"] != "system"
                && !message["content"]
                    .as_str()
                    .is_some_and(|text| text.starts_with("<system-reminder>"))
        })
        .collect();
    // 第二次请求的对话段与第一次完全一致：一问一答，末尾没有被塞进新的用户消息。
    assert_eq!(conversation.len(), 2, "上下文原样重发：一问一答");
    assert_eq!(conversation[0]["role"], "user");
    assert_eq!(conversation[0]["content"], "第一问");
    assert_eq!(conversation[1]["role"], "assistant");
    assert_eq!(conversation[1]["content"], "收到");
    h.close().await;
}

#[tokio::test]
async fn retry_without_context_is_rejected_before_any_request() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("收到"), "text/event-stream"),
        )
        .expect(0)
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;

    let error = h
        .client
        .call::<SessionRetry>(SessionRetryParams {
            session_id: id.clone(),
        })
        .await
        .expect_err("空会话不可重试");
    assert_eq!(error.code(), Some(RpcErrorCode::InvalidParams.code()));
    h.close().await;
}

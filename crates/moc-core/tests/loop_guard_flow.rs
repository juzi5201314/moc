mod support;

use moc_llm::FinishReason;
use moc_protocol::methods::SessionUpdate;
use moc_protocol::{Role, SessionMode};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use support::{Harness, text_stream, tool_stream};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn loops_discard_failed_generation_release_lock_and_do_not_poison_next_prompt() {
    for field in ["content", "reasoning_content"] {
        let upstream = MockServer::start().await;
        let requests = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&requests);
        Mock::given(method("POST")).and(path("/v1/chat/completions"))
            .respond_with(move |_: &wiremock::Request| {
                let stream = if seen.fetch_add(1, Ordering::SeqCst) == 0 {
                    let delta = json!({"choices":[{"index":0,"delta":{field:"反复检查现有情况，然后再次确认当前结果。".repeat(150)},"finish_reason":null}]});
                    // 检测必须丢弃同一网络块中随后到达的工具，不能先累加或执行。
                    format!("data: {delta}\n\n{}", tool_stream(vec![("bad", "write", json!({"path":"bad.txt","content":"不应写入"}))]))
                } else { text_stream("下一提示正常完成") };
                ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream")
            }).mount(&upstream).await;
        let h = Harness::new(
            &upstream.uri(),
            "openai_chat",
            "[compaction]\nenabled=false",
        )
        .await;
        let id = h.session(SessionMode::Normal).await;
        let result = h.prompt(&id, "执行循环场景").await;
        assert_eq!(result.finish_reason, FinishReason::LoopDetected);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
        assert!(!h.root.path().join("work/bad.txt").exists());
        let loaded = h.load(&id).await;
        assert!(loaded.session.lock.is_none());
        assert!(
            loaded
                .entries
                .iter()
                .filter(|entry| entry.message.role == Role::Assistant)
                .all(|entry| !entry.message.text().contains("反复检查现有情况"))
        );
        assert_eq!(
            h.prompt(&id, "继续新任务").await.message.text(),
            "下一提示正常完成"
        );
        let received = upstream.received_requests().await.unwrap();
        let replay: serde_json::Value = serde_json::from_slice(&received[1].body).unwrap();
        assert!(!replay["messages"].to_string().contains("反复检查现有情况"));
        h.close().await;
    }
}

#[tokio::test]
async fn repeated_tools_redirect_once_then_terminate_without_another_request() {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            tool_stream(vec![("read", "read", json!({"path":"sample.txt"}))]),
            "text/event-stream",
        ))
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    std::fs::write(h.root.path().join("work/sample.txt"), "固定证据").unwrap();
    let id = h.session(SessionMode::Normal).await;
    let result = h.prompt(&id, "持续读同一文件").await;
    assert_eq!(result.finish_reason, FinishReason::LoopDetected);
    let received = upstream.received_requests().await.unwrap();
    assert_eq!(received.len(), 6);
    let redirected: serde_json::Value = serde_json::from_slice(&received[5].body).unwrap();
    assert!(redirected["messages"].to_string().contains("tool_loop"));
    {
        let updates = h.updates.lock();
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(
                    update,
                    SessionUpdate::GuardNotice {
                        terminal: false,
                        ..
                    }
                ))
                .count(),
            1
        );
        assert_eq!(
            updates
                .iter()
                .filter(|update| matches!(update, SessionUpdate::PromptFinished { .. }))
                .count(),
            1
        );
    }
    h.close().await;
}

#[tokio::test]
async fn response_and_prompt_budgets_are_not_model_length_continuations() {
    for extra in [
        "max_response_bytes=32",
        "max_prompt_bytes=32",
        "max_turns_per_prompt=1",
    ] {
        let upstream = MockServer::start().await;
        let stream = if extra.starts_with("max_turns") {
            tool_stream(vec![("read", "read", json!({"path":"sample.txt"}))])
        } else {
            text_stream("这是正常文本而非循环，字节预算依然必须独立生效，不能视为供应商长度截断。")
        };
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream"))
            .mount(&upstream)
            .await;
        let h = Harness::new(
            &upstream.uri(),
            "openai_chat",
            &format!("[compaction]\nenabled=false\n[limits]\n{extra}"),
        )
        .await;
        std::fs::write(h.root.path().join("work/sample.txt"), "固定证据").unwrap();
        let id = h.session(SessionMode::Normal).await;
        assert_eq!(
            h.prompt(&id, "执行预算场景").await.finish_reason,
            FinishReason::ResourceLimit
        );
        assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
        h.close().await;
    }
}

#[tokio::test]
async fn guard_stop_reaps_background_processes_owned_by_the_failed_prompt() {
    let upstream = MockServer::start().await;
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let stream = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_stream(vec![(
                    "worker",
                    "bash",
                    json!({"command":"sleep 30", "async":true, "lifecycle":"persistent"}),
                )])
            } else {
                tool_stream(vec![("read", "read", json!({"path":"sample.txt"}))])
            };
            ResponseTemplate::new(200).set_body_raw(stream, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    std::fs::write(h.root.path().join("work/sample.txt"), "固定证据").unwrap();
    let id = h.session(SessionMode::Normal).await;
    let mut events = h.events.subscribe();
    let result = h.prompt(&id, "创建后台任务后循环读取").await;
    assert_eq!(result.finish_reason, FinishReason::LoopDetected);
    let exit_code = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            if let SessionUpdate::JobFinished {
                command, exit_code, ..
            } = events.recv().await.unwrap()
                && command == "sleep 30"
            {
                break exit_code;
            }
        }
    })
    .await
    .expect("保护停止应回收后台进程组而不是让进程继续睡眠");
    assert_eq!(exit_code, None);
    h.close().await;
}

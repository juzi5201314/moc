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
    // 正文重复直接终止；思维链重复先纠偏再重试，两者行为不同，分开验证。
    for (field, terminates) in [("content", true), ("reasoning_content", false)] {
        let upstream = MockServer::builder().start().await;
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
        if terminates {
            assert_eq!(result.finish_reason, FinishReason::LoopDetected);
            assert_eq!(requests.load(Ordering::SeqCst), 1);
        } else {
            // 思维链重复是软纠偏：写入提醒后重新请求，第二次拿到正常响应。
            assert_eq!(result.finish_reason, FinishReason::Stop);
            assert_eq!(result.message.text(), "下一提示正常完成");
            assert_eq!(requests.load(Ordering::SeqCst), 2);
            let replay: serde_json::Value =
                serde_json::from_slice(&upstream.received_requests().await.unwrap()[1].body)
                    .unwrap();
            assert!(
                replay["messages"].to_string().contains("thinking_loop"),
                "纠偏提醒必须进到重试请求里"
            );
        }
        assert!(!h.root.path().join("work/bad.txt").exists());
        let loaded = h.load(&id).await;
        // 循环中止的提示不能毒化下一次提示；写租约本身持有到会话卸载。
        assert!(
            loaded
                .session
                .lock
                .as_ref()
                .is_some_and(|lock| lock.pid == std::process::id())
        );
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
    let upstream = MockServer::builder().start().await;
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

/// 思维链反复重复且供应商每次都重复时，纠偏达到上限后按循环终止，不会无限重试。
#[tokio::test]
async fn thinking_loop_gives_up_after_the_redirect_budget() {
    let upstream = MockServer::builder().start().await;
    let requests = Arc::new(AtomicUsize::new(0));
    let seen = Arc::clone(&requests);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_: &wiremock::Request| {
            seen.fetch_add(1, Ordering::SeqCst);
            let delta = json!({"choices":[{"index":0,"delta":{
                "reasoning_content":"反复检查现有情况，然后再次确认当前结果。".repeat(150)
            },"finish_reason":null}]});
            ResponseTemplate::new(200)
                .set_body_raw(format!("data: {delta}\n\n"), "text/event-stream")
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
    let result = h.prompt(&id, "反复思考").await;
    assert_eq!(
        result.finish_reason,
        FinishReason::LoopDetected,
        "纠偏预算耗尽后仍应按循环终止"
    );
    assert_eq!(
        requests.load(Ordering::SeqCst),
        4,
        "首次请求 + 3 次纠偏重试"
    );
    // 首轮常规提醒一条 + 每次纠偏各一条。
    let loaded = h.load(&id).await;
    let thinking = loaded
        .entries
        .iter()
        .filter(|entry| entry.message.text().contains("thinking_loop"))
        .count();
    assert_eq!(thinking, 3, "三次纠偏各留一条思维链提醒");
    assert!(
        loaded
            .entries
            .iter()
            .filter(|entry| entry.message.role == Role::User && entry.reminder.is_none())
            .count()
            == 1,
        "提醒不得被算成用户消息"
    );
    h.close().await;
}

#[tokio::test]
async fn slow_request_only_warns_and_still_finishes() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(std::time::Duration::from_millis(1100))
                .set_body_raw(text_stream("慢请求正常完成"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false\n[limits]\nrequest_warn_secs=1",
    )
    .await;
    let id = h.session(SessionMode::Normal).await;
    let result = h.prompt(&id, "执行慢请求场景").await;
    assert_eq!(result.message.text(), "慢请求正常完成");
    assert_eq!(result.finish_reason, FinishReason::Stop);
    {
        let notices = h.updates.lock();
        assert!(
            notices.iter().any(|update| matches!(
                update,
                SessionUpdate::GuardNotice {
                    terminal: false,
                    message,
                    ..
                } if message.contains("已超过 1 秒")
            )),
            "慢请求只应发出一次非终止警告：{notices:?}"
        );
    }
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    h.close().await;
}

#[tokio::test]
async fn guard_stop_reaps_background_processes_owned_by_the_failed_prompt() {
    let upstream = MockServer::builder().start().await;
    let seen = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&seen);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let stream = if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                tool_stream(vec![(
                    "worker",
                    "bash",
                    json!({"description":"Start an idle background job","command":"sleep 30", "async":true, "lifecycle":"persistent"}),
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

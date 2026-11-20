use super::*;

#[path = "../../../tests/integration/support/mod.rs"]
mod support;

#[test]
fn run_id_charset_rejects_traversal() {
    assert!(valid_run_id("run_1735_001"));
    assert!(valid_run_id("a-b_C1"));
    assert!(!valid_run_id("../x"));
    assert!(!valid_run_id("a/b"));
    assert!(!valid_run_id("a.b"));
    assert!(!valid_run_id(""));
    assert!(!valid_run_id(&"x".repeat(65)));
}

/// 首次卸载等待被丢弃后，重试和并发等待仍须等到主循环收尾并落盘。
#[tokio::test]
async fn shutdown_retry_waits_for_terminal_after_first_wait_is_dropped() {
    use moc_protocol::methods::*;
    use serde_json::json;
    use std::time::Duration;
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    assert!(support::require_moc_bin("workflow shutdown"));
    let upstream = MockServer::builder().start().await;
    let requests = AtomicU64::new(0);
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let body = match requests.fetch_add(1, Ordering::Relaxed) {
                0 => support::tool_stream(vec![(
                    "skill",
                    "skill",
                    json!({"name": "workflow-authoring"}),
                )]),
                1 => support::tool_stream(vec![(
                    "create",
                    "create_workflow",
                    json!({"script": "return await hole<string>(\"pending\", \"Wait for input\");"}),
                )]),
                _ => support::text_stream("workflow launched"),
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let harness = support::Harness::with_ask_support(&upstream.uri(), "openai_chat", "").await;
    let session = harness.session(SessionEngine::Standard).await;
    harness
        .client
        .call::<SessionWorkflowActivate>(SessionWorkflowActivateParams {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    let mut events = harness.events.subscribe();
    let (_, run_id) = tokio::time::timeout(Duration::from_secs(20), async {
        tokio::join!(harness.prompt(&session, "启动等待输入的 workflow"), async {
            loop {
                if let SessionUpdate::Workflow { run_id, event } = events.recv().await.unwrap() {
                    match event {
                        WorkflowEvent::ApprovalRequested { request } => {
                            harness
                                .client
                                .call::<SessionWorkflowApprove>(SessionWorkflowApproveParams {
                                    session_id: session.clone(),
                                    request_id: request.request_id,
                                    decision: WorkflowDecision::Approve {
                                        role_models: None,
                                        model_ref_map: None,
                                    },
                                })
                                .await
                                .unwrap();
                        }
                        WorkflowEvent::HoleOpened { .. } => break run_id,
                        _ => {}
                    }
                }
            }
        })
    })
    .await
    .expect("run 应进入等待输入状态");
    let runtime = harness.core.inner.runtime(&session).await;
    let manager = runtime.workflows.get().unwrap();
    let handle = manager.running(&run_id).unwrap();
    assert_eq!(read_run_meta(&handle.dir).unwrap().status, "running");

    // 单线程运行时中连续 poll 不让主循环推进，精确模拟卸载 RPC 在等待中被取消。
    let mut first = Box::pin(manager.shutdown());
    assert!(futures_util::poll!(&mut first).is_pending());
    drop(first);
    let mut retry = Box::pin(manager.shutdown());
    assert!(
        futures_util::poll!(&mut retry).is_pending(),
        "首次等待被取消不能让重试跳过主循环收尾"
    );
    let mut concurrent = Box::pin(manager.shutdown());
    assert!(
        futures_util::poll!(&mut concurrent).is_pending(),
        "并发调用也必须订阅同一个完成信号"
    );
    // 外层期限短于 shutdown 的兜底，保证成功来自完成通知而非内部超时。
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(retry, concurrent);
    })
    .await
    .expect("主循环收尾后应唤醒所有等待者");
    assert!(manager.running(&run_id).is_none());
    let meta = read_run_meta(&handle.dir).unwrap();
    assert_eq!(meta.status, "stopped");
    assert_eq!(meta.stop_reason.as_deref(), Some("user"));
    assert!(meta.finished_at_ms.is_some());
    assert!(futures_util::poll!(Box::pin(manager.shutdown())).is_ready());
    harness.close().await;
}

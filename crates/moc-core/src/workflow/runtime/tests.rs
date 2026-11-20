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

/// 卸载重试与并发等待均须收到收尾通知；旧任务 panic 后不能移除恢复运行的新注册。
#[tokio::test]
async fn shutdown_retry_and_panic_notify_all_waiters() {
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

    // 复用已收尾的 handle，通过生产启动包装模拟主循环或失败收尾中的 panic。
    handle.finished.store(false, Ordering::Release);
    manager
        .runs
        .lock()
        .insert(run_id.clone(), Arc::clone(&handle));
    manager.spawn_run(Arc::clone(&handle), async {
        panic!("模拟 workflow 运行任务 panic");
    });
    // 单线程连续 poll，确保两个等待者都在运行任务 panic 前订阅。
    let mut shutdown = Box::pin(manager.shutdown());
    assert!(futures_util::poll!(&mut shutdown).is_pending());
    let mut concurrent = Box::pin(manager.shutdown());
    assert!(futures_util::poll!(&mut concurrent).is_pending());

    // 不让出执行权，在旧 supervisor 收尾前注册同一 run_id 的新 handle，模拟 resume。
    let (to_worker, _rx) = tokio::sync::mpsc::unbounded_channel();
    let resumed = Arc::new(RunHandle {
        id: run_id.clone(),
        dir: handle.dir.clone(),
        meta: Mutex::new(handle.meta.lock().clone()),
        view: Mutex::new(RunView {
            phases: Vec::new(),
            roles: Vec::new(),
            usage: json!({}),
            terminal: None,
            actor_sessions: HashMap::new(),
            actor_first_sent: HashSet::new(),
        }),
        engine: Mutex::new(Engine::new(
            Box::new(crate::workflow::engine::MemoryJournal::default()),
            crate::workflow::engine::EngineConfig::default(),
        )),
        publish_lock: Mutex::new(()),
        cancel: CancellationToken::new(),
        finished: AtomicBool::new(false),
        finish_notify: Notify::new(),
        cwd: handle.cwd.clone(),
        request: handle.request.clone(),
        policy: Mutex::new(None),
        to_worker,
    });
    manager
        .runs
        .lock()
        .insert(run_id.clone(), Arc::clone(&resumed));
    let cancel = resumed.cancel.clone();
    let (finish_tx, finish_rx) = oneshot::channel();
    manager.spawn_run(Arc::clone(&resumed), async move {
        cancel.cancelled().await;
        finish_rx.await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(3), async {
        tokio::join!(shutdown, concurrent);
    })
    .await
    .expect("运行任务 panic 后应及时唤醒所有等待者");
    assert!(handle.finished.load(Ordering::Acquire));
    assert!(Arc::ptr_eq(&manager.running(&run_id).unwrap(), &resumed));
    assert!(!resumed.finished.load(Ordering::Acquire));
    assert!(!resumed.cancel.is_cancelled());

    // 新注册仍可被 shutdown 取消，且必须等它自己的任务收尾。
    let mut shutdown = Box::pin(manager.shutdown());
    assert!(futures_util::poll!(&mut shutdown).is_pending());
    assert!(resumed.cancel.is_cancelled());
    finish_tx.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(3), shutdown)
        .await
        .expect("恢复运行的任务收尾后应唤醒卸载等待者");
    assert!(resumed.finished.load(Ordering::Acquire));
    assert!(manager.running(&run_id).is_none());
    assert!(futures_util::poll!(Box::pin(manager.shutdown())).is_ready());
    harness.close().await;
}

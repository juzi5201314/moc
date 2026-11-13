use super::*;

/// `plan = "none"`：进入与退出计划模式都不切换模型，也不推送模型变化。
#[tokio::test]
async fn plan_role_none_keeps_the_session_model_across_mode_changes() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(text_stream("plain response"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-test:off",
        "none",
        "",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "first").await;
    let planned = harness
        .client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: session.clone(),
            active: true,
        })
        .await
        .expect("enter plan mode");
    assert_eq!(planned.model, "test/gpt-test:off");
    harness.prompt(&session, "plan").await;
    let exited = harness
        .client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: session,
            active: false,
        })
        .await
        .expect("leave plan mode");
    assert_eq!(exited.model, "test/gpt-test:off");
    assert!(
        !harness
            .updates
            .lock()
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. })),
        "none 不应产生模型变化通知"
    );
    let models: Vec<String> = upstream
        .received_requests()
        .await
        .expect("capture requests")
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("请求体 JSON")["model"]
                .as_str()
                .expect("请求模型")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["gpt-test", "gpt-test"]);
    harness.close().await;
}

/// 未选择执行模型时使用审批时的会话模型，不恢复进入计划前的模型。
#[tokio::test]
async fn plan_approval_defaults_to_current_session_model() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use serde_json::json;

    let plan_arguments = json!({
        "title": "Build a small feature",
        "markdown": "# Approved plan\n\n1. Inspect the workspace.\n2. Implement the change.",
    });
    let responses = [
        support::tool_stream(vec![
            ("call_plan", "plan", plan_arguments),
            ("call_exit", "exit_plan", json!({})),
        ]),
        text_stream("Implementation started."),
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
    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/ds:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.ds]\ncontext_window=100000\nmax_output_tokens=4096\n",
    )
    .await;

    let session = harness
        .client
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
    assert_eq!(session.model, "test/glm:off");
    harness.prompt(&session.id, "Design the change").await;

    let loaded = harness.load(&session.id).await;
    let plan = loaded
        .plans
        .iter()
        .find(|plan| plan.status == PlanStatus::Submitted)
        .expect("提交的计划");
    let approved = harness
        .client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.id.clone(),
            plan_id: plan.plan_id.clone(),
            action: PlanAction::Execute,
            execution_engine: None,
        })
        .await
        .expect("approve plan in place");
    assert_eq!(approved.engine, SessionEngine::Standard);
    assert_eq!(
        approved.auto_prompt.as_deref(),
        Some("Execute the approved plan.")
    );
    let reloaded = harness.load(&session.id).await;
    assert_eq!(reloaded.session.engine, SessionEngine::Standard);
    assert_eq!(reloaded.session.model, "test/glm:off");
    harness
        .prompt(&session.id, "Execute the approved plan.")
        .await;
    let models: Vec<String> = upstream
        .received_requests()
        .await
        .expect("capture requests")
        .iter()
        .map(|request| {
            serde_json::from_slice::<serde_json::Value>(&request.body)
                .expect("请求体 JSON")["model"]
                .as_str()
                .expect("请求模型")
                .to_owned()
        })
        .collect();
    assert_eq!(models, ["glm", "glm"]);
    harness.close().await;
}

#[tokio::test]
async fn plan_new_session_inherits_current_model() {
    use serde_json::json;
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("请求 JSON");
            assert_eq!(body["model"], "planner");
            let response = if body.to_string().contains("<active-plan") {
                text_stream("子会话执行")
            } else {
                support::tool_stream(vec![
                    (
                        "save",
                        "plan",
                        json!({"title":"子会话方案", "markdown":"# 方案\n实现接口"}),
                    ),
                    ("exit", "exit_plan", json!({})),
                ])
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/planner:off",
        "test/gpt-test:off",
        "[providers.test.models.planner]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.executor]\ncontext_window=200000\nmax_output_tokens=4096\n",
    )
    .await;
    let id = h.session_plan(SessionEngine::Standard).await;
    h.prompt(&id, "设计子会话方案").await;
    let plan = h.load(&id).await.plans.pop().expect("待审计划");
    let approved = h
        .client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: id.clone(),
            plan_id: plan.plan_id.clone(),
            action: PlanAction::ExecuteNewSession,
            execution_engine: None,
        })
        .await
        .expect("新会话审批");
    assert_ne!(approved.session_id, id);
    let child = h.load(&approved.session_id).await;
    assert_eq!(child.session.engine, SessionEngine::Standard);
    assert_eq!(child.session.model, "test/planner:off");
    assert_eq!(child.plans[0].status, PlanStatus::Approved);
    assert_eq!(child.plans[0].path, format!("local://{}.md", plan.plan_id));
    assert!(child.entries[0].message.text().contains("<active-plan"));
    let parent = h.load(&id).await;
    assert!(parent.session.plan_active);
    assert_eq!(parent.session.model, "test/planner:off");
    assert_eq!(
        h.prompt(
            &approved.session_id,
            approved.auto_prompt.as_deref().expect("执行提示")
        )
        .await
        .message
        .text(),
        "子会话执行"
    );
    assert_eq!(parent.plans[0].status, PlanStatus::Approved);
    assert_eq!(h.load(&id).await.entries, parent.entries);
    h.close().await;
}

/// 保存配置后既有会话必须重新读取指令文件，而不是沿用首次提示的缓存。
#[tokio::test]
async fn config_save_invalidates_the_session_instruction_cache() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|_: &wiremock::Request| {
            ResponseTemplate::new(200).set_body_raw(text_stream("好的"), "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    std::fs::write(
        h.root.path().join("work/AGENTS.md"),
        "配置后指令：始终使用中文回答\n",
    )
    .expect("写指令文件");
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "第一次").await;
    let cwd = h.root.path().join("work").to_string_lossy().into_owned();
    let reply = h
        .client
        .call::<ConfigManage>(ConfigAction::Read {
            scope: ConfigScope::Global,
            cwd: cwd.clone(),
        })
        .await
        .expect("读取配置");
    let ConfigReply::Settings(document) = reply else {
        panic!("设置文档");
    };
    let mut edited = document.document.clone();
    edited["context_files"] = serde_json::json!({"enabled": true});
    h.client
        .call::<ConfigManage>(ConfigAction::Save {
            scope: ConfigScope::Global,
            cwd,
            document: edited,
            revision: document.revision,
        })
        .await
        .expect("保存配置");
    h.prompt(&id, "第二次").await;
    let requests = upstream.received_requests().await.expect("请求记录");
    let first: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert!(
        !first.to_string().contains("配置后指令"),
        "指令文件未启用时不得进入系统提示"
    );
    let second: serde_json::Value = serde_json::from_slice(&requests[1].body).unwrap();
    assert!(
        second.to_string().contains("配置后指令"),
        "保存配置后既有会话必须重新读取指令文件"
    );
    h.close().await;
}

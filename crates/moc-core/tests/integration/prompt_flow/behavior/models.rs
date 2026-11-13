use super::*;

/// 目标模型装得下当前上下文时不打扰用户，照常切换。
#[tokio::test]
async fn model_switch_reports_no_overflow_when_the_target_window_fits() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(complete(&support::soft_checkpoint("checkpoint")))
            }
        })
        .mount(&upstream)
        .await;

    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/glm:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.big]\ncontext_window=2000000\nmax_output_tokens=4096\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, "先垫一点历史").await;

    let switched = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/big:off".into(),
            overflow: SessionModelOverflowAction::Ask,
        })
        .await
        .expect("切换到宽窗口模型");
    assert!(switched.overflow.is_none(), "{:?}", switched.overflow);
    assert_eq!(switched.session.model, "test/big:off");
    assert!(switched.compaction_error.is_none());

    let updates = harness.updates.lock().clone();
    assert!(
        updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. })),
        "{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompacted { .. })),
        "{updates:?}"
    );
    harness.close().await;
}

/// `switch` 动作直接切换并回报缺口，交给后续请求的既有压缩逻辑。
#[tokio::test]
async fn model_switch_action_switch_reports_overflow_after_switching() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(text_stream("答复"), "text/event-stream")
            } else {
                ResponseTemplate::new(200)
                    .set_body_json(complete(&support::soft_checkpoint("checkpoint")))
            }
        })
        .mount(&upstream)
        .await;

    let harness = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/glm:off",
        "test/glm:off",
        "[providers.test.models.glm]\ncontext_window=100000\nmax_output_tokens=4096\n\
         [providers.test.models.small]\ncontext_window=32000\nmax_output_tokens=4096\n\
         [compaction]\nmethod='moc'\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
    harness.prompt(&session, &"上下文填充".repeat(8000)).await;

    let switched = harness
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session.clone(),
            selector: "test/small:off".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .expect("直接切换");
    assert_eq!(switched.session.model, "test/small:off");
    let overflow = switched.overflow.expect("回报容量缺口供前端告警");
    assert!(overflow.tokens > overflow.context_window);
    assert!(switched.compaction_error.is_none(), "switch 动作不压缩");

    let updates = harness.updates.lock().clone();
    assert!(
        updates.iter().any(|update| matches!(
            update,
            SessionUpdate::ModelChanged { model } if model == "test/small:off"
        )),
        "{updates:?}"
    );
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ContextCompacted { .. })),
        "{updates:?}"
    );
    harness.close().await;
}

/// 即使旧配置指定计划模型，进出计划与恢复会话都保持当前模型。
#[tokio::test]
async fn plan_mode_keeps_model_across_creation_transitions_and_reload() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&request.body).expect("请求体是 JSON");
            let model = body["model"].as_str().expect("请求带模型");
            ResponseTemplate::new(200).set_body_raw(
                text_stream(match model {
                    "glm" => "glm response",
                    "ds" => "ds response",
                    other => panic!("unexpected model {other}"),
                }),
                "text/event-stream",
            )
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
            plan_active: false,
            fast_mode: false,
        })
        .await
        .expect("create session");
    assert_eq!(session.model, "test/glm:off");
    harness.prompt(&session.id, "first").await;

    let planned = harness
        .client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: session.id.clone(),
            active: true,
        })
        .await
        .expect("enter plan mode");
    assert!(planned.plan_active);
    assert_eq!(planned.model, "test/glm:off", "进入计划模式不改变模型");
    harness.prompt(&session.id, "plan").await;

    // 卸载再恢复：计划状态与会话模型分别从 transcript 复原。
    harness
        .client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: session.id.clone(),
        })
        .await
        .expect("unload session");
    let resumed = harness.load(&session.id).await;
    assert!(resumed.session.plan_active);
    assert_eq!(resumed.session.model, "test/glm:off");
    assert!(
        !harness
            .updates
            .lock()
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. }))
    );
    harness
        .client
        .call::<moc_protocol::SessionSetModel>(moc_protocol::SessionSetModelParams {
            session_id: session.id.clone(),
            selector: "test/ds:off".into(),
            overflow: moc_protocol::SessionModelOverflowAction::Switch,
        })
        .await
        .unwrap();
    harness.updates.lock().clear();

    let exited = harness
        .client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: session.id.clone(),
            active: false,
        })
        .await
        .expect("leave plan mode");
    assert_eq!(
        exited.model, "test/ds:off",
        "退出计划模式保持计划期间主动选择的模型"
    );
    harness.prompt(&session.id, "implement").await;

    // 直接以计划模式创建也保持创建时模型，退出不会触发切换。
    let created = harness
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
    assert_eq!(created.model, "test/glm:off");
    let exited = harness
        .client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: created.id.clone(),
            active: false,
        })
        .await
        .expect("leave plan mode");
    assert_eq!(exited.model, "test/glm:off");

    let updates = harness.updates.lock().clone();
    assert!(
        !updates
            .iter()
            .any(|update| matches!(update, SessionUpdate::ModelChanged { .. })),
        "模式切换不得产生模型变化：{updates:?}"
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
    assert_eq!(models, ["glm", "glm", "ds"]);
    harness.close().await;
}

use super::*;
use moc_protocol::{ConfigGet, ConfigGetParams, PlanInfo, PlanStatus};
use tokio::sync::mpsc::{UnboundedReceiver, unbounded_channel};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

struct PlanHarness {
    _root: tempfile::TempDir,
    _upstream: MockServer,
    core: Arc<Core>,
    connection: EmbeddedConnection,
    app: App,
    events: UnboundedReceiver<AppEvent>,
}

impl PlanHarness {
    async fn new() -> Self {
        let upstream = MockServer::builder().start().await;
        Mock::given(method("POST")).respond_with(|request: &wiremock::Request| {
            let body: serde_json::Value = serde_json::from_slice(&request.body).expect("请求 JSON");
            let delta = if body.to_string().contains("<active-plan") {
                assert_eq!(body["model"], "executor");
                serde_json::json!({"role":"assistant", "content":"执行完成"})
            } else {
                serde_json::json!({"role":"assistant", "tool_calls":[
                    {"index":0,"id":"save","type":"function","function":{"name":"write_plan","arguments":serde_json::json!({"title":"审阅计划","markdown":"# 实施方案\n保持接口\n完成实现"}).to_string()}},
                    {"index":1,"id":"exit","type":"function","function":{"name":"exit_plan","arguments":"{}"}}
                ]})
            };
            let response = format!("data: {}\n\ndata: [DONE]\n\n", serde_json::json!({"id":"response", "model":body["model"], "choices":[{"index":0,"delta":delta,"finish_reason":if delta.get("tool_calls").is_some(){"tool_calls"}else{"stop"}}]}));
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        }).mount(&upstream).await;
        let root = tempfile::tempdir().expect("测试目录");
        std::fs::write(root.path().join("config.toml"), format!(
            "[models.roles]\ndefault='test/planner:off'\nplan='test/planner:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\nretry={{max_attempts=1}}\n\
             [providers.test.models.planner]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [providers.test.models.executor]\ncontext_window=200000\nmax_output_tokens=4096\n\
             [workspace]\nwatch=false\n[context_files]\nenabled=false\n[skills]\nenabled=false\n[compaction]\nauto=false\n", upstream.uri()
        )).expect("测试配置");
        let core = Core::new(moc_core::CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            overlays: Vec::new(),
            session_dir: Some(root.path().join("sessions")),
            client: Default::default(),
            ptc_worker: None,
        })
        .expect("测试内核");
        let (tx, events) = unbounded_channel();
        let connection = connect_embedded(core.clone(), tx.clone())
            .await
            .expect("协议连接");
        let session = connection
            .client
            .call::<SessionNew>(SessionNewParams {
                cwd: None,
                model: None,
                title: None,
                mode: SessionMode::Plan,
            })
            .await
            .expect("计划会话");
        let mut app = App::new(Theme::global(), tx);
        app.models = connection
            .client
            .call::<ConfigGet>(ConfigGetParams::default())
            .await
            .expect("模型目录")
            .models;
        app.session = Some(session);
        Self {
            _root: root,
            _upstream: upstream,
            core,
            connection,
            app,
            events,
        }
    }

    async fn write_and_review(&mut self) -> PlanInfo {
        let id = self.app.session.as_ref().expect("会话").id.clone();
        self.connection
            .client
            .call::<moc_protocol::SessionPrompt>(moc_protocol::PromptParams {
                session_id: id.clone(),
                input: vec![InputPart::Text {
                    text: "设计方案".into(),
                }],
                mode: None,
                agent_message: None,
            })
            .await
            .expect("计划轮次");
        self.updates();
        self.connection
            .client
            .call::<SessionLoad>(SessionLoadParams {
                session_id: id,
                max_entries: None,
            })
            .await
            .expect("计划加载")
            .plans
            .pop()
            .expect("实体计划")
    }

    fn updates(&mut self) {
        while let Ok(params) = self.connection.updates.try_recv() {
            apply_update_and_refresh(
                &mut self.app,
                &self.connection,
                params.session_id,
                params.update,
            );
        }
    }

    fn key(&mut self, code: KeyCode) {
        handle_modal_key(&mut self.app, &self.connection, KeyModifiers::NONE, code);
    }

    async fn approval(&mut self) {
        loop {
            let event = tokio::time::timeout(std::time::Duration::from_secs(3), self.events.recv())
                .await
                .expect("审批回执到达")
                .expect("事件通道");
            if matches!(event, AppEvent::PlanApproval(_)) {
                self.updates();
                crate::interaction::handle_event(&mut self.app, &self.connection, event);
                break;
            }
        }
    }

    async fn close(self) {
        self.connection.shutdown().await;
        self.core.shutdown().await;
    }
}

#[tokio::test]
async fn plan_review_model_picker_changes_execution_model_without_switching_session() {
    let mut h = PlanHarness::new().await;
    let plan = h.write_and_review().await;
    let original = h.app.session.as_ref().expect("会话").clone();
    let id = original.id.clone();
    let before = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: id.clone(),
            max_entries: None,
        })
        .await
        .expect("历史")
        .entries;
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.selected = 2;
        state.scroll_offset = 1;
        state.error = Some("保留错误状态".into());
    }
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制审阅");
    let area = h.app.plan_model_area;
    assert!(area.width > 0);
    let rendered = (area.x..area.right())
        .map(|x| {
            terminal
                .backend()
                .buffer()
                .cell((x, area.y))
                .expect("模型文字单元格")
                .symbol()
        })
        .collect::<String>();
    assert_eq!(rendered, crate::app::model_selector_label(&original.model));
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(matches!(
        h.app.modal,
        Some(Modal::Model {
            target: crate::modal::ModelTarget::PlanReview(_),
            ..
        })
    ));
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: area.x,
            row: area.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(matches!(
        h.app.modal,
        Some(Modal::Model {
            target: crate::modal::ModelTarget::PlanReview(_),
            ..
        })
    ));
    for ch in "executor".chars() {
        h.key(KeyCode::Char(ch));
    }
    h.key(KeyCode::Enter);
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("恢复审阅窗口")
    };
    assert_eq!(state.execution_model, "test/executor:off");
    assert_eq!(state.selected, 2);
    assert_eq!(state.scroll_offset, 1);
    assert_eq!(state.error.as_deref(), Some("保留错误状态"));
    assert_eq!(state.path, plan.path);
    assert_eq!(h.app.session.as_ref().expect("会话"), &original);
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: id.clone(),
            max_entries: None,
        })
        .await
        .expect("历史");
    assert_eq!(loaded.entries, before);
    assert_eq!(loaded.session.model, original.model);
    handle_modal_key(
        &mut h.app,
        &h.connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('p'),
    );
    h.key(KeyCode::Esc);
    assert!(
        matches!(&h.app.modal, Some(Modal::Plan { state }) if state.execution_model == "test/executor:off" && state.scroll_offset == 1)
    );
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.selected = 0;
    }
    h.key(KeyCode::Enter);
    h.approval().await;
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: id,
            max_entries: None,
        })
        .await
        .expect("审批后会话");
    assert_eq!(loaded.session.model, "test/executor:off");
    assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
    h.close().await;
}

#[tokio::test]
async fn plan_review_approval_loads_new_session() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let parent = h.app.session.as_ref().expect("会话").id.clone();
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.execution_model = "test/missing:off".into();
    }
    h.key(KeyCode::Enter);
    h.approval().await;
    assert!(
        matches!(&h.app.modal, Some(Modal::Plan { state }) if !state.submitting && state.error.is_some() && state.execution_model == "test/missing:off")
    );
    assert_eq!(
        h.app.session.as_ref().expect("会话").mode,
        SessionMode::Plan
    );
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.execution_model = "test/executor:off".into();
        state.selected = 2;
    }
    h.key(KeyCode::Enter);
    h.approval().await;
    assert!(h.app.modal.is_none());
    assert_eq!(
        h.app.session.as_ref().expect("审批回执后仍等待加载").id,
        parent
    );
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), h.events.recv())
            .await
            .expect("执行会话加载")
            .expect("事件");
        if matches!(event, AppEvent::PlanExecutionLoaded { .. }) {
            crate::interaction::handle_event(&mut h.app, &h.connection, event);
            break;
        }
    }
    let child = h.app.session.as_ref().expect("执行会话").id.clone();
    assert_ne!(child, parent);
    assert_eq!(
        h.app.session.as_ref().expect("执行会话").model,
        "test/executor:off"
    );
    assert_eq!(
        h.app.session.as_ref().expect("执行会话").mode,
        SessionMode::Normal
    );
    assert!(h.app.blocks.iter().any(|entry| matches!(&entry.block, crate::transcript::Block::User { text } if text.contains("<active-plan"))));
    assert!(h.app.inflight_prompts.contains(&child));
    assert!(!h.app.inflight_prompts.contains(&parent));
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), h.events.recv())
            .await
            .expect("执行完成")
            .expect("事件");
        if let AppEvent::PromptReturned { session, result } = event {
            assert_eq!(session, child);
            result.expect("执行成功");
            break;
        }
    }
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: child,
            max_entries: None,
        })
        .await
        .expect("执行历史");
    assert_eq!(
        loaded.entries.last().expect("执行回复").message.text(),
        "执行完成"
    );
    let parent_loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: parent,
            max_entries: None,
        })
        .await
        .expect("父会话");
    assert_eq!(parent_loaded.session.mode, SessionMode::Plan);
    assert!(
        !parent_loaded
            .entries
            .iter()
            .any(|entry| entry.message.text() == "Execute the approved plan.")
    );
    h.close().await;
}

#[tokio::test]
async fn plan_review_continue_keeps_plan_mode_and_confirm_plan_reopens() {
    let mut h = PlanHarness::new().await;
    let plan = h.write_and_review().await;
    let session = h.app.session.as_ref().expect("计划会话").clone();
    h.app.prompt.insert_str_raw("尚未提交的输入");
    for _ in 0..3 {
        h.key(KeyCode::Right);
    }
    h.key(KeyCode::Enter);
    assert!(h.app.modal.is_none());
    assert_eq!(h.app.session.as_ref().expect("会话"), &session);
    assert_eq!(h.app.prompt.text, "尚未提交的输入");
    assert!(!h.app.prompt_running);
    assert!(h.app.inflight_prompts.is_empty());
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            session_id: session.id.clone(),
            max_entries: None,
        })
        .await
        .expect("待审状态");
    assert_eq!(loaded.plans[0], plan);
    assert_eq!(loaded.plans[0].status, PlanStatus::Submitted);
    assert_eq!(
        h._upstream.received_requests().await.expect("请求").len(),
        1
    );
    h.connection
        .client
        .call::<moc_protocol::SessionUnload>(moc_protocol::SessionUnloadParams {
            session_id: session.id.clone(),
        })
        .await
        .expect("卸载后恢复实体计划");
    execute_command(&mut h.app, &h.connection, "/confirm-plan".into());
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), h.events.recv())
            .await
            .expect("权威计划载入")
            .expect("事件");
        if let AppEvent::PlanReview(result) = event {
            apply_plan_review(&mut h.app, result);
            break;
        }
    }
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("重新打开审阅窗口")
    };
    assert_eq!(state.plan_id, plan.plan_id);
    assert_eq!(state.path, plan.path);
    assert_eq!(state.markdown, plan.markdown);
    assert_eq!(state.execution_model, session.model);
    assert_eq!(h.app.prompt.text, "尚未提交的输入");
    assert!(!h.app.prompt_running);
    h.key(KeyCode::Esc);
    h.connection
        .client
        .call::<SessionSetMode>(SessionSetModeParams {
            session_id: session.id,
            mode: SessionMode::Normal,
        })
        .await
        .expect("离开计划模式");
    h.updates();
    execute_command(&mut h.app, &h.connection, "/confirm-plan".into());
    assert!(h.app.modal.is_none());
    assert_eq!(
        h.app.session.as_ref().expect("标准会话").mode,
        SessionMode::Normal
    );
    assert_eq!(
        h.app.notice.as_deref().expect("模式门控提示"),
        "只有计划模式可以重新审阅计划"
    );
    let empty = h
        .connection
        .client
        .call::<SessionNew>(SessionNewParams {
            cwd: None,
            model: None,
            title: None,
            mode: SessionMode::Plan,
        })
        .await
        .expect("尚无计划文件的会话");
    h.app.switch_session(empty, Vec::new());
    execute_command(&mut h.app, &h.connection, "/confirm-plan".into());
    loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), h.events.recv())
            .await
            .expect("空计划载入")
            .expect("事件");
        if let AppEvent::PlanReview(result) = event {
            assert!(result.is_err());
            apply_plan_review(&mut h.app, result);
            break;
        }
    }
    assert!(h.app.modal.is_none());
    assert!(
        h.app
            .notice
            .as_deref()
            .expect("空计划提示")
            .contains("没有已写入的待审计划")
    );
    assert!(!h.app.prompt_running);
    assert!(h.app.inflight_prompts.is_empty());
    h.app.session = None;
    execute_command(&mut h.app, &h.connection, "/confirm-plan".into());
    assert_eq!(
        h.app.notice.as_deref().expect("无会话提示"),
        "当前没有活动会话"
    );
    assert_eq!(
        h._upstream
            .received_requests()
            .await
            .expect("没有新增模型调用")
            .len(),
        1
    );
    h.close().await;
}

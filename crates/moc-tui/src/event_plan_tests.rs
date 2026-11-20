use super::*;
use crate::event::commands::execute_command;
use crate::event::modal_key::handle_modal_key;
use crate::event::mouse::handle_mouse_with_connection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::InputPart;
use moc_protocol::SessionLoad;
use moc_protocol::SessionLoadParams;
use moc_protocol::SessionModelOverflowAction;
use moc_protocol::SessionNew;
use moc_protocol::SessionNewParams;
use moc_protocol::SessionPermission;
use moc_protocol::SessionSetModel;
use moc_protocol::SessionSetModelParams;
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
            if body["stream"] != true {
                // 压缩摘要走非流式 complete：回一个带固定段落的 checkpoint。
                let mut checkpoint = String::from("计划和关键决策：保留接口。\n");
                for section in [
                    "Primary Request and Intent",
                    "Key Technical Concepts",
                    "Decisions and Rejected Options",
                    "Files and Code",
                    "Errors and Fixes",
                    "Pending Jobs",
                    "Plan Status",
                    "Current Work",
                    "Next Step",
                    "Critical Context",
                ] {
                    checkpoint.push_str("## ");
                    checkpoint.push_str(section);
                    checkpoint.push('\n');
                }
                return ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "id":"summary","model":body["model"],
                    "choices":[{"index":0,"message":{"role":"assistant","content":checkpoint},"finish_reason":"stop"}]
                }));
            }
            let delta = if body.to_string().contains("<active-plan") {
                assert_eq!(body["model"], "planner");
                serde_json::json!({"role":"assistant", "content":"执行完成"})
            } else {
                serde_json::json!({"role":"assistant", "tool_calls":[
                    {"index":0,"id":"save","type":"function","function":{"name":"plan","arguments":serde_json::json!({"title":"审阅计划","markdown":"# 实施方案\n保持接口\n完成实现"}).to_string()}},
                    {"index":1,"id":"exit","type":"function","function":{"name":"exit_plan","arguments":"{}"}}
                ]})
            };
            let response = format!("data: {}\n\ndata: [DONE]\n\n", serde_json::json!({"id":"response", "model":body["model"], "choices":[{"index":0,"delta":delta,"finish_reason":if delta.get("tool_calls").is_some(){"tool_calls"}else{"stop"}}]}));
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        }).mount(&upstream).await;
        let root = tempfile::tempdir().expect("测试目录");
        std::fs::write(
            root.path().join("config.toml"),
            format!(
                "language='zh'\n[models.roles]\ndefault='test/planner:off'\nplan='test/planner:off'\n\
             [providers.test]\napi='openai_chat'\nbase_url='{}/v1'\nretry={{max_attempts=1}}\n\
             [providers.test.models.planner]\ncontext_window=100000\nmax_output_tokens=4096\n\
             [providers.test.models.executor]\ncontext_window=200000\nmax_output_tokens=4096\n\
             [context_files]\nenabled=false\n[skills]\nenabled=false\n[compaction]\nauto=false\n",
                upstream.uri()
            ),
        )
        .expect("测试配置");
        let core = Core::new(moc_core::CoreConfig {
            home: root.path().to_owned(),
            cwd: root.path().to_owned(),
            user_home: Some(root.path().join("userhome")),
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
                engine: SessionEngine::Standard,
                plan_active: true,
                fast_mode: false,
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
                goal_round: None,
                max_turns: None,
            })
            .await
            .expect("计划轮次");
        self.updates();
        self.connection
            .client
            .call::<SessionLoad>(SessionLoadParams {
                omit_auxiliary: false,
                session_id: id,
                max_entries: None,
                read_only: false,
            })
            .await
            .expect("计划加载")
            .plans
            .pop()
            .expect("实体计划")
    }

    fn updates(&mut self) {
        while let Ok(params) = self.connection.updates.try_recv() {
            apply_ui_update(&mut self.app, &self.connection, params);
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
async fn plan_command_submits_arguments_and_bare_command_only_toggles() {
    let mut h = PlanHarness::new().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"计划答复\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
            "text/event-stream",
        ))
        .with_priority(1)
        .mount(&h._upstream)
        .await;
    let session_id = h.app.session.as_ref().unwrap().id.clone();
    h.app.session = Some(
        h.connection
            .client
            .call::<moc_protocol::SessionPlanMode>(moc_protocol::SessionPlanModeParams {
                session_id: session_id.clone(),
                active: false,
            })
            .await
            .unwrap(),
    );
    h.updates();

    let prompts = ["设计第一步\n再写第二步", "补充风险清单"];
    for text in prompts {
        h.app.prompt.insert_str_raw(&format!("/plan  {text}  "));
        crate::event::keyboard::commit_prompt(&mut h.app, &h.connection);
        assert!(h.app.prompt_running, "切换模式与提交提示之间也必须占住会话");
        let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let event = h.events.recv().await.unwrap();
                if let AppEvent::PromptReturned { result, .. } = &event {
                    result.as_ref().expect("带参数的计划命令应完成提示");
                    break event;
                }
            }
        })
        .await
        .expect("提示回执");
        h.updates();
        crate::interaction::handle_event(&mut h.app, &h.connection, event);
        assert!(h.app.session.as_ref().unwrap().plan_active);
        assert!(!h.app.prompt_running);
    }

    for active in [false, true] {
        execute_command(&mut h.app, &h.connection, "/plan".into());
        tokio::time::timeout(std::time::Duration::from_secs(3), async {
            loop {
                let update = h.connection.updates.recv().await.unwrap();
                let changed = matches!(&update, crate::protocol::UiUpdate::Session(params)
                    if matches!(params.update, moc_protocol::SessionUpdate::PlanModeChanged { active: value } if value == active));
                apply_ui_update(&mut h.app, &h.connection, update);
                if changed { break; }
            }
        }).await.expect("无参数命令应切换模式");
        assert_eq!(h.app.session.as_ref().unwrap().plan_active, active);
    }

    let requests = h._upstream.received_requests().await.unwrap();
    let bodies: Vec<serde_json::Value> = requests
        .iter()
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .filter(|body: &serde_json::Value| body["stream"] == true)
        .collect();
    assert_eq!(bodies.len(), prompts.len(), "无参数命令不应请求模型");
    for (body, text) in bodies.iter().zip(prompts) {
        assert!(
            body["tools"]
                .as_array()
                .unwrap()
                .iter()
                .any(|tool| tool["function"]["name"] == "exit_plan"),
            "提示必须在计划模式下发出"
        );
        // 运行时提醒可能单独占据最后一条 user 消息；查找真正的用户输入。
        assert!(
            body["messages"].as_array().unwrap().iter().any(|message| {
                message["role"] == "user" && message["content"].as_str() == Some(text)
            }),
            "提示词与内部换行必须完整传给模型"
        );
    }
    h.close().await;
}

#[tokio::test]
async fn plan_command_does_not_send_prompt_when_mode_change_fails() {
    let mut h = PlanHarness::new().await;
    h.app.session.as_mut().unwrap().id = "ses_missing".into();
    execute_command(&mut h.app, &h.connection, "/plan 不应发出的提示".into());
    let event = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let event = h.events.recv().await.unwrap();
            if let AppEvent::PromptReturned { result, .. } = &event {
                assert!(result.is_err(), "切换失败必须回报，而不是继续请求模型");
                break event;
            }
        }
    })
    .await
    .expect("失败回执");
    crate::interaction::handle_event(&mut h.app, &h.connection, event);
    assert!(!h.app.prompt_running);
    assert!(h.app.inflight_prompts.is_empty());
    assert!(h._upstream.received_requests().await.unwrap().is_empty());
    h.close().await;
}

#[tokio::test]
async fn plan_review_preserves_model_and_loaded_history() {
    let mut h = PlanHarness::new().await;
    let plan = h.write_and_review().await;
    let original = h.app.session.as_ref().expect("会话").clone();
    let id = original.id.clone();
    let before = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("历史")
        .entries;
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.focus = crate::modal::PlanControl::RunNew;
        state.scroll_offset = 1;
        state.error = Some("保留错误状态".into());
    }
    handle_modal_key(
        &mut h.app,
        &h.connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('p'),
    );
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("模型快捷键不得切走审阅窗口")
    };
    assert_eq!(state.focus, crate::modal::PlanControl::RunNew);
    assert_eq!(state.scroll_offset, 1);
    assert_eq!(state.error.as_deref(), Some("保留错误状态"));
    assert_eq!(state.path, plan.path);
    assert_eq!(h.app.session.as_ref().expect("会话"), &original);
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: id.clone(),
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("历史");
    assert_eq!(loaded.entries, before);
    let paged = load_display_session(&h.connection.client, id.clone(), false)
        .await
        .expect("分页恢复计划");
    assert_eq!(paged.plans, loaded.plans);
    assert_eq!(paged.entries, loaded.entries);
    assert_eq!(loaded.session.model, original.model);
    assert!(matches!(&h.app.modal, Some(Modal::Plan { state }) if state.scroll_offset == 1));
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.focus = crate::modal::PlanControl::Run;
    }
    h.key(KeyCode::Enter);
    h.approval().await;
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("审批后会话");
    assert_eq!(loaded.session.model, original.model);
    assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
    // 单独的历史会话只写本地日志，不把大文本送给模型或终端渲染器。
    let table = moc_core::session::SessionTable::new(h._root.path().join("sessions"), true);
    let history = table
        .create(
            "/tmp",
            "test/planner:off",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            moc_core::session::entries::SessionOrigin::root(),
            false,
        )
        .await
        .expect("历史会话");
    let text = "文🙂\0".repeat(150_000);
    table
        .append_message(
            &history.id,
            moc_protocol::Message::user_text(&text),
            None,
            None,
        )
        .await
        .unwrap();
    let phases = vec![moc_protocol::TodoPhase {
        phase: "执行".into(),
        items: vec![moc_protocol::TodoItem {
            task: "完整任务".into(),
            status: moc_protocol::TodoStatus::Pending,
            reason: None,
        }],
    }];
    table
        .append(
            &history.id,
            moc_core::session::entries::EntryPayload::Todos {
                phases: phases.clone(),
            },
        )
        .await
        .unwrap();
    table
        .append(
            &history.id,
            moc_core::session::entries::EntryPayload::GoalChange {
                goal: Some(moc_core::session::entries::GoalSnapshot {
                    id: "goal-history".into(),
                    revision: 1,
                    objective: "完整目标".into(),
                    phase: moc_protocol::GoalPhase::Active,
                    blocked_reason: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                }),
            },
        )
        .await
        .unwrap();
    drop(table);
    let restored = load_display_session(&h.connection.client, history.id, true)
        .await
        .expect("跨块恢复历史");
    assert_eq!(restored.entries[0].message.text(), text);
    assert_eq!(restored.todos, phases);
    assert_eq!(restored.goal.unwrap().objective, "完整目标");
    h.close().await;
}

/// 审阅卡片的动作区按钮可点：点「继续计划」等同激活 Resume（关闭窗口、保持待审）。
#[tokio::test]
async fn plan_review_action_rows_accept_mouse_clicks() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制审阅");
    let (_, rect) = h
        .app
        .plan_control_hits
        .iter()
        .find(|(control, _)| *control == crate::modal::PlanControl::Resume)
        .copied()
        .expect("「继续计划」按钮可点");
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(h.app.modal.is_none(), "点「继续计划」关闭审阅窗口");
    h.close().await;
}

/// 计划标签 gutter 点 `+` 直接打开批注编辑器，行区间等于该块的源码区间。
#[tokio::test]
async fn plan_gutter_click_opens_editor_at_block() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制审阅");
    let body = h.app.plan_body_area;
    let (row_index, (start, end)) = h
        .app
        .plan_line_sources
        .iter()
        .enumerate()
        .find_map(|(index, source)| source.map(|range| (index, range)))
        .expect("计划正文应有可映射的源码块");
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: body.x,
            row: body.y + u16::try_from(row_index).unwrap_or(0),
            modifiers: KeyModifiers::NONE,
        },
    );
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("审阅窗口仍在");
    };
    let editor = state.editor.as_ref().expect("gutter 点开编辑器");
    assert_eq!((editor.start_line, editor.end_line), (start, end));
    h.close().await;
}

/// 计划标签正文按下并拖到另一块：选区是两个块源码区间的并集。
#[tokio::test]
async fn plan_body_drag_selects_block_union() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制审阅");
    let body = h.app.plan_body_area;
    // 找两个不同块的正文行：首块与最后一个不同区间各取一行。
    let first = h
        .app
        .plan_line_sources
        .iter()
        .copied()
        .flatten()
        .next()
        .expect("正文有块");
    let (last_index, last) = h
        .app
        .plan_line_sources
        .iter()
        .enumerate()
        .find_map(|(index, source)| match *source {
            Some(range) if range != first => Some((index, range)),
            _ => None,
        })
        .expect("计划正文至少两个块");
    let press_row = body.y + u16::try_from(first_index(&h.app, first)).unwrap_or(0);
    let drag_row = body.y + u16::try_from(last_index).unwrap_or(0);
    for (kind, row) in [
        (
            crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            press_row,
        ),
        (
            crossterm::event::MouseEventKind::Drag(crossterm::event::MouseButton::Left),
            drag_row,
        ),
    ] {
        handle_mouse_with_connection(
            &mut h.app,
            &h.connection,
            crossterm::event::MouseEvent {
                kind,
                column: body.x + 4,
                row,
                modifiers: KeyModifiers::NONE,
            },
        );
    }
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("审阅窗口仍在");
    };
    assert_eq!(
        (state.selection().0 + 1, state.selection().1 + 1),
        (first.0.min(last.0), first.1.max(last.1)),
        "拖选 = 两块源码区间并集"
    );
    // Esc 两段语义：先清选区，再按一次才关闭窗口。
    h.key(KeyCode::Esc);
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("第一次 Esc 只清选区，不关闭审阅窗口");
    };
    assert!(!state.has_selection(), "第一次 Esc 清掉选区");
    h.key(KeyCode::Esc);
    assert!(h.app.modal.is_none(), "第二次 Esc 关闭审阅窗口");
    h.close().await;
}

/// 首个映射行所在的渲染行下标。
fn first_index(app: &App, block: (usize, usize)) -> usize {
    app.plan_line_sources
        .iter()
        .position(|source| *source == Some(block))
        .unwrap_or(0)
}

/// 悬停动作区按钮即移动焦点；单击「执行」走审批路径。
#[tokio::test]
async fn plan_button_hover_focuses_and_click_approves() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制审阅");
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.focus = crate::modal::PlanControl::Resume;
    }
    let (_, rect) = h
        .app
        .plan_control_hits
        .iter()
        .find(|(control, _)| *control == crate::modal::PlanControl::Run)
        .copied()
        .expect("「执行」按钮可点");
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Moved,
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("审阅窗口仍在");
    };
    assert_eq!(state.focus, crate::modal::PlanControl::Run, "悬停移焦点");
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    h.approval().await;
    assert!(h.app.modal.is_none(), "审批完成关闭审阅");
    h.close().await;
}

/// 模型芯片停放计划审阅；选择器 Esc 后还原且草稿保留。
#[tokio::test]
async fn plan_model_chip_parks_and_esc_restores() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制审阅");
    // 先落一条草稿，验证还原后仍在。
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.select_lines(1, 1);
        state.open_editor();
        state
            .editor
            .as_mut()
            .expect("编辑器")
            .body
            .insert_str_raw("草稿批注");
        state.save_editor();
    }
    let (_, rect) = h
        .app
        .plan_control_hits
        .iter()
        .find(|(control, _)| *control == crate::modal::PlanControl::Model)
        .copied()
        .expect("模型芯片可点");
    handle_mouse_with_connection(
        &mut h.app,
        &h.connection,
        crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        },
    );
    assert!(h.app.parked_plan.is_some(), "计划被停放");
    assert!(
        matches!(h.app.modal, Some(Modal::Model { .. })),
        "模型选择器打开"
    );
    h.key(KeyCode::Esc);
    crate::event::restore_parked_plan(&mut h.app);
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("Esc 后还原计划审阅");
    };
    assert_eq!(state.drafts.len(), 1, "还原后草稿保留");
    h.close().await;
}

/// `n` 打开新会话模型选择器：计划停放、选中写回 `execution_model`、会话模型不变。
#[tokio::test]
async fn plan_new_model_chip_picks_execution_model() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    h.key(KeyCode::Char('n'));
    assert!(h.app.parked_plan.is_some(), "计划被停放");
    {
        let Some(Modal::Model {
            entries,
            picker,
            target,
            ..
        }) = h.app.modal.as_mut()
        else {
            panic!("n 打开模型选择器");
        };
        assert!(
            matches!(target, crate::modal::ModelTarget::PlanExecution),
            "选择器目标是新会话模型"
        );
        picker.selected = entries
            .iter()
            .position(|entry| entry.selector == "test/executor")
            .expect("候选里有 executor");
    }
    h.key(KeyCode::Enter);
    crate::event::restore_parked_plan(&mut h.app);
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("确认后还原计划审阅");
    };
    assert_eq!(state.execution_model.as_deref(), Some("test/executor:off"));
    assert_eq!(
        h.app.session.as_ref().expect("会话").model,
        "test/planner:off"
    );
    h.close().await;
}

/// 引擎芯片下拉：打开 → 移动 → 确认，`execution_engine` 变成选中项。
#[tokio::test]
async fn plan_engine_menu_applies_selection() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let items: Vec<_> = h.app.engines().collect();
    assert!(items.len() > 1, "引擎候选至少两项");
    h.key(KeyCode::Char('e'));
    {
        let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
            panic!("审阅窗口仍在");
        };
        assert!(state.engine_menu.is_some(), "e 打开引擎下拉");
    }
    h.key(KeyCode::Down);
    h.key(KeyCode::Enter);
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("审阅窗口仍在");
    };
    assert!(state.engine_menu.is_none());
    assert_eq!(state.execution_engine, items[1]);
    h.close().await;
}

#[tokio::test]
async fn plan_review_approval_loads_new_session() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let parent = h.app.session.as_ref().expect("会话").id.clone();
    if let Some(Modal::Plan { state }) = h.app.modal.as_mut() {
        state.focus = crate::modal::PlanControl::RunNew;
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
        "test/planner:off"
    );
    assert_eq!(
        h.app.session.as_ref().expect("执行会话").engine,
        SessionEngine::Standard
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
            omit_auxiliary: false,
            session_id: child,
            max_entries: None,
            read_only: false,
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
            omit_auxiliary: false,
            session_id: parent,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("父会话");
    assert!(parent_loaded.session.plan_active);
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
    for _ in 0..4 {
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
            omit_auxiliary: false,
            session_id: session.id.clone(),
            max_entries: None,
            read_only: false,
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
            apply_plan_review(&mut h.app, &h.connection, result);
            break;
        }
    }
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("重新打开审阅窗口")
    };
    assert_eq!(state.plan_id, plan.plan_id);
    assert_eq!(state.path, plan.path);
    assert_eq!(state.markdown, plan.markdown);
    assert_eq!(h.app.session.as_ref().unwrap().model, session.model);
    assert_eq!(h.app.prompt.text, "尚未提交的输入");
    assert!(!h.app.prompt_running);
    h.key(KeyCode::Esc);
    h.connection
        .client
        .call::<moc_protocol::SessionPlanMode>(moc_protocol::SessionPlanModeParams {
            session_id: session.id,
            active: false,
        })
        .await
        .expect("离开计划状态");
    h.updates();
    execute_command(&mut h.app, &h.connection, "/confirm-plan".into());
    assert!(h.app.modal.is_none());
    assert!(!h.app.session.as_ref().expect("标准会话").plan_active);
    assert_eq!(
        h.app.notice.as_deref().expect("状态门控提示"),
        "只有计划状态可以重新审阅计划"
    );
    let empty = h
        .connection
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
            apply_plan_review(&mut h.app, &h.connection, result);
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

/// 批注 → 修订提示 → 自动发送：选区、编辑器、suggestion 围栏与提示内容都走真实 core。
#[tokio::test]
async fn plan_annotate_flow_sends_revision_prompt() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;

    // a 进入批注视图，v 选行，c 写批注。
    h.key(KeyCode::Char('a'));
    h.key(KeyCode::Down);
    h.key(KeyCode::Char('v'));
    h.key(KeyCode::Down);
    h.key(KeyCode::Char('c'));
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("审阅窗口仍在");
    };
    assert_eq!(state.tab, crate::modal::PlanTab::Source);
    let editor = state.editor.as_ref().expect("编辑器已打开");
    assert_eq!((editor.start_line, editor.end_line), (2, 3));

    for character in "请写具体".chars() {
        h.key(KeyCode::Char(character));
    }
    handle_modal_key(
        &mut h.app,
        &h.connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('g'),
    );
    handle_modal_key(
        &mut h.app,
        &h.connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('s'),
    );
    let Some(Modal::Plan { state }) = h.app.modal.as_ref() else {
        panic!("保存后审阅窗口仍在");
    };
    assert!(state.editor.is_none());
    assert_eq!(state.drafts.len(), 1);
    assert!(
        state.drafts[0].body.contains("```suggestion"),
        "Ctrl+G 预填建议围栏"
    );

    // s 提交给 core，等待修订提示回执。
    h.key(KeyCode::Char('s'));
    let prompt = loop {
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), h.events.recv())
            .await
            .expect("修订回执到达")
            .expect("事件通道");
        if let AppEvent::PlanRevised(result) = event {
            break result.expect("批注提交成功").prompt;
        }
    };
    assert!(prompt.contains("### 1. Lines 2-3"), "{prompt}");
    assert!(prompt.contains("> 保持接口"), "{prompt}");
    assert!(prompt.contains("请写具体"), "{prompt}");
    assert!(prompt.contains("```suggestion"), "{prompt}");
    h.close().await;
}

/// 修订回执到达后，审阅窗口关闭并把 core 生成的提示当普通提示发出去。
#[tokio::test]
async fn plan_revised_closes_review_and_sends_prompt() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    let session = h.app.session.as_ref().expect("会话").id.clone();
    crate::event::apply_plan_revised(
        &mut h.app,
        &h.connection,
        Ok(moc_protocol::methods::SessionPlanReviseResult {
            session_id: session.clone(),
            engine: SessionEngine::Standard,
            plan_active: true,
            prompt: "已根据批注修订".into(),
        }),
    );
    assert!(h.app.modal.is_none());
    assert!(h.app.inflight_prompts.contains(&session));
    assert!(h.app.prompt_running);
    assert!(h.app.blocks.iter().any(|entry| match &entry.block {
        crate::transcript::Block::User { text } => text == "已根据批注修订",
        _ => false,
    }));
    h.close().await;
}

/// 批注视图与编辑器绘制：正文几何与行列映射可用，光标落在编辑框里。
#[tokio::test]
async fn plan_annotate_pane_draws_body_and_editor() {
    let mut h = PlanHarness::new().await;
    h.write_and_review().await;
    h.key(KeyCode::Char('a'));
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 32)).expect("测试终端");
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制批注视图");
    assert!(!h.app.plan_row_map.is_empty());
    assert_eq!(h.app.plan_row_map[0], 0);
    assert!(h.app.plan_body_area.height > 0);
    h.key(KeyCode::Char('c'));
    terminal
        .draw(|frame| render::draw(frame, &mut h.app, &mut view))
        .expect("绘制批注编辑器");
    assert!(matches!(
        h.app.modal,
        Some(Modal::Plan { ref state }) if state.editor.is_some()
    ));
    h.close().await;
}

/// unattended：计划提交不弹审阅，前端直接代用户批准「压缩并执行」。
#[tokio::test]
async fn unattended_autonomy_auto_approves_submitted_plan() {
    let mut h = PlanHarness::new().await;
    let session_id = h.app.session.as_ref().expect("会话").id.clone();
    h.connection
        .client
        .call::<moc_protocol::SessionSetAutonomy>(moc_protocol::SessionSetAutonomyParams {
            session_id: session_id.clone(),
            autonomy: moc_protocol::SessionAutonomy::Unattended,
        })
        .await
        .expect("切到 unattended");
    h.updates();
    assert_eq!(
        h.app.session.as_ref().expect("会话").autonomy,
        moc_protocol::SessionAutonomy::Unattended
    );
    // 自动批准用当前会话模型作执行模型：先切到 executor，对齐 mock 的执行断言。
    h.connection
        .client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: session_id.clone(),
            selector: "test/executor:off".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .expect("切执行模型");
    h.updates();

    let plan = h.write_and_review().await;
    assert!(
        !matches!(h.app.modal, Some(Modal::Plan { .. })),
        "unattended 不得弹审阅窗"
    );
    h.approval().await;

    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("批准后加载");
    assert_eq!(loaded.plans[0].plan_id, plan.plan_id);
    assert_eq!(
        loaded.plans[0].status,
        PlanStatus::Approved,
        "notice: {:?}",
        h.app.notice
    );
    assert!(!loaded.session.plan_active);
    h.close().await;
}

/// don't ask：代答范围只到 ask 工具——计划提交仍弹审阅，不自动批准。
#[tokio::test]
async fn dont_ask_autonomy_still_opens_plan_review() {
    let mut h = PlanHarness::new().await;
    let session_id = h.app.session.as_ref().expect("会话").id.clone();
    h.connection
        .client
        .call::<moc_protocol::SessionSetAutonomy>(moc_protocol::SessionSetAutonomyParams {
            session_id: session_id.clone(),
            autonomy: moc_protocol::SessionAutonomy::DontAsk,
        })
        .await
        .expect("切到 don't ask");
    h.updates();

    let plan = h.write_and_review().await;
    assert!(
        matches!(h.app.modal, Some(Modal::Plan { .. })),
        "don't ask 不得自动批准计划"
    );
    let loaded = h
        .connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id,
            max_entries: None,
            read_only: false,
        })
        .await
        .expect("提交后加载");
    assert_eq!(loaded.plans[0].plan_id, plan.plan_id);
    assert_eq!(loaded.plans[0].status, PlanStatus::Submitted);
    h.close().await;
}

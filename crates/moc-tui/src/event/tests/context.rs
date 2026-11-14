use super::*;

/// `/context` 打开统一总览，并装入 core 的真实快照。
#[tokio::test]
async fn context_command_opens_content_window_with_core_sections() {
    use moc_protocol::methods::ContextContentResult;

    let (core, connection, mut app, mut receiver, _root) = app_with_core().await;
    execute_command(&mut app, &connection, "/context".into());
    assert!(
        matches!(app.modal, Some(Modal::ContextContent { .. })),
        "/context 打开统一上下文面板"
    );

    let event = tokio::time::timeout(std::time::Duration::from_secs(5), receiver.recv())
        .await
        .expect("正文分块应在超时前返回")
        .expect("事件队列未关闭");
    let AppEvent::ContextContent {
        request,
        session_id,
        result,
    } = event
    else {
        panic!("应收到上下文正文事件");
    };
    assert_eq!(app.session.as_ref().unwrap().id, session_id);
    let content = result.expect("读取上下文正文");

    apply_context_content(&mut app, request, Ok(content));
    let Some(Modal::ContextContent { .. }) = app.modal.as_ref() else {
        panic!("正文窗应保持打开");
    };

    // 窗已关掉时回执不得把窗重新打开。
    app.modal = None;
    apply_context_content(
        &mut app,
        request,
        Ok(ContextContentResult {
            context_window: 1,
            total_tokens: 1,
            sections: Vec::new(),
            ..Default::default()
        }),
    );
    assert!(app.modal.is_none());
    core.shutdown().await;
}

/// 上下文面板的鼠标：滚轮滚动、点击进入条目、拖滑块换算位置。
#[test]
fn context_window_mouse_toggles_and_drags() {
    use crate::modal::{ContextLayout, ContextScrollbar};
    use moc_protocol::methods::{ContextContentResult, ContextContentSection};
    use ratatui::layout::Rect;

    let mut app = app();
    let mut state = ContextContentState::default();
    state.set_content(ContextContentResult {
        context_window: 100_000,
        total_tokens: 1_000,
        sections: (0..3)
            .map(|index| ContextContentSection {
                id: index.to_string(),
                name: format!("block{index}"),
                tokens: 10,
                ..Default::default()
            })
            .collect(),
        ..Default::default()
    });
    state.viewport = 10;
    state.layout = Some(ContextLayout {
        width: 40,
        spans: vec![0..5, 5..12, 12..30],
        lines: vec![ratatui::text::Line::default(); 30],
    });
    state.scrollbar = ContextScrollbar {
        track: Rect::new(50, 4, 1, 10),
        thumb: 3,
        max_offset: 20,
    };
    app.modal = Some(Modal::ContextContent {
        state: Box::new(state),
    });
    let mouse = |kind, column, row| crossterm::event::MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };
    let scroll = |app: &App| match app.modal.as_ref() {
        Some(Modal::ContextContent { state }) => state.scroll,
        _ => panic!("正文窗应保持打开"),
    };

    // 滚轮：一次三行，夹在 [0, max_offset]。
    handle_context_content_mouse(&mut app, mouse(MouseEventKind::ScrollDown, 10, 6));
    assert_eq!(scroll(&app), 3);
    handle_context_content_mouse(&mut app, mouse(MouseEventKind::ScrollUp, 10, 6));
    assert_eq!(scroll(&app), 0);

    // 抓滑块顶端拖到轨道末端：偏移走满。
    handle_context_content_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 50, 4),
    );
    handle_context_content_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), 50, 11),
    );
    assert_eq!(scroll(&app), 20);
    handle_context_content_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), 50, 11),
    );
    let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
        panic!("正文窗应保持打开");
    };
    assert!(state.drag.is_none(), "抬起后结束拖拽");

    // 点击滚动位置对应的条目，进入独立正文页。
    handle_context_content_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 10, 6),
    );
    let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
        panic!("正文窗应保持打开");
    };
    assert_eq!(state.current.as_ref().unwrap().id, "2");
    assert_eq!(state.scroll, 0);
    assert_eq!(state.navigation.last().unwrap().1, 20);
    assert!(state.layout.is_none());
    // 轨道之外的点不改变焦点。
    let Some(Modal::ContextContent { state }) = app.modal.as_mut() else {
        panic!("正文窗应保持打开");
    };
    state.layout = Some(ContextLayout {
        width: 40,
        spans: vec![0..5, 5..12, 12..30],
        lines: vec![ratatui::text::Line::default(); 30],
    });
    handle_context_content_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 10, 30),
    );
    let Some(Modal::ContextContent { state }) = app.modal.as_ref() else {
        panic!("正文窗应保持打开");
    };
    assert!(state.focus.is_none(), "正文区之外的点不命中条目");
}

#[tokio::test]
async fn input_bang_commands_render_results_and_remain_out_of_agent_context_when_private() {
    use crate::transcript::Block;
    use moc_core::{Core, CoreConfig};
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.toml"), "[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:1/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n").unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, mut receiver) = unbounded_channel();
    let mut connection = connect_embedded(core.clone(), events.clone())
        .await
        .unwrap();
    let session = connection
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
        .unwrap();
    let id = session.id.clone();
    let mut app = App::new(Theme::global(), events);
    app.session = Some(session);
    app.tool_detail = DetailLevel::Full;
    // 长粘贴经 chip 展开，再作为多行 Bash 脚本执行。
    app.prompt.insert_str_raw("!");
    app.prompt.insert_text("printf 'TUI_PUBLIC\\n'\nprintf 'TUI_SECOND\\n'\nprintf 'TUI_THIRD\\n'\nprintf 'TUI_FOURTH\\n'");
    assert!(app.prompt.text.contains("[paste#"));
    commit_prompt(&mut app, &connection);
    // 命令在运行时再次提交 `!!` 必须排队，不能作为 steering 注入模型。
    app.prompt
        .insert_text("!!printf TUI_PRIVATE\nprintf done > tui-marker\n:\n:");
    commit_prompt(&mut app, &connection);
    assert_eq!(app.queues[&id].len(), 1);
    tokio::time::timeout(std::time::Duration::from_secs(3), async {
        let mut completed = 0;
        while completed < 2 {
            let event = receiver.recv().await.unwrap();
            if let AppEvent::PromptReturned { result, .. } = &event {
                result.as_ref().unwrap();
                completed += 1;
            }
            while let Ok(update) = connection.updates.try_recv() {
                if let crate::protocol::UiUpdate::Session(update) = update {
                    apply_update(&mut app, update.update);
                }
            }
            if let event @ (AppEvent::PromptReturned { .. } | AppEvent::SteerReturned { .. }) =
                event
            {
                crate::interaction::handle_event(&mut app, &connection, event)
            }
        }
        while let Ok(update) = connection.updates.try_recv() {
            if let crate::protocol::UiUpdate::Session(update) = update {
                apply_update(&mut app, update.update);
            }
        }
    })
    .await
    .unwrap();
    assert!(!app.prompt_running);
    assert_eq!(
        std::fs::read_to_string(root.path().join("tui-marker")).unwrap(),
        "done"
    );
    assert!(app.blocks.iter().any(|entry| matches!(&entry.block, Block::Tool(tool) if tool.result.as_deref().is_some_and(|result| result.contains("TUI_FOURTH")))));
    assert!(app.blocks.iter().any(|entry| matches!(&entry.block, Block::Tool(tool) if tool.result.as_deref() == Some("TUI_PRIVATE"))));
    assert!(
        !app.blocks
            .iter()
            .any(|entry| matches!(entry.block, Block::User { .. } | Block::Assistant { .. }))
    );
    let loaded = connection
        .client
        .call::<SessionLoad>(SessionLoadParams {
            omit_auxiliary: false,
            session_id: id,
            max_entries: None,
            read_only: false,
        })
        .await
        .unwrap();
    assert!(loaded.entries.iter().any(|entry| {
        entry
            .context_message()
            .is_some_and(|message| message.text().contains("TUI_PUBLIC"))
    }));
    assert!(
        !loaded
            .entries
            .iter()
            .filter_map(|entry| entry.context_message())
            .any(|message| message.text().contains("TUI_PRIVATE"))
    );
    app.blocks = crate::transcript_restore::blocks_from_entries(&loaded.entries);
    app.view_gen += 1;
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 34)).unwrap();
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut app, &mut view))
        .unwrap();
    let surface = terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(surface.contains("! bash"));
    assert!(surface.contains("!! bash"));
    assert!(surface.contains("TUI_PUBLIC"));
    assert!(surface.contains("TUI_PRIVATE"));
    connection.shutdown().await;
    core.shutdown().await;
}

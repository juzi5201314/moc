use super::*;
use moc_protocol::methods::{
    PermissionOption, PermissionOptionKind, PermissionRequestParams, PermissionResult,
};
use tokio::sync::oneshot::{self, error::TryRecvError};

fn permission(
    app: &mut App,
    session_id: &str,
    request_id: &str,
) -> oneshot::Receiver<Result<PermissionResult, moc_protocol::RpcError>> {
    let (reply, receiver) = oneshot::channel();
    apply_request_modal(
        app,
        AppEvent::Permission {
            params: PermissionRequestParams {
                session_id: session_id.into(),
                prompt_id: "prompt_test".into(),
                request_id: request_id.into(),
                tool_call_id: request_id.into(),
                tool_name: "bash".into(),
                title: "执行命令".into(),
                body: None,
                diff: None,
                options: vec![PermissionOption {
                    id: "allow_once".into(),
                    label: "允许一次".into(),
                    kind: PermissionOptionKind::AllowOnce,
                }],
            },
            reply,
        },
    );
    receiver
}

fn ask(
    app: &mut App,
    session_id: &str,
    request_id: &str,
) -> oneshot::Receiver<Result<AskResult, moc_protocol::RpcError>> {
    let (reply, receiver) = oneshot::channel();
    apply_request_modal(
        app,
        AppEvent::Ask {
            params: AskParams {
                session_id: session_id.into(),
                prompt_id: "prompt_test".into(),
                request_id: request_id.into(),
                questions: vec![ask_question("q1"), ask_question("q2")],
            },
            reply,
        },
    );
    receiver
}

fn current_request(app: &App) -> &str {
    match app.modal.as_ref().expect("待答请求应显示") {
        Modal::Permission { params, .. } => &params.request_id,
        Modal::Ask { params, .. } => &params.request_id,
        _ => panic!("应为授权或提问"),
    }
}

fn terminal_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    handle_terminal_event(
        app,
        connection,
        Event::Key(crossterm::event::KeyEvent::new(code, modifiers)),
    );
}

#[tokio::test]
async fn request_modals_keep_fifo_across_keyboard_mouse_and_parked_ask() {
    let (core, connection, mut app) = setup_test_app().await;
    let session = app.session.as_ref().unwrap().id.clone();
    let mut first = permission(&mut app, &session, "p1");
    let mut second = permission(&mut app, &session, "p2");
    let mut third = ask(&mut app, &session, "a1");
    let mut fourth = ask(&mut app, &session, "a2");
    assert_eq!(current_request(&app), "p1");
    assert_eq!(app.pending_modals.len(), 3);
    assert_eq!(first.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(second.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(third.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(fourth.try_recv(), Err(TryRecvError::Empty));

    // 首次打开也必须先绘制；渲染前的按键不能批准请求。
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(first.try_recv(), Err(TryRecvError::Empty));
    let _ = draw_frame(&mut app, 100, 30);
    let (_, approve) = app.modal_item_hits[0];
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(first.try_recv().unwrap().unwrap().option_id, "allow_once");
    assert_eq!(current_request(&app), "p2");
    assert!(!app.modal_drawn);

    // 仍显示旧授权卡时，旧按钮坐标和连发按键都不能答复下一条请求。
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(left_click(approve.x, approve.y)),
    );
    for code in [KeyCode::Enter, KeyCode::Char('1'), KeyCode::Esc] {
        terminal_key(&mut app, &connection, KeyModifiers::NONE, code);
    }
    assert_eq!(second.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(current_request(&app), "p2");

    // 终端小到画不出卡片时也不能放行；恢复正常尺寸后键盘取消才生效。
    let _ = draw_frame(&mut app, 19, 5);
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(second.try_recv(), Err(TryRecvError::Empty));
    let _ = draw_frame(&mut app, 100, 30);
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(second.try_recv().unwrap().is_err());
    assert_eq!(current_request(&app), "a1");
    handle_terminal_event(&mut app, &connection, Event::Paste("旧输入".into()));
    assert!(app.prompt_text().is_empty());

    // 折叠不是作答结束；此时新授权仍必须排在两条提问之后。
    let _ = draw_frame(&mut app, 100, 30);
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    let mut fifth = permission(&mut app, &session, "p3");
    assert_eq!(current_request(&app), "a1");
    assert!(matches!(app.modal, Some(Modal::Ask { parked: true, .. })));
    assert_eq!(third.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(fifth.try_recv(), Err(TryRecvError::Empty));
    let _ = draw_frame(&mut app, 100, 30);
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);

    // 两问逐项点击推进，让正文导航的「确认」走鼠标专用提交路径。
    for _ in 0..2 {
        let _ = draw_frame(&mut app, 100, 30);
        let (_, option) = app.ask_option_hits[0];
        handle_terminal_event(
            &mut app,
            &connection,
            Event::Mouse(left_click(option.x, option.y)),
        );
    }
    let _ = draw_frame(&mut app, 100, 30);
    let (_, submit) = app
        .ask_nav_hits
        .iter()
        .find(|(index, _)| *index == 2)
        .unwrap();
    let submit = left_click(submit.x, submit.y);
    handle_terminal_event(&mut app, &connection, Event::Mouse(submit));
    assert_eq!(
        third.try_recv().unwrap().unwrap().answers[0].selected,
        ["选项一"]
    );
    assert_eq!(current_request(&app), "a2");
    handle_terminal_event(&mut app, &connection, Event::Mouse(submit));
    terminal_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(fourth.try_recv(), Err(TryRecvError::Empty));

    let _ = draw_frame(&mut app, 100, 30);
    terminal_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(fourth.try_recv().unwrap().is_err());
    assert_eq!(current_request(&app), "p3");
    let _ = draw_frame(&mut app, 100, 30);
    terminal_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('1'),
    );
    assert_eq!(fifth.try_recv().unwrap().unwrap().option_id, "allow_once");
    assert!(app.modal.is_none());
    assert!(app.pending_modals.is_empty());

    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn request_modals_wait_for_other_windows_and_drop_on_session_exit() {
    let (core, connection, mut app) = setup_test_app().await;
    let session = app.session.as_ref().unwrap().id.clone();
    app.set_modal(Some(Modal::Help { scroll: 0 }));
    let mut first = permission(&mut app, &session, "p1");
    assert_eq!(
        current_request(&app),
        "p1",
        "首个请求沿用替换浏览窗口的行为"
    );
    let mut second = ask(&mut app, &session, "a1");
    let mut other = permission(&mut app, "other", "p2");

    // 无关弹窗仍按既有语义替换当前请求；队列等待空位，新请求不能插队。
    app.set_modal(Some(Modal::Help { scroll: 0 }));
    assert_eq!(first.try_recv(), Err(TryRecvError::Closed));
    let mut last = ask(&mut app, &session, "a2");
    assert!(!app.show_next_pending_modal());
    let _ = draw_frame(&mut app, 100, 30);
    terminal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert_eq!(current_request(&app), "a1");

    apply_session_closed(&mut app, &session);
    assert_eq!(second.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(last.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(other.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(current_request(&app), "p2");

    let mut queued = ask(&mut app, "other", "a4");
    drop(app);
    assert_eq!(other.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(queued.try_recv(), Err(TryRecvError::Closed));
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn session_load_and_focus_receipts_clean_up_request_modals() {
    let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
    let home = app.session.clone().unwrap();
    let target = new_child(&connection, "目标会话").await;

    for focus_receipt in [false, true] {
        for current_is_leaving in [false, true] {
            app.session = Some(home.clone());
            let mut visible = permission(
                &mut app,
                if current_is_leaving {
                    &home.id
                } else {
                    &target.id
                },
                "visible",
            );
            let mut old_ask = ask(&mut app, &home.id, "old-ask");
            let mut next = ask(&mut app, &target.id, "target-ask");
            let mut old_permission = permission(&mut app, &home.id, "old-permission");
            let mut unrelated = permission(&mut app, "unrelated", "unrelated-permission");
            let _ = draw_frame(&mut app, 100, 30);
            let page = load_page(&connection, &target.id).await.unwrap();

            if focus_receipt {
                apply_focus_loaded(&mut app, &connection, false, Ok(page));
            } else {
                apply_loaded(
                    &mut app,
                    &connection,
                    AppEvent::Loaded(
                        page.session,
                        Vec::new(),
                        page.has_more,
                        None,
                        page.todos,
                        false,
                        page.goal,
                    ),
                );
            }

            assert_eq!(app.session.as_ref().unwrap().id, target.id);
            assert_eq!(old_ask.try_recv(), Err(TryRecvError::Closed));
            assert_eq!(old_permission.try_recv(), Err(TryRecvError::Closed));
            assert_eq!(next.try_recv(), Err(TryRecvError::Empty));
            assert_eq!(unrelated.try_recv(), Err(TryRecvError::Empty));
            // 加载回执总会先关当前窗；聚焦回执只清理离开会话的请求。
            let preserve_visible = focus_receipt && !current_is_leaving;
            let expected_queue = if preserve_visible {
                assert_eq!(visible.try_recv(), Err(TryRecvError::Empty));
                assert_eq!(current_request(&app), "visible");
                assert!(app.modal_drawn);
                vec!["target-ask", "unrelated-permission"]
            } else {
                assert_eq!(visible.try_recv(), Err(TryRecvError::Closed));
                assert_eq!(current_request(&app), "target-ask");
                assert!(!app.modal_drawn);
                vec!["unrelated-permission"]
            };
            let queued: Vec<_> = app
                .pending_modals
                .iter()
                .map(|modal| match modal {
                    Modal::Permission { params, .. } => params.request_id.as_str(),
                    Modal::Ask { params, .. } => params.request_id.as_str(),
                    _ => panic!("队列只应包含待答请求"),
                })
                .collect();
            assert_eq!(queued, expected_queue);
            app.set_modal(None);
            app.pending_modals.clear();
        }
    }

    connection.shutdown().await;
    core.shutdown().await;
}

#[test]
fn pending_modals_skip_dropped_and_closed_receivers() {
    let mut app = app();
    let _visible = permission(&mut app, "session", "visible");
    let stale_permission = permission(&mut app, "session", "stale-permission");
    let mut stale_ask = ask(&mut app, "session", "stale-ask");
    let mut next = ask(&mut app, "session", "next");
    drop(stale_permission);
    stale_ask.close();

    assert!(!app.show_next_pending_modal());
    assert_eq!(current_request(&app), "visible");
    assert_eq!(app.pending_modals.len(), 1);
    app.set_modal(None);
    assert!(app.show_next_pending_modal());
    assert_eq!(current_request(&app), "next");
    assert_eq!(next.try_recv(), Err(TryRecvError::Empty));
    assert!(app.pending_modals.is_empty());
}

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
                questions: vec![ask_question("q1")],
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

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(first.try_recv().unwrap().unwrap().option_id, "allow_once");
    assert_eq!(current_request(&app), "p2");
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(second.try_recv().unwrap().is_err());
    assert_eq!(current_request(&app), "a1");

    // 折叠不是作答结束；此时新授权仍必须排在两条提问之后。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    let mut fifth = permission(&mut app, &session, "p3");
    assert_eq!(current_request(&app), "a1");
    assert!(matches!(app.modal, Some(Modal::Ask { parked: true, .. })));
    assert_eq!(third.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(fifth.try_recv(), Err(TryRecvError::Empty));
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);

    app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
    app.ask_nav_hits = vec![(1, ratatui::layout::Rect::new(10, 8, 10, 1))];
    handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
    handle_mouse_with_connection(&mut app, &connection, left_click(10, 8));
    assert_eq!(
        third.try_recv().unwrap().unwrap().answers[0].selected,
        ["选项一"]
    );
    assert_eq!(current_request(&app), "a2");

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(fourth.try_recv().unwrap().is_err());
    assert_eq!(current_request(&app), "p3");
    handle_key(
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
    app.modal = Some(Modal::Help { scroll: 0 });
    let mut first = permission(&mut app, &session, "p1");
    assert_eq!(
        current_request(&app),
        "p1",
        "首个请求沿用替换浏览窗口的行为"
    );
    let mut second = ask(&mut app, &session, "a1");
    let mut other = permission(&mut app, "other", "p2");

    // 无关弹窗仍按既有语义替换当前请求；队列等待空位，新请求不能插队。
    app.modal = Some(Modal::Help { scroll: 0 });
    assert_eq!(first.try_recv(), Err(TryRecvError::Closed));
    let mut last = ask(&mut app, &session, "a2");
    assert!(!app.show_next_pending_modal());
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert_eq!(current_request(&app), "a1");

    apply_session_closed(&mut app, &session);
    assert_eq!(second.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(last.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(other.try_recv(), Err(TryRecvError::Empty));
    assert_eq!(current_request(&app), "p2");

    let mut old_session = ask(&mut app, &session, "a3");
    let mut info = app.session.clone().unwrap();
    info.id = "other".into();
    app.switch_session(info, Vec::new());
    assert_eq!(old_session.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(current_request(&app), "p2");

    let mut queued = ask(&mut app, "other", "a4");
    drop(app);
    assert_eq!(other.try_recv(), Err(TryRecvError::Closed));
    assert_eq!(queued.try_recv(), Err(TryRecvError::Closed));
    connection.shutdown().await;
    core.shutdown().await;
}

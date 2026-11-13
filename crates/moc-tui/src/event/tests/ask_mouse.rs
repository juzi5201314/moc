use super::*;

/// 鼠标点选：单选点击即选中并提交整单。
#[tokio::test]
async fn ask_mouse_click_selects_and_submits_single_choice() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));
    app.ask_option_hits = vec![
        (0, ratatui::layout::Rect::new(10, 5, 40, 1)),
        (1, ratatui::layout::Rect::new(10, 6, 40, 1)),
    ];

    handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
    match app.modal.as_ref() {
        Some(Modal::Ask { question, .. }) => {
            assert_eq!(*question, 1, "单选点击选中并推进到确认页")
        }
        _ => unreachable!("确认页上模态应保留"),
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let result = reply_rx
        .try_recv()
        .expect("确认页 Enter 应提交")
        .expect("提问结果");
    assert_eq!(result.answers[0].selected, ["选项一"]);
    assert!(app.modal.is_none(), "提交后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 鼠标点选：多选点击切换勾选、不推进；点击自定义行进入编辑。
#[tokio::test]
async fn ask_mouse_click_toggles_multi_and_opens_custom_editor() {
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(ask_modal_with(vec![multi_ask_question()], None));
    app.ask_option_hits = vec![
        (0, ratatui::layout::Rect::new(10, 5, 40, 1)),
        (1, ratatui::layout::Rect::new(10, 6, 40, 1)),
        (2, ratatui::layout::Rect::new(10, 7, 40, 1)),
        (3, ratatui::layout::Rect::new(10, 8, 40, 1)),
    ];

    handle_mouse_with_connection(&mut app, &connection, left_click(10, 6));
    match app.modal.as_ref() {
        Some(Modal::Ask {
            selected,
            highlight,
            ..
        }) => {
            assert_eq!(selected[0], vec!["日志".to_owned()], "多选点击切换勾选");
            assert_eq!(highlight[0], 1, "点击行获得聚焦");
        }
        _ => unreachable!("多选点击不推进"),
    }
    handle_mouse_with_connection(&mut app, &connection, left_click(10, 6));
    if let Some(Modal::Ask { selected, .. }) = app.modal.as_ref() {
        assert!(selected[0].is_empty(), "重复点击取消勾选");
    }

    handle_mouse_with_connection(&mut app, &connection, left_click(10, 8));
    match app.modal.as_ref() {
        Some(Modal::Ask {
            custom, highlight, ..
        }) => {
            assert!(custom[0].editing, "点击自定义行进入编辑");
            assert_eq!(highlight[0], 3);
        }
        _ => unreachable!("编辑态模态应保持打开"),
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// 折叠态点击细条：只展开回作答（不选择）；展开后点选项才提交。
#[tokio::test]
async fn ask_mouse_click_strip_unparks_without_selecting() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));
    app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
    app.ask_strip_area = ratatui::layout::Rect::new(3, 20, 114, 3);
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);

    // 折叠态选项命中区已清空：点在旧选项位置不动作，点细条才展开。
    handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
    match app.modal.as_ref() {
        Some(Modal::Ask { parked, .. }) => assert!(*parked, "折叠态点卡片外不动作"),
        _ => unreachable!("折叠时模态应保留"),
    }
    handle_mouse_with_connection(&mut app, &connection, left_click(30, 21));
    match app.modal.as_ref() {
        Some(Modal::Ask { parked, .. }) => assert!(!*parked, "点细条应展开回作答"),
        _ => unreachable!("模态应保留"),
    }
    assert!(reply_rx.try_recv().is_err(), "展开本身不提交");

    app.ask_option_hits = vec![(0, ratatui::layout::Rect::new(10, 5, 40, 1))];
    handle_mouse_with_connection(&mut app, &connection, left_click(10, 5));
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let result = reply_rx
        .try_recv()
        .expect("展开后点选项选中，确认页 Enter 提交")
        .expect("提问结果");
    assert_eq!(result.answers[0].selected, ["选项一"]);
    assert!(app.modal.is_none(), "提交后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 作答态滚轮悬停分流：落在可滚文本区（溢出的问题窗/详情区）滚该区偏移，
/// 其余位置照旧移动选项光标；命中区域只在溢出时登记。
#[tokio::test]
async fn ask_mouse_wheel_routes_by_hover_region() {
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(ask_modal_with(vec![multi_ask_question()], None));
    app.ask_question_area = ratatui::layout::Rect::new(10, 3, 60, 5);
    app.ask_detail_area = ratatui::layout::Rect::new(10, 20, 60, 5);

    // 悬停问题正文窗口：滚动问题偏移，光标不动。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(MouseEventKind::ScrollDown, 20, 4),
    );
    match app.modal.as_ref() {
        Some(Modal::Ask {
            highlight,
            question_scroll,
            preview_scroll,
            ..
        }) => {
            assert_eq!(*question_scroll, ASK_PREVIEW_SCROLL_STEP);
            assert_eq!(*preview_scroll, 0);
            assert_eq!(highlight[0], 0, "悬停文本区不动光标");
        }
        _ => unreachable!("模态应保留"),
    }

    // 悬停详情区：滚动详情偏移。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(MouseEventKind::ScrollDown, 20, 21),
    );
    match app.modal.as_ref() {
        Some(Modal::Ask { preview_scroll, .. }) => {
            assert_eq!(*preview_scroll, ASK_PREVIEW_SCROLL_STEP)
        }
        _ => unreachable!("模态应保留"),
    }

    // 两区之外（选项/空白）：恢复移动光标语义。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(MouseEventKind::ScrollDown, 20, 12),
    );
    match app.modal.as_ref() {
        Some(Modal::Ask { highlight, .. }) => assert_eq!(highlight[0], 1),
        _ => unreachable!("模态应保留"),
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// 滚动条轨道在转写区外：作答态按下 = 折叠并开始拖拽；折叠态拖动/抬起走通用路径。
#[tokio::test]
async fn ask_scrollbar_drag_works_while_parked() {
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(ask_modal());
    app.ask_card_area = ratatui::layout::Rect::new(10, 5, 80, 10);
    app.ask_strip_area = ratatui::layout::Rect::new(3, 20, 114, 2);
    app.scrollback_area = ratatui::layout::Rect::new(3, 2, 114, 18);
    app.scrollbar = crate::app::ScrollbackScrollbar {
        track: ratatui::layout::Rect::new(118, 2, 2, 18),
        thumb: 6,
        max_offset: 36,
    };
    app.scroll_max_offset = 36;

    // 作答态按下轨道空白：折叠 + 拖拽开始（滑块中线跟到光标行）。
    handle_mouse_with_connection(&mut app, &connection, left_click(118, 10));
    match app.modal.as_ref() {
        Some(Modal::Ask { parked, .. }) => assert!(*parked, "按滚动条应折叠"),
        _ => unreachable!("模态应保留"),
    }
    assert!(app.scrollbar_drag.is_some(), "按下即进入拖拽");
    assert!(app.scroll_offset > 0, "按轨道空白滑块应跟到光标");

    // 折叠态拖到轨道顶：走满历史（通用拖拽路径）。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(MouseEventKind::Drag(MouseButton::Left), 119, 2),
    );
    assert_eq!(app.scroll_offset, 36, "拖到轨道顶应滚到历史顶端");

    // 抬起结束拖拽，卡片保持折叠。
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(MouseEventKind::Up(MouseButton::Left), 119, 2),
    );
    assert!(app.scrollbar_drag.is_none(), "抬起后结束拖拽");
    assert!(
        matches!(app.modal, Some(Modal::Ask { parked: true, .. })),
        "拖拽全程卡片保持折叠"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

/// 点击导航条跳问（只动指针不提交）；点击卡片外折叠。
#[tokio::test]
async fn ask_mouse_nav_click_jumps_and_outside_click_parks() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(
        vec![ask_question("q1"), ask_question("q2")],
        Some(reply_tx),
    ));
    app.ask_card_area = ratatui::layout::Rect::new(10, 5, 80, 10);
    app.scrollback_area = ratatui::layout::Rect::new(3, 18, 114, 5);
    app.ask_nav_hits = vec![
        (0, ratatui::layout::Rect::new(12, 6, 10, 1)),
        (1, ratatui::layout::Rect::new(24, 6, 10, 1)),
    ];

    handle_mouse_with_connection(&mut app, &connection, left_click(26, 6));
    match app.modal.as_ref() {
        Some(Modal::Ask {
            question, parked, ..
        }) => {
            assert_eq!(*question, 1, "点导航条应跳到第二问");
            assert!(!*parked, "跳问不折叠");
        }
        _ => unreachable!("跳问不应提交"),
    }
    assert!(reply_rx.try_recv().is_err(), "跳问不提交");

    // 卡片外但非转写区（输入框/状态栏死区）：不折叠。
    handle_mouse_with_connection(&mut app, &connection, left_click(5, 26));
    match app.modal.as_ref() {
        Some(Modal::Ask { parked, .. }) => assert!(!*parked, "死区点击不折叠"),
        _ => unreachable!("模态应保留"),
    }

    handle_mouse_with_connection(&mut app, &connection, left_click(5, 20));
    match app.modal.as_ref() {
        Some(Modal::Ask { parked, .. }) => assert!(*parked, "点转写区应折叠"),
        _ => unreachable!("模态应保留"),
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// 导航条「确认」项是纯鼠标的提交链路：作答页点它落到确认页，
/// 确认页上再点即提交（多选末问也能由此离开作答页）。
#[tokio::test]
async fn ask_mouse_confirm_nav_jumps_and_submits() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(
        vec![multi_ask_question(), ask_question("q2")],
        Some(reply_tx),
    ));
    app.ask_card_area = ratatui::layout::Rect::new(10, 5, 80, 10);
    app.ask_nav_hits = vec![
        (0, ratatui::layout::Rect::new(12, 6, 10, 1)),
        (1, ratatui::layout::Rect::new(24, 6, 10, 1)),
        (2, ratatui::layout::Rect::new(36, 6, 8, 1)),
    ];

    // 作答页点「确认」（下标 = questions.len()）：多选不切勾、不提交，落确认页。
    handle_mouse_with_connection(&mut app, &connection, left_click(38, 6));
    match app.modal.as_ref() {
        Some(Modal::Ask {
            question,
            selected,
            preview_scroll,
            question_scroll,
            ..
        }) => {
            assert_eq!(*question, 2, "点「确认」应落到确认页");
            assert!(selected.iter().all(|set| set.is_empty()), "跳页不代选");
            assert_eq!(*preview_scroll + *question_scroll, 0, "跳页复位滚动");
        }
        _ => unreachable!("落确认页不应提交"),
    }
    assert!(reply_rx.try_recv().is_err(), "未点第二次不应提交");

    // 确认页上再点「确认」：提交并关闭模态。
    handle_mouse_with_connection(&mut app, &connection, left_click(38, 6));
    assert!(app.modal.is_none(), "确认页点「确认」应提交并关闭");
    let answers = reply_rx
        .await
        .expect("应收到提交结果")
        .expect("提交应成功")
        .answers;
    assert_eq!(answers.len(), 2, "每问一条答案记录");

    connection.shutdown().await;
    core.shutdown().await;
}

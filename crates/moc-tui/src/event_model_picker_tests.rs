fn scroll_test_model(index: usize) -> crate::modal::ModelEntry {
    let selector = format!("p{}/model{index:02}", index / 6);
    crate::modal::ModelEntry {
        label: selector.clone(),
        selector,
        display_name: String::new(),
        levels: crate::modal::DEFAULT_LEVELS
            .iter()
            .map(|level| (*level).into())
            .collect(),
        context_window: None,
        max_output_tokens: None,
        inputs: Vec::new(),
        pricing: None,
        fast_mode: false,
    }
}

#[tokio::test]
async fn model_picker_scroll_and_hover_preserve_selection_and_details() {
    use crossterm::event::MouseEventKind::{Moved, ScrollDown, ScrollUp};
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(Modal::Model {
        entries: (0..36).map(scroll_test_model).collect(),
        current: "p0/model00:low".into(),
        picker: Default::default(),
        level: crate::modal::LevelState {
            manual: Some("low".into()),
            ..Default::default()
        },
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    });
    let _ = draw_frame(&mut app, 140, 30);
    let visible = app.modal_item_hits.clone();
    let (control_row, control) = app.modal_detail_hits[0];
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(Moved, control.x, control.y),
    );
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(ScrollDown, control.x, control.y),
    );
    let _ = draw_frame(&mut app, 140, 30);
    assert_eq!(app.modal_item_hits, visible, "详情卡上的滚轮不滚动列表");
    let Some(Modal::Model { detail, .. }) = &app.modal else {
        panic!("选择器应保持打开")
    };
    assert_eq!(detail.focus, Some(control_row), "详情卡悬停聚焦对应控制行");

    let (_, row) = app.modal_item_hits[2];
    for _ in 0..8 {
        handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, row.x, row.y));
        let _ = draw_frame(&mut app, 140, 30);
        let Some(Modal::Model { picker, level, .. }) = &app.modal else {
            panic!("选择器应保持打开")
        };
        assert_eq!(picker.selected, 0, "纯滚轮不能改选中项");
        assert_eq!(level.manual.as_deref(), Some("low"));
        assert!(!picker.loading);
    }
    assert!(app.modal_item_hits[0].0 > 0, "选中项允许滚出视口");
    let visible = app.modal_item_hits.clone();
    let (hover_index, hover_row) = app.modal_item_hits[2];
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(Moved, hover_row.x, hover_row.y),
    );
    let _ = draw_frame(&mut app, 140, 30);
    assert_eq!(app.modal_item_hits, visible, "悬停切换焦点不拉回视口");
    let Some(Modal::Model { picker, detail, .. }) = &app.modal else {
        panic!("选择器应保持打开")
    };
    assert_eq!(picker.selected, hover_index, "焦点应跟随鼠标指针移动");
    assert_eq!(detail.focus, None);
    handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollUp, row.x, row.y));
    let _ = draw_frame(&mut app, 140, 30);
    assert!(
        app.modal_item_hits[0].0 < visible[0].0,
        "向上滚轮反向滚动视口"
    );

    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Home);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    let _ = draw_frame(&mut app, 140, 30);
    assert!(
        app.modal_item_hits.iter().any(|(index, _)| *index == 1),
        "键盘导航重新显露选中项"
    );
    let (index, row) = *app.modal_item_hits.last().unwrap();
    handle_mouse_with_connection(&mut app, &connection, mouse_event(Moved, row.x, row.y));
    let terminal = draw_frame(&mut app, 140, 30);
    let Some(Modal::Model { picker, .. }) = &app.modal else {
        panic!("选择器应保持打开")
    };
    assert_eq!(picker.selected, index, "鼠标移动后列表焦点切到悬停行");
    assert_eq!(
        terminal.backend().buffer()[(row.x, row.y)].bg,
        app.theme.bg_visual
    );

    for ch in "model35".chars() {
        handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
    }
    let _ = draw_frame(&mut app, 140, 30);
    assert_eq!(app.modal_item_hits.len(), 1, "搜索后从匹配结果开头显示");
    handle_modal_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('u'),
    );
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::End);
    let _ = draw_frame(&mut app, 140, 30);
    assert_eq!(app.modal_item_hits.last().unwrap().0, 35);
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn model_picker_menu_scroll_preserves_draft_until_clicked() {
    use crossterm::event::MouseEventKind::{Moved, ScrollDown};
    let (core, connection, mut app) = setup_test_app().await;
    let mut fusion = scroll_test_model(0);
    fusion.selector = crate::modal::FUSION_ENTRY_SELECTOR.into();
    fusion.label = "Fusion".into();
    fusion.levels.clear();
    app.modal = Some(Modal::Model {
        entries: std::iter::once(fusion)
            .chain((0..30).map(scroll_test_model))
            .collect(),
        current: "p0/model00".into(),
        picker: Default::default(),
        level: Default::default(),
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    });
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let _ = draw_frame(&mut app, 140, 30);
    let before = app.modal_menu_hits.clone();
    handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, 0, 0));
    let _ = draw_frame(&mut app, 140, 30);
    assert_eq!(app.modal_menu_hits, before, "选择器外滚轮不影响下拉");
    let (_, row) = app.modal_menu_hits[2];
    for step in 0..5 {
        handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, row.x, row.y));
        let _ = draw_frame(&mut app, 140, 30);
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(Moved, row.x, row.y + step % 2),
        );
        let _ = draw_frame(&mut app, 140, 30);
        let Some(Modal::Model { picker, detail, .. }) = &app.modal else {
            panic!("选择器应保持打开")
        };
        assert_eq!(picker.selected, 0);
        let hovered = app
            .modal_menu_hits
            .iter()
            .find(|(_, area)| area.contains((row.x, row.y + step % 2).into()))
            .map(|(index, _)| *index)
            .unwrap();
        assert_eq!(
            detail.menu.as_ref().unwrap().selected,
            hovered,
            "下拉悬停焦点应跟随鼠标指针"
        );
        assert!(detail.fusion.lead.is_empty(), "浏览不能改 Fusion 草稿");
    }
    assert!(app.modal_menu_hits[0].0 > 0, "下拉可独立滚动");
    let before = app.modal_menu_hits.clone();
    let _ = draw_frame(&mut app, 140, 30);
    assert_eq!(app.modal_menu_hits, before);
    let (index, row) = *app.modal_menu_hits.last().unwrap();
    handle_mouse_with_connection(&mut app, &connection, left_click(row.x, row.y));
    let Some(Modal::Model { detail, .. }) = &app.modal else {
        panic!("选择器应保持打开")
    };
    assert!(detail.menu.is_none());
    assert_eq!(
        detail.fusion.lead,
        scroll_test_model(index).selector_with(Some("max"))
    );
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn model_picker_short_menu_can_scroll_to_last_level() {
    use crossterm::event::MouseEventKind::{Moved, ScrollDown};
    let (core, connection, mut app) = setup_test_app().await;
    let mut entry = scroll_test_model(0);
    entry.levels = (0..12).map(|index| format!("level{index}")).collect();
    app.modal = Some(Modal::Model {
        entries: vec![entry],
        current: "p0/model00".into(),
        picker: Default::default(),
        level: Default::default(),
        detail: Default::default(),
        target: crate::modal::ModelTarget::Session,
    });
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    let _ = draw_frame(&mut app, 140, 10);
    assert!(
        app.modal_menu_hits.iter().any(|(index, _)| *index == 11),
        "打开时当前等级可见"
    );
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Home);
    let _ = draw_frame(&mut app, 140, 10);
    for _ in 0..12 {
        let (_, row) = app.modal_menu_hits[0];
        handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, row.x, row.y));
        handle_mouse_with_connection(&mut app, &connection, mouse_event(Moved, row.x, row.y));
        let _ = draw_frame(&mut app, 140, 10);
    }
    assert_eq!(
        app.modal_menu_hits.last().unwrap().0,
        11,
        "短屏下末尾等级可达"
    );
    let Some(Modal::Model { detail, .. }) = &app.modal else {
        panic!("选择器应保持打开")
    };
    assert_eq!(
        detail.menu.as_ref().unwrap().selected,
        app.modal_menu_hits[0].0
    );
    connection.shutdown().await;
    core.shutdown().await;
}

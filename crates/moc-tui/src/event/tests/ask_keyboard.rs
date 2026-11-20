use super::*;

/// 提问预览的滚轮键位：PgUp/PgDn 改预览偏移，切选项或切问题归零。
#[tokio::test]
async fn ask_preview_keys_scroll_and_reset_on_focus_change() {
    use moc_core::{Core, CoreConfig};

    let root = tempfile::tempdir().unwrap();
    std::fs::write(
        root.path().join("config.toml"),
        "language='zh'\n[models.roles]\ndefault='test/model'\n[providers.test]\napi='openai_chat'\nbase_url='http://127.0.0.1:9/v1'\n[providers.test.models.model]\n[context_files]\nenabled=false\n[skills]\nenabled=false\n",
    )
    .unwrap();
    let core = Core::new(CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        user_home: Some(root.path().join("userhome")),
        overlays: Vec::new(),
        session_dir: Some(root.path().join("sessions")),
        client: Default::default(),
        ptc_worker: None,
    })
    .unwrap();
    let (events, _receiver) = unbounded_channel();
    let connection = connect_embedded(core.clone(), events).await.unwrap();
    let mut app = app();
    let question = || moc_protocol::methods::AskQuestion {
        id: "layout".into(),
        question: "选哪个布局？".into(),
        header: None,
        multi: false,
        recommended: Vec::new(),
        options: (0..2)
            .map(|index| moc_protocol::methods::AskOption {
                label: format!("选项{index}"),
                description: None,
                preview: Some("```\n+--+\n```".into()),
            })
            .collect(),
    };
    app.modal = Some(Modal::Ask {
        params: moc_protocol::methods::AskParams {
            session_id: "ses_1".into(),
            prompt_id: "prompt_1".into(),
            request_id: "ask_1".into(),
            questions: vec![question(), question()],
        },
        question: 0,
        highlight: vec![0, 0],
        selected: vec![Vec::new(), Vec::new()],
        custom: vec![crate::modal::CustomAnswer::default(); 2],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply: None,
    });

    let preview_scroll = |app: &App| match app.modal.as_ref() {
        Some(Modal::Ask { preview_scroll, .. }) => *preview_scroll,
        other => panic!(
            "提问模态应保持打开：{}",
            other.map_or_else(|| "无模态".to_owned(), Modal::title)
        ),
    };

    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
    assert_eq!(preview_scroll(&app), 2 * ASK_PREVIEW_SCROLL_STEP);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageUp);
    assert_eq!(preview_scroll(&app), ASK_PREVIEW_SCROLL_STEP);

    // 切聚焦项 / 切问题：偏移归零，避免把上一项的滚动位置带到下一项。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    assert_eq!(preview_scroll(&app), 0);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    assert_eq!(preview_scroll(&app), 0);
    assert!(matches!(
        app.modal.as_ref(),
        Some(Modal::Ask { question: 1, .. })
    ));
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::PageDown);
    handle_modal_key(&mut app, &connection, KeyModifiers::SHIFT, KeyCode::BackTab);
    assert_eq!(preview_scroll(&app), 0);
    assert!(matches!(
        app.modal.as_ref(),
        Some(Modal::Ask { question: 0, .. })
    ));

    connection.shutdown().await;
    core.shutdown().await;
}

/// 小9：多问时自定义答案各归各问，第 1 问的输入不会挂到最后一问。
#[tokio::test]
async fn ask_custom_answers_stay_with_their_question() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(
        vec![ask_question("q1"), ask_question("q2")],
        Some(reply_tx),
    ));

    // 第 1 问：z 进自定义、输入「甲」、Enter 推进到第 2 问
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('z'),
    );
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('甲'),
    );
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(ask_custom(&app), (String::new(), 0), "第 2 问的缓冲是空的");
    match app.modal.as_ref() {
        Some(Modal::Ask {
            custom, question, ..
        }) => {
            assert_eq!(*question, 1, "已推进到第 2 问");
            assert_eq!(custom[0].text, "甲", "第 1 问的答案留在第 1 问");
            assert!(custom[1].text.is_empty());
        }
        _ => unreachable!("ask 模态"),
    }

    // 第 2 问不做自定义，末问 enter 进入确认页，确认页 enter 提交：答案必须各归各问
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char(' '),
    );
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    match app.modal.as_ref() {
        Some(Modal::Ask { question, .. }) => {
            assert_eq!(*question, 2, "越过最后一问应落在确认页")
        }
        _ => unreachable!("确认页上模态应保留"),
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let result = reply_rx
        .try_recv()
        .expect("确认页提交后应回报")
        .expect("提问结果");
    assert_eq!(result.answers.len(), 2);
    assert_eq!(result.answers[0].custom.as_deref(), Some("甲"));
    assert_eq!(
        result.answers[1].custom, None,
        "第 2 问不得带上第 1 问的答案"
    );
    assert_eq!(result.answers[1].selected, vec!["选项一".to_owned()]);
    assert!(app.modal.is_none(), "提交后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 多选按空格切换多个选项，取消后保留顺序并提交所选标签。
#[tokio::test]
async fn ask_multi_select_toggles_multiple_options() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(vec![multi_ask_question()], Some(reply_tx)));

    for key in [
        KeyCode::Char(' '),
        KeyCode::Down,
        KeyCode::Char(' '),
        KeyCode::Down,
        KeyCode::Char(' '),
        KeyCode::Char(' '),
    ] {
        handle_key(&mut app, &connection, KeyModifiers::NONE, key);
    }

    match app.modal.as_ref() {
        Some(Modal::Ask { selected, .. }) => assert_eq!(
            selected[0],
            vec!["缓存".to_owned(), "日志".to_owned()],
            "取消最后一项后保留之前的选择顺序"
        ),
        _ => unreachable!("ask 模态应保持打开"),
    }

    // 末问 enter 进入确认页，确认页 enter 才提交。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert!(reply_rx.try_recv().is_err(), "进确认页本身不提交");
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let result = reply_rx
        .try_recv()
        .expect("确认后应收到 ask 回执")
        .expect("提问结果");
    assert_eq!(result.answers[0].selected, ["缓存", "日志"]);
    assert!(app.modal.is_none(), "提交后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 多选 Enter = 空格：非末问只切换聚焦项不跳问；末问 Enter 是通往
/// 确认页的手势（不切勾），空集合按未回答提交。
#[tokio::test]
async fn ask_multi_enter_toggles_and_confirms_only_on_last() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    let mut second = multi_ask_question();
    second.id = "features2".into();
    app.modal = Some(ask_modal_with(
        vec![multi_ask_question(), second],
        Some(reply_tx),
    ));

    // 第 1 问非末问：Enter 切勾聚焦项「缓存」，不跳问。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    match app.modal.as_ref() {
        Some(Modal::Ask {
            selected, question, ..
        }) => {
            assert_eq!(selected[0], ["缓存"], "Enter 与空格同为切换手势");
            assert_eq!(*question, 0, "非末问 Enter 不跳问");
        }
        _ => unreachable!("ask 模态应保持打开"),
    }

    // tab 换到第 2 问；末问 Enter 进确认页，不切勾第 2 问的聚焦项。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert!(
        matches!(app.modal.as_ref(), Some(Modal::Ask { question, .. }) if *question == 2),
        "末问 Enter 应落在确认页"
    );
    match app.modal.as_ref() {
        Some(Modal::Ask { selected, .. }) => {
            assert!(selected[1].is_empty(), "末问 Enter 不做切换")
        }
        _ => unreachable!("确认页上模态应保留"),
    }

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    let result = reply_rx
        .try_recv()
        .expect("确认页 Enter 应提交")
        .expect("提问结果");
    assert_eq!(result.answers[0].selected, ["缓存"]);
    assert!(
        result.answers[1].selected.is_empty() && result.answers[1].custom.is_none(),
        "末问未勾选按未回答提交"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

/// tab 只在问题间循环：末问 tab 回第一问，永不落确认页；
/// shift+tab 反向循环：第一问回末问。
#[tokio::test]
async fn ask_tab_cycles_questions_and_never_reaches_confirm() {
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(ask_modal_with(
        vec![ask_question("q1"), ask_question("q2")],
        None,
    ));
    let question_of = |app: &App| match app.modal.as_ref() {
        Some(Modal::Ask { question, .. }) => *question,
        _ => unreachable!("ask 模态"),
    };

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    assert_eq!(question_of(&app), 1, "tab 到下一问");
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    assert_eq!(question_of(&app), 0, "末问 tab 绕回第一问，不进确认页");
    handle_key(&mut app, &connection, KeyModifiers::SHIFT, KeyCode::BackTab);
    assert_eq!(question_of(&app), 1, "第一问 shift+tab 反向绕回末问");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 数字直达：单选选中即推进下一问；多选只切换勾选，不推进。
#[tokio::test]
async fn ask_digit_shortcuts_pick_and_toggle() {
    let (core, connection, mut app) = setup_test_app().await;
    let single = AskQuestion {
        id: "single".into(),
        question: "单选哪个？".into(),
        header: None,
        multi: false,
        recommended: Vec::new(),
        options: vec![
            AskOption {
                label: "甲".into(),
                description: None,
                preview: None,
            },
            AskOption {
                label: "乙".into(),
                description: None,
                preview: None,
            },
        ],
    };
    let (reply_tx, _reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(
        vec![single, multi_ask_question()],
        Some(reply_tx),
    ));

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('2'),
    );
    match app.modal.as_ref() {
        Some(Modal::Ask {
            question, selected, ..
        }) => {
            assert_eq!(*question, 1, "单选数字直达后推进到下一问");
            assert_eq!(selected[0], vec!["乙".to_owned()]);
        }
        _ => unreachable!("还有未答的问题，模态应保持打开"),
    }

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('1'),
    );
    match app.modal.as_ref() {
        Some(Modal::Ask {
            question, selected, ..
        }) => {
            assert_eq!(*question, 1, "多选数字只切换勾选，不推进");
            assert_eq!(selected[1], vec!["缓存".to_owned()]);
        }
        _ => unreachable!("多选切换后模态应保持打开"),
    }
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('1'),
    );
    if let Some(Modal::Ask { selected, .. }) = app.modal.as_ref() {
        assert!(selected[1].is_empty(), "重复数字应取消勾选");
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// Shift+X 跳过整单提问：按未回答提交空答案，模态关闭。
#[tokio::test]
async fn ask_shift_x_dismisses_with_empty_answers() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::SHIFT,
        KeyCode::Char('X'),
    );
    let result = reply_rx
        .try_recv()
        .expect("跳过后应回报")
        .expect("跳过按未回答成功收场");
    assert!(result.answers.is_empty(), "跳过提交空答案");
    assert!(app.modal.is_none(), "跳过后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// Esc 分层：第一次折叠（模态保留、键盘交给转写区），Tab 回卡片；
/// 折叠态再 Esc 才取消整单。
#[tokio::test]
async fn ask_esc_parks_before_cancelling() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    match app.modal.as_ref() {
        Some(Modal::Ask { parked, .. }) => {
            assert!(*parked, "第一次 Esc 应折叠而不是取消")
        }
        _ => unreachable!("折叠时卡片应保留"),
    }
    assert!(reply_rx.try_recv().is_err(), "折叠不提交也不取消");

    app.scroll_max_offset = 100;
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Up);
    assert_eq!(app.scroll_offset, 3, "折叠态 ↑ 应滚动转写");

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    if let Some(Modal::Ask { parked, .. }) = app.modal.as_ref() {
        assert!(!parked, "Tab 应回到提问卡片");
    }

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(
        reply_rx.try_recv().expect("折叠后 Esc 应取消").is_err(),
        "取消按错误回报"
    );
    assert!(app.modal.is_none(), "取消后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 粘贴在自定义答案编辑态写入那一行，而不是落到背后的主输入框。
#[tokio::test]
async fn ask_paste_lands_in_editing_custom_answer() {
    let (core, connection, mut app) = setup_test_app().await;
    app.set_modal(Some(ask_modal()));
    let _ = draw_frame(&mut app, 100, 30);

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('z'),
    );
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Paste("/etc/config.toml".into()),
    );
    assert_eq!(ask_custom(&app).0, "/etc/config.toml");
    assert!(app.prompt.text.is_empty(), "粘贴不得落到主输入框");

    connection.shutdown().await;
    core.shutdown().await;
}

/// Ctrl+C 在提问卡片任何状态下直接取消整单（与权限弹窗同语义）。
#[tokio::test]
async fn ask_ctrl_c_cancels_from_any_state() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(vec![ask_question("q1")], Some(reply_tx)));

    // 编辑态下 Ctrl+C 也取消，而不是被输入行吞掉。
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('z'),
    );
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(
        reply_rx.try_recv().expect("Ctrl+C 应取消提问").is_err(),
        "取消按错误回报"
    );
    assert!(app.modal.is_none(), "取消后模态关闭");

    connection.shutdown().await;
    core.shutdown().await;
}

/// ←→ 只翻看问题指针：不写入也不提交，已选答案原样保留。
#[tokio::test]
async fn ask_arrow_keys_browse_questions_without_submitting() {
    let (core, connection, mut app) = setup_test_app().await;
    let (reply_tx, mut reply_rx) = tokio::sync::oneshot::channel();
    app.modal = Some(ask_modal_with(
        vec![ask_question("q1"), multi_ask_question()],
        Some(reply_tx),
    ));

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char(' '),
    );
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    match app.modal.as_ref() {
        Some(Modal::Ask { question, .. }) => assert_eq!(*question, 1, "→ 应翻到下一问"),
        _ => unreachable!("翻看问题不应提交"),
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
        assert_eq!(*question, 2, "最后一问上 → 进入确认页");
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
        assert_eq!(*question, 2, "确认页上 → 不再前进");
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
        assert_eq!(*question, 1, "确认页 ← 回最后一问");
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    match app.modal.as_ref() {
        Some(Modal::Ask {
            question, selected, ..
        }) => {
            assert_eq!(*question, 0, "← 应翻回上一问");
            assert_eq!(selected[0], vec!["选项一".to_owned()], "翻看不丢已选答案");
        }
        _ => unreachable!("模态应保持打开"),
    }
    assert!(reply_rx.try_recv().is_err(), "翻看问题不得提交");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 折叠态 ←→ 同样翻看问题，且不解除折叠。
#[tokio::test]
async fn ask_parked_arrow_keys_browse_questions() {
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(ask_modal_with(
        vec![ask_question("q1"), ask_question("q2")],
        None,
    ));

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    match app.modal.as_ref() {
        Some(Modal::Ask {
            question, parked, ..
        }) => {
            assert!(*parked, "翻看问题不解除折叠");
            assert_eq!(*question, 1);
        }
        _ => unreachable!("卡片应保留"),
    }
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    if let Some(Modal::Ask { question, .. }) = app.modal.as_ref() {
        assert_eq!(*question, 0, "折叠态 ← 应翻回上一问");
    }

    connection.shutdown().await;
    core.shutdown().await;
}

/// 默认高亮落在推荐项上；没有推荐才停在第一项。
#[test]
fn ask_highlight_defaults_to_recommendation() {
    assert_eq!(recommended_highlight(&multi_ask_question()), 0);
    let mut question = ask_question("q1");
    question.options.push(AskOption {
        label: "选项二".into(),
        description: None,
        preview: None,
    });
    question.recommended = vec![1];
    assert_eq!(recommended_highlight(&question), 1);
}

/// C7：ask 自定义输入的光标按字符移动，多字节字符不得被切在中间。
#[tokio::test]
async fn ask_custom_cursor_steps_by_char() {
    let (core, connection, mut app) = setup_test_app().await;
    app.modal = Some(ask_modal());

    // z 进入自定义编辑态，输入两个汉字。
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('z'),
    );
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('中'),
    );
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('文'),
    );
    assert_eq!(ask_custom(&app), ("中文".to_owned(), "中文".len()));

    // ← 回退一个字符后重绘：按字节移动会在 split_at 处 panic。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    assert_eq!(ask_custom(&app).1, "中".len(), "光标停在字符边界");
    draw_ask(&mut app);

    // Delete 删光标处字符、Backspace 删前一个字符，光标随之回退。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Delete);
    assert_eq!(ask_custom(&app), ("中".to_owned(), "中".len()));
    draw_ask(&mut app);
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Backspace,
    );
    assert_eq!(ask_custom(&app), (String::new(), 0));
    draw_ask(&mut app);

    // → 到行尾饱和；行首 ← 与空串 Backspace/Delete 都不越界。
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('中'),
    );
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    assert_eq!(ask_custom(&app).1, "中".len());
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Left);
    assert_eq!(ask_custom(&app).1, 0, "行首左移饱和");
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Backspace,
    );
    assert_eq!(ask_custom(&app), ("中".to_owned(), 0), "行首无前驱可删");
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Delete);
    assert_eq!(ask_custom(&app), (String::new(), 0), "Delete 删光标处字符");
    draw_ask(&mut app);

    connection.shutdown().await;
    core.shutdown().await;
}

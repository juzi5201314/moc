use super::*;

#[test]
fn completion_navigation_wraps_and_accepts_with_enter() {
    let mut app = app_with_completion();
    assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(0));

    // 向上循环到最后一项
    assert!(handle_completion_key(
        &mut app,
        KeyModifiers::NONE,
        KeyCode::Up
    ));
    assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(1));

    // 向下循环回到第一项
    assert!(handle_completion_key(
        &mut app,
        KeyModifiers::NONE,
        KeyCode::Down
    ));
    assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(0));

    // Enter 键确认当前选中的补全
    assert!(handle_completion_key(
        &mut app,
        KeyModifiers::NONE,
        KeyCode::Enter
    ));
    assert_eq!(app.prompt.text, "/resume ");
    assert!(app.completion.is_none());

    // Tab 与 Enter 共用确认分支：切换选中项后同样接受当前项。
    let mut app = app_with_completion();
    assert!(handle_completion_key(
        &mut app,
        KeyModifiers::NONE,
        KeyCode::Down
    ));
    assert_eq!(
        app.completion
            .as_ref()
            .map(|completion| completion.chosen().value.as_str()),
        Some("/retry")
    );
    assert!(handle_completion_key(
        &mut app,
        KeyModifiers::NONE,
        KeyCode::Tab
    ));
    assert_eq!(app.prompt.text, "/retry ");
    assert!(app.completion.is_none());
    assert_eq!(app.focus, Focus::Prompt);
}

#[tokio::test]
async fn terminal_event_arrows_preserve_completion_selection() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_str_raw("/co");
    app.completion = crate::widgets::completion::CompletionState::derive(&app);
    assert!(app.completion.is_some());
    assert_eq!(app.completion.as_ref().map(|c| c.selected), Some(0));

    // 经由终端事件向下移动选中项，不应被 prompt_changed 重置
    handle_terminal_event(
        &mut app,
        &connection,
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )),
    );
    assert_eq!(
        app.completion.as_ref().map(|c| c.selected),
        Some(1),
        "下方向键应切换到下一个候选而不被重置"
    );

    // 再次向下
    handle_terminal_event(
        &mut app,
        &connection,
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Down,
            KeyModifiers::NONE,
        )),
    );
    assert_eq!(
        app.completion.as_ref().map(|c| c.selected),
        Some(2),
        "再次按下方向键继续切换"
    );

    // 向上移动
    handle_terminal_event(
        &mut app,
        &connection,
        crossterm::event::Event::Key(crossterm::event::KeyEvent::new(
            KeyCode::Up,
            KeyModifiers::NONE,
        )),
    );
    assert_eq!(
        app.completion.as_ref().map(|c| c.selected),
        Some(1),
        "上方向键应切换回上一项"
    );

    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn ctrl_c_clears_input_when_nonempty_within_three_seconds() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_text("hello moc");
    assert_eq!(app.prompt_text(), "hello moc");

    // 第一次 Ctrl+C：提示 "再次按 Ctrl+C 清空输入"，不应清空
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "hello moc");
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));
    assert!(!app.should_quit);
    assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    // 3 秒内第二次 Ctrl+C：清空输入框
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "");
    assert!(!app.should_quit);
    assert!(!app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    // 此时输入框变空，再次按 Ctrl+C：应提示退出
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 退出"));
    assert!(!app.should_quit);
    assert!(app.has_ctrl_c_pending(CtrlCPending::Quit));

    // 3 秒内第四次 Ctrl+C：退出
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(app.should_quit);

    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn ctrl_c_times_out_after_three_seconds() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_text("some text");

    // 非空输入框：第一次按 Ctrl+C
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));

    // 模拟超过 3 秒
    if let Some((_, at)) = app.ctrl_c_pending.as_mut() {
        *at -= std::time::Duration::from_secs(4);
    }

    // 再次按 Ctrl+C：超时，不应清空，应重新触发第一次提示
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "some text");
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));
    assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    // 清空后测试空输入框超时
    app.prompt.clear();
    app.ctrl_c_pending = None;

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 退出"));
    assert!(!app.should_quit);

    // 模拟超过 3 秒
    if let Some((_, at)) = app.ctrl_c_pending.as_mut() {
        *at -= std::time::Duration::from_secs(4);
    }

    // 再次按 Ctrl+C：超时，不应退出
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(!app.should_quit);
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 退出"));

    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn other_key_resets_ctrl_c_pending() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_text("hello");

    // 第一次 Ctrl+C
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    // 按其他键（例如向右光标键）
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Right);
    assert!(!app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    // 再次按 Ctrl+C：应当重新作为第一次按，提示而不是清空
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "hello");
    assert_eq!(app.notice.as_deref(), Some("再次按 Ctrl+C 清空输入"));

    connection.shutdown().await;
    core.shutdown().await;
}

/// C5：Ctrl+C 清空输入后补全菜单立即失效，Enter/Tab 不得按陈旧区间替换空串。
#[tokio::test]
async fn ctrl_c_clear_invalidates_completion_menu() {
    let (core, connection, mut app) = setup_test_app().await;
    for ch in "/mo".chars() {
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
    }
    assert!(app.completion.is_some(), "输入 /mo 应打开补全菜单");

    // 连按两次 Ctrl+C：第一次提示，第二次清空。
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "/mo", "首次 Ctrl+C 只提示");
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "");
    assert!(app.completion.is_none(), "清空后补全菜单必须失效");

    // 空串上按 Enter/Tab：既不能越界替换，也不能吞掉按键语义。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(app.focus, Focus::Prompt, "空输入 Enter 无操作");
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Tab);
    assert_eq!(app.focus, Focus::Scrollback, "Tab 仍按焦点切换处理");

    connection.shutdown().await;
    core.shutdown().await;
}

/// C5：双 Esc 清空输入后同样不得留下补全菜单。
#[tokio::test]
async fn esc_clear_invalidates_completion_menu() {
    let (core, connection, mut app) = setup_test_app().await;
    for ch in "/re".chars() {
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
    }
    assert!(app.completion.is_some());

    // 首次 Esc 只关菜单，第二次进入清空确认，第三次清空。
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert!(app.completion.is_none());
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert_eq!(app.prompt_text(), "/re");
    assert_eq!(app.notice.as_deref(), Some("再次按 Esc 清空输入"));
    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Esc);
    assert_eq!(app.prompt_text(), "");
    assert!(app.completion.is_none(), "清空后补全菜单必须失效");

    handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Enter);
    assert_eq!(app.prompt_text(), "", "空输入 Enter 无操作");

    connection.shutdown().await;
    core.shutdown().await;
}

/// M16：没有选区时小写 `y` 照常输入；有选区时才走复制快捷键。
#[tokio::test]
async fn lowercase_y_inserts_without_selection() {
    let (core, connection, mut app) = setup_test_app().await;
    for ch in "yes".chars() {
        handle_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Char(ch));
    }
    assert_eq!(app.prompt_text(), "yes", "无选区时 y 必须进入输入框");

    // 拖选后 y 仍是复制键：内容不进输入框（选区正文由 paint_lines 提供）。
    app.prompt.clear();
    app.prompt_changed();
    app.selection = Some(((4, 2), (9, 2)));
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('y'),
    );
    assert_eq!(app.prompt_text(), "", "有选区时 y 不进入输入框");
    assert!(app.selection.is_some(), "空选区内容不清空选区");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 输入框选区按原文复制：宽字符、组合字符与尾部空格不丢失，反向拖选一致。
#[test]
fn prompt_drag_selection_preserves_text_and_highlight() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = app();
    let text = "甲乙 e\u{301}  \n尾行 ";
    app.prompt = crate::widgets::prompt::PromptState::from_text(text.into());
    let mut terminal = Terminal::new(TestBackend::new(30, 18)).unwrap();
    let mut view = crate::render::scrollback::ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .unwrap();
    let left = app.prompt_area.x + 4;
    let top = app.prompt_area.y + 1;
    let first = (left + 1, top);
    let last = (left + 4, top + 1);
    for (start, end) in [(first, last), (last, first)] {
        for (kind, (column, row)) in [
            (MouseEventKind::Down(MouseButton::Left), start),
            (MouseEventKind::Drag(MouseButton::Left), end),
            (MouseEventKind::Up(MouseButton::Left), end),
        ] {
            handle_mouse(
                &mut app,
                MouseEvent {
                    kind,
                    column,
                    row,
                    modifiers: KeyModifiers::NONE,
                },
            );
        }
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .unwrap();
        assert_eq!(selection_text(&app), text);
        let buffer = terminal.backend().buffer();
        assert_eq!(buffer[(left, top)].bg, app.theme.bg_selection);
        assert_eq!(buffer[(left + 4, top + 1)].bg, app.theme.bg_selection);
        assert_ne!(buffer[(left - 1, top)].bg, app.theme.bg_selection);
        assert_ne!(buffer[(left + 5, top + 1)].bg, app.theme.bg_selection);
    }
    app.selection = Some(((left + 5, top), (left + 5, top)));
    assert_eq!(selection_text(&app), "e\u{301}");
}

/// 拖选复制按显示列切片：端点格子包含在内，跨行时首尾行裁边、中间整行。
#[test]
fn selection_text_slices_columns_inclusively() {
    let mut app = app();
    app.scrollback_area = ratatui::layout::Rect::new(2, 1, 20, 4);
    app.paint_lines = vec![
        (1, "abcdef".into()),
        (2, "123456".into()),
        (3, "ABCDEF".into()),
    ];

    app.selection = Some(((4, 1), (6, 3)));
    assert_eq!(selection_text(&app), "cdef\n123456\nABCDE");
    app.selection = Some(((6, 3), (4, 1)));
    assert_eq!(
        selection_text(&app),
        "cdef\n123456\nABCDE",
        "反向拖选结果一致"
    );

    // 宽字符按显示列整字切，不劈开半个字。
    app.paint_lines = vec![(1, "中文字".into())];
    app.selection = Some(((2, 1), (5, 1)));
    assert_eq!(selection_text(&app), "中文");
    app.selection = Some(((2, 1), (7, 1)));
    assert_eq!(selection_text(&app), "中文字");
}

/// 图例里的复制与取消作用于选区，不停止生成、不关闭补全，也不覆盖错误通知。
#[tokio::test]
async fn selection_legend_actions_preserve_prompt_and_error() {
    use ratatui::{Terminal, backend::TestBackend};

    let (core, connection, mut app, _root) = setup_test_app_keeping_root().await;
    app.prompt.insert_text("/re");
    app.prompt_changed();
    app.prompt_running = true;
    app.set_error_notice("需要保留的错误");
    let original_notice = app.notice.clone();
    let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
    let mut view = crate::render::ScrollbackView::new();
    for code in [KeyCode::Esc, KeyCode::Char('c')] {
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .unwrap();
        let area = app.prompt_area;
        app.selection = Some(((area.x + 4, area.y + 1), (area.x + 6, area.y + 1)));
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .unwrap();
        let hit = app
            .key_hits
            .iter()
            .find(|hit| hit.code == code && hit.area.y == area.bottom() - 1)
            .copied()
            .expect("选区操作应当可以点击");
        handle_mouse_with_connection(
            &mut app,
            &connection,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column: hit.area.x,
                row: hit.area.y,
                modifiers: KeyModifiers::NONE,
            },
        );
        assert!(app.selection.is_none());
        assert_eq!(app.prompt_text(), "/re");
        assert!(app.completion.is_some());
        assert!(app.prompt_running);
        assert!(!app.cancel_requested);
        assert!(!app.should_quit);
        assert!(app.modal.is_none());
        assert_eq!(app.notice, original_notice);
        assert!(app.notice_error);
    }
    connection.shutdown().await;
    core.shutdown().await;
}

/// 原生语义：有选区时 Ctrl+C 复制选区，而不是清空输入或取消。
#[tokio::test]
async fn ctrl_c_copies_selection_when_present() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_text("pending text");
    app.scrollback_area = ratatui::layout::Rect::new(0, 1, 20, 4);
    app.paint_lines = vec![(1, "copy me".into())];
    app.selection = Some(((0, 1), (6, 1)));

    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert_eq!(app.prompt_text(), "pending text", "复制优先于清空输入");
    assert!(app.selection.is_none());
    assert!(!app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    // 没有选区时恢复 Ctrl+C 原语义：第一次只提示清空输入。
    handle_key(
        &mut app,
        &connection,
        KeyModifiers::CONTROL,
        KeyCode::Char('c'),
    );
    assert!(app.has_ctrl_c_pending(CtrlCPending::ClearPrompt));

    connection.shutdown().await;
    core.shutdown().await;
}

/// 右键复制：有选区时与 Windows Terminal 一样直接落剪贴板并清掉选区。
#[tokio::test]
async fn right_click_copies_selection() {
    let mut app = app();
    app.scrollback_area = ratatui::layout::Rect::new(0, 1, 20, 4);
    app.paint_lines = vec![(1, "copy me".into())];
    app.selection = Some(((0, 1), (6, 1)));
    let mouse = |kind| MouseEvent {
        kind,
        column: 3,
        row: 1,
        modifiers: KeyModifiers::NONE,
    };

    handle_mouse(&mut app, mouse(MouseEventKind::Up(MouseButton::Right)));
    assert!(app.selection.is_none());
}

/// 普通提示（非 `/` 命令）发送后必须能按 ↑ 召回，不能只有斜杠命令进历史。
#[tokio::test]
async fn submitted_prompt_enters_input_history() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt_running = true;
    app.prompt.insert_str_raw("继续");
    crate::queue::submit(&mut app, &connection, false);
    assert!(app.prompt.is_empty(), "提交后输入框清空");
    assert!(app.prompt.history_up(), "刚发送的提示应在历史里");
    assert_eq!(app.prompt.text, "继续");
    connection.shutdown().await;
    core.shutdown().await;
}

/// 提交历史会落盘；重新挂载（等价重启）后仍能召回。
#[tokio::test]
async fn submitted_history_survives_restart() {
    let (core, connection, mut app) = setup_test_app().await;
    let home = core.config_home();
    app.load_input_history(&home);
    app.prompt.insert_str_raw("继续");
    crate::queue::submit(&mut app, &connection, false);
    assert!(app.prompt.is_empty());

    let mut restarted = App::new(Theme::global(), app.events.clone());
    restarted.load_input_history(&home);
    assert!(restarted.prompt.history_up(), "重启后仍应召回上次的提示");
    assert_eq!(restarted.prompt.text, "继续");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 「按项目」范围只召回当前会话目录的提示。
#[tokio::test]
async fn project_history_scope_filters_by_session_cwd() {
    let (core, connection, mut app) = setup_test_app().await;
    let home = core.config_home();
    let mut history = crate::history::InputHistory::load(&home);
    history.record("A 项目的提示", "/project/a");
    history.record("B 项目的提示", "/project/b");
    app.history = history;
    app.preferences.history_scope = crate::history::HistoryScope::Project;
    app.session.as_mut().expect("测试会话").cwd = "/project/a".into();
    app.refresh_input_history();

    assert!(app.prompt.history_up());
    assert_eq!(app.prompt.text, "A 项目的提示");
    assert!(!app.prompt.history_up(), "只允许当前项目的条目");

    connection.shutdown().await;
    core.shutdown().await;
}

/// 没有会话时提交不能把草稿吃掉。
#[tokio::test]
async fn submitting_without_session_keeps_the_draft() {
    let (core, connection, mut app) = setup_test_app().await;
    app.session = None;
    app.prompt.insert_str_raw("别丢了我");
    crate::queue::submit(&mut app, &connection, false);
    assert_eq!(app.prompt.text, "别丢了我", "被拒绝的草稿要回到输入框");
    assert_eq!(
        app.notice.as_deref(),
        Some("请先用 /model 选择模型；没有候选时到 /config 添加供应商")
    );
    connection.shutdown().await;
    core.shutdown().await;
}

#[tokio::test]
async fn slash_paths_submit_as_normal_messages() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt_running = true;
    let session = app.session.as_ref().unwrap().id.clone();
    for text in [
        "/tmp/a.txt",
        "/tmp",
        "/goal/file.txt",
        "/tmp/my file.txt 请检查",
        "/unknown-command",
    ] {
        for paste in [false, true] {
            if paste {
                app.prompt.insert_text(text);
            } else {
                for ch in text.chars() {
                    app.prompt.insert_char(ch);
                }
            }
            commit_prompt(&mut app, &connection);
            assert_eq!(app.steers[&session].back().unwrap().draft.text, text);
            assert!(app.prompt.is_empty(), "提交后清空输入框");
            assert!(
                app.notice
                    .as_deref()
                    .is_none_or(|text| !text.starts_with("未知命令"))
            );
        }
    }
    connection.shutdown().await;
    core.shutdown().await;
}

/// 斜杠命令不吃附件：带图片时拒绝执行并原样保留输入，删掉占位后同一条命令可执行。
#[tokio::test]
async fn slash_command_with_image_keeps_input() {
    let (core, connection, mut app) = setup_test_app().await;
    app.prompt.insert_str_raw("/help ");
    app.prompt.insert_image("image/png".into(), "AAAA".into());
    commit_prompt(&mut app, &connection);
    assert!(app.modal.is_none(), "带图片时不得执行命令");
    assert_eq!(
        app.notice.as_deref(),
        Some("斜杠命令不支持图片，请移除图片后执行")
    );
    assert_eq!(app.prompt.text, "/help [image#1]", "输入原样保留");

    app.prompt.backspace();
    commit_prompt(&mut app, &connection);
    assert!(matches!(app.modal, Some(Modal::Help { .. })));
    assert!(app.prompt.is_empty());
    connection.shutdown().await;
    core.shutdown().await;
}

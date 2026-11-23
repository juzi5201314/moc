use super::*;

/// HUD 鼠标：单击切换展开/折叠、按住卡片拖动；HUD 上的按下不启动转写点击。
#[test]
fn todo_hud_mouse_toggles_and_drags() {
    let mut app = app();
    app.scrollback_area = ratatui::layout::Rect::new(0, 2, 80, 20);
    let mouse = |kind: MouseEventKind, column: u16, row: u16| MouseEvent {
        kind,
        column,
        row,
        modifiers: KeyModifiers::NONE,
    };

    // 展开态只有卡片命中区；悬停算命中但不改状态。
    app.todo_hud.card_area = ratatui::layout::Rect::new(40, 2, 35, 10);
    app.hover_chart = Some((0, 0));
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Moved, 42, 3)
    ));
    assert!(app.todo_hud.is_open());
    assert!(app.hover_chart.is_none(), "悬停 HUD 时清掉图表高亮");

    // 单击卡片折叠成胶囊；不进入转写点击/拖选状态。
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Down(MouseButton::Left), 42, 3)
    ));
    assert!(app.pending_click.is_none());
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Up(MouseButton::Left), 42, 3)
    ));
    assert!(!app.todo_hud.is_open());

    // 折叠态单击胶囊回到展开。
    app.todo_hud.chip_area = ratatui::layout::Rect::new(60, 2, 15, 1);
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Down(MouseButton::Left), 62, 2)
    ));
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Up(MouseButton::Left), 62, 2)
    ));
    assert!(app.todo_hud.is_open());

    // 按住卡片拖过阈值：更新记忆位置，保持展开。
    app.todo_hud.card_area = ratatui::layout::Rect::new(40, 2, 35, 10);
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Down(MouseButton::Left), 42, 3)
    ));
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Drag(MouseButton::Left), 45, 6)
    ));
    assert_eq!(app.todo_hud.drag_origin, Some((43, 5)));
    assert!(handle_todo_hud_mouse(
        &mut app,
        &mouse(MouseEventKind::Up(MouseButton::Left), 45, 6)
    ));
    assert!(app.todo_hud.is_open());
}

/// 列表类模态（PickerState）：滚轮只滚 viewport，悬停只铺底色，选中项不动；
/// 键盘导航重新显露选中项。语义与模型选择器一致。
#[tokio::test]
async fn sessions_wheel_and_hover_preserve_selection() {
    use crossterm::event::MouseEventKind::{Moved, ScrollDown};
    let (core, connection, mut app) = setup_test_app().await;
    let session = |index: usize| moc_protocol::methods::SessionInfo {
        id: format!("ses_{index:04}"),
        title: format!("会话 {index}"),
        model: "model".into(),
        permission: Default::default(),
        engine: Default::default(),
        autonomy: Default::default(),
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        parent_session_id: None,
        forked_from: None,
        agent_preset: None,
        delegation_depth: 0,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    };
    app.modal = Some(Modal::Sessions {
        sessions: (0..30).map(session).collect(),
        cwd: "/tmp".into(),
        scope: crate::modal::SessionScope::All,
        picker: Default::default(),
    });
    let _ = draw_frame(&mut app, 100, 24);
    let (_, row) = *app.modal_item_hits.first().expect("列表有命中行");
    for step in 0..4 {
        handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, row.x, row.y));
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(Moved, row.x, row.y + step % 2),
        );
        let _ = draw_frame(&mut app, 100, 24);
        let Some(Modal::Sessions { picker, .. }) = &app.modal else {
            panic!("选择器应保持打开")
        };
        assert_eq!(picker.selected, 0, "滚轮与悬停不能改选中项");
        assert_eq!(picker.offset, (step as usize + 1) * 3);
        assert_eq!(app.modal_item_hits.first().unwrap().0, picker.offset);
    }
    // 普通重绘不把视口拉回选中项。
    let before = app.modal_item_hits.clone();
    let _ = draw_frame(&mut app, 100, 24);
    assert_eq!(app.modal_item_hits, before, "重绘不能拉回选中项");
    // 悬停只铺底色。
    let (_, hover_row) = *app.modal_item_hits.last().unwrap();
    handle_mouse_with_connection(
        &mut app,
        &connection,
        mouse_event(Moved, hover_row.x, hover_row.y),
    );
    let terminal = draw_frame(&mut app, 100, 24);
    assert_eq!(
        terminal.backend().buffer()[(hover_row.x, hover_row.y)].bg,
        app.theme.bg_hover
    );
    // 键盘导航重新显露选中项。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Down);
    let _ = draw_frame(&mut app, 100, 24);
    let Some(Modal::Sessions { picker, .. }) = &app.modal else {
        panic!("选择器应保持打开")
    };
    assert_eq!(picker.selected, 1);
    assert!(
        app.modal_item_hits.iter().any(|(index, _)| *index == 1),
        "键盘导航后选中项在视口内"
    );
    connection.shutdown().await;
    core.shutdown().await;
}

/// /jobs：滚轮只滚 viewport；悬停不换行也不再误撤待确认标记。
#[tokio::test]
async fn jobs_wheel_and_hover_preserve_selection_and_pending_kill() {
    use crossterm::event::MouseEventKind::{Moved, ScrollDown};
    let (core, connection, mut app) = setup_test_app().await;
    let job = |index: usize| moc_protocol::methods::JobInfo {
        job_id: format!("job-{index}"),
        command: format!("sleep {index}"),
        description: None,
        tool_call_id: format!("call-{index}"),
        status: moc_protocol::methods::JobStatus::Running,
        exit_code: None,
        lifecycle: moc_protocol::methods::JobLifecycle::Monitored,
        origin: moc_protocol::methods::JobOrigin::Explicit,
        stalled: false,
        output_elided: false,
        started_at_ms: 0,
        duration_ms: None,
        log_uri: format!("artifact://jobs/{index}"),
    };
    app.modal = Some(Modal::Jobs {
        jobs: (0..30).map(job).collect(),
        selected: 0,
        pending_kill: None,
        offset: 0,
        scroll: Default::default(),
        pointer: None,
    });
    let _ = draw_frame(&mut app, 100, 24);
    // 待确认标记：按一次 x。
    handle_modal_key(
        &mut app,
        &connection,
        KeyModifiers::NONE,
        KeyCode::Char('x'),
    );
    let (_, row) = *app.modal_item_hits.last().expect("列表有命中行");
    handle_mouse_with_connection(&mut app, &connection, mouse_event(Moved, row.x, row.y));
    let Some(Modal::Jobs {
        selected,
        pending_kill,
        ..
    }) = &app.modal
    else {
        panic!("任务列表应保持打开")
    };
    assert_eq!(*selected, 0, "悬停不能改选中项");
    assert_eq!(*pending_kill, Some(0), "悬停不能撤掉待确认标记");
    for _ in 0..3 {
        handle_mouse_with_connection(&mut app, &connection, mouse_event(ScrollDown, row.x, row.y));
    }
    let _ = draw_frame(&mut app, 100, 24);
    let Some(Modal::Jobs {
        selected, offset, ..
    }) = &app.modal
    else {
        panic!("任务列表应保持打开")
    };
    assert_eq!(*selected, 0, "滚轮不能改选中项");
    assert_eq!(*offset, 9, "滚轮推进列表视口");
    assert_eq!(app.modal_item_hits.first().unwrap().0, 9);
    connection.shutdown().await;
    core.shutdown().await;
}

/// 回退时间线：滚轮只滚列表 viewport，不改选中项；键盘导航重新显露。
#[tokio::test]
async fn rewind_wheel_scrolls_timeline_not_selection() {
    use crossterm::event::MouseEventKind::ScrollUp;
    let (core, connection, mut app) = setup_test_app().await;
    let entry = |index: usize| moc_protocol::methods::TranscriptEntry {
        id: format!("e{index}"),
        at_ms: index as u64,
        message: moc_protocol::Message::user_text(format!("提问 {index}")),
        tool: None,
        bash: None,
        agent: None,
        agent_result: None,
        goal_round: None,
        reminder: None,
        job_notice: None,
        response: None,
    };
    let mut state = crate::modal::RewindState::default();
    state.set_entries((0..30).map(entry).collect(), vec![]);
    let selected = state.selected;
    app.modal = Some(Modal::Rewind { state });
    let _ = draw_frame(&mut app, 120, 32);
    let timeline = app.rewind_timeline_area;
    assert!(!timeline.is_empty(), "时间线区域已登记");
    for _ in 0..3 {
        handle_mouse_with_connection(
            &mut app,
            &connection,
            mouse_event(ScrollUp, timeline.x + 1, timeline.y + 2),
        );
    }
    let _ = draw_frame(&mut app, 120, 32);
    let scrolled = match &app.modal {
        Some(Modal::Rewind { state }) => {
            assert_eq!(state.selected, selected, "滚轮不能改选中项");
            assert!(state.list_scroll > 0, "时间线视口已滚动");
            state.list_scroll
        }
        _ => panic!("回退视图应保持打开"),
    };
    // 普通重绘不把视口拉回选中项。
    let _ = draw_frame(&mut app, 120, 32);
    let Some(Modal::Rewind { state }) = &app.modal else {
        panic!("回退视图应保持打开")
    };
    assert_eq!(state.list_scroll, scrolled, "重绘不能拉回选中项");
    // 键盘导航重新显露选中项：视口向选中项回移。
    handle_modal_key(&mut app, &connection, KeyModifiers::NONE, KeyCode::Up);
    let _ = draw_frame(&mut app, 120, 32);
    let Some(Modal::Rewind { state }) = &app.modal else {
        panic!("回退视图应保持打开")
    };
    assert_eq!(state.selected, selected - 1);
    assert!(state.list_scroll > scrolled, "键盘导航把选中项收回窗内");
    connection.shutdown().await;
    core.shutdown().await;
}

/// 表格入口：点中切换图表形态，再点还原；列在按钮外不触发。
#[test]
fn clicking_the_table_entry_toggles_the_chart() {
    use crate::app::ChartHit;
    use crate::transcript::Block;

    let mut app = app();
    let text = "| 项 | 耗时 | 内存 |\n|---|---|---|\n| a | 10 ms | 2 MiB |\n| b | 20 ms | 4 MiB |";
    app.push_block(Block::Assistant { text: text.into() });
    app.scrollback_area = ratatui::layout::Rect::new(3, 0, 80, 24);
    // 命中表由绘制填充；这里直接给出按钮所在行与列区间
    app.chart_hits = vec![ChartHit {
        entry: 0,
        table: 0,
        row: 12,
        x: 60,
        w: 12,
    }];

    let click = |app: &mut App, column: u16| {
        handle_mouse(
            app,
            MouseEvent {
                kind: MouseEventKind::Down(MouseButton::Left),
                column,
                row: 12,
                modifiers: KeyModifiers::NONE,
            },
        );
        handle_mouse(
            app,
            MouseEvent {
                kind: MouseEventKind::Up(MouseButton::Left),
                column,
                row: 12,
                modifiers: KeyModifiers::NONE,
            },
        );
    };

    // 列落在按钮之外：既不切换，也不动档位
    let generation = app.blocks[0].generation;
    click(&mut app, 59);
    assert!(
        app.blocks[0].table_chart(0).is_none(),
        "按钮外点击不该展开图表"
    );
    assert_eq!(app.blocks[0].generation, generation);

    // 点中按钮：展开图表（渲染缓存失效）
    click(&mut app, 66);
    let state = app.blocks[0].table_chart(0).expect("应当展开图表");
    assert_eq!(state.image_id, 1, "首张图分配 1 号图像 ID");
    assert!(state.payload.is_none(), "载荷在贴图时才生成");
    assert!(app.blocks[0].generation > generation, "展开必须让缓存失效");

    // 再点一次：还原成表格
    click(&mut app, 66);
    assert!(app.blocks[0].table_chart(0).is_none(), "二次点击应还原");
}

/// 滚轮方向：上滚进入更早的历史，下滚回到尾部（offset 语义为距尾部行数）。
#[test]
fn wheel_scrolls_toward_and_away_from_tail() {
    fn mouse(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.scroll_offset = 10;

    handle_mouse(&mut app, mouse(MouseEventKind::ScrollUp));
    assert_eq!(app.scroll_offset, 13, "上滚应查看更早的历史");
    handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown));
    assert_eq!(app.scroll_offset, 10, "下滚应回到尾部");

    app.scroll_offset = 1;
    handle_mouse(&mut app, mouse(MouseEventKind::ScrollDown));
    assert_eq!(app.scroll_offset, 0, "已在尾部时下滚饱和为 0");
}

/// 点击「回到底部」胶囊 = 贴尾；按下不进入文本选区。
#[test]
fn jump_bottom_pill_click_returns_to_tail() {
    use ratatui::layout::Rect;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.scroll_offset = 10;
    app.jump_bottom_area = Rect::new(30, 18, 1, 1);

    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 30, 18),
    );
    assert_eq!(app.scroll_offset, 0, "点中胶囊直接贴尾");
    assert!(app.pending_click.is_none(), "按下不进入文本选区");

    app.scroll_offset = 10;
    app.jump_bottom_area = Rect::default();
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 30, 18),
    );
    assert_eq!(app.scroll_offset, 10, "胶囊隐藏后同位置是普通点击");
}

/// 右缘滚动条：按在滑块上保持抓取点拖动，按在轨道空白处把滑块中线挪过来；
/// 命中滚动条的按下与拖动不进入文本选区。
#[test]
fn scrollbar_click_and_drag_move_the_viewport() {
    use crate::app::ScrollbackScrollbar;
    use ratatui::layout::Rect;

    fn mouse(kind: MouseEventKind, column: u16, row: u16) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    // 终端 80 列、轨道占滚动区 4..14 行：右缘 79 列，10 行里滑块 3 行。
    app.scrollbar = ScrollbackScrollbar {
        track: Rect::new(79, 4, 1, 10),
        thumb: 3,
        max_offset: 20,
    };
    app.scroll_max_offset = 20;

    // 跟随尾部时滑块贴底（第 11 行起）：按在它上面不跳动，只进入拖拽。
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 79, 11),
    );
    assert_eq!(app.scroll_offset, 0, "按在滑块上不改变偏移");
    assert!(app.scrollbar_drag.is_some());
    assert!(app.pending_click.is_none(), "滚动条按下不进入文本选区");

    // 拖到轨道顶端：走满历史。
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Drag(MouseButton::Left), 79, 4),
    );
    assert_eq!(app.scroll_offset, 20);
    assert!(app.selection.is_none(), "拖动滚动条不产生选区");
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), 79, 4),
    );
    assert!(app.scrollbar_drag.is_none(), "抬起后结束拖拽");

    // 按在轨道空白处：滑块中线（3/2 行）挪到光标所在行。
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 79, 8),
    );
    assert_eq!(app.scroll_offset, 12);
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Up(MouseButton::Left), 79, 8),
    );

    // 轨道之外的按下仍走普通点击路径。
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 40, 8),
    );
    assert!(app.pending_click.is_some());

    // 内容不溢出（没有画过滚动条）时，右缘不构成命中区。
    app.scrollbar = ScrollbackScrollbar::default();
    app.pending_click = None;
    handle_mouse(
        &mut app,
        mouse(MouseEventKind::Down(MouseButton::Left), 79, 8),
    );
    assert!(app.scrollbar_drag.is_none());
    assert!(app.pending_click.is_some(), "没有滚动条时右缘仍是普通点击");
}

/// 悬停展开不重排正文；从扩展列抓取后跨区域拖动，松开与失焦均能收起。
#[tokio::test]
async fn scrollbar_hover_and_drag_preserve_layout_and_capture() {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};

    let (core, connection, mut app) = setup_test_app().await;
    app.push_block(crate::transcript::Block::Assistant {
        text: "用于验证正文换行不随滚动条展开变化的长内容。".repeat(400),
    });
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    let mut view = render::ScrollbackView::new();
    terminal
        .draw(|frame| render::draw(frame, &mut app, &mut view))
        .unwrap();
    let area = app.scrollback_area;
    let track = app.scrollbar.track;
    let original = terminal.backend().buffer().clone();
    let draw = |app: &mut App,
                terminal: &mut Terminal<TestBackend>,
                view: &mut render::ScrollbackView,
                expanded: bool| {
        terminal
            .draw(|frame| render::draw(frame, app, view))
            .unwrap();
        let buffer = terminal.backend().buffer();
        assert_eq!(
            buffer[(78, track.y)].symbol(),
            if expanded { "█" } else { " " }
        );
        assert_eq!(
            buffer[(79, track.y)].symbol(),
            if expanded { "█" } else { "▐" }
        );
        assert_eq!(app.scrollback_area, area, "展开不挤压正文");
    };
    draw(&mut app, &mut terminal, &mut view, false);

    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(mouse_event(MouseEventKind::Moved, 78, track.y)),
    );
    draw(&mut app, &mut terminal, &mut view, true);
    assert_eq!(app.scroll_offset, 0, "悬停不改变滚动位置");
    for y in area.top()..area.bottom() {
        for x in area.left()..area.right() {
            assert_eq!(
                terminal.backend().buffer()[(x, y)],
                original[(x, y)],
                "正文不重排"
            );
        }
    }

    // BTW 会吞掉落入其区域的鼠标事件；移出仍须收起，已开始的拖拽不能中断。
    app.btw.area = Rect::new(10, track.y, 20, track.height);
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(mouse_event(MouseEventKind::Moved, 10, track.y)),
    );
    draw(&mut app, &mut terminal, &mut view, false);
    let thumb_row = track.y + app.scrollbar.thumb_top(app.scroll_offset);
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(left_click(78, thumb_row)),
    );
    assert_eq!(app.scroll_offset, 0, "扩展列按在滑块上不跳动");
    assert!(app.pending_click.is_none());
    draw(&mut app, &mut terminal, &mut view, true);

    app.btw.area = Rect::new(10, track.y, 20, track.height);
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(mouse_event(
            MouseEventKind::Drag(MouseButton::Left),
            10,
            track.y,
        )),
    );
    assert_eq!(
        app.scroll_offset, app.scroll_max_offset,
        "横向移出后仍能拖到历史顶端"
    );
    assert!(app.selection.is_none());
    draw(&mut app, &mut terminal, &mut view, true);
    app.btw.area = Rect::new(10, track.y, 20, track.height);
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(mouse_event(
            MouseEventKind::Up(MouseButton::Left),
            10,
            track.y,
        )),
    );
    assert!(app.scrollbar_drag.is_none());
    draw(&mut app, &mut terminal, &mut view, false);

    handle_terminal_event(&mut app, &connection, Event::Mouse(left_click(78, track.y)));
    draw(&mut app, &mut terminal, &mut view, true);
    handle_terminal_event(&mut app, &connection, Event::FocusLost);
    assert!(app.scrollbar_drag.is_none());
    draw(&mut app, &mut terminal, &mut view, false);

    app.modal = Some(Modal::Help { scroll: 0 });
    handle_terminal_event(
        &mut app,
        &connection,
        Event::Mouse(mouse_event(MouseEventKind::Moved, 78, track.y)),
    );
    draw(&mut app, &mut terminal, &mut view, false);
    connection.shutdown().await;
    core.shutdown().await;
}

/// 长输入的拖选不因按下移动光标而跳动视口，软折行不混入换行符。
#[test]
fn prompt_drag_selection_keeps_scrolled_viewport() {
    use ratatui::{Terminal, backend::TestBackend};

    let mut app = app();
    app.prompt = crate::widgets::prompt::PromptState::from_text("abcdefghij".repeat(24));
    let mut terminal = Terminal::new(TestBackend::new(30, 18)).unwrap();
    let mut view = crate::render::scrollback::ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .unwrap();
    let width = usize::from(app.prompt_area.width - 6);
    app.prompt.cursor = width * 2;
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .unwrap();
    let left = app.prompt_area.x + 4;
    let top = app.prompt_area.y + 1;
    for (kind, column, row) in [
        (MouseEventKind::Down(MouseButton::Left), left + 2, top + 1),
        (MouseEventKind::Drag(MouseButton::Left), left + 4, top + 2),
        (MouseEventKind::Up(MouseButton::Left), left + 4, top + 2),
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
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .unwrap();
        assert_eq!(app.prompt.cursor, width * 2);
    }
    assert_eq!(
        selection_text(&app),
        &app.prompt.text[width * 3 + 2..width * 4 + 5]
    );
}

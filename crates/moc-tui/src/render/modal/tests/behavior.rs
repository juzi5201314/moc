use super::*;

/// 回退模态：在终端缓冲里正确绘制时间线分界、回退横幅与底部 dock 操作栏。
#[test]
fn rewind_modal_draws_omp_timeline_and_dock_in_buffer() {
    use moc_protocol::{Message, TranscriptEntry};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _receiver) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);

    let mut rewind_state = crate::modal::RewindState::default();
    rewind_state.set_entries(
        vec![
            TranscriptEntry {
                id: "u1".into(),
                at_ms: 1700000000000,
                message: Message::user_text("第一句提问：实现快速排序"),
                tool: None,
                bash: None,
                agent: None,
                agent_result: None,
                goal_round: None,
                reminder: None,
            },
            TranscriptEntry {
                id: "a1".into(),
                at_ms: 1700000001000,
                message: Message::assistant_text("好的，已为你实现快排"),
                tool: None,
                bash: None,
                agent: None,
                agent_result: None,
                goal_round: None,
                reminder: None,
            },
        ],
        vec![],
    );
    app.modal = Some(Modal::Rewind {
        state: rewind_state,
    });

    let (width, height) = (120u16, 32u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");

    let rows: Vec<String> = (0..height)
        .map(|row| {
            (0..width)
                .filter_map(|x| terminal.backend().buffer().cell((x, row)))
                .map(|cell| cell.symbol())
                .collect::<String>()
        })
        .collect();
    let full_output: String = rows.join("\n").replace(' ', "");

    assert!(
        full_output.contains("回退"),
        "应包含回退标识：{full_output}"
    );
    assert!(
        full_output.contains("回退到此处") || full_output.contains("从此处继续"),
        "应包含 omp 风格的回退横幅或按钮动作：{full_output}"
    );
    assert!(
        full_output.contains("第一句提问"),
        "时间线应渲染用户提问：{full_output}"
    );
}

#[test]
fn modal_card_renders_solid_opaque_background() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Rect;

    let (events, _rx) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    app.modal = Some(Modal::Help { scroll: 0 });

    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
    terminal
        .draw(|frame| {
            frame.buffer_mut().set_style(
                Rect::new(0, 0, 100, 30),
                ratatui::style::Style::new().bg(Color::Red),
            );
            super::super::draw_modal(frame, &mut app);
        })
        .expect("绘制成功");

    let theme = app.theme;
    let buffer = terminal.backend().buffer();
    // 居中卡片区域：在 100x30 终端下检查中心卡片覆盖的所有单元格
    // 验证中心点必然是不透底的实色
    let center_cell = buffer.cell((50, 15)).expect("中心单元格存在");
    assert_ne!(center_cell.bg, Color::Reset, "模态卡片中心不应为 Reset");
    assert_ne!(center_cell.bg, Color::Red, "模态卡片中心不应泄露底层背景");
    assert!(
        center_cell.bg == theme.bg_base || center_cell.bg == theme.bg_visual,
        "模态卡片中心应为 bg_base，实际为 {:?}",
        center_cell.bg
    );
}

/// 权限确认正文：事件层只加不减的偏移由渲染写回收紧。
#[test]
fn permission_scroll_is_clamped_on_draw() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _rx) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let body = (1..=60)
        .map(|index| format!("第 {index} 行命令全文"))
        .collect::<Vec<_>>()
        .join("\n");
    app.modal = Some(Modal::Permission {
        params: moc_protocol::methods::PermissionRequestParams {
            session_id: "s".into(),
            prompt_id: "p".into(),
            request_id: "r".into(),
            tool_call_id: "c".into(),
            tool_name: "bash".into(),
            title: "权限确认".into(),
            body: Some(body),
            diff: None,
            options: vec![moc_protocol::methods::PermissionOption {
                id: "allow".into(),
                label: "允许".into(),
                kind: moc_protocol::methods::PermissionOptionKind::AllowOnce,
            }],
        },
        selected: 0,
        scroll_offset: usize::MAX,
        reply: None,
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
    let snapshot = |terminal: &Terminal<TestBackend>| -> String {
        (0..30u16)
            .map(|y| {
                (0..100u16)
                    .filter_map(|x| terminal.backend().buffer().cell((x, y)).map(|c| c.symbol()))
                    .collect::<String>()
            })
            .collect()
    };
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");
    let Some(Modal::Permission {
        scroll_offset: max, ..
    }) = app.modal
    else {
        unreachable!()
    };
    assert!((1..1000).contains(&max), "写回应把偏移收紧到底部：{max}");
    let first = snapshot(&terminal);
    // 正文溢出：末行应给出标准滚动状态行而非静默截断。
    assert!(first.contains('⋯'), "溢出应显示滚动状态行");

    if let Some(Modal::Permission { scroll_offset, .. }) = &mut app.modal {
        *scroll_offset = max - 1;
    }
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");
    assert_ne!(first, snapshot(&terminal), "偏移减一应改变画面");
}

/// 防复发守卫：通用卡片压在满屏宽字符上时，左边框每一行都必须真的发到终端。
///
/// 两帧绘制让 TestBackend 走真实 diff；背景起始列取 0/1 两种奇偶，保证
/// 「宽字符引导格恰在边框左邻」这一触发条件对任意卡片 x 都会出现。
#[test]
fn modal_left_border_survives_wide_char_background() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::style::Style;

    // 帮助（双列表）、上下文占用（紧凑宽度）、会话列表（通用宽度）：覆盖三种卡宽。
    let modal = |kind: usize| match kind {
        0 => Modal::Help { scroll: 0 },
        1 => Modal::ContextContent {
            state: Default::default(),
        },
        _ => Modal::Sessions {
            sessions: Vec::new(),
            cwd: "/tmp".into(),
            scope: SessionScope::All,
            picker: PickerState::default(),
        },
    };
    for (width, height) in [(120u16, 40u16), (80, 30), (71, 24)] {
        for kind in 0..3 {
            for start in [0u16, 1] {
                let (events, _rx) = unbounded_channel();
                let mut app = App::new(Theme::global(), events);
                app.modal = Some(modal(kind));
                let background = |frame: &mut ratatui::Frame| {
                    let fill = "中".repeat(usize::from(width) / 2);
                    for y in 0..height {
                        frame.buffer_mut().set_string(start, y, &fill, Style::new());
                    }
                };
                let mut terminal =
                    Terminal::new(TestBackend::new(width, height)).expect("构造测试终端");
                terminal.draw(background).expect("首帧");
                terminal
                    .draw(|frame| {
                        background(frame);
                        super::super::draw_modal(frame, &mut app);
                    })
                    .expect("模态帧");
                let screen = terminal.backend().buffer();
                let (left, top) = (0..height)
                    .find_map(|y| {
                        (0..width)
                            .find(|&x| screen[(x, y)].symbol() == "╭")
                            .map(|x| (x, y))
                    })
                    .expect("卡片左上角");
                let bottom = (top..height)
                    .find(|&y| screen[(left, y)].symbol() == "╰")
                    .expect("卡片左下角");
                for y in top..=bottom {
                    let symbol = screen[(left, y)].symbol();
                    assert!(
                        matches!(symbol, "│" | "╭" | "╰"),
                        "{width}×{height} 起始列 {start} 的 {} 第 {y} 行左边框缺失：{symbol:?}",
                        modal(kind).title()
                    );
                }
            }
        }
    }
}

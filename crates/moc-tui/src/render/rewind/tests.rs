use super::*;
use moc_protocol::{Message, TranscriptEntry};
use ratatui::{Terminal, backend::TestBackend};

#[test]
fn rewind_modal_panels_render_with_rounded_borders() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let mut rewind_state = RewindState::default();
    rewind_state.set_entries(
        vec![
            TranscriptEntry {
                id: "entry-1".into(),
                at_ms: 1700000000000,
                message: Message::user_text("提问一：详细提问内容"),
                tool: None,
                bash: None,
                agent: None,
                agent_result: None,
                goal_round: None,
                reminder: None,
                job_notice: None,
            },
            TranscriptEntry {
                id: "entry-2".into(),
                at_ms: 1700000001000,
                message: Message::assistant_text("回复一：详细回复内容"),
                tool: None,
                bash: None,
                agent: None,
                agent_result: None,
                goal_round: None,
                reminder: None,
                job_notice: None,
            },
        ],
        vec![],
    );
    rewind_state.selected = 0;
    app.modal = Some(Modal::Rewind {
        state: rewind_state,
    });

    let (width, height) = (100u16, 30u16);
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            draw(frame, &mut app, area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    let win = modal_area(Rect::new(0, 0, width, height));
    assert_eq!(buffer.cell((win.x, win.y)).unwrap().symbol(), "╭");
    assert_eq!(buffer.cell((win.right() - 1, win.y)).unwrap().symbol(), "╮");
    assert_eq!(
        buffer.cell((win.x, win.bottom() - 1)).unwrap().symbol(),
        "╰"
    );
    assert_eq!(
        buffer
            .cell((win.right() - 1, win.bottom() - 1))
            .unwrap()
            .symbol(),
        "╯"
    );

    let all_symbols: String = (0..height)
        .flat_map(|y| (0..width).map(move |x| buffer.cell((x, y)).unwrap().symbol()))
        .collect();
    assert!(all_symbols.contains('╭'), "必须包含圆角 ╭");
    assert!(all_symbols.contains('╮'), "必须包含圆角 ╮");
    assert!(all_symbols.contains('╰'), "必须包含圆角 ╰");
    assert!(all_symbols.contains('╯'), "必须包含圆角 ╯");
    assert!(!all_symbols.contains('┌'), "不应含有直角 ┌");
    assert!(!all_symbols.contains('┐'), "不应含有直角 ┐");
    assert!(!all_symbols.contains('└'), "不应含有直角 └");
    assert!(!all_symbols.contains('┘'), "不应含有直角 ┘");
}

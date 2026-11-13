use super::*;

/// 计划阅读面板：事件层只加不减的偏移由渲染写回收紧，贴底正文不为空。
#[test]
fn plan_read_scroll_is_clamped_on_draw() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (events, _rx) = unbounded_channel();
    let mut app = App::new(Theme::global(), events);
    let markdown = (1..=200)
        .map(|index| format!("第 {index} 行计划正文"))
        .collect::<Vec<_>>()
        .join("\n");
    let mut state = crate::modal::PlanReviewState::new(
        moc_protocol::methods::PlanInfo {
            plan_id: "p1".into(),
            title: "计划".into(),
            path: "plan.md".into(),
            markdown,
            status: moc_protocol::methods::PlanStatus::Submitted,
            version: 1,
            previous_plan_id: None,
            annotations: Vec::new(),
        },
        SessionEngine::Standard,
    );
    state.scroll_offset = usize::MAX;
    app.modal = Some(Modal::Plan {
        state: Box::new(state),
    });
    let mut terminal = Terminal::new(TestBackend::new(100, 30)).expect("构造测试终端");
    terminal
        .draw(|frame| super::super::draw_modal(frame, &mut app))
        .expect("绘制成功");
    let Some(Modal::Plan { state }) = &app.modal else {
        unreachable!()
    };
    assert!(
        (1..1000).contains(&state.scroll_offset),
        "写回应把偏移收紧到底部：{}",
        state.scroll_offset
    );
    // 贴底时正文区显示计划末尾内容，不是空白。
    let text: String = (0..30u16)
        .map(|y| {
            (0..100u16)
                .filter_map(|x| {
                    terminal
                        .backend()
                        .buffer()
                        .cell((x, y))
                        .map(|c| c.symbol().to_owned())
                })
                .collect::<String>()
        })
        .collect();
    assert!(
        text.replace(' ', "").contains("第200行"),
        "贴底应显示末尾正文：{text:?}"
    );
}

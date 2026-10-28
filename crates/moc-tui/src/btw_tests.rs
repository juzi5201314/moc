//! 旁路状态、输入隔离与 dock 渲染的轻量测试；不启动 core 或真实模型。

use super::*;
use crate::widgets::prompt::PromptState;
use moc_protocol::{SessionInfo, SessionMode, SessionPermission};
use ratatui::{Terminal, backend::TestBackend, buffer::CellWidth, layout::Rect, style::Style};

fn session(id: &str) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        title: "测试".into(),
        model: "test/model".into(),
        permission: SessionPermission::Full,
        mode: SessionMode::Normal,
        parent_session_id: None,
        forked_from: None,
        cwd: "/tmp".into(),
        created_at_ms: 0,
        updated_at_ms: 0,
        lock: None,
    }
}

fn app() -> App {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.session = Some(session("a"));
    app
}

fn result(params: &BtwParams, reason: FinishReason) -> BtwResult {
    BtwResult {
        session_id: params.session_id.clone(),
        request_id: params.request_id,
        model: "test/model".into(),
        text: "完整答案".into(),
        finish_reason: reason,
        usage: Usage {
            input_tokens: 17,
            output_tokens: 5,
            ..Usage::default()
        },
        history_turns_used: params.history.len(),
    }
}

fn update(params: &BtwParams, update: BtwUpdate) -> BtwUpdateParams {
    BtwUpdateParams {
        session_id: params.session_id.clone(),
        request_id: params.request_id,
        update,
    }
}

#[test]
fn parsing_distinguishes_command_and_preserves_question() {
    assert_eq!(question(" /btw \n问题\n第二行 "), Some("问题\n第二行"));
    assert_eq!(question("/btw"), Some(""));
    assert_eq!(question("/btwhatever"), None);
    assert_eq!(question("普通 /btw 问题"), None);
}

#[test]
fn only_complete_successes_replay_and_memory_has_twenty_turns() {
    let mut state = BtwState::default();
    for index in 0..MAX_BTW_HISTORY_TURNS + 3 {
        let params = state.begin("a".into(), format!("问题{index}"));
        let reason = if index == 4 {
            FinishReason::Length
        } else {
            FinishReason::Stop
        };
        state.finish("a", params.request_id, Ok(result(&params, reason)));
    }
    let thread = state.thread("a").unwrap();
    assert_eq!(thread.turns.len(), MAX_BTW_HISTORY_TURNS);
    assert_eq!(thread.turns.front().unwrap().question, "问题3");
    let params = state.begin("a".into(), "新问题".into());
    assert_eq!(params.history.len(), MAX_BTW_HISTORY_TURNS - 2);
    assert!(!params.history.iter().any(|entry| entry.question == "问题4"));
}

#[test]
fn clear_before_started_retains_cancel_rendezvous_and_does_not_resurrect() {
    let mut state = BtwState::default();
    let params = state.begin("a".into(), "问题".into());
    state.clear("a");
    assert!(state.flight.is_some());
    assert!(state.take_cancel().is_none());
    state.apply_update(update(&params, BtwUpdate::Started));
    let cancel = state.take_cancel().unwrap();
    assert_eq!(cancel.session_id, "a");
    assert_eq!(cancel.request_id, params.request_id);
    assert!(state.take_cancel().is_none(), "只发送一次取消");
    state.apply_update(update(
        &params,
        BtwUpdate::TextDelta {
            text: "迟到".into(),
        },
    ));
    state.finish(
        "a",
        params.request_id,
        Ok(result(&params, FinishReason::Stop)),
    );
    assert!(state.thread("a").is_none());
    assert!(state.flight.is_none());
    let next = state.begin("a".into(), "新问题".into());
    assert!(next.request_id > params.request_id);
    assert!(next.history.is_empty());
}

#[test]
fn terminal_rpc_replaces_partial_text_and_rejects_late_delta() {
    let mut state = BtwState::default();
    let params = state.begin("a".into(), "问题".into());
    state.apply_update(update(
        &params,
        BtwUpdate::TextDelta {
            text: "半截".into(),
        },
    ));
    state.finish(
        "a",
        params.request_id,
        Ok(result(&params, FinishReason::Stop)),
    );
    state.apply_update(update(
        &params,
        BtwUpdate::TextDelta {
            text: "重复尾巴".into(),
        },
    ));
    assert_eq!(state.thread("a").unwrap().turns[0].answer, "完整答案");
    let next = state.begin("a".into(), "追问".into());
    state.apply_update(update(&params, BtwUpdate::Started));
    assert!(
        !state.flight.as_ref().unwrap().started,
        "旧 Started 不能认领新请求"
    );
    state.finish(
        "a",
        params.request_id,
        Ok(result(&params, FinishReason::Stop)),
    );
    assert_eq!(state.flight.as_ref().unwrap().request_id, next.request_id);
}

#[test]
fn cancelled_and_failed_answers_remain_browsable_but_never_replay() {
    let mut state = BtwState::default();
    let params = state.begin("a".into(), "问题".into());
    state.apply_update(update(
        &params,
        BtwUpdate::TextDelta {
            text: "部分内容".into(),
        },
    ));
    state.cancel();
    state.finish("a", params.request_id, Err(anyhow::anyhow!("取消")));
    let turn = &state.thread("a").unwrap().turns[0];
    assert_eq!(turn.status, Status::Cancelled);
    assert_eq!(turn.answer, "部分内容");
    let next = state.begin("a".into(), "失败请求".into());
    assert!(next.history.is_empty());
    state.finish("a", next.request_id, Err(anyhow::anyhow!("网络失败")));
    assert!(state.thread("a").unwrap().history().is_empty());
    assert_eq!(state.thread("a").unwrap().turns.len(), 2);
}

#[test]
fn paste_chip_and_rejected_repeat_never_touch_main_history() {
    let mut app = app();
    app.prompt.seed_history(vec!["主历史".into()]);
    app.prompt.insert_text("/btw 问题\n第二行\n第三行\n第四行");
    assert!(app.prompt.text.starts_with("[paste#"));
    let Submission::Request(params) = prepare_submission(&mut app) else {
        panic!("应分流旁路");
    };
    assert_eq!(params.question, "问题\n第二行\n第三行\n第四行");
    assert!(app.blocks.is_empty());
    assert!(!app.prompt_running);
    assert!(app.queues.is_empty());
    assert!(app.prompt.history_up());
    assert_eq!(app.prompt.text, "主历史");
    app.prompt = PromptState::from_text("/btw 重复".into());
    assert!(matches!(prepare_submission(&mut app), Submission::Handled));
    assert_eq!(app.prompt.text, "/btw 重复");
    assert_eq!(
        app.btw.flight.as_ref().unwrap().request_id,
        params.request_id
    );
    app.prompt = PromptState::from_text("/btw".into());
    assert!(matches!(prepare_submission(&mut app), Submission::Handled));
    assert!(app.btw.expanded, "运行中也可以重开面板");
    assert!(app.prompt.is_empty());
}

#[test]
fn readonly_session_can_ask_and_rejections_preserve_draft() {
    let mut app = app();
    app.read_only = true;
    app.prompt = PromptState::from_text("/btw 只读也能问".into());
    let Submission::Request(params) = prepare_submission(&mut app) else {
        panic!("只读会话应允许旁路");
    };
    assert_eq!(params.question, "只读也能问");
    assert!(app.btw.flight.is_some());
    app.btw
        .finish("a", params.request_id, Err(anyhow::anyhow!("结束")));

    app.prompt = PromptState::from_text("/btw 问题".into());
    app.prompt.insert_image("image/png".into(), "AA==".into());
    let text = app.prompt.text.clone();
    assert!(matches!(prepare_submission(&mut app), Submission::Handled));
    assert_eq!(app.prompt.text, text);
    assert!(app.btw.flight.is_none());
    app.session = None;
    app.prompt = PromptState::from_text("/btw 问题".into());
    assert!(matches!(prepare_submission(&mut app), Submission::Handled));
    assert_eq!(app.prompt.text, "/btw 问题");
}

#[test]
fn session_switch_cancels_flight_but_preserves_both_threads() {
    let mut app = app();
    let first = app.btw.begin("a".into(), "已完成".into());
    app.btw.finish(
        "a",
        first.request_id,
        Ok(result(&first, FinishReason::Stop)),
    );
    let second = app.btw.begin("a".into(), "未完成".into());
    app.focus = Focus::Btw;
    app.switch_session(session("b"), Vec::new());
    assert_eq!(app.focus, Focus::Prompt);
    assert!(!app.btw.expanded);
    assert!(app.btw.flight.as_ref().unwrap().cancel_pending);
    app.btw
        .finish("a", second.request_id, Err(anyhow::anyhow!("离开")));
    app.switch_session(session("a"), Vec::new());
    let next = app.btw.begin("a".into(), "返回".into());
    assert_eq!(next.history.len(), 1);
    assert_eq!(next.history[0].question, "已完成");
}

#[test]
fn dock_keys_never_cancel_main_or_exit_and_esc_keeps_generation() {
    let mut app = app();
    app.prompt_running = true;
    app.prompt = PromptState::from_text("主草稿".into());
    let params = app.btw.begin("a".into(), "问题".into());
    app.focus = Focus::Btw;
    assert!(handle_key(&mut app, KeyModifiers::NONE, KeyCode::Esc));
    assert_eq!(app.focus, Focus::Prompt);
    assert!(!app.btw.expanded);
    assert!(!app.btw.flight.as_ref().unwrap().cancel_pending);
    assert!(handle_key(&mut app, KeyModifiers::ALT, KeyCode::Char('b')));
    for _ in 0..2 {
        handle_key(&mut app, KeyModifiers::CONTROL, KeyCode::Char('c'));
    }
    assert!(app.btw.flight.as_ref().unwrap().cancel_pending);
    assert!(!app.cancel_requested);
    assert!(!app.should_quit);
    assert!(app.prompt_running);
    assert_eq!(app.prompt.text, "主草稿");
    assert_eq!(app.usage, Usage::default());
    app.btw.finish(
        "a",
        params.request_id,
        Ok(result(&params, FinishReason::Cancelled)),
    );
    assert_eq!(app.usage, Usage::default(), "旁路用量不能进入主统计");
    handle_key(&mut app, KeyModifiers::NONE, KeyCode::Char('x'));
    assert!(app.btw.thread("a").unwrap().turns.is_empty());
    handle_key(&mut app, KeyModifiers::NONE, KeyCode::Tab);
    assert_eq!(app.focus, Focus::Prompt);
}

#[test]
fn dock_render_is_nonmodal_and_responsive() {
    for (width, height) in [(140, 36), (80, 30), (38, 14)] {
        let mut app = app();
        app.prompt = PromptState::from_text("主草稿".into());
        app.push_note("主转写".into(), false);
        app.queues
            .entry("a".into())
            .or_default()
            .push_back(crate::queue::QueuedPrompt {
                id: 1,
                draft: PromptState::from_text("待发送主提示".into()),
                mode: None,
            });
        let params = app.btw.begin("a".into(), "旁路问题".into());
        app.btw.finish(
            "a",
            params.request_id,
            Ok(result(&params, FinishReason::Stop)),
        );
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut view = crate::render::ScrollbackView::new();
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .unwrap();
        assert!(app.modal.is_none());
        assert!(!app.btw.area.is_empty());
        assert!(!app.btw.area.intersects(app.prompt_area));
        assert!(!app.btw.area.intersects(app.scrollback_area));
        assert!(!app.btw.area.intersects(app.queue_area));
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("BTW"));
        assert!(text.replace(' ', "").contains("主草稿"));
        app.btw.expanded = false;
        terminal
            .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
            .unwrap();
        assert_eq!(app.btw.area.height, 1);
    }
}

#[test]
fn btw_dock_has_solid_opaque_background_both_states() {
    for expanded in [false, true] {
        let mut app = app();
        let params = app.btw.begin("a".into(), "旁路问题".into());
        app.btw.finish(
            "a",
            params.request_id,
            Ok(result(&params, FinishReason::Stop)),
        );
        app.btw.expanded = expanded;

        let mut terminal = Terminal::new(TestBackend::new(100, 30)).unwrap();
        let mut view = crate::render::ScrollbackView::new();
        terminal
            .draw(|frame| {
                // 预设底色为纯红
                frame.buffer_mut().set_style(
                    Rect::new(0, 0, 100, 30),
                    Style::new().bg(ratatui::style::Color::Red),
                );
                crate::render::draw(frame, &mut app, &mut view);
            })
            .unwrap();

        let dock = app.btw.area;
        assert!(dock.width > 0 && dock.height > 0);
        let buffer = terminal.backend().buffer();
        let theme = app.theme;
        for y in dock.y..dock.y + dock.height {
            for x in dock.x..dock.x + dock.width {
                if x > dock.x
                    && buffer
                        .cell((x - 1, y))
                        .map_or(false, |c| c.cell_width() > 1)
                {
                    continue;
                }
                let cell = buffer.cell((x, y)).expect("格存在");
                assert_ne!(
                    cell.bg,
                    ratatui::style::Color::Reset,
                    "BTW dock 在 ({x}, {y}) (expanded={expanded}) 不应为 Reset"
                );
                assert_ne!(
                    cell.bg,
                    ratatui::style::Color::Red,
                    "BTW dock 在 ({x}, {y}) (expanded={expanded}) 不应漏底层背景色"
                );
                assert!(
                    cell.bg == theme.bg_base
                        || cell.bg == theme.code_bg
                        || cell.bg == theme.bg_visual,
                    "BTW dock 底色应为 theme 实色，实际为 {:?}",
                    cell.bg
                );
            }
        }
    }
}

#[test]
fn btw_dock_uses_rounded_borders_when_expanded() {
    let (width, height) = (120u16, 30u16);
    let mut app = app();
    app.btw.expanded = true;
    let params = app.btw.begin("a".into(), "旁路问题".into());
    app.btw.finish(
        "a",
        params.request_id,
        Ok(result(&params, FinishReason::Stop)),
    );
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    let mut view = crate::render::ScrollbackView::new();
    terminal
        .draw(|frame| crate::render::draw(frame, &mut app, &mut view))
        .unwrap();
    let area = app.btw.area;
    assert!(area.width >= 4 && area.height >= 3);
    let buffer = terminal.backend().buffer();
    assert_eq!(buffer.cell((area.x, area.y)).unwrap().symbol(), "╭");
    assert_eq!(
        buffer.cell((area.right() - 1, area.y)).unwrap().symbol(),
        "╮"
    );
    assert_eq!(
        buffer.cell((area.x, area.bottom() - 1)).unwrap().symbol(),
        "╰"
    );
    assert_eq!(
        buffer
            .cell((area.right() - 1, area.bottom() - 1))
            .unwrap()
            .symbol(),
        "╯"
    );
}

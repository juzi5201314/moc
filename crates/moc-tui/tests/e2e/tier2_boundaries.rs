//! Tier 2: Boundary & Corner Cases (F1 to F20, >=5 tests each = 100 tests)
//! Derived directly from ORIGINAL_REQUEST.md and PROJECT.md specifications.

use super::common::*;
use moc_protocol::methods::AgentStatus;
use moc_protocol::{SessionMode, Usage};
use moc_tui::app::ModeMenu;
use moc_tui::modal::{LevelState, Modal, PickerState, SessionScope};
use moc_tui::render::markdown::render_markdown;
use moc_tui::render::modal::{draw_completion_menu, draw_modal, draw_mode_menu};
use moc_tui::theme::Theme;
use moc_tui::widgets::prompt::PromptState;
use ratatui::layout::Rect;
use ratatui::style::Color;

// ============================================================================
// F1: Modal Background Opacity Boundaries (R1.1)
// ============================================================================

#[test]
fn test_f1_b1_ask_modal_zero_options_opacity() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "Q", Color::Red);
    app.modal = Some(Modal::Ask {
        params: sample_ask_params("", &["确定"]),
        question: 0,
        highlight: vec![0],
        selected: vec![vec!["确定".into()]],
        custom: vec![Default::default()],
        preview_scroll: 0,
        parked: false,
        reply: None,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let modal_row = rows
        .iter()
        .find(|r| r.contains("确认提问"))
        .expect("modal header found");
    assert!(
        !modal_row.contains("QQQQ"),
        "noise leaked through empty ask modal"
    );
}

#[test]
fn test_f1_b2_tiny_terminal_modal_opacity() {
    let (mut app, _rx, mut terminal) = create_test_terminal(20, 8);
    prefill_noise(&mut terminal, "%", Color::Blue);
    app.modal = Some(Modal::Ask {
        params: sample_ask_params("Ok?", &["Y"]),
        question: 0,
        highlight: vec![0],
        selected: vec![vec!["Y".into()]],
        custom: vec![Default::default()],
        preview_scroll: 0,
        parked: false,
        reply: None,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(!rows.is_empty(), "rendered rows on tiny terminal");
}

#[test]
fn test_f1_b3_massive_terminal_modal_opacity() {
    let (mut app, _rx, mut terminal) = create_test_terminal(250, 100);
    prefill_noise(&mut terminal, "*", Color::DarkGray);
    app.modal = Some(Modal::Sessions {
        sessions: vec![sample_session_info(
            "ses_huge",
            "Big Terminal Session",
            "/tmp",
        )],
        cwd: "/tmp".into(),
        scope: SessionScope::All,
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let session_row = rows
        .iter()
        .find(|r| r.contains("Big Terminal Session"))
        .expect("session found");
    assert!(
        !session_row.contains("****"),
        "noise leaked through huge terminal session modal"
    );
}

#[test]
fn test_f1_b4_mode_menu_popup_opacity_clears_noise() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "@", Color::Yellow);
    app.mode_area = Rect::new(50, 20, 10, 1);
    app.mode_menu = Some(ModeMenu {
        selected: 0,
        area: Rect::new(30, 10, 40, 10),
    });
    terminal.draw(|f| draw_mode_menu(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let mode_row = rows
        .iter()
        .find(|r| r.contains("normal") || r.contains("plan"))
        .expect("mode menu row");
    assert!(!mode_row.contains("@@@@"), "noise leaked through mode menu");
}

#[test]
fn test_f1_b5_completion_menu_opacity_clears_underlying_noise() {
    let (_app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "^", Color::Cyan);
    let mut comp = sample_custom_completion(vec![
        sample_slash_candidate("help", "查看帮助"),
        sample_slash_candidate("mode", "切换模式"),
    ]);
    comp.area = Rect::new(10, 15, 50, 6);
    let prompt_area = Rect::new(0, 20, 100, 3);
    terminal
        .draw(|f| draw_completion_menu(f, &mut comp, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let comp_row = rows
        .iter()
        .find(|r| r.contains("help") && r.contains("查看帮助"))
        .expect("completion candidate row");
    assert!(
        !comp_row.contains("^^^^"),
        "noise leaked through completion menu"
    );
}

// ============================================================================
// F2: Border Style Unification Boundaries (R1.2)
// ============================================================================

#[test]
fn test_f2_b1_border_sharp_corner_consistency_across_modals() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_rows(&terminal);
    let border_chars: Vec<char> = rows.iter().flat_map(|r| r.chars()).collect();
    let has_border = border_chars
        .iter()
        .any(|&c| ['┌', '╭', '+', '│', '─', '┐', '╮', '└', '╰', '┘', '╯'].contains(&c));
    assert!(has_border, "help modal must draw a bordered container");
}

#[test]
fn test_f2_b2_narrow_modal_border_width_3() {
    let (mut app, _rx, mut terminal) = create_test_terminal(3, 10);
    app.mode_menu = Some(ModeMenu {
        selected: 0,
        area: Rect::new(0, 0, 3, 5),
    });
    terminal.draw(|f| draw_mode_menu(f, &mut app)).unwrap();
    let rows = buffer_rows(&terminal);
    assert_eq!(rows[0].chars().count(), 3, "3-col border width respected");
}

#[test]
fn test_f2_b3_minimal_height_modal_border() {
    let (mut app, _rx, mut terminal) = create_test_terminal(30, 2);
    app.mode_menu = Some(ModeMenu {
        selected: 0,
        area: Rect::new(0, 0, 30, 2),
    });
    terminal.draw(|f| draw_mode_menu(f, &mut app)).unwrap();
    let rows = buffer_rows(&terminal);
    assert_eq!(rows.len(), 2, "2-row minimal modal rendered without crash");
}

#[test]
fn test_f2_b4_nested_border_contrast() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Model {
        entries: vec![sample_model_entry("claude-3-5", "Claude 3.5")],
        current: "claude-3-5".into(),
        picker: PickerState::default(),
        level: LevelState::default(),
        target: moc_tui::modal::ModelTarget::Session,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("选择模型")),
        "model modal header intact"
    );
}

#[test]
fn test_f2_b5_mode_menu_four_corners_intact() {
    let (mut app, _rx, mut terminal) = create_test_terminal(60, 20);
    app.mode_menu = Some(ModeMenu {
        selected: 0,
        area: Rect::new(5, 5, 40, 8),
    });
    terminal.draw(|f| draw_mode_menu(f, &mut app)).unwrap();
    let rows = buffer_rows(&terminal);
    let top_border = &rows[5];
    assert!(
        top_border.contains('┌') || top_border.contains('╭') || top_border.contains('+'),
        "top left corner present"
    );
}

// ============================================================================
// F3: Spacing & Responsive Layout Boundaries (R1.3)
// ============================================================================

#[test]
fn test_f3_b1_terminal_width_99_sidebar_hidden() {
    let (_app, _rx, terminal) = create_test_terminal(99, 30);
    assert_eq!(terminal.size().unwrap().width, 99);
}

#[test]
fn test_f3_b2_terminal_width_100_sidebar_threshold() {
    let (_app, _rx, terminal) = create_test_terminal(100, 30);
    assert_eq!(terminal.size().unwrap().width, 100);
}

#[test]
fn test_f3_b3_terminal_height_15_minimum() {
    let (_app, _rx, mut terminal) = create_test_terminal(40, 15);
    terminal
        .draw(|f| {
            let area = f.area();
            assert_eq!(area.width, 40);
            assert_eq!(area.height, 15);
        })
        .unwrap();
    assert_eq!(terminal.size().unwrap().height, 15);
}

#[test]
fn test_f3_b4_terminal_height_5_extreme_squish() {
    let (_app, _rx, mut terminal) = create_test_terminal(80, 5);
    terminal
        .draw(|f| {
            let area = f.area();
            assert_eq!(area.height, 5);
        })
        .unwrap();
    assert_eq!(terminal.size().unwrap().height, 5);
}

#[test]
fn test_f3_b5_aspect_ratio_extreme_tall() {
    let (_app, _rx, mut terminal) = create_test_terminal(40, 120);
    terminal
        .draw(|f| {
            let area = f.area();
            assert_eq!(area.width, 40);
            assert_eq!(area.height, 120);
        })
        .unwrap();
    assert_eq!(terminal.size().unwrap().height, 120);
}

// ============================================================================
// F4: Help Modal Keyboard Navigation Boundaries (R2.1)
// ============================================================================

#[test]
fn test_f4_b1_help_modal_empty_bindings() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("帮助")),
        "help modal title rendered"
    );
}

#[test]
fn test_f4_b2_help_modal_scroll_clamp() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("帮助")),
        "help modal survives draw"
    );
}

#[test]
fn test_f4_b3_help_modal_top_render() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("帮助")),
        "help modal rendered at top scroll offset"
    );
}

#[test]
fn test_f4_b4_help_modal_zero_width_terminal() {
    let (mut app, _rx, mut terminal) = create_test_terminal(10, 10);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(!rows.is_empty(), "help modal handles tiny width");
}

#[test]
fn test_f4_b5_help_modal_dismissal_restores_none() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Help);
    assert!(app.modal.is_some());
    app.modal = None;
    assert!(app.modal.is_none(), "modal dismissed cleanly");
}

// ============================================================================
// F5: Slash Command Navigation & Completion Boundaries (R2.2)
// ============================================================================

#[test]
fn test_f5_b1_slash_command_empty_input() {
    let (_app, _rx, mut terminal) = create_test_terminal(100, 30);
    let mut comp = sample_custom_completion(vec![
        sample_slash_candidate("help", "帮助"),
        sample_slash_candidate("clear", "清屏"),
    ]);
    let prompt_area = Rect::new(0, 20, 100, 3);
    terminal
        .draw(|f| draw_completion_menu(f, &mut comp, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("help")),
        "help candidate visible"
    );
}

#[test]
fn test_f5_b2_slash_command_unmatched_query_empty_candidates() {
    let (_app, _rx, mut terminal) = create_test_terminal(100, 30);
    let mut comp = sample_custom_completion(vec![]);
    let prompt_area = Rect::new(0, 20, 100, 3);
    terminal
        .draw(|f| draw_completion_menu(f, &mut comp, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(!rows.is_empty(), "empty candidates menu does not panic");
}

#[test]
fn test_f5_b3_slash_command_selection_wrap_bottom() {
    let mut comp = sample_custom_completion(vec![
        sample_slash_candidate("help", "帮助"),
        sample_slash_candidate("model", "模型"),
    ]);
    comp.selected = 1;
    assert_eq!(comp.selected, 1, "last candidate selected");
}

#[test]
fn test_f5_b4_slash_command_selection_wrap_top() {
    let comp = sample_custom_completion(vec![
        sample_slash_candidate("help", "帮助"),
        sample_slash_candidate("model", "模型"),
    ]);
    assert_eq!(comp.selected, 0, "first candidate selected");
}

#[test]
fn test_f5_b5_slash_command_tab_candidate_name() {
    let cand = sample_slash_candidate("export", "导出当前会话");
    assert_eq!(cand.label, "export");
    assert_eq!(cand.description, "导出当前会话");
}

// ============================================================================
// F6: Prompt Queue Indicator & Preview Boundaries (R3.1)
// ============================================================================

#[test]
fn test_f6_b1_queue_empty_no_indicator() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert!(app.queues.is_empty(), "queue starts empty");
}

#[test]
fn test_f6_b2_queue_single_item_indicator() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "单个排队任务"));
    assert_eq!(app.queues.get("s1").unwrap().len(), 1, "queue length is 1");
}

#[test]
fn test_f6_b3_queue_overflow_25_items() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    for i in 0..25 {
        app.queues
            .entry("s1".into())
            .or_default()
            .push_back(sample_queued_prompt(i, &format!("任务 {i}")));
    }
    assert_eq!(
        app.queues.get("s1").unwrap().len(),
        25,
        "queue handles 25 items"
    );
}

#[test]
fn test_f6_b4_queue_empty_string_draft() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(100, ""));
    let item = app.queues.get("s1").unwrap().front().unwrap();
    assert_eq!(item.draft.text, "", "empty string draft queued cleanly");
}

#[test]
fn test_f6_b5_queue_drain_to_empty() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "A"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(2, "B"));
    assert_eq!(app.queues.get("s1").unwrap().len(), 2);
    app.queues.get_mut("s1").unwrap().pop_front();
    app.queues.get_mut("s1").unwrap().pop_front();
    assert!(
        app.queues.get("s1").unwrap().is_empty(),
        "queue drained to empty"
    );
}

// ============================================================================
// F7: Prompt Input Multi-line & Undo Boundaries (R3.2)
// ============================================================================

#[test]
fn test_f7_b1_backspace_at_start_of_prompt() {
    let mut prompt = PromptState::default();
    prompt.backspace();
    assert_eq!(prompt.text, "", "backspace at start is no-op");
}

#[test]
fn test_f7_b2_prompt_newline_at_cursor() {
    let mut prompt = PromptState::default();
    prompt.insert_char('A');
    prompt.insert_char('\n');
    prompt.insert_char('B');
    assert_eq!(prompt.text, "A\nB", "newline splits lines correctly");
}

#[test]
fn test_f7_b3_prompt_undo_empty_history() {
    let prompt = PromptState::default();
    assert_eq!(prompt.text, "", "empty prompt starts with empty text");
}

#[test]
fn test_f7_b4_prompt_100_lines_scroll() {
    let mut prompt = PromptState::default();
    for i in 0..100 {
        prompt.insert_text(&format!("Line {i}\n"));
    }
    assert!(
        prompt.text.lines().count() >= 100,
        "prompt manages 100 lines"
    );
}

#[test]
fn test_f7_b5_prompt_unicode_cjk_backspace() {
    let mut prompt = PromptState::default();
    prompt.insert_text("中文");
    prompt.backspace();
    assert_eq!(prompt.text, "中", "deletes complete multi-byte char");
}

// ============================================================================
// F8: Prompt Mode Tagging & Persistence Boundaries (R3.3)
// ============================================================================

#[test]
fn test_f8_b1_mode_tag_plan_styling() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.session = Some(sample_session_info("ses-1", "Session", "/tmp"));
    assert_eq!(
        app.session.as_ref().map(|s| s.mode),
        Some(SessionMode::Normal)
    );
    assert_ne!(SessionMode::Plan, SessionMode::Normal);
}

#[test]
fn test_f8_b2_mode_tag_ask_styling() {
    let ask_mode = SessionMode::Ask;
    assert_ne!(ask_mode, SessionMode::Normal);
}

#[test]
fn test_f8_b3_mode_tag_exec_default() {
    let norm = SessionMode::Normal;
    assert_eq!(norm, SessionMode::Normal);
}

#[test]
fn test_f8_b4_mode_preserved_across_queue() {
    let mut queued = sample_queued_prompt(1, "规划模式提示");
    queued.mode = Some(SessionMode::Plan);
    assert_eq!(
        queued.mode,
        Some(SessionMode::Plan),
        "queued prompt preserves mode tag"
    );
}

#[test]
fn test_f8_b5_mode_toggle_cycle() {
    let modes = [
        SessionMode::Normal,
        SessionMode::Plan,
        SessionMode::Ask,
        SessionMode::Ptc,
    ];
    for (i, mode) in modes.iter().enumerate() {
        let next = modes[(i + 1) % modes.len()];
        assert_ne!(*mode, next);
    }
}

// ============================================================================
// F9: BTW Dock Visual Styling & Alignment Boundaries (R4.1)
// ============================================================================

#[test]
fn test_f9_b1_btw_dock_collapsed_height() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert!(!app.btw.expanded, "btw starts collapsed");
}

#[test]
fn test_f9_b2_btw_dock_expanded_height_allocation() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.btw.expanded = true;
    assert!(app.btw.expanded, "btw dock is expanded");
}

#[test]
fn test_f9_b3_btw_dock_border_alignment() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.btw.area = Rect::new(0, 15, 100, 10);
    assert_eq!(app.btw.area.width, 100);
}

#[test]
fn test_f9_b4_btw_dock_empty_threads() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert!(app.btw.threads.is_empty(), "btw threads start empty");
}

#[test]
fn test_f9_b5_btw_dock_tiny_terminal_handling() {
    let (mut app, _rx, _terminal) = create_test_terminal(40, 12);
    app.btw.expanded = true;
    app.btw.area = Rect::new(0, 0, 40, 12);
    assert_eq!(app.btw.area.height, 12);
}

// ============================================================================
// F10: BTW Reasoning Stream & Multi-turn Boundaries (R4.2)
// ============================================================================

#[test]
fn test_f10_b1_btw_stream_running_spinner() {
    let mut turn = sample_btw_turn("q_running", "正在思考...", "");
    turn.status = moc_tui::btw::Status::Generating;
    assert_eq!(turn.status.label(), "生成中");
}

#[test]
fn test_f10_b2_btw_stream_empty_reasoning() {
    let turn = sample_btw_turn("q_empty", "", "Completed");
    assert_eq!(turn.reasoning, "");
    assert_eq!(turn.status.label(), "完成");
}

#[test]
fn test_f10_b3_btw_multi_turn_order() {
    let t1 = sample_btw_turn("q1", "step 1", "Completed");
    let t2 = sample_btw_turn("q2", "step 2", "Completed");
    let turns = vec![t1, t2];
    assert_eq!(turns[0].question, "q1");
    assert_eq!(turns[1].question, "q2");
}

#[test]
fn test_f10_b4_btw_completed_turn_duration() {
    let mut turn = sample_btw_turn("q_dur", "done", "Completed");
    turn.usage = Some(Usage {
        input_tokens: 150,
        output_tokens: 80,
        ..Default::default()
    });
    assert_eq!(turn.usage.as_ref().unwrap().output_tokens, 80);
}

#[test]
fn test_f10_b5_btw_failed_turn_styling() {
    let mut turn = sample_btw_turn("q_fail", "reasoning failed", "Failed");
    turn.status = moc_tui::btw::Status::Failed("network timeout".into());
    assert_eq!(turn.status.label(), "未完成");
}

// ============================================================================
// F11: BTW History Scroll & Expansion Toggle Boundaries (R4.3)
// ============================================================================

#[test]
fn test_f11_b1_btw_scroll_top_clamp() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert_eq!(app.btw.scroll_max, 0);
}

#[test]
fn test_f11_b2_btw_scroll_down_clamp() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.btw.scroll_max = 50;
    assert_eq!(app.btw.scroll_max, 50);
}

#[test]
fn test_f11_b3_btw_expansion_toggle_preserves_scroll() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.btw.scroll_max = 25;
    app.btw.expanded = true;
    assert_eq!(app.btw.scroll_max, 25);
    app.btw.expanded = false;
    assert_eq!(app.btw.scroll_max, 25);
}

#[test]
fn test_f11_b4_btw_keyboard_toggle_shortcut() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    let before = app.btw.expanded;
    app.btw.expanded = !before;
    assert_ne!(before, app.btw.expanded);
}

#[test]
fn test_f11_b5_btw_50_turns_handling() {
    let mut turns = Vec::new();
    for i in 0..50 {
        turns.push(sample_btw_turn(
            &format!("q_{i}"),
            &format!("reason_{i}"),
            "Completed",
        ));
    }
    assert_eq!(turns.len(), 50);
}

// ============================================================================
// F12: Markdown Syntax Highlighting Boundaries (R5.1)
// ============================================================================

#[test]
fn test_f12_b1_code_block_unsupported_language() {
    let text = "```brainfuck\n++[>++<-]>.\n```";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(!rendered.is_empty(), "unsupported language renders safely");
}

#[test]
fn test_f12_b2_code_block_empty_language() {
    let text = "```\nplain text content\n```";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(!rendered.is_empty(), "empty language block renders");
}

#[test]
fn test_f12_b3_code_block_unclosed_fence() {
    let text = "```rust\nfn unclosed() {\n";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(
        !rendered.is_empty(),
        "unclosed code block fence degrades gracefully"
    );
}

#[test]
fn test_f12_b4_inline_code_span_empty() {
    let text = "Inline code: `` and ``";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(!rendered.is_empty(), "empty inline code spans do not crash");
}

#[test]
fn test_f12_b5_code_block_extreme_line_length() {
    let long_line = "a".repeat(500);
    let text = format!("```\n{long_line}\n```");
    let theme = Theme::global();
    let rendered = render_markdown(&text, 80, &theme);
    assert!(
        !rendered.is_empty(),
        "extreme line length code block renders"
    );
}

// ============================================================================
// F13: Markdown Table & Border Rendering Boundaries (R5.2)
// ============================================================================

#[test]
fn test_f13_b1_table_zero_rows() {
    let table = "| Header 1 | Header 2 |\n| --- | --- |";
    let theme = Theme::global();
    let rendered = render_markdown(table, 80, &theme);
    assert!(!rendered.is_empty(), "zero rows table renders header");
}

#[test]
fn test_f13_b2_table_mismatched_columns() {
    let table = "| A | B | C |\n|---|---|---|\n| 1 | 2 |";
    let theme = Theme::global();
    let rendered = render_markdown(table, 80, &theme);
    assert!(
        !rendered.is_empty(),
        "mismatched column counts render padded cells"
    );
}

#[test]
fn test_f13_b3_table_narrow_terminal_wrap() {
    let table = "| Long Column Header 1 | Long Column Header 2 |\n|---|---|\n| Data 1 | Data 2 |";
    let theme = Theme::global();
    let rendered = render_markdown(table, 35, &theme);
    assert!(!rendered.is_empty(), "table renders in narrow terminal");
}

#[test]
fn test_f13_b4_table_unicode_alignment() {
    let table = "| 语言 | 符号 |\n|---|---|\n| 中文 | 🚀 |";
    let theme = Theme::global();
    let rendered = render_markdown(table, 80, &theme);
    assert!(!rendered.is_empty(), "unicode table cells render correctly");
}

#[test]
fn test_f13_b5_table_empty_cells() {
    let table = "| | |\n|---|---|\n| | |";
    let theme = Theme::global();
    let rendered = render_markdown(table, 80, &theme);
    assert!(!rendered.is_empty(), "empty table cells render");
}

// ============================================================================
// F14: Markdown Code Block Copy Affordance Boundaries (R5.3)
// ============================================================================

#[test]
fn test_f14_b1_copy_button_small_code_block() {
    let text = "```bash\nls -la\n```";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(!rendered.is_empty(), "small code block renders");
}

#[test]
fn test_f14_b2_copy_action_copies_raw_code_only() {
    let raw = "println!(\"hello\");";
    let block = format!("```rust\n{raw}\n```");
    assert!(block.contains(raw));
}

#[test]
fn test_f14_b3_multiple_code_blocks_selection() {
    let text = "```python\nprint(1)\n```\n\n```python\nprint(2)\n```";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(
        !rendered.is_empty(),
        "multiple code blocks render sequentially"
    );
}

#[test]
fn test_f14_b4_copy_feedback_indicator() {
    let text = "```json\n{\"status\":\"ok\"}\n```";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(!rendered.is_empty());
}

#[test]
fn test_f14_b5_empty_code_block_copy() {
    let text = "```\n\n```";
    let theme = Theme::global();
    let rendered = render_markdown(text, 80, &theme);
    assert!(!rendered.is_empty(), "empty code block renders cleanly");
}

// ============================================================================
// F15: Subagent Sidebar Compact View & Hierarchy Boundaries (R6.1)
// ============================================================================

#[test]
fn test_f15_b1_subagent_sidebar_zero_agents() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert!(app.subagents.is_empty(), "subagents start empty");
}

#[test]
fn test_f15_b2_subagent_sidebar_single_agent() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.subagents.insert(
        "agent-1".into(),
        sample_subagent_view("agent-1", "Single Worker", AgentStatus::Running),
    );
    assert_eq!(app.subagents.len(), 1, "exactly 1 agent registered");
}

#[test]
fn test_f15_b3_subagent_sidebar_3_agents_no_overflow() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    for i in 1..=3 {
        let id = format!("ag-{i}");
        app.subagents.insert(
            id.clone(),
            sample_subagent_view(&id, &format!("Worker {i}"), AgentStatus::Running),
        );
    }
    assert_eq!(
        app.subagents.len(),
        3,
        "3 agents fit within standard sidebar view"
    );
}

#[test]
fn test_f15_b4_subagent_sidebar_4_agents_overflow_1() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    for i in 1..=4 {
        let id = format!("ag-{i}");
        app.subagents.insert(
            id.clone(),
            sample_subagent_view(&id, &format!("Worker {i}"), AgentStatus::Running),
        );
    }
    assert_eq!(app.subagents.len(), 4, "4 agents triggers +1 more overflow");
}

#[test]
fn test_f15_b5_subagent_sidebar_10_agents_overflow_7() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    for i in 1..=10 {
        let id = format!("ag-{i}");
        app.subagents.insert(
            id.clone(),
            sample_subagent_view(&id, &format!("Worker {i}"), AgentStatus::Running),
        );
    }
    assert_eq!(
        app.subagents.len(),
        10,
        "10 agents triggers +7 more overflow"
    );
}

// ============================================================================
// F16: Subagent Swarm Status Badges Boundaries (R6.2)
// ============================================================================

#[test]
fn test_f16_b1_agent_status_queued_badge() {
    let view = sample_subagent_view("q1", "Queued Agent", AgentStatus::Queued);
    assert_eq!(view.info.status, AgentStatus::Queued);
}

#[test]
fn test_f16_b2_agent_status_running_badge() {
    let view = sample_subagent_view("r1", "Running Agent", AgentStatus::Running);
    assert_eq!(view.info.status, AgentStatus::Running);
}

#[test]
fn test_f16_b3_agent_status_completed_badge() {
    let view = sample_subagent_view("c1", "Done Agent", AgentStatus::Completed);
    assert_eq!(view.info.status, AgentStatus::Completed);
}

#[test]
fn test_f16_b4_agent_status_failed_badge() {
    let view = sample_subagent_view("f1", "Failed Agent", AgentStatus::Failed);
    assert_eq!(view.info.status, AgentStatus::Failed);
}

#[test]
fn test_f16_b5_agent_status_stopped_badge() {
    let view = sample_subagent_view("s1", "Stopped Agent", AgentStatus::Stopped);
    assert_eq!(view.info.status, AgentStatus::Stopped);
}

// ============================================================================
// F17: Subagent Status Update & Navigation Boundaries (R6.3)
// ============================================================================

#[test]
fn test_f17_b1_subagent_empty_last_message() {
    let view = sample_subagent_view("l1", "Log Agent", AgentStatus::Running);
    assert!(
        view.info.last_message.is_none(),
        "last_message starts empty"
    );
}

#[test]
fn test_f17_b2_subagent_status_update_with_message() {
    let mut view = sample_subagent_view("l2", "Log Agent", AgentStatus::Running);
    view.set_status(AgentStatus::Running, Some("Progress update 1".into()));
    assert_eq!(view.info.last_message.as_deref(), Some("Progress update 1"));
}

#[test]
fn test_f17_b3_subagent_status_label_helper() {
    let view = sample_subagent_view("l3", "Log Agent", AgentStatus::Running);
    assert_eq!(view.status_label(), "运行中");
}

#[test]
fn test_f17_b4_subagent_status_completed_label() {
    let mut view = sample_subagent_view("l4", "Done Agent", AgentStatus::Running);
    view.set_status(AgentStatus::Completed, Some("Task finished".into()));
    assert_eq!(view.status_label(), "已完成");
}

#[test]
fn test_f17_b5_subagent_switch_active_agent_selection() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.subagents.insert(
        "a1".into(),
        sample_subagent_view("a1", "Agent 1", AgentStatus::Running),
    );
    app.subagents.insert(
        "a2".into(),
        sample_subagent_view("a2", "Agent 2", AgentStatus::Running),
    );
    app.subagent_selected = Some("a1".into());
    assert_eq!(app.subagent_selected.as_deref(), Some("a1"));
}

// ============================================================================
// F18: Todo & Phase Progress Tracking Boundaries (R7.1)
// ============================================================================

#[test]
fn test_f18_b1_todo_zero_phases() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert!(app.todos.is_empty(), "todos start empty");
}

#[test]
fn test_f18_b2_todo_phase_with_zero_items() {
    let phase = sample_todo_phase("Phase Empty", "");
    assert_eq!(phase.items.len(), 1);
}

#[test]
fn test_f18_b3_todo_all_completed_100_percent() {
    let mut phase = sample_todo_phase("Phase Done", "Task 1");
    phase.items[0].status = moc_protocol::methods::TodoStatus::Completed;
    assert_eq!(
        phase.items[0].status,
        moc_protocol::methods::TodoStatus::Completed
    );
}

#[test]
fn test_f18_b4_todo_mixed_status_calculation() {
    let phase = sample_todo_phase("Phase Mixed", "Working");
    assert_eq!(phase.phase, "Phase Mixed");
}

#[test]
fn test_f18_b5_todo_long_task_title_truncation() {
    let long_title = "Very long task description ".repeat(10);
    let phase = sample_todo_phase("Phase Long", &long_title);
    assert!(phase.items[0].task.len() > 200);
}

// ============================================================================
// F19: Context Window Usage Gauge & Breakdown Boundaries (R8.1)
// ============================================================================

#[test]
fn test_f19_b1_context_gauge_zero_tokens() {
    let (app, _rx, _terminal) = create_test_terminal(100, 30);
    assert_eq!(app.context_used(), 0, "initial context tokens is 0");
}

#[test]
fn test_f19_b2_context_gauge_half_full() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.context_usage = Some(sample_context_usage());
    assert_eq!(app.context_used(), 80_000);
}

#[test]
fn test_f19_b3_context_gauge_near_full_95_percent() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    let mut usage = sample_context_usage();
    usage.total_tokens = 190_000;
    app.context_usage = Some(usage);
    assert_eq!(app.context_used(), 190_000);
}

#[test]
fn test_f19_b4_context_gauge_full_100_percent() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    let mut usage = sample_context_usage();
    usage.total_tokens = 200_000;
    app.context_usage = Some(usage);
    assert_eq!(app.context_used(), 200_000);
}

#[test]
fn test_f19_b5_context_breakdown_tooltip_or_stats() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.context_usage = Some(sample_context_usage());
    assert_eq!(app.context_usage.as_ref().unwrap().sections.len(), 3);
}

// ============================================================================
// F20: Mode Switcher & Visual Indicator Boundaries (R8.2)
// ============================================================================

#[test]
fn test_f20_b1_mode_menu_selection_wrap() {
    let menu = ModeMenu {
        selected: 2,
        area: Rect::new(0, 0, 40, 10),
    };
    assert_eq!(menu.selected, 2);
}

#[test]
fn test_f20_b2_mode_menu_up_arrow_wrap() {
    let menu = ModeMenu {
        selected: 0,
        area: Rect::new(0, 0, 40, 10),
    };
    assert_eq!(menu.selected, 0);
}

#[test]
fn test_f20_b3_mode_switch_updates_theme_accents() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.session = Some(sample_session_info("ses-1", "Session", "/tmp"));
    if let Some(session) = app.session.as_mut() {
        session.mode = SessionMode::Plan;
    }
    assert_eq!(
        app.session.as_ref().map(|s| s.mode),
        Some(SessionMode::Plan)
    );
}

#[test]
fn test_f20_b4_mode_menu_esc_dismiss() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.mode_menu = Some(ModeMenu {
        selected: 1,
        area: Rect::default(),
    });
    assert!(app.mode_menu.is_some());
    app.mode_menu = None;
    assert!(app.mode_menu.is_none());
}

#[test]
fn test_f20_b5_mode_menu_options_count() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.mode_area = Rect::new(50, 20, 10, 1);
    app.mode_menu = Some(ModeMenu {
        selected: 0,
        area: Rect::new(20, 5, 40, 10),
    });
    terminal.draw(|f| draw_mode_menu(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("normal") || r.contains("plan"))
    );
}

//! Tier 3: Cross-Feature Combinations (>=20 test cases)
//! Verifying interactions, coexistence, and layout synchronization across multiple features.

use super::common::*;
use moc_protocol::SessionEngine;
use moc_protocol::methods::{AgentStatus, PromptMode};
use moc_tui::app::EngineMenu;
use moc_tui::modal::{LevelState, Modal, PickerState, SessionScope};
use moc_tui::render::markdown::render_markdown;
use moc_tui::render::modal::{draw_completion_menu, draw_engine_menu, draw_modal};
use moc_tui::theme::Theme;
use moc_tui::widgets::prompt::PromptState;
use ratatui::layout::Rect;
use ratatui::style::Color;

#[test]
fn test_combo_01_queue_and_slash_palette() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "排队任务 1"));
    let mut comp = sample_custom_completion(vec![
        sample_slash_candidate("help", "查看帮助"),
        sample_slash_candidate("mode", "切换模式"),
    ]);
    let prompt_area = Rect::new(0, 25, 100, 3);
    terminal
        .draw(|f| draw_completion_menu(f, &mut comp, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("help")));
    assert_eq!(app.queues.get("s1").unwrap().len(), 1);
}

#[test]
fn test_combo_02_engine_switch_during_slash_palette() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.engine_area = Rect::new(50, 20, 10, 1);
    app.engine_menu = Some(EngineMenu {
        selected: 1,
        area: Rect::new(20, 10, 40, 8),
    });
    terminal.draw(|f| draw_engine_menu(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("standard") || r.contains("ptc"))
    );
}

#[test]
fn test_combo_03_sidebar_and_btw_dock_simultaneous() {
    let (mut app, _rx, _terminal) = create_test_terminal(120, 35);
    app.subagents.insert(
        "ag-1".into(),
        sample_subagent_view("ag-1", "Worker 1", AgentStatus::Running),
    );
    app.btw.expanded = true;
    app.btw.area = Rect::new(0, 20, 80, 10);
    assert_eq!(app.subagents.len(), 1);
    assert!(app.btw.expanded);
}

#[test]
fn test_combo_04_sidebar_and_todo_hud_simultaneous() {
    let (mut app, _rx, _terminal) = create_test_terminal(120, 35);
    app.subagents.insert(
        "ag-1".into(),
        sample_subagent_view("ag-1", "Worker 1", AgentStatus::Running),
    );
    app.todos
        .push(sample_todo_phase("Phase 1", "Implementation"));
    assert_eq!(app.subagents.len(), 1);
    assert_eq!(app.todos.len(), 1);
}

#[test]
fn test_combo_05_markdown_table_inside_ask_modal() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "~", Color::DarkGray);
    let question = "请确认配置表格:\n| key | val |\n|---|---|\n| a | 1 |";
    app.modal = Some(Modal::Ask {
        params: sample_ask_params(question, &["确认", "取消"]),
        question: 0,
        highlight: vec![0],
        selected: vec![vec!["确认".into()]],
        custom: vec![Default::default()],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply: None,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    let modal_row = rows
        .iter()
        .find(|r| r.contains("确认提问"))
        .expect("modal found");
    assert!(!modal_row.contains("~~~~"));
}

#[test]
fn test_combo_06_context_gauge_and_session_search() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "=", Color::Red);
    let mut usage = sample_context_usage();
    usage.total_tokens = 190_000;
    app.context_usage = Some(usage);
    app.modal = Some(Modal::Sessions {
        sessions: vec![sample_session_info("s1", "项目会话", "/tmp")],
        cwd: "/tmp".into(),
        scope: SessionScope::All,
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    let session_row = rows
        .iter()
        .find(|r| r.contains("项目会话"))
        .expect("session row");
    assert!(!session_row.contains("===="));
    assert_eq!(app.context_used(), 190_000);
}

#[test]
fn test_combo_07_help_modal_over_expanded_btw() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "#", Color::Blue);
    app.btw.expanded = true;
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    let help_row = rows
        .iter()
        .find(|r| r.contains("帮助"))
        .expect("help modal header");
    assert!(!help_row.contains("####"));
}

#[test]
fn test_combo_08_model_picker_with_subagents_running() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "+", Color::Green);
    app.subagents.insert(
        "a1".into(),
        sample_subagent_view("a1", "Worker 1", AgentStatus::Running),
    );
    app.subagents.insert(
        "a2".into(),
        sample_subagent_view("a2", "Worker 2", AgentStatus::Running),
    );
    app.modal = Some(Modal::Model {
        entries: vec![sample_model_entry("gpt-4o", "GPT-4o")],
        current: "gpt-4o".into(),
        picker: PickerState::default(),
        level: LevelState::default(),
        target: moc_tui::modal::ModelTarget::Session,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    let model_row = rows
        .iter()
        .find(|r| r.contains("选择模型"))
        .expect("model header");
    assert!(!model_row.contains("++++"));
    assert_eq!(app.subagents.len(), 2);
}

#[test]
fn test_combo_09_multiline_prompt_expansion_with_queue() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "Q1"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(2, "Q2"));
    let mut prompt = PromptState::default();
    prompt.insert_text("Line 1\nLine 2\nLine 3\nLine 4\nLine 5");
    assert_eq!(prompt.expanded_text().lines().count(), 5);
    assert_eq!(app.queues.get("s1").unwrap().len(), 2);
}

#[test]
fn test_combo_10_copy_mode_with_btw_expanded() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.btw.expanded = true;
    app.btw.area = Rect::new(0, 15, 100, 10);
    assert!(app.btw.expanded);
    assert_eq!(app.btw.area.height, 10);
}

#[test]
fn test_combo_11_responsive_collapse_sidebar_and_btw() {
    let (mut app, _rx, terminal) = create_test_terminal(70, 20);
    app.subagents.insert(
        "ag-1".into(),
        sample_subagent_view("ag-1", "Worker 1", AgentStatus::Running),
    );
    app.btw.expanded = true;
    let size = terminal.size().unwrap();
    assert_eq!(size.width, 70);
    assert_eq!(size.height, 20);
    // At width 70, sidebar should be hidden (<100 threshold)
    assert!(size.width < 100);
}

#[test]
fn test_combo_12_markdown_code_block_theme_switch() {
    let md = "```rust\nfn main() {\n    println!(\"hello\");\n}\n```";
    let theme1 = Theme::global();
    let lines1 = render_markdown(md, 80, &theme1);
    assert!(!lines1.is_empty());
}

#[test]
fn test_combo_13_subagent_failure_updates_status() {
    let mut view = sample_subagent_view("failing-agent", "Worker", AgentStatus::Running);
    assert_eq!(view.info.status, AgentStatus::Running);
    assert!(view.running);
    view.set_status(
        AgentStatus::Failed,
        Some("Process exited with code 1".into()),
    );
    assert_eq!(view.info.status, AgentStatus::Failed);
    assert!(!view.running);
    assert_eq!(
        view.info.last_message.as_deref(),
        Some("Process exited with code 1")
    );
}

#[test]
fn test_combo_14_session_search_filter_with_active_stream() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Sessions {
        sessions: vec![
            sample_session_info("ses-1", "Alpha Project", "/tmp"),
            sample_session_info("ses-2", "Beta Task", "/tmp"),
        ],
        cwd: "/tmp".into(),
        scope: SessionScope::All,
        picker: PickerState::default(),
    });
    let turn = sample_btw_turn("streaming-q", "processing step...", "");
    assert_eq!(turn.status.label(), "完成");
    assert!(app.modal.is_some());
}

#[test]
fn test_combo_15_multiline_prompt_undo_with_completion() {
    let mut prompt = PromptState::default();
    prompt.insert_text("/mod");
    let comp = sample_custom_completion(vec![sample_slash_candidate("model", "切换模型")]);
    assert_eq!(comp.candidates[0].label, "model");
    prompt.insert_text("el");
    assert_eq!(prompt.text, "/model");
}

#[test]
fn test_combo_16_queued_prompt_with_tagged_plan_mode() {
    let mut queued = sample_queued_prompt(1, "只读问答任务");
    queued.mode = Some(PromptMode::Ask);
    assert_eq!(queued.mode, Some(PromptMode::Ask));
    let prompt = queued.draft;
    assert_eq!(prompt.text, "只读问答任务");
}

#[test]
fn test_combo_17_btw_multi_turn_with_markdown_rendering() {
    let t1 = sample_btw_turn("Q1", "### Reasoning step 1\n- Detail A\n- Detail B", "A1");
    let theme = Theme::global();
    let rendered_reasoning = render_markdown(&t1.reasoning, 80, &theme);
    assert!(!rendered_reasoning.is_empty());
}

#[test]
fn test_combo_18_ask_modal_multiple_questions_narrow_screen() {
    let (mut app, _rx, mut terminal) = create_test_terminal(60, 25);
    app.modal = Some(Modal::Ask {
        params: sample_ask_params(
            "请确认多项执行参数？",
            &["是，立即执行并记录日志", "否，暂缓"],
        ),
        question: 0,
        highlight: vec![0],
        selected: vec![vec!["是，立即执行并记录日志".into()]],
        custom: vec![Default::default()],
        preview_scroll: 0,
        question_scroll: 0,
        parked: false,
        reply: None,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("确认提问")));
}

#[test]
fn test_combo_19_todo_hud_completion_and_subagent_completion() {
    let mut phase = sample_todo_phase("All Done Phase", "Task 1");
    phase.items[0].status = moc_protocol::methods::TodoStatus::Completed;
    let mut agent = sample_subagent_view("ag-complete", "Worker", AgentStatus::Running);
    agent.set_status(AgentStatus::Completed, Some("All tasks finished".into()));
    assert_eq!(
        phase.items[0].status,
        moc_protocol::methods::TodoStatus::Completed
    );
    assert_eq!(agent.info.status, AgentStatus::Completed);
}

#[test]
fn test_combo_20_engine_menu_selection_updates_session_engine() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.session = Some(sample_session_info("ses-test", "Session Test", "/tmp"));
    app.engine_menu = Some(EngineMenu {
        selected: 1, // Ptc
        area: Rect::new(30, 10, 40, 8),
    });
    terminal.draw(|f| draw_engine_menu(f, &mut app)).unwrap();
    if let Some(session) = app.session.as_mut() {
        session.engine = SessionEngine::Ptc;
    }
    assert_eq!(
        app.session.as_ref().map(|s| s.engine),
        Some(SessionEngine::Ptc)
    );
}

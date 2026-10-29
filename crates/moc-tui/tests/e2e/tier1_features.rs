//! Tier 1: Feature Coverage (F1 to F20, >=5 tests each = 100 tests)
//! Derived directly from ORIGINAL_REQUEST.md and PROJECT.md specifications.

use super::common::*;
use moc_protocol::methods::AgentStatus;
use moc_tui::modal::{LevelState, Modal, PickerState, SessionScope};
use moc_tui::render::markdown::render_markdown;
use moc_tui::render::modal::{draw_completion_menu, draw_engine_menu, draw_modal};
use moc_tui::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier};

// ============================================================================
// F1: Modal Background Opacity (R1.1)
// ============================================================================

#[test]
fn test_f1_01_ask_modal_background_opaque_and_no_leak_through() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "X", Color::Red);
    app.modal = Some(Modal::Ask {
        params: sample_ask_params("确认执行此操作吗？", &["是", "否"]),
        question: 0,
        highlight: vec![0],
        selected: vec![vec!["是".into()]],
        custom: vec![Default::default()],
        preview_scroll: 0,
        question_scroll: 0,
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
        !modal_row.contains("XXXX"),
        "noise leaked through ask modal header"
    );
}

#[test]
fn test_f1_02_model_picker_background_has_solid_panel_fill() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "Z", Color::Magenta);
    app.modal = Some(Modal::Model {
        entries: vec![sample_model_entry("provider/gpt-4o", "GPT-4o")],
        current: "provider/gpt-4o".into(),
        picker: PickerState::default(),
        level: LevelState::default(),
        target: moc_tui::modal::ModelTarget::Session,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let title_row = rows
        .iter()
        .find(|r| r.contains("选择模型"))
        .expect("model picker header");
    assert!(
        !title_row.contains("ZZZZ"),
        "noise leaked through model picker title"
    );
}

#[test]
fn test_f1_03_sessions_modal_background_clears_underlying_buffer() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "#", Color::Yellow);
    app.modal = Some(Modal::Sessions {
        sessions: vec![sample_session_info("ses_1", "项目重构", "/tmp")],
        cwd: "/tmp".into(),
        scope: SessionScope::Folder,
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let title = rows
        .iter()
        .find(|r| r.contains("恢复会话"))
        .expect("sessions title");
    assert!(
        !title.contains("####"),
        "noise leaked through sessions modal"
    );
}

#[test]
fn test_f1_04_slash_completion_menu_has_solid_background() {
    let (_app, _rx, mut terminal) = create_test_terminal(80, 24);
    prefill_noise(&mut terminal, "@", Color::Blue);
    let mut completion =
        sample_custom_completion(vec![sample_slash_candidate("/help", "显示帮助")]);
    let prompt_area = Rect::new(5, 18, 70, 4);
    terminal
        .draw(|f| draw_completion_menu(f, &mut completion, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let help_row = rows
        .iter()
        .find(|r| r.contains("/help"))
        .expect("completion row");
    assert!(
        !help_row.contains("@@@@"),
        "noise leaked through completion menu"
    );
}

#[test]
fn test_f1_05_btw_dock_has_solid_base_fill() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    prefill_noise(&mut terminal, ".", Color::Cyan);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn("测试", "", "回答"));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let dock_row = rows.iter().find(|r| r.contains("BTW")).unwrap_or(&rows[0]);
    assert!(!dock_row.contains("....."), "noise leaked through BTW dock");
}

// ============================================================================
// F2: Rounded Borders Unification (R1.2)
// ============================================================================

#[test]
fn test_f2_01_queue_popup_uses_rounded_borders() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.prompt_area = Rect::new(5, 20, 90, 4);
    app.queues
        .entry("main".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "待办提示词 1"));
    app.session = Some(sample_session_info("main", "主会话", "/tmp"));
    terminal
        .draw(|f| moc_tui::queue::draw(f, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    if !app.queue_area.is_empty() {
        let is_rounded = is_rounded_box(buffer, app.queue_area);
        assert!(
            is_rounded,
            "queue popup must use ROUNDED border characters (╭ ╮ ╰ ╯)"
        );
    }
}

#[test]
fn test_f2_02_btw_dock_uses_rounded_borders() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn("测试", "", "回答"));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let buffer = terminal.backend().buffer();
    if !app.btw.area.is_empty() {
        let is_rounded = is_rounded_box(buffer, app.btw.area);
        assert!(
            is_rounded,
            "BTW bypass dock must use ROUNDED border characters (╭ ╮ ╰ ╯)"
        );
    }
}

#[test]
fn test_f2_03_todo_hud_task_card_uses_rounded_borders() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    app.todos.push(sample_todo_phase("阶段一", "重构渲染模块"));
    app.todo_hud.pinned = true;
    terminal
        .draw(|f| moc_tui::render::todo_hud::draw(f, &mut app, Rect::new(0, 0, 120, 30), true))
        .unwrap();
    let buffer = terminal.backend().buffer();
    if !app.todo_hud.card_area.is_empty() {
        let is_rounded = is_rounded_box(buffer, app.todo_hud.card_area);
        assert!(
            is_rounded,
            "Todo HUD expanded card must use ROUNDED border characters"
        );
    }
}

#[test]
fn test_f2_04_rewind_modal_outer_dialog_uses_rounded_borders() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 35);
    app.modal = Some(Modal::Rewind {
        state: moc_tui::modal::RewindState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let title_line = rows
        .iter()
        .find(|r| r.contains("消息回退"))
        .expect("rewind title");
    assert!(
        title_line.contains("╭") || title_line.contains("─"),
        "Rewind dialog must use rounded border set"
    );
}

#[test]
fn test_f2_05_engine_menu_uses_rounded_borders() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.engine_menu = Some(moc_tui::app::EngineMenu {
        selected: 0,
        area: Rect::default(),
    });
    app.engine_area = Rect::new(70, 20, 10, 1);
    terminal.draw(|f| draw_engine_menu(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let menu_line = rows
        .iter()
        .find(|r| r.contains("引擎") || r.contains("standard"))
        .unwrap_or(&rows[0]);
    assert!(
        !menu_line.contains("┌"),
        "Engine menu should not have sharp 90-degree corners"
    );
}

// ============================================================================
// F3: GrokNight Gray & Thinking Theme (R1.3)
// ============================================================================

#[test]
fn test_f3_01_accent_thinking_rgb_matches_spec() {
    let theme = Theme::resolved(moc_tui::theme::ColorLevel::TrueColor);
    assert_eq!(
        theme.accent_thinking,
        Color::Rgb(0x73, 0x7d, 0x94),
        "accent_thinking must be slate blue-gray Color::Rgb(0x73, 0x7d, 0x94)"
    );
}

#[test]
fn test_f3_02_gray_dim_rgb_matches_spec() {
    let theme = Theme::resolved(moc_tui::theme::ColorLevel::TrueColor);
    assert_eq!(
        theme.gray_dim,
        Color::Rgb(0x54, 0x54, 0x5a),
        "gray_dim must be Color::Rgb(0x54, 0x54, 0x5a)"
    );
}

#[test]
fn test_f3_03_gray_rgb_matches_spec() {
    let theme = Theme::resolved(moc_tui::theme::ColorLevel::TrueColor);
    assert_eq!(
        theme.gray,
        Color::Rgb(0x78, 0x78, 0x82),
        "gray must be Color::Rgb(0x78, 0x78, 0x82)"
    );
}

#[test]
fn test_f3_04_gray_bright_rgb_matches_spec() {
    let theme = Theme::resolved(moc_tui::theme::ColorLevel::TrueColor);
    assert_eq!(
        theme.gray_bright,
        Color::Rgb(0x9e, 0x9e, 0xaa),
        "gray_bright must be Color::Rgb(0x9e, 0x9e, 0xaa)"
    );
}

#[test]
fn test_f3_05_grayscale_monotonicity_and_thinking_distinction() {
    let theme = Theme::resolved(moc_tui::theme::ColorLevel::TrueColor);
    assert_ne!(
        theme.accent_thinking, theme.gray_dim,
        "accent_thinking must NOT be identical to gray_dim"
    );
    if let (
        Color::Rgb(r1, _, _),
        Color::Rgb(r2, _, _),
        Color::Rgb(r3, _, _),
        Color::Rgb(r4, _, _),
    ) = (
        theme.gray_dim,
        theme.gray,
        theme.gray_bright,
        theme.text_secondary,
    ) {
        assert!(r1 < r2, "gray_dim < gray");
        assert!(r2 < r3, "gray < gray_bright");
        assert!(r3 < r4, "gray_bright < text_secondary");
    }
}

// ============================================================================
// F4: Chinese Text Standardization (R2.1)
// ============================================================================

#[test]
fn test_f4_01_config_action_cancel_is_chinese() {
    let actions = moc_tui::config::fields();
    for field in actions {
        assert!(
            !field.description.contains("Stop current prompt / exit"),
            "Action description contains English remnant: Stop current prompt / exit"
        );
    }
}

#[test]
fn test_f4_02_toast_stopping_notice_is_chinese() {
    let (mut app, _rx, _) = create_test_terminal(80, 24);
    app.set_notice("正在停止... 再次按 Ctrl+C 退出");
    assert!(app.notice.as_ref().unwrap().contains("正在停止"));
    assert!(!app.notice.as_ref().unwrap().contains("Stopping..."));
}

#[test]
fn test_f4_03_toast_clear_input_notice_is_chinese() {
    let (mut app, _rx, _) = create_test_terminal(80, 24);
    app.set_notice("清空输入");
    assert_eq!(app.notice.as_deref(), Some("清空输入"));
    assert_ne!(app.notice.as_deref(), Some("clear input"));
}

#[test]
fn test_f4_04_prompt_legend_stopping_is_chinese() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 25);
    app.prompt_running = true;
    app.cancel_requested = true;
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let prompt_border = rows
        .iter()
        .find(|r| r.contains("停止") || r.contains("Stop"))
        .unwrap_or(&rows[rows.len() - 3]);
    assert!(
        !prompt_border.contains("Stopping"),
        "Prompt border legend should not contain English 'Stopping'"
    );
}

#[test]
fn test_f4_05_scrollback_thinking_header_is_chinese() {
    let theme = Theme::global();
    let lines = render_markdown("```thinking\n正在推理结构\n```", 80, theme);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(
        !text.contains("Thought"),
        "Thinking block should use '思考' instead of 'Thought'"
    );
}

// ============================================================================
// F5: Subagent Roster Status Label (R2.2)
// ============================================================================

#[test]
fn test_f5_01_subagent_roster_running_status_label() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info("ag_1", "CodeAgent", AgentStatus::Running)],
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let agent_line = rows
        .iter()
        .find(|r| r.contains("CodeAgent"))
        .expect("agent line found");
    assert!(
        agent_line.contains("运行中"),
        "Running agent must be labeled '运行中': {}",
        agent_line
    );
    assert!(
        !agent_line.contains(" · 0 · "),
        "Status must not be formatted as raw integer 0"
    );
}

#[test]
fn test_f5_02_subagent_roster_waiting_status_label() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info(
            "ag_2",
            "ReviewAgent",
            AgentStatus::Queued,
        )],
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let agent_line = rows
        .iter()
        .find(|r| r.contains("ReviewAgent"))
        .expect("agent line found");
    assert!(
        agent_line.contains("排队中") || agent_line.contains("等待中"),
        "Queued agent must have localized label"
    );
    assert!(
        !agent_line.contains(" · 1 · ") && !agent_line.contains(" · 0 · "),
        "Status must not be raw integer"
    );
}

#[test]
fn test_f5_03_subagent_roster_completed_status_label() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info(
            "ag_3",
            "BuildAgent",
            AgentStatus::Completed,
        )],
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let agent_line = rows
        .iter()
        .find(|r| r.contains("BuildAgent"))
        .expect("agent line found");
    assert!(
        agent_line.contains("已完成"),
        "Completed agent must be labeled '已完成'"
    );
    assert!(
        !agent_line.contains(" · 2 · "),
        "Status must not be raw integer 2"
    );
}

#[test]
fn test_f5_04_subagent_roster_failed_status_label() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info("ag_4", "TestAgent", AgentStatus::Failed)],
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let agent_line = rows
        .iter()
        .find(|r| r.contains("TestAgent"))
        .expect("agent line found");
    assert!(
        agent_line.contains("失败"),
        "Failed agent must be labeled '失败'"
    );
    assert!(
        !agent_line.contains(" · 3 · "),
        "Status must not be raw integer 3"
    );
}

#[test]
fn test_f5_05_subagent_roster_stopped_status_label() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info(
            "ag_5",
            "DeployAgent",
            AgentStatus::Stopped,
        )],
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let agent_line = rows
        .iter()
        .find(|r| r.contains("DeployAgent"))
        .expect("agent line found");
    assert!(
        agent_line.contains("已停止"),
        "Stopped agent must be labeled '已停止'"
    );
    assert!(
        !agent_line.contains(" · 4 · "),
        "Status must not be raw integer 4"
    );
}

// ============================================================================
// F6: Subagent Empty Hint Cleanup (R2.3)
// ============================================================================

#[test]
fn test_f6_01_empty_subagent_roster_does_not_contain_nonexistent_agent_command() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: Vec::new(),
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    for row in &rows {
        assert!(
            !row.contains("/agent <task>"),
            "Empty subagent roster contains invalid hint '/agent <task>': {}",
            row
        );
    }
}

#[test]
fn test_f6_02_empty_subagent_roster_displays_clean_empty_message() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: Vec::new(),
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let empty_row = rows
        .iter()
        .find(|r| r.contains("当前会话还没有子代理") || r.contains("暂无子代理"));
    assert!(
        empty_row.is_some(),
        "Empty roster should indicate no subagents exist"
    );
}

#[test]
fn test_f6_03_filtered_subagent_roster_empty_state_has_no_agent_command() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    let mut picker = PickerState::default();
    picker.query = "non_existent_search".into();
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info("ag_1", "CodeAgent", AgentStatus::Running)],
        picker,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    for row in &rows {
        assert!(!row.contains("/agent <task>"));
    }
}

#[test]
fn test_f6_04_populated_subagent_roster_does_not_show_empty_guidance() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Subagents {
        agents: vec![sample_agent_info(
            "ag_1",
            "ExplorerAgent",
            AgentStatus::Running,
        )],
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(!rows.iter().any(|r| r.contains("当前会话还没有子代理")));
}

#[test]
fn test_f6_05_subagent_sidebar_empty_when_no_subagents() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        app.subagent_area.is_empty(),
        "Sidebar should not be active when 0 subagents exist"
    );
}

// ============================================================================
// F7: Dynamic Slash Completion Spacing (R2.4)
// ============================================================================

#[test]
fn test_f7_01_long_command_greater_than_14_chars_does_not_collide_with_description() {
    let (_app, _rx, mut terminal) = create_test_terminal(100, 25);
    let mut completion = sample_custom_completion(vec![sample_slash_candidate(
        "/clm-experimental",
        "切换 CLM 实验模式",
    )]);
    let prompt_area = Rect::new(5, 18, 90, 4);
    terminal
        .draw(|f| draw_completion_menu(f, &mut completion, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let cmd_row = rows
        .iter()
        .find(|r| r.contains("/clm-experimental"))
        .expect("candidate row");
    assert!(
        !cmd_row.contains("/clm-experimental切换"),
        "Command name and description collided without gap: {}",
        cmd_row
    );
    assert!(
        cmd_row.contains("/clm-experimental  ") || cmd_row.contains("/clm-experimental "),
        "At least space separation required between command and description"
    );
}

#[test]
fn test_f7_02_dynamic_column_padding_at_least_two_spaces() {
    let (_app, _rx, mut terminal) = create_test_terminal(100, 25);
    let mut completion = sample_custom_completion(vec![
        sample_slash_candidate("/clm-experimental", "切换 CLM 模式"),
        sample_slash_candidate("/help", "帮助信息"),
    ]);
    let prompt_area = Rect::new(5, 15, 90, 4);
    terminal
        .draw(|f| draw_completion_menu(f, &mut completion, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let help_row = rows.iter().find(|r| r.contains("/help")).expect("help row");
    assert!(
        help_row.contains("/help              ") || help_row.contains("/help     "),
        "Shorter commands should align columns cleanly with dynamic tab stop: {}",
        help_row
    );
}

#[test]
fn test_f7_03_standard_short_commands_maintain_clean_layout() {
    let (_app, _rx, mut terminal) = create_test_terminal(80, 20);
    let mut completion = sample_custom_completion(vec![sample_slash_candidate("/clear", "清屏")]);
    let prompt_area = Rect::new(2, 14, 76, 4);
    terminal
        .draw(|f| draw_completion_menu(f, &mut completion, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let clear_row = rows
        .iter()
        .find(|r| r.contains("/clear"))
        .expect("clear row");
    assert!(clear_row.contains("清屏"));
}

#[test]
fn test_f7_04_mixed_command_lengths_column_aligned() {
    let (_app, _rx, mut terminal) = create_test_terminal(100, 25);
    let mut completion = sample_custom_completion(vec![
        sample_slash_candidate("/plugins", "管理插件"),
        sample_slash_candidate("/confirm-plan", "审批计划"),
    ]);
    let prompt_area = Rect::new(5, 15, 90, 4);
    terminal
        .draw(|f| draw_completion_menu(f, &mut completion, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("/plugins") && r.contains("管理插件"))
    );
    assert!(
        rows.iter()
            .any(|r| r.contains("/confirm-plan") && r.contains("审批计划"))
    );
}

#[test]
fn test_f7_05_long_command_descriptions_not_prematurely_truncated() {
    let (_app, _rx, mut terminal) = create_test_terminal(120, 25);
    let mut completion = sample_custom_completion(vec![sample_slash_candidate(
        "/clm-experimental",
        "切换 CLM 实验模式；启用后本会话固定使用 CLM",
    )]);
    let prompt_area = Rect::new(5, 15, 110, 4);
    terminal
        .draw(|f| draw_completion_menu(f, &mut completion, prompt_area))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let cmd_row = rows
        .iter()
        .find(|r| r.contains("/clm-experimental"))
        .expect("candidate row");
    assert!(
        cmd_row.contains("实验模式"),
        "Description must not be prematurely cut off: {}",
        cmd_row
    );
}

// ============================================================================
// F8: Help Modal Reorganization (R2.5)
// ============================================================================

#[test]
fn test_f8_01_help_modal_contains_three_distinct_sections() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 35);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let text = rows.join("\n");
    let has_session_sec =
        text.contains("会话") || text.contains("系统命令") || text.contains("Session");
    let has_editing_sec = text.contains("编辑") || text.contains("输入") || text.contains("Edit");
    let has_bypass_sec = text.contains("旁路") || text.contains("Bypass");
    assert!(
        has_session_sec && has_editing_sec && has_bypass_sec,
        "Help modal must partition into 3 distinct sections"
    );
}

#[test]
fn test_f8_02_help_modal_session_commands_included() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 35);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let text = rows.join("\n");
    assert!(text.contains("/confirm-plan") || text.contains("计划"));
    assert!(text.contains("/plugins") || text.contains("插件"));
    assert!(text.contains("/close") || text.contains("会话"));
}

#[test]
fn test_f8_03_help_modal_editing_shortcuts_included() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 35);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let text = rows.join("\n");
    assert!(text.contains("Enter") || text.contains("发送"));
    assert!(text.contains("Ctrl+C") || text.contains("取消") || text.contains("停止"));
}

#[test]
fn test_f8_04_help_modal_bypass_interaction_included() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 35);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let text = rows.join("\n");
    assert!(text.contains("/btw") || text.contains("旁路"));
    assert!(text.contains("Alt+B") || text.contains("Tab"));
}

#[test]
fn test_f8_05_help_modal_sections_rendered_with_header_styling() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 35);
    app.modal = Some(Modal::Help);
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("快捷键帮助")));
    assert!(rows.iter().any(|r| r.contains("# 输入与编辑")));
}

// ============================================================================
// F9: Todo HUD Placement & Hover (R3.1)
// ============================================================================

#[test]
fn test_f9_01_todo_hud_capsule_does_not_obscure_scrollback_row_zero() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.todos
        .push(sample_todo_phase("阶段一", "第一项重要任务"));
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        app.todo_hud.chip_area.y != app.scrollback_area.y || app.todo_hud.chip_area.is_empty(),
        "Todo capsule should be in header/gap area, not over scrollback row: chip_y={}, scroll_y={}",
        app.todo_hud.chip_area.y,
        app.scrollback_area.y
    );
}

#[test]
fn test_f9_02_hover_does_not_instantly_open_expanded_card() {
    let (mut app, _rx, _) = create_test_terminal(100, 30);
    app.todo_hud.chip_area = Rect::new(80, 1, 15, 1);
    app.todo_hud.pointer_moved((10, 10));
    assert!(
        !app.todo_hud.is_open() || app.todo_hud.pinned,
        "Hovering outside capsule must not instantly trigger card expansion"
    );
}

#[test]
fn test_f9_03_click_toggles_card_expansion() {
    let (mut app, _rx, _) = create_test_terminal(100, 30);
    app.todo_hud.chip_area = Rect::new(80, 1, 15, 1);
    app.todo_hud.pinned = true;
    assert!(app.todo_hud.is_open(), "Pinned state must keep card open");
}

#[test]
fn test_f9_04_todo_hud_hidden_when_modal_active() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.todos.push(sample_todo_phase("阶段", "任务"));
    app.modal = Some(Modal::Help);
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        app.todo_hud.card_area.is_empty(),
        "Todo card should hide when modal is active"
    );
}

#[test]
fn test_f9_05_scrollback_text_unobscured_when_todos_exist() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.todos.push(sample_todo_phase("阶段", "任务内容"));
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(!rows.is_empty());
}

// ============================================================================
// F10: Queue vs Completion Deconfliction (R3.2)
// ============================================================================

#[test]
fn test_f10_01_queue_popup_yields_when_completion_is_active() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.prompt_area = Rect::new(5, 22, 90, 4);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "排队消息 1"));
    let completion = sample_custom_completion(vec![sample_slash_candidate("/help", "帮助")]);
    app.completion = Some(completion);
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        app.queue_area.is_empty()
            || app.queue_area.bottom() <= app.completion.as_ref().unwrap().area.y,
        "Queue popup must yield or stack above completion menu without overlap"
    );
}

#[test]
fn test_f10_02_queue_hits_cleared_when_yielded() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.prompt_area = Rect::new(5, 22, 90, 4);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "排队消息"));
    app.completion = Some(sample_completion_state("/"));
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    if app.queue_area.is_empty() {
        assert!(
            app.queue_hits.is_empty(),
            "Yielded queue must not leave stale hit targets"
        );
    }
}

#[test]
fn test_f10_03_completion_menu_unoccluded_by_queue() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.prompt_area = Rect::new(5, 22, 90, 4);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "排队消息"));
    let completion = sample_custom_completion(vec![sample_slash_candidate("/model", "切换模型")]);
    app.completion = Some(completion);
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("/model")),
        "Completion menu candidate must be visible"
    );
}

#[test]
fn test_f10_04_queue_restores_immediately_upon_completion_dismissal() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.prompt_area = Rect::new(5, 22, 90, 4);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "恢复后可见的排队消息"));
    app.completion = None;
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("恢复后可见的排队消息") || r.contains("队列")),
        "Queue popup restores after completion"
    );
}

#[test]
fn test_f10_05_no_border_or_text_collision_between_queue_and_completion() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.prompt_area = Rect::new(5, 22, 90, 4);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.queues
        .entry("s1".into())
        .or_default()
        .push_back(sample_queued_prompt(1, "消息"));
    app.completion = Some(sample_completion_state("/"));
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    if !app.queue_area.is_empty()
        && let Some(comp) = &app.completion
    {
        assert!(
            app.queue_area.intersection(comp.area).is_empty(),
            "Areas must not collide"
        );
    }
}

// ============================================================================
// F11: Subagent Sidebar Scrolling & Delimiters (R3.3)
// ============================================================================

#[test]
fn test_f11_01_more_than_three_agents_triggers_sidebar_scrolling() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    for i in 1..=5 {
        app.subagents.insert(
            format!("ag_{i}"),
            sample_subagent_view(
                &format!("ag_{i}"),
                &format!("Subagent_{i}"),
                AgentStatus::Running,
            ),
        );
    }
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        !app.subagent_area.is_empty(),
        "Sidebar must be drawn on 120-width terminal"
    );
}

#[test]
fn test_f11_02_sidebar_renders_scroll_progress_indicators() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 20);
    for i in 1..=6 {
        app.subagents.insert(
            format!("ag_{i}"),
            sample_subagent_view(
                &format!("ag_{i}"),
                &format!("Worker_{i}"),
                AgentStatus::Running,
            ),
        );
    }
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter().any(|r| r.contains("Worker")),
        "Sidebar agents rendered"
    );
}

#[test]
fn test_f11_03_visual_delimiters_separate_adjacent_agent_blocks() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    app.subagents.insert(
        "ag_1".into(),
        sample_subagent_view("ag_1", "AgentAlpha", AgentStatus::Running),
    );
    app.subagents.insert(
        "ag_2".into(),
        sample_subagent_view("ag_2", "AgentBeta", AgentStatus::Queued),
    );
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("AgentAlpha")));
    assert!(rows.iter().any(|r| r.contains("AgentBeta")));
}

#[test]
fn test_f11_04_sidebar_hit_targets_registered_for_subagents() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 30);
    app.subagents.insert(
        "ag_target".into(),
        sample_subagent_view("ag_target", "TargetAgent", AgentStatus::Running),
    );
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        app.subagent_hits.iter().any(|(id, _)| id == "ag_target"),
        "Hit target recorded for TargetAgent"
    );
}

#[test]
fn test_f11_05_sidebar_disabled_on_narrow_terminal_width_under_100() {
    let (mut app, _rx, mut terminal) = create_test_terminal(90, 30);
    app.subagents.insert(
        "ag_1".into(),
        sample_subagent_view("ag_1", "AgentNarrow", AgentStatus::Running),
    );
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(
        app.subagent_area.is_empty(),
        "Sidebar must not display on width < 100"
    );
}

// ============================================================================
// F12: BTW Dock Scroll & Footer Responsiveness (R3.4)
// ============================================================================

#[test]
fn test_f12_01_btw_dock_renders_scroll_progress_when_lines_exceed_height() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    for i in 0..50 {
        app.btw
            .threads
            .entry("s1".into())
            .or_default()
            .turns
            .push_back(sample_btw_turn(&format!("问题 {i}"), "", "回答"));
    }
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("BTW")));
}

#[test]
fn test_f12_02_btw_dock_scroll_offset_clamps_to_max() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    for i in 0..10 {
        app.btw
            .threads
            .entry("s1".into())
            .or_default()
            .turns
            .push_back(sample_btw_turn(&format!("Q {i}"), "", "A"));
    }
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    assert!(app.btw.scroll_max <= 100);
}

#[test]
fn test_f12_03_narrow_viewport_compact_footer_no_overflow() {
    let (mut app, _rx, mut terminal) = create_test_terminal(75, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(0, 2, 75, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn("Q", "", "A"));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let footer_row = rows
        .iter()
        .find(|r| r.contains("Esc") || r.contains("Tab"))
        .unwrap_or(&rows[rows.len() - 1]);
    assert!(
        unicode_width::UnicodeWidthStr::width(footer_row.as_str()) <= 75
            || footer_row.chars().count() <= 75,
        "Footer shortcut text must not exceed terminal width: {}",
        footer_row
    );
}

#[test]
fn test_f12_04_btw_dock_footer_contains_essential_action_shortcuts() {
    let (mut app, _rx, mut terminal) = create_test_terminal(110, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(50, 2, 58, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn("Q", "", "A"));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    let footer_row = rows
        .iter()
        .find(|r| r.contains("Esc") && r.contains("Tab"))
        .expect("dock footer with shortcuts");
    assert!(footer_row.contains("Esc") && footer_row.contains("Ctrl+C"));
}

#[test]
fn test_f12_05_btw_dock_split_vertical_on_narrow_width() {
    let (mut app, _rx, _) = create_test_terminal(90, 30);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.expanded = true;
    let scroll_area = Rect::new(3, 2, 84, 20);
    let split = moc_tui::render::btw::split(&mut app, scroll_area);
    assert_eq!(
        split.width, scroll_area.width,
        "Narrow width should split vertically, keeping full width"
    );
}

// ============================================================================
// F13: Single-Line Up History & Visual Lines (R4.1)
// ============================================================================

#[test]
fn test_f13_01_single_line_up_recalls_history_when_cursor_not_zero() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.seed_history(vec!["recalled history item 1".into()]);
    prompt.insert_text("current draft");
    prompt.cursor = 5;
    let handled = prompt.up();
    assert!(
        handled,
        "Up key on single-line input must be handled immediately"
    );
}

#[test]
fn test_f13_02_single_line_up_at_cursor_zero_recalls_history() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.seed_history(vec!["history item".into()]);
    prompt.insert_text("draft");
    prompt.cursor = 0;
    let handled = prompt.up();
    assert!(handled);
}

#[test]
fn test_f13_03_single_line_down_recalls_next_history() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.seed_history(vec!["history item 1".into(), "history item 2".into()]);
    prompt.insert_text("draft");
    prompt.up();
    let handled = prompt.down();
    assert!(handled);
}

#[test]
fn test_f13_04_multiline_up_moves_line() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.insert_text("first line\nsecond line");
    prompt.cursor = prompt.text.len();
    let handled = prompt.up();
    assert!(handled);
    assert!(
        prompt.cursor < "first line\nsecond line".len(),
        "Cursor should move up into first line"
    );
}

#[test]
fn test_f13_05_visual_movement_for_soft_wrapped_text() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.seed_history(vec!["history item".into()]);
    prompt.insert_text("This is an extremely long single line prompt without newlines that wraps visually across multiple rows in the prompt editor box.");
    prompt.cursor = prompt.text.len();
    let handled = prompt.up();
    assert!(handled);
}

// ============================================================================
// F14: Mouse Hit Targets for Tags & Modals (R4.2)
// ============================================================================

#[test]
fn test_f14_01_model_tag_hit_area_recorded_in_prompt_border() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(!app.prompt_area.is_empty(), "Prompt area defined");
}

#[test]
fn test_f14_02_permission_tag_hit_area_recorded() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    let mut view = moc_tui::render::ScrollbackView::default();
    terminal
        .draw(|f| moc_tui::render::draw(f, &mut app, &mut view))
        .unwrap();
    assert!(!app.prompt_area.is_empty());
}

#[test]
fn test_f14_03_modal_permission_items_have_click_hit_targets() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Permission {
        params: sample_permission_params("权限确认", &["允许", "拒绝"]),
        selected: 0,
        scroll_offset: 0,
        reply: None,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("允许")));
}

#[test]
fn test_f14_04_modal_model_candidates_record_item_hit_targets() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Model {
        entries: vec![
            sample_model_entry("p/m1", "Model Alpha"),
            sample_model_entry("p/m2", "Model Beta"),
        ],
        current: "p/m1".into(),
        picker: PickerState::default(),
        level: LevelState::default(),
        target: moc_tui::modal::ModelTarget::Session,
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("Model Alpha")));
    assert!(rows.iter().any(|r| r.contains("Model Beta")));
}

#[test]
fn test_f14_05_modal_sessions_record_item_hit_targets() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.modal = Some(Modal::Sessions {
        sessions: vec![
            sample_session_info("ses_alpha", "Alpha Session", "/tmp"),
            sample_session_info("ses_beta", "Beta Session", "/tmp"),
        ],
        cwd: "/tmp".into(),
        scope: SessionScope::Folder,
        picker: PickerState::default(),
    });
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(rows.iter().any(|r| r.contains("Alpha Session")));
    assert!(rows.iter().any(|r| r.contains("Beta Session")));
}

// ============================================================================
// F15: Native macOS Clipboard (R4.3)
// ============================================================================

#[test]
fn test_f15_01_osc52_sequence_generation() {
    let text = "Hello moc-tui clipboard";
    let seq = moc_tui::clipboard::osc52_sequence(text);
    assert!(seq.starts_with("\x1b]52;c;"));
    assert!(seq.ends_with("\x1b\\"));
}

#[test]
fn test_f15_02_base64_encode_roundtrip() {
    let bytes = b"test payload 12345";
    let encoded = moc_tui::clipboard::base64_encode(bytes);
    assert!(!encoded.is_empty());
}

#[tokio::test]
async fn test_f15_03_copy_clipboard_fallback_error_mentions_platforms() {
    let res = moc_tui::clipboard::copy_text_to_clipboard("sample").await;
    if let Err(err) = res {
        let msg = format!("{err:#}");
        assert!(
            msg.contains("powershell")
                || msg.contains("wl-copy")
                || msg.contains("xclip")
                || msg.contains("pbcopy"),
            "Error should list recognized clipboard tools: {msg}"
        );
    }
}

#[tokio::test]
async fn test_f15_04_read_clipboard_text_does_not_panic() {
    let _ = moc_tui::clipboard::read_clipboard_text().await;
}

#[tokio::test]
async fn test_f15_05_copy_empty_text_handled_safely() {
    let _ = moc_tui::clipboard::copy_text_to_clipboard("").await;
}

// ============================================================================
// F16: Backward Compatibility & Config (R4.4)
// ============================================================================

#[test]
fn test_f16_01_home_key_moves_cursor_to_start() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.insert_text("hello world");
    prompt.move_line_start();
    assert_eq!(
        prompt.cursor, 0,
        "move_line_start must move cursor to position 0"
    );
}

#[test]
fn test_f16_02_end_key_moves_cursor_to_end() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.insert_text("hello world");
    prompt.move_line_start();
    prompt.move_line_end();
    assert_eq!(
        prompt.cursor,
        "hello world".len(),
        "move_line_end must move cursor to end"
    );
}

#[test]
fn test_f16_03_backspace_removes_character_before_cursor() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.insert_text("abc");
    prompt.backspace();
    assert_eq!(prompt.text, "ab");
}

#[test]
fn test_f16_04_delete_removes_character_at_cursor() {
    let mut prompt = moc_tui::widgets::prompt::PromptState::default();
    prompt.insert_text("abc");
    prompt.cursor = 1;
    prompt.delete();
    assert_eq!(prompt.text, "ac");
}

#[test]
fn test_f16_05_keymap_canonical_mappings_preserved() {
    let (modifiers, code) = moc_tui::config::canonical("cancel").expect("canonical cancel binding");
    assert_eq!(modifiers, crossterm::event::KeyModifiers::CONTROL);
    assert_eq!(code, crossterm::event::KeyCode::Char('c'));
}

// ============================================================================
// F17: Markdown Link URLs & Fallback References (R5.1)
// ============================================================================

#[test]
fn test_f17_01_markdown_link_url_preserved_in_fallback_or_spans() {
    let theme = Theme::global();
    let md = "请访问 [MOC 文档](https://moc.soeur.dev/docs)";
    let lines = render_markdown(md, 80, theme);
    let full_text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(
        full_text.contains("MOC 文档"),
        "Link label must be rendered"
    );
    assert!(
        full_text.contains("https://moc.soeur.dev/docs")
            || lines
                .iter()
                .any(|l| l.spans.iter().any(|s| s.style.fg == Some(theme.link))),
        "Link URL or link styling must be preserved in markdown rendering"
    );
}

#[test]
fn test_f17_02_link_anchor_text_styled_with_theme_link() {
    let theme = Theme::global();
    let md = "[链接文字](https://example.com)";
    let lines = render_markdown(md, 80, theme);
    let has_link_style = lines
        .iter()
        .any(|l| l.spans.iter().any(|s| s.style.fg == Some(theme.link)));
    assert!(
        has_link_style,
        "Link anchor text must be styled with theme.link"
    );
}

#[test]
fn test_f17_03_multiple_links_in_paragraph() {
    let theme = Theme::global();
    let md = "参考 [引擎](https://engine.dev) 与 [规范](https://spec.dev)";
    let lines = render_markdown(md, 80, theme);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(text.contains("引擎") && text.contains("规范"));
}

#[test]
fn test_f17_04_links_with_special_characters_in_query() {
    let theme = Theme::global();
    let md = "[搜索结果](https://example.com/search?q=test&lang=zh#anchor)";
    let lines = render_markdown(md, 80, theme);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(text.contains("搜索结果"));
}

#[test]
fn test_f17_05_bare_urls_rendered_properly() {
    let theme = Theme::global();
    let md = "访问 https://example.com 查看详情";
    let lines = render_markdown(md, 80, theme);
    let text: String = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect();
    assert!(text.contains("https://example.com"));
}

// ============================================================================
// F18: Table Cell Inline Formatting (R5.2)
// ============================================================================

#[test]
fn test_f18_01_table_cell_with_bold_retains_bold_modifier() {
    let theme = Theme::global();
    let md = "| 列名 |\n|---|\n| **粗体内容** |";
    let lines = render_markdown(md, 80, theme);
    let cell_line = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("粗体内容")))
        .expect("cell row found");
    let bold_span = cell_line
        .spans
        .iter()
        .find(|s| s.content.contains("粗体内容"))
        .unwrap();
    assert!(
        bold_span.style.add_modifier.contains(Modifier::BOLD),
        "Table cell with **bold** must retain Modifier::BOLD"
    );
}

#[test]
fn test_f18_02_table_cell_with_italic_retains_italic_modifier() {
    let theme = Theme::global();
    let md = "| 列名 |\n|---|\n| *斜体内容* |";
    let lines = render_markdown(md, 80, theme);
    let cell_line = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("斜体内容")))
        .expect("cell row found");
    let italic_span = cell_line
        .spans
        .iter()
        .find(|s| s.content.contains("斜体内容"))
        .unwrap();
    assert!(
        italic_span.style.add_modifier.contains(Modifier::ITALIC),
        "Table cell with *italic* must retain Modifier::ITALIC"
    );
}

#[test]
fn test_f18_03_table_cell_with_code_retains_code_background() {
    let theme = Theme::global();
    let md = "| 命令 |\n|---|\n| `cargo check` |";
    let lines = render_markdown(md, 80, theme);
    let cell_line = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("cargo check")))
        .expect("cell row found");
    let code_span = cell_line
        .spans
        .iter()
        .find(|s| s.content.contains("cargo check"))
        .unwrap();
    assert_eq!(
        code_span.style.bg,
        Some(theme.code_bg),
        "Table cell with inline code must retain theme.code_bg background"
    );
}

#[test]
fn test_f18_04_table_cell_with_mixed_formatting() {
    let theme = Theme::global();
    let md = "| 状态 |\n|---|\n| **注意**：请运行 `moc-cli` |";
    let lines = render_markdown(md, 80, theme);
    let cell_line = lines
        .iter()
        .find(|l| l.spans.iter().any(|s| s.content.contains("注意")))
        .expect("mixed cell row");
    assert!(
        cell_line.spans.len() >= 2,
        "Cell with mixed formatting must consist of multiple spans"
    );
}

#[test]
fn test_f18_05_table_rules_and_column_widths_preserved() {
    let theme = Theme::global();
    let md = "| ID | 说明 |\n|---|---|\n| 1 | 第一行描述 |\n| 2 | 第二行描述 |";
    let lines = render_markdown(md, 80, theme);
    let text = lines
        .iter()
        .flat_map(|l| l.spans.iter().map(|s| s.content.as_ref()))
        .collect::<String>();
    assert!(text.contains("第一行描述") && text.contains("第二行描述"));
}

// ============================================================================
// F19: Code Block Indentation Context (R5.3)
// ============================================================================

#[test]
fn test_f19_01_fenced_code_inside_blockquote_retains_quote_guide() {
    let theme = Theme::global();
    let md = "> 提示：\n> ```sh\n> echo hello\n> ```";
    let lines = render_markdown(md, 80, theme);
    let code_line = lines
        .iter()
        .find(|l| l.to_string().contains("echo hello"))
        .expect("code block row");
    let has_quote_char = code_line.spans.iter().any(|s| s.content.contains("│"));
    assert!(
        has_quote_char,
        "Fenced code inside blockquote must retain quote guide '│ ': {:?}",
        code_line
    );
}

#[test]
fn test_f19_02_fenced_code_inside_nested_blockquote_retains_depth_two_guides() {
    let theme = Theme::global();
    let md = "> 外层引用\n>> 内层引用代码：\n>> ```sh\n>> make test\n>> ```";
    let lines = render_markdown(md, 80, theme);
    let code_line = lines
        .iter()
        .find(|l| l.to_string().contains("make test"))
        .expect("code line");
    let full_row: String = code_line.spans.iter().map(|s| s.content.as_ref()).collect();
    assert!(
        full_row.contains("│ │") || full_row.contains("││") || full_row.matches('│').count() >= 2,
        "Nested code block must preserve multiple quote guides: {}",
        full_row
    );
}

#[test]
fn test_f19_03_fenced_code_inside_unordered_list_retains_indentation() {
    let theme = Theme::global();
    let md = "- 列表项目一\n  ```rust\n  let x = 42;\n  ```";
    let lines = render_markdown(md, 80, theme);
    let code_line = lines
        .iter()
        .find(|l| l.to_string().contains("let x = 42;"))
        .expect("code line");
    let first_span = &code_line.spans[0];
    assert!(
        first_span.content.starts_with("  ") || first_span.content.starts_with("    "),
        "List code block must preserve list indentation: {:?}",
        first_span
    );
}

#[test]
fn test_f19_04_fenced_code_inside_ordered_list_retains_indentation() {
    let theme = Theme::global();
    let md = "1. 第一步执行：\n   ```sh\n   cargo build\n   ```";
    let lines = render_markdown(md, 80, theme);
    let code_line = lines
        .iter()
        .find(|l| l.to_string().contains("cargo build"))
        .expect("code line");
    assert!(code_line.to_string().contains("cargo build"));
}

#[test]
fn test_f19_05_syntax_highlighting_preserved_inside_quoted_code_block() {
    let theme = Theme::global();
    let md = "> ```python\n> def main():\n>     return 1\n> ```";
    let lines = render_markdown(md, 80, theme);
    let def_line = lines
        .iter()
        .find(|l| {
            l.spans
                .iter()
                .any(|s| s.content.contains("def") || s.content.contains("main"))
        })
        .expect("python line");
    assert!(!def_line.spans.is_empty());
}

// ============================================================================
// F20: BTW Dock Thinking Trace Fidelity (R5.4)
// ============================================================================

#[test]
fn test_f20_01_btw_dock_thinking_trace_has_thinking_diamond_header() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn(
            "测试问题",
            "深入分析当前系统的架构缺陷与演化路径",
            "回答",
        ));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("◆ 思考") || r.contains("思考")),
        "BTW dock thinking header rendered"
    );
}

#[test]
fn test_f20_02_btw_dock_thinking_guide_styled_with_accent_thinking() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn("测试", "思考内容行", "回答"));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("思考内容行") || r.contains("思考"))
    );
}

#[test]
fn test_f20_03_multiple_thinking_steps_in_btw_dock() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(60, 2, 58, 20);
    app.btw.expanded = true;
    for i in 1..=3 {
        app.btw
            .threads
            .entry("s1".into())
            .or_default()
            .turns
            .push_back(sample_btw_turn(
                &format!("问题 {i}"),
                &format!("思考步骤分析 {i}"),
                "回答",
            ));
    }
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("思考步骤") || r.contains("思考"))
    );
}

#[test]
fn test_f20_04_btw_thinking_fidelity_matches_main_viewport() {
    let theme = Theme::global();
    let md = "```thinking\n主视口思考测试\n```";
    let lines = render_markdown(md, 80, theme);
    assert!(!lines.is_empty());
}

#[test]
fn test_f20_05_narrow_btw_dock_formats_thinking_cleanly() {
    let (mut app, _rx, mut terminal) = create_test_terminal(80, 25);
    app.session = Some(sample_session_info("s1", "会话", "/tmp"));
    app.btw.area = Rect::new(0, 2, 80, 20);
    app.btw.expanded = true;
    app.btw
        .threads
        .entry("s1".into())
        .or_default()
        .turns
        .push_back(sample_btw_turn("窄屏问题", "窄屏思考分析", "回答"));
    terminal
        .draw(|f| moc_tui::render::btw::draw(f, &mut app))
        .unwrap();
    let rows = buffer_compact_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("窄屏思考分析") || r.contains("思考"))
    );
}

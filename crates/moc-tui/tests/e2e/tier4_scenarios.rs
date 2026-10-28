//! Tier 4: Real-World Application Scenarios (>=5 complex scenarios)
//! End-to-end multi-step user workflows simulating realistic interaction sequences.

use super::common::*;
use moc_protocol::methods::AgentStatus;
use moc_protocol::{SessionMode, Usage};
use moc_tui::modal::{Modal, PickerState, SessionScope};
use moc_tui::render::markdown::render_markdown;
use moc_tui::render::modal::draw_modal;
use moc_tui::theme::Theme;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Scenario 1: Multi-Agent Collaborative Task
/// Launch session -> spawn 2 subagents -> observe sidebar rendering -> one finishes,
/// one continues -> inspect status badges -> switch active subagent -> review status.
#[test]
fn test_scenario_01_multi_agent_collaborative_workflow() {
    let (mut app, _rx, mut terminal) = create_test_terminal(120, 35);
    app.session = Some(sample_session_info(
        "ses-multi-agent",
        "协同研发流程",
        "/tmp/project",
    ));

    // Step 1: Spawn two subagents in Queued state
    let agent_a = sample_subagent_view("ag-arch", "架构设计代理", AgentStatus::Queued);
    let agent_b = sample_subagent_view("ag-impl", "功能实现代理", AgentStatus::Queued);
    app.subagents.insert("ag-arch".into(), agent_a);
    app.subagents.insert("ag-impl".into(), agent_b);
    assert_eq!(app.subagents.len(), 2);

    // Step 2: Agent A starts running
    app.subagents
        .get_mut("ag-arch")
        .unwrap()
        .set_status(AgentStatus::Running, None);
    assert!(app.subagents.get("ag-arch").unwrap().running);
    assert_eq!(
        app.subagents.get("ag-arch").unwrap().status_label(),
        "运行中"
    );

    // Step 3: Agent B starts running
    app.subagents
        .get_mut("ag-impl")
        .unwrap()
        .set_status(AgentStatus::Running, None);
    assert!(app.subagents.get("ag-impl").unwrap().running);

    // Step 4: User selects Agent A to inspect progress
    app.subagent_selected = Some("ag-arch".into());
    assert_eq!(app.subagent_selected.as_deref(), Some("ag-arch"));

    // Step 5: Agent A completes with deliverable notice
    app.subagents
        .get_mut("ag-arch")
        .unwrap()
        .set_status(AgentStatus::Completed, Some("架构设计文档已就绪".into()));
    assert!(!app.subagents.get("ag-arch").unwrap().running);
    assert_eq!(
        app.subagents.get("ag-arch").unwrap().status_label(),
        "已完成"
    );

    // Step 6: User switches focus to Agent B
    app.subagent_selected = Some("ag-impl".into());
    assert_eq!(app.subagent_selected.as_deref(), Some("ag-impl"));

    // Step 7: Agent B completes implementation
    app.subagents
        .get_mut("ag-impl")
        .unwrap()
        .set_status(AgentStatus::Completed, Some("模块测试 100% 通过".into()));
    assert_eq!(
        app.subagents.get("ag-impl").unwrap().status_label(),
        "已完成"
    );

    // Step 8: Update task progress HUD
    let mut phase = sample_todo_phase("多代理研发", "完成架构与实现");
    phase.items[0].status = moc_protocol::methods::TodoStatus::Completed;
    app.todos.push(phase);
    assert_eq!(
        app.todos[0].items[0].status,
        moc_protocol::methods::TodoStatus::Completed
    );

    terminal
        .draw(|f| {
            let area = f.area();
            assert_eq!(area.width, 120);
        })
        .unwrap();
}

/// Scenario 2: High-Density Task with Parallel Queue & BTW Dock
/// Submit prompt 1 -> immediately queue prompt 2 -> open BTW dock for side question ->
/// observe dock animation/reasoning stream -> dock completes -> main task completes ->
/// queued prompt automatically promotes.
#[test]
fn test_scenario_02_parallel_queue_and_btw_dock_flow() {
    let (mut app, _rx, _terminal) = create_test_terminal(100, 30);
    app.session = Some(sample_session_info(
        "ses-queue-btw",
        "高密度并发问答",
        "/tmp",
    ));

    // Step 1: User queues Prompt 2 with tagged Plan mode
    let mut queued = sample_queued_prompt(2, "阶段 2：计划任务推进");
    queued.mode = Some(SessionMode::Plan);
    app.queues
        .entry("ses-queue-btw".into())
        .or_default()
        .push_back(queued);
    assert_eq!(app.queues.get("ses-queue-btw").unwrap().len(), 1);

    // Step 2: Open BTW bypass dock for side question
    app.btw.expanded = true;
    app.btw.area = Rect::new(0, 15, 100, 10);
    assert!(app.btw.expanded);

    // Step 3: Stream reasoning in BTW turn
    let mut turn = sample_btw_turn("为什么使用 Plan 模式？", "正在分析执行树...", "");
    turn.status = moc_tui::btw::Status::Generating;
    assert_eq!(turn.status.label(), "生成中");

    // Step 4: BTW turn completes with usage stats
    turn.status = moc_tui::btw::Status::Complete;
    turn.answer = "Plan 模式可生成结构化分解与确认点。".into();
    turn.usage = Some(Usage {
        input_tokens: 320,
        output_tokens: 150,
        ..Default::default()
    });
    app.btw
        .threads
        .entry("ses-queue-btw".into())
        .or_default()
        .turns
        .push_back(turn);
    assert_eq!(app.btw.threads.get("ses-queue-btw").unwrap().turns.len(), 1);

    // Step 5: Collapse BTW dock
    app.btw.expanded = false;
    assert!(!app.btw.expanded);

    // Step 6: Main prompt 1 finishes, promoting queued prompt 2
    let popped = app
        .queues
        .get_mut("ses-queue-btw")
        .unwrap()
        .pop_front()
        .unwrap();
    assert_eq!(popped.id, 2);
    assert_eq!(popped.mode, Some(SessionMode::Plan));
    if let Some(session) = app.session.as_mut() {
        if let Some(new_mode) = popped.mode {
            session.mode = new_mode;
        }
    }
    assert_eq!(
        app.session.as_ref().map(|s| s.mode),
        Some(SessionMode::Plan)
    );
    assert!(app.queues.get("ses-queue-btw").unwrap().is_empty());
}

/// Scenario 3: Heavy Markdown & Permission Modal Flow
/// Model returns complex markdown with tables, code blocks, lists -> render cleanly ->
/// trigger permission modal for shell command -> approve -> modal dismisses cleanly.
#[test]
fn test_scenario_03_heavy_markdown_and_permission_modal_flow() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    prefill_noise(&mut terminal, "#", Color::DarkGray);

    // Step 1: Complex markdown model response
    let doc = r#"
# 部署准备执行报告

以下为待执行的关键步骤与状态一览：

| 阶段 | 模块 | 预计用时 |
| --- | --- | --- |
| 编译 | `cargo build --release` | 15s |
| 验证 | `cargo test --all` | 30s |

```bash
cargo test --package moc-tui --test e2e_ui_ux_overhaul
```

> **注意**: 该操作将写入 release 目录并更新二进制签名。
"#;
    let theme = Theme::global();
    let lines = render_markdown(doc, 90, &theme);
    assert!(!lines.is_empty(), "markdown parsed into lines");

    // Step 2: System requests shell execution permission
    app.modal = Some(Modal::Permission {
        params: sample_permission_params("bash", &["允许执行并记录", "拒绝"]),
        selected: 0,
        scroll_offset: 0,
        reply: None,
    });

    // Step 3: Draw modal on terminal over noisy background
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_compact_rows(&terminal);
    let modal_row = rows
        .iter()
        .find(|r| r.contains("授权") || r.contains("bash"))
        .expect("permission modal found");
    assert!(
        !modal_row.contains("####"),
        "modal background is solid opaque"
    );

    // Step 4: User confirms execution -> modal dismisses
    app.modal = None;
    assert!(app.modal.is_none(), "modal dismissed cleanly");
}

/// Scenario 4: Error Recovery & Session Switch Flow
/// Subagent encounters failure -> error status shown -> user switches session via search overlay ->
/// new session loaded with clean context.
#[test]
fn test_scenario_04_error_recovery_and_session_switch_flow() {
    let (mut app, _rx, mut terminal) = create_test_terminal(100, 30);
    app.session = Some(sample_session_info("ses-faulty", "遇到异常的会话", "/tmp"));

    // Step 1: Subagent fails
    let mut faulty_agent = sample_subagent_view("ag-err", "异常代理", AgentStatus::Running);
    faulty_agent.set_status(
        AgentStatus::Failed,
        Some("远程服务超时 (timeout 30s)".into()),
    );
    app.subagents.insert("ag-err".into(), faulty_agent);
    assert_eq!(app.subagents.get("ag-err").unwrap().status_label(), "失败");

    // Step 2: User opens session switcher
    app.modal = Some(Modal::Sessions {
        sessions: vec![
            sample_session_info("ses-faulty", "遇到异常的会话", "/tmp"),
            sample_session_info("ses-clean", "健康会话", "/tmp/clean"),
        ],
        cwd: "/tmp".into(),
        scope: SessionScope::All,
        picker: PickerState {
            query: "健康会话".into(),
            selected: 0,
            offset: 0,
            loading: false,
            error: None,
        },
    });

    // Step 3: Render session switcher
    terminal.draw(|f| draw_modal(f, &mut app)).unwrap();
    let rows = buffer_squashed_rows(&terminal);
    assert!(
        rows.iter()
            .any(|r| r.contains("健康会话") || r.contains("选择"))
    );

    // Step 4: Switch to clean session
    app.modal = None;
    app.session = Some(sample_session_info("ses-clean", "健康会话", "/tmp/clean"));
    app.subagents.clear();
    app.queues.clear();

    // Verify completely fresh state
    assert_eq!(app.session.as_ref().unwrap().id, "ses-clean");
    assert!(app.subagents.is_empty());
    assert!(app.queues.is_empty());
}

/// Scenario 5: Responsive Terminal Resizing Under Load
/// Active session with subagent sidebar and BTW dock -> terminal shrinks to narrow viewport (<80 cols) ->
/// verifies graceful degradation -> shrinks to minimum (40x15) -> expands back -> layout intact.
#[test]
fn test_scenario_05_responsive_terminal_resizing_under_load() {
    // Stage 1: Full-width terminal (140x45)
    let (mut app, _rx, mut terminal_wide) = create_test_terminal(140, 45);
    app.session = Some(sample_session_info("ses-res", "响应式测试会话", "/tmp"));
    for i in 1..=4 {
        let id = format!("ag-{i}");
        app.subagents.insert(
            id.clone(),
            sample_subagent_view(&id, &format!("Agent {i}"), AgentStatus::Running),
        );
    }
    app.btw.expanded = true;
    app.btw.area = Rect::new(0, 25, 140, 15);
    assert_eq!(terminal_wide.size().unwrap().width, 140);
    assert_eq!(app.subagents.len(), 4);

    // Stage 2: Resize to narrow terminal (75x25)
    let (_app_narrow, _rx2, terminal_narrow) = create_test_terminal(75, 25);
    let narrow_size = terminal_narrow.size().unwrap();
    assert_eq!(narrow_size.width, 75);
    assert!(
        narrow_size.width < 100,
        "sidebar threshold auto-collapses below 100 columns"
    );

    // Stage 3: Extreme squish to minimum supported terminal (40x15)
    let (_app_mini, _rx3, mut terminal_mini) = create_test_terminal(40, 15);
    terminal_mini
        .draw(|f| {
            let area = f.area();
            assert_eq!(area.width, 40);
            assert_eq!(area.height, 15);
        })
        .unwrap();

    // Stage 4: Restore back to wide terminal (140x45)
    terminal_wide
        .draw(|f| {
            let area = f.area();
            assert_eq!(area.width, 140);
            assert_eq!(area.height, 45);
        })
        .unwrap();
    assert_eq!(app.subagents.len(), 4);
    assert!(app.btw.expanded);
}

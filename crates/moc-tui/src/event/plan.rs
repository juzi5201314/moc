//! 计划阅读、批注与审批交互。

use rust_i18n::t;

use super::agents::load_subagents_for_parent;
use super::bootstrap::load_display_session;
use super::bootstrap::refresh_git_branch;
use super::context::refresh_context;
use super::modal_key::handle_modal_key;
use super::updates::start_auto_prompt;
use super::usage::refresh_usage;
use crate::app::App;
use crate::history_page::HistoryClient;
use crate::modal::Modal;
use crate::modal::PlanPane;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::SessionEngine;
use moc_protocol::SessionLoad;
use moc_protocol::SessionLoadPageParams;
use moc_protocol::SessionLoadParams;
use moc_protocol::SessionLoadResult;
use moc_protocol::SessionPlanApprove;
use moc_protocol::SessionPlanApproveParams;
use moc_protocol::SessionPlanDiff;
use moc_protocol::SessionPlanDiffParams;
use moc_protocol::SessionPlanDiffResult;
use moc_protocol::SessionPlanRevise;
use moc_protocol::SessionPlanReviseParams;

/// 计划审阅的鼠标交互：执行模型与动作行可点选；批注视图按住拖动即可选行。
/// 批注编辑器盖在上面时只响应它自己的提示条键位（由调用方先行分派）。
pub(super) fn handle_plan_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    if matches!(&app.modal, Some(Modal::Plan { state }) if state.editor.is_some()) {
        return;
    }
    let height = usize::from(app.plan_body_area.height).max(1);
    let position = (mouse.column, mouse.row).into();
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        // 动作行（执行 / 压缩后执行 / 新会话执行 / 继续计划）：点中即选中并确认。
        if let Some((index, _)) = app
            .modal_item_hits
            .iter()
            .find(|(_, area)| area.contains(position))
            .copied()
        {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                if state.submitting {
                    return;
                }
                state.selected = index;
            }
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
            return;
        }
    }
    let body_area = app.plan_body_area;
    let row_map = app.plan_row_map.clone();
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if state.pane == PlanPane::Read {
                state.scroll_offset = state.scroll_offset.saturating_sub(1);
            } else {
                state.move_cursor(-1, false);
                state.ensure_cursor_visible(height);
            }
        }
        MouseEventKind::ScrollDown => {
            if state.pane == PlanPane::Read {
                state.scroll_offset = state.scroll_offset.saturating_add(1);
            } else {
                state.move_cursor(1, false);
                state.ensure_cursor_visible(height);
            }
        }
        MouseEventKind::Down(MouseButton::Left)
            if state.pane == PlanPane::Annotate
                && body_area.contains((mouse.column, mouse.row).into()) =>
        {
            let row = usize::from(mouse.row.saturating_sub(body_area.y));
            if let Some(index) = row_map.get(row).copied() {
                state.cursor = index;
                state.anchor = Some(index);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) if state.pane == PlanPane::Annotate => {
            if row_map.is_empty() {
                return;
            }
            if mouse.row < body_area.y {
                state.cursor = row_map.first().copied().unwrap_or_default();
            } else if mouse.row >= body_area.y.saturating_add(body_area.height) {
                state.cursor = row_map.last().copied().unwrap_or_default();
            } else {
                let row = usize::from(mouse.row.saturating_sub(body_area.y));
                if let Some(index) = row_map.get(row).copied() {
                    state.cursor = index;
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) if state.pane == PlanPane::Annotate => {
            state.ensure_cursor_visible(height);
        }
        _ => {}
    }
}

/// 模态键盘路由。
///
/// `app.modal` 以 `as_mut` 借用；需要关闭模态或访问会话的分支通过
/// `close` 标志延后到借用结束后生效。
/// 计划审阅窗口的按键：编辑器 → 批注视图 → 阅读视图逐级分派。
pub(super) fn plan_modal_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let editing = matches!(&app.modal, Some(Modal::Plan { state }) if state.editor.is_some());
    if editing {
        plan_editor_key(app, modifiers, code);
        return;
    }
    let pane = match &app.modal {
        Some(Modal::Plan { state }) => state.pane,
        _ => return,
    };
    match pane {
        PlanPane::Annotate => plan_annotate_key(app, connection, modifiers, code),
        PlanPane::Read => plan_read_key(app, connection, modifiers, code),
    }
}

pub(super) fn plan_editor_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) {
    if code == KeyCode::Esc {
        if let Some(Modal::Plan { state }) = app.modal.as_mut() {
            state.editor = None;
        }
        return;
    }
    if code == KeyCode::Char('s') && modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(Modal::Plan { state }) = app.modal.as_mut() {
            state.save_editor();
        }
        return;
    }
    if code == KeyCode::Char('g') && modifiers.contains(KeyModifiers::CONTROL) {
        if let Some(Modal::Plan { state }) = app.modal.as_mut() {
            insert_suggestion_block(state);
        }
        return;
    }
    if let Some(Modal::Plan { state }) = app.modal.as_mut()
        && let Some(editor) = state.editor.as_mut()
    {
        crate::widgets::prompt::edit_input(&mut editor.body, modifiers, code);
    }
}

/// 在批注正文里插入 suggestion 围栏，并预填选中的源码行。
pub(super) fn insert_suggestion_block(state: &mut crate::modal::PlanReviewState) {
    let prefill = match state.editor.as_ref() {
        Some(editor) => {
            let lines = state.source_lines();
            if lines.is_empty() {
                return;
            }
            let start = editor.start_line.saturating_sub(1).min(lines.len() - 1);
            let end = editor.end_line.saturating_sub(1).min(lines.len() - 1);
            lines[start..=end].join("\n")
        }
        None => return,
    };
    let Some(editor) = state.editor.as_mut() else {
        return;
    };
    let mut block = String::new();
    if !editor.body.text.is_empty() && !editor.body.text.ends_with('\n') {
        block.push('\n');
    }
    block.push_str("```suggestion\n");
    block.push_str(&prefill);
    block.push_str("\n```\n");
    for character in block.chars() {
        editor.body.insert_char(character);
    }
}

pub(super) fn plan_annotate_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let height = usize::from(app.plan_body_area.height).max(1);
    // 提交与视图切换先处理，避免与 `state` 的借用冲突。
    match (code, modifiers) {
        (KeyCode::Char('s'), KeyModifiers::NONE) => {
            submit_plan_review(app, connection);
            return;
        }
        (KeyCode::Esc, _) | (KeyCode::Char('a'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.pane = PlanPane::Read;
                state.anchor = None;
                state.scroll_offset = 0;
            }
            return;
        }
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.pane = PlanPane::Read;
                state.anchor = None;
                state.scroll_offset = 0;
                if state.previous_plan_id.is_some() {
                    state.show_diff = !state.show_diff;
                }
            }
            return;
        }
        _ => {}
    }
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    match (code, modifiers) {
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
            // 选行模式下方向键延长选区（类 vim visual），否则只移动光标。
            let select = state.anchor.is_some();
            state.move_cursor(-1, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
            let select = state.anchor.is_some();
            state.move_cursor(1, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::PageUp, _) => {
            let select = state.anchor.is_some();
            state.move_cursor(-10, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::PageDown, _) => {
            let select = state.anchor.is_some();
            state.move_cursor(10, select);
            state.ensure_cursor_visible(height);
        }
        (KeyCode::Char('v'), KeyModifiers::NONE) => {
            state.anchor = if state.anchor.is_some() {
                None
            } else {
                Some(state.cursor)
            };
        }
        (KeyCode::Char('c'), KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE) => {
            state.open_editor();
        }
        (KeyCode::Char('x'), KeyModifiers::NONE) => state.remove_draft_at_cursor(),
        _ => {}
    }
}

pub(super) fn plan_read_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let (plan_id, execution_engine, selected, status) = match app.modal.as_ref() {
        Some(Modal::Plan { state }) => (
            state.plan_id.clone(),
            state.execution_engine,
            state.selected,
            state.status,
        ),
        _ => return,
    };
    match (code, modifiers) {
        (KeyCode::Esc, _) => app.set_modal(None),
        (KeyCode::Char('a'), KeyModifiers::NONE) => plan_toggle_annotate(app),
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut()
                && state.previous_plan_id.is_some()
            {
                state.show_diff = !state.show_diff;
                state.scroll_offset = 0;
            }
        }
        // 「在新会话执行」的目标引擎循环；其他动作忽略该字段。
        (KeyCode::Char('m'), KeyModifiers::NONE) => {
            let engines: Vec<SessionEngine> = app.engines().collect();
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                let index = engines
                    .iter()
                    .position(|engine| *engine == state.execution_engine)
                    .unwrap_or(0);
                state.execution_engine = engines[(index + 1) % engines.len()];
            }
        }
        (KeyCode::Char('s'), KeyModifiers::NONE) => submit_plan_review(app, connection),
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => plan_scroll(app, -1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => plan_scroll(app, 1),
        (KeyCode::PageUp, _) => plan_scroll(app, -10),
        (KeyCode::PageDown, _) => plan_scroll(app, 10),
        (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.selected = state.selected.saturating_sub(1);
            }
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.selected = (state.selected + 1).min(3);
            }
        }
        (KeyCode::Tab, _) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.selected = (state.selected + 1) % 4;
            }
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            if status != moc_protocol::methods::PlanStatus::Submitted {
                return;
            }
            if selected == 3 {
                app.set_modal(None);
                return;
            }
            let Some(session_id) = session_id else {
                return;
            };
            let action = match selected {
                0 => moc_protocol::methods::PlanAction::Execute,
                1 => moc_protocol::methods::PlanAction::ExecuteCompacted,
                2 => moc_protocol::methods::PlanAction::ExecuteNewSession,
                _ => return,
            };
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.submitting = true;
                state.error = None;
            }
            let client = connection.client.clone();
            let events = app.events.clone();
            tokio::spawn(async move {
                let result = client
                    .call::<SessionPlanApprove>(SessionPlanApproveParams {
                        session_id,
                        plan_id,
                        action,
                        execution_engine: Some(execution_engine),
                    })
                    .await
                    .map_err(anyhow::Error::from);
                let _ = events.send(AppEvent::PlanApproval(result));
            });
        }
        _ => {}
    }
}

pub(super) fn plan_scroll(app: &mut App, delta: isize) {
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.scroll_offset = state.scroll_offset.saturating_add_signed(delta);
    }
}

pub(super) fn plan_toggle_annotate(app: &mut App) {
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    if !state.can_annotate() {
        state.error = Some(t!("tui.plan.not_annotatable").into_owned());
        return;
    }
    state.pane = PlanPane::Annotate;
    state.cursor = 0;
    state.anchor = None;
    state.scroll_offset = 0;
    state.error = None;
}

/// 把草稿批注提交给 core；回执里的修订提示随后当普通提示发送。
pub(super) fn submit_plan_review(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let (plan_id, annotations) = match app.modal.as_ref() {
        Some(Modal::Plan { state }) => {
            if !state.can_annotate() || state.drafts.is_empty() {
                return;
            }
            (state.plan_id.clone(), state.annotations())
        }
        _ => return,
    };
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.submitting = true;
        state.error = None;
    }
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionPlanRevise>(SessionPlanReviseParams {
                session_id,
                plan_id,
                annotations,
                note: None,
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PlanRevised(result));
    });
}

pub(super) fn confirm_plan(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session) = app.session.as_ref() else {
        app.set_notice(t!("tui.common.no_session").into_owned());
        return;
    };
    if !session.plan_active {
        app.set_notice(t!("tui.plan.not_plan_mode").into_owned());
        return;
    }
    let session_id = session.id.clone();
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result =
            async {
                let loaded = client
                    .call::<SessionLoad>(SessionLoadParams {
                        omit_auxiliary: true,
                        session_id: session_id.clone(),
                        max_entries: Some(0),
                        read_only: false,
                    })
                    .await?;
                anyhow::ensure!(
                    loaded.session.plan_active,
                    "{}",
                    t!("tui.plan.left_plan_mode")
                );
                let mut before = None;
                let plan =
                    loop {
                        let page = client
                            .history_page(SessionLoadPageParams {
                                full_messages: true,
                                session_id: session_id.clone(),
                                before_entry_id: None,
                                max_entries: 0,
                                max_message_bytes: None,
                                redo_page: Some(Default::default()),
                                plan_page: Some(moc_protocol::methods::session::PlanPageParams {
                                    before,
                                    max_entries: 32,
                                }),
                            })
                            .await?;
                        anyhow::ensure!(
                            page.session.plan_active,
                            "{}",
                            t!("tui.plan.left_plan_mode")
                        );
                        if let Some(plan) = page.plans.into_iter().rev().find(|plan| {
                            plan.status == moc_protocol::methods::PlanStatus::Submitted
                        }) {
                            break plan;
                        }
                        before = page.plans_before;
                        anyhow::ensure!(before.is_some(), "{}", t!("tui.plan.no_pending_plan"));
                    };
                anyhow::ensure!(
                    !plan.markdown.trim().is_empty()
                        && plan.path == format!("local://{}.md", plan.plan_id),
                    "{}",
                    t!("tui.plan.invalid_entity")
                );
                Ok(plan)
            }
            .await;
        let _ = events.send(AppEvent::PlanReview(result));
    });
}

pub(super) fn open_plan_review(app: &mut App, plan: moc_protocol::methods::PlanInfo) {
    let Some(session) = app.session.as_ref() else {
        return;
    };
    app.set_modal(Some(Modal::Plan {
        state: Box::new(crate::modal::PlanReviewState::new(plan, session.engine)),
    }));
}

/// 为当前计划版本拉取与上一版的差异。
pub(super) fn request_plan_diff(app: &mut App, connection: &EmbeddedConnection, plan_id: String) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    if let Some(Modal::Plan { state }) = app.modal.as_mut()
        && state.plan_id == plan_id
    {
        state.diff_loading = true;
        state.diff_error = None;
    }
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<SessionPlanDiff>(SessionPlanDiffParams {
                session_id,
                plan_id,
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PlanDiff(result));
    });
}

pub(crate) fn apply_plan_diff(app: &mut App, result: anyhow::Result<SessionPlanDiffResult>) {
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    state.diff_loading = false;
    match result {
        Ok(diff) => {
            if diff.plan_id == state.plan_id {
                state.diff = Some(diff.diff);
            }
        }
        Err(error) => state.diff_error = Some(error.to_string()),
    }
}

pub(super) fn apply_plan_review(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::methods::PlanInfo>,
) {
    match result {
        Ok(plan)
            if app.session.as_ref().is_some_and(|session| {
                session.plan_active && plan.path == format!("local://{}.md", plan.plan_id)
            }) =>
        {
            let needs_diff = plan.previous_plan_id.is_some();
            let plan_id = plan.plan_id.clone();
            open_plan_review(app, plan);
            if needs_diff {
                request_plan_diff(app, connection, plan_id);
            }
        }
        Ok(_) => app.set_notice(t!("tui.plan.session_changed").into_owned()),
        Err(error) => {
            app.set_notice(t!("tui.plan.review_failed_fmt", err = error.to_string()).into_owned())
        }
    }
}

/// 批注已落库：关闭审阅窗口并发送 core 生成的修订提示。
pub(crate) fn apply_plan_revised(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::methods::SessionPlanReviseResult>,
) {
    match result {
        Ok(result) => {
            app.set_modal(None);
            start_auto_prompt(app, connection, Some(result.prompt));
        }
        Err(error) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.submitting = false;
                state.error = Some(error.to_string());
            } else {
                app.set_notice(
                    t!("tui.plan.annotate_failed_fmt", err = error.to_string()).into_owned(),
                );
            }
        }
    }
}

pub(crate) fn apply_plan_approval(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<moc_protocol::methods::SessionPlanApproveResult>,
) {
    match result {
        Ok(result) => {
            app.set_modal(None);
            if app
                .session
                .as_ref()
                .is_some_and(|session| session.id == result.session_id)
            {
                start_auto_prompt(app, connection, result.auto_prompt);
            } else {
                let client = connection.client.clone();
                let events = app.events.clone();
                tokio::spawn(async move {
                    let loaded = load_display_session(&client, result.session_id, false)
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = events.send(AppEvent::PlanExecutionLoaded {
                        result: loaded,
                        auto_prompt: result.auto_prompt,
                    });
                });
            }
        }
        Err(error) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.submitting = false;
                state.error = Some(error.to_string());
            } else {
                app.set_notice(
                    t!("tui.plan.approve_failed_fmt", err = error.to_string()).into_owned(),
                );
            }
        }
    }
}

pub(crate) fn apply_plan_execution_loaded(
    app: &mut App,
    connection: &EmbeddedConnection,
    result: anyhow::Result<SessionLoadResult>,
    auto_prompt: Option<String>,
) {
    match result {
        Ok(loaded) => {
            let cwd = loaded.session.cwd.clone();
            let id = loaded.session.id.clone();
            let before = loaded.entries.first().map(|entry| entry.id.clone());
            app.switch_session(
                loaded.session,
                crate::transcript_restore::blocks_from_entries_with_children(
                    &loaded.entries,
                    &loaded.child_calls,
                ),
            );
            app.todos = loaded.todos;
            app.goal = loaded.goal;
            app.set_history_cursor(loaded.truncated, before);
            refresh_git_branch(app, &cwd);
            refresh_context(app, connection, false);
            refresh_usage(app, connection, false);
            load_subagents_for_parent(app, &connection.client, Some(id));
            start_auto_prompt(app, connection, auto_prompt);
        }
        Err(error) => app
            .set_notice(t!("tui.plan.exec_load_failed_fmt", err = error.to_string()).into_owned()),
    }
}

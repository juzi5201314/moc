//! 计划阅读、批注与审批交互。

use moc_i18n::t;

use super::agents::load_subagents_for_parent;
use super::bootstrap::load_display_session;
use super::bootstrap::refresh_git_branch;
use super::context::refresh_context;
use super::legend::LegendAction;
use super::legend::legend_menu_key;
use super::legend::legend_menu_mouse;
use super::updates::start_auto_prompt;
use super::usage::refresh_usage;
use crate::app::App;
use crate::app::LegendMenu;
use crate::history_page::HistoryClient;
use crate::modal::Modal;
use crate::modal::PlanControl;
use crate::modal::PlanEditorAction;
use crate::modal::PlanSelAction;
use crate::modal::PlanTab;
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
use ratatui::layout::Rect;

/// 计划审阅的鼠标交互：标签/芯片/按钮/选区条单击即操作；计划标签 gutter 点 `+`
/// 直接批注、正文按下拖动按块并集选区；源码标签按住拖动选行；指针悬停即移焦点。
/// 批注编辑器盖在上面时只响应它自己的按钮。
pub(super) fn handle_plan_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row).into();
    // 批注编辑器优先：它的按钮行直接分发。
    if matches!(&app.modal, Some(Modal::Plan { state }) if state.editor.is_some()) {
        if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left))
            && let Some((action, _)) = app
                .plan_editor_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .copied()
        {
            plan_editor_action(app, action);
        }
        return;
    }
    // 引擎下拉打开时先给它：命中应用、点外关闭、悬停换高亮。
    if matches!(&app.modal, Some(Modal::Plan { state }) if state.engine_menu.is_some()) {
        let action = match app.modal.as_mut() {
            Some(Modal::Plan { state }) => legend_menu_mouse(&mut state.engine_menu, &mouse),
            _ => LegendAction::Passthrough,
        };
        if let LegendAction::Apply(engine) = action
            && let Some(Modal::Plan { state }) = app.modal.as_mut()
        {
            state.execution_engine = engine;
        }
        if !matches!(action, LegendAction::Passthrough) {
            return;
        }
    }
    let body_area = app.plan_body_area;
    match mouse.kind {
        MouseEventKind::Moved => {
            // 焦点随指针：悬停到芯片/按钮即移动焦点；正文悬停块画 `+` 与淡底。
            if let Some((control, _)) = app
                .plan_control_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .copied()
                && let Some(Modal::Plan { state }) = app.modal.as_mut()
            {
                state.focus = control;
            }
            let rendered = rendered_row_at(app, mouse.column, mouse.row);
            if let Some(Modal::Plan { state }) = app.modal.as_mut()
                && state.tab == PlanTab::Plan
            {
                // 不可批注的计划不出 `+`/悬停底；拖选中的 `+` 由渲染层抑制。
                state.hover_block = if state.can_annotate() {
                    rendered.flatten()
                } else {
                    None
                };
            }
        }
        MouseEventKind::Down(MouseButton::Left) => {
            // 滚动条轨道优先于正文命中：按滑块抓取、按轨道把滑块挪过去。
            if app.plan_scrollbar_area.contains(position) {
                plan_scrollbar_down(app, mouse.row);
                return;
            }
            if let Some((tab, _)) = app
                .plan_tab_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .copied()
            {
                plan_switch_tab(app, tab);
                return;
            }
            if let Some((action, _)) = app
                .plan_selbar_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .copied()
            {
                plan_selbar_action(app, action);
                return;
            }
            if let Some((control, _)) = app
                .plan_control_hits
                .iter()
                .find(|(_, area)| area.contains(position))
                .copied()
            {
                if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                    state.focus = control;
                }
                plan_activate_control(app, connection, control);
                return;
            }
            if body_area.contains(position) {
                plan_body_down(app, mouse.column, mouse.row);
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            // 滚动条抓取中：拖动直接换算成 scroll_offset。
            if matches!(&app.modal, Some(Modal::Plan { state }) if state.scroll_grab.is_some()) {
                plan_scrollbar_drag(app, mouse.row);
                return;
            }
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                match state.tab {
                    PlanTab::Plan if state.dragging => {
                        // 拖选 = 起点块与当前块的源码区间并集。
                        let bottom = body_area.bottom().saturating_sub(1).max(body_area.y);
                        let row = usize::from(mouse.row.clamp(body_area.y, bottom))
                            .saturating_sub(usize::from(body_area.y));
                        let source = app
                            .plan_line_sources
                            .get(state.scroll_offset + row)
                            .copied()
                            .flatten();
                        if let Some((start, end)) = source
                            && let Some((press_start, press_end)) = state.drag_start
                        {
                            state.select_lines(press_start.min(start), press_end.max(end));
                        }
                    }
                    PlanTab::Source if state.dragging => {
                        let row_map = app.plan_row_map.clone();
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
                    _ => {}
                }
            }
        }
        MouseEventKind::Up(MouseButton::Left) => {
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                state.dragging = false;
                state.drag_start = None;
                state.scroll_grab = None;
                if state.tab == PlanTab::Source {
                    state.ensure_cursor_visible(usize::from(body_area.height).max(1));
                }
            }
        }
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
            if !body_area.contains(position) {
                return;
            }
            let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                -1
            } else {
                1
            };
            if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                match state.tab {
                    PlanTab::Plan | PlanTab::Diff => {
                        state.scroll_offset = state.scroll_offset.saturating_add_signed(delta);
                    }
                    PlanTab::Source => {
                        state.move_cursor(delta, false);
                        state.ensure_cursor_visible(usize::from(body_area.height).max(1));
                    }
                }
            }
        }
        _ => {}
    }
}

/// 计划标签正文上某行对应的渲染行 → 源码区间（不在正文区返回 `None`）。
fn rendered_row_at(app: &App, column: u16, row: u16) -> Option<Option<(usize, usize)>> {
    let body = app.plan_body_area;
    if !body.contains((column, row).into()) {
        return None;
    }
    let offset = match app.modal.as_ref() {
        Some(Modal::Plan { state }) => state.scroll_offset,
        _ => 0,
    };
    let index = offset + usize::from(row.saturating_sub(body.y));
    app.plan_line_sources.get(index).copied()
}

/// 计划标签正文按下：gutter 命中块 → 选中该块并直接开编辑器；正文命中块 →
/// 选中该块并开始拖选。
fn plan_body_down(app: &mut App, column: u16, row: u16) {
    let Some(Modal::Plan { state }) = app.modal.as_mut() else {
        return;
    };
    let body = app.plan_body_area;
    match state.tab {
        PlanTab::Plan => {
            if !state.can_annotate() {
                return;
            }
            let index = state.scroll_offset + usize::from(row.saturating_sub(body.y));
            let Some(Some((start, end))) = app.plan_line_sources.get(index).copied() else {
                return;
            };
            state.select_lines(start, end);
            if column < body.x + 2 {
                // gutter `+`/`▎`：直接以块区间打开编辑器（覆盖草稿时进编辑态）。
                state.open_editor();
            } else {
                state.dragging = true;
                state.drag_start = Some((start, end));
            }
        }
        PlanTab::Source => {
            if !state.can_annotate() {
                return;
            }
            let index = usize::from(row.saturating_sub(body.y));
            if let Some(index) = app.plan_row_map.get(index).copied() {
                state.cursor = index;
                state.anchor = Some(index);
                state.dragging = true;
            }
        }
        PlanTab::Diff => {}
    }
}

/// 计划正文滚动条几何：轨道矩形、滑块行数、正文总行数、scroll_offset 上限、
/// 滑块顶部可移动的行程。无滚动条（未溢出）返回 `None`。
fn plan_scrollbar_metrics(app: &App) -> Option<(Rect, usize, usize, usize, usize)> {
    let track = app.plan_scrollbar_area;
    if track.is_empty() {
        return None;
    }
    let total = app.plan_body_total;
    let height = usize::from(track.height);
    if total <= height || height == 0 {
        return None;
    }
    // 与渲染层同一公式：滑块 = 窗口占比，行程 = 轨道高 - 滑块。
    let thumb = (height * height / total).max(1);
    Some((track, thumb, total, total - height, height - thumb))
}

/// 把滑块顶部钉在给定轨道行并回写 scroll_offset；
/// 源码标签的滚动是光标窗口，顺带把光标钳进可见窗口。
fn plan_scrollbar_apply(app: &mut App, top: usize) {
    let Some((track, _thumb, total, max_offset, top_range)) = plan_scrollbar_metrics(app) else {
        return;
    };
    let top = top.min(top_range);
    let offset = top * max_offset / top_range.max(1);
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.scroll_offset = offset;
        if state.tab == PlanTab::Source {
            let window = usize::from(track.height);
            let last = (offset + window.saturating_sub(1)).min(total.saturating_sub(1));
            state.cursor = state.cursor.clamp(offset, last);
        }
    }
}

/// 按下滚动条：滑块内保持抓取点，轨道处把滑块中心挪到点击位置。
fn plan_scrollbar_down(app: &mut App, row: u16) {
    let Some((track, thumb, _total, max_offset, top_range)) = plan_scrollbar_metrics(app) else {
        return;
    };
    let row_rel = usize::from(row.saturating_sub(track.y));
    let thumb_top = match &app.modal {
        Some(Modal::Plan { state }) => {
            state.scroll_offset.min(max_offset) * top_range / max_offset.max(1)
        }
        _ => return,
    };
    let grab = if (thumb_top..thumb_top + thumb).contains(&row_rel) {
        row_rel - thumb_top
    } else {
        thumb / 2
    };
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.scroll_grab = Some(grab);
    }
    plan_scrollbar_apply(app, row_rel.saturating_sub(grab));
}

/// 拖动滚动条：滑块顶部跟随指针（扣除按下时的抓取行距）。
fn plan_scrollbar_drag(app: &mut App, row: u16) {
    let Some((track, _thumb, _total, _max_offset, _top_range)) = plan_scrollbar_metrics(app) else {
        return;
    };
    let grab = match &app.modal {
        Some(Modal::Plan { state }) => state.scroll_grab.unwrap_or(0),
        _ => return,
    };
    let row_rel = usize::from(row.saturating_sub(track.y));
    plan_scrollbar_apply(app, row_rel.saturating_sub(grab));
}

/// 切标签：清选区、回滚动顶、清旧错误。
fn plan_switch_tab(app: &mut App, tab: PlanTab) {
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        if tab == PlanTab::Source && !state.can_annotate() {
            state.error = Some(t!("tui.plan.not_annotatable").into_owned());
            return;
        }
        state.tab = tab;
        state.anchor = None;
        state.scroll_offset = 0;
        state.dragging = false;
        state.drag_start = None;
        state.scroll_grab = None;
        state.hover_block = None;
        state.error = None;
    }
}

/// 选区条按钮。
fn plan_selbar_action(app: &mut App, action: PlanSelAction) {
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        match action {
            PlanSelAction::Annotate => state.open_editor(),
            PlanSelAction::Delete => state.remove_draft_at_cursor(),
            // 计划标签的「选择块」由 anchor+cursor 共同表达，清 anchor 即无选区。
            PlanSelAction::Clear => state.anchor = None,
        }
    }
}

/// 编辑器按钮行。
fn plan_editor_action(app: &mut App, action: PlanEditorAction) {
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        match action {
            PlanEditorAction::Suggest => insert_suggestion_block(state),
            PlanEditorAction::Delete => {
                if let Some(editor) = state.editor.take()
                    && let Some(index) = editor.editing
                    && index < state.drafts.len()
                {
                    state.drafts.remove(index);
                }
            }
            PlanEditorAction::Cancel => state.editor = None,
            PlanEditorAction::Save => state.save_editor(),
        }
    }
}

/// 动作区控件激活：芯片开下层 UI，按钮各就各位。
fn plan_activate_control(app: &mut App, connection: &EmbeddedConnection, control: PlanControl) {
    if matches!(&app.modal, Some(Modal::Plan { state }) if state.submitting) {
        return;
    }
    match control {
        PlanControl::Model | PlanControl::NewModel => {
            park_plan_for_model_picker(app, control);
        }
        PlanControl::Engine => plan_open_engine_menu(app),
        PlanControl::Resume => app.set_modal(None),
        PlanControl::Submit => submit_plan_review(app, connection),
        PlanControl::Run | PlanControl::RunCompacted | PlanControl::RunNew => {
            plan_approve(app, connection, control);
        }
    }
}

/// 「新会话引擎」芯片的下拉：候选沿用状态栏引擎清单。
fn plan_open_engine_menu(app: &mut App) {
    let items: Vec<SessionEngine> = app.engines().collect();
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        let selected = items
            .iter()
            .position(|engine| *engine == state.execution_engine)
            .unwrap_or(0);
        state.engine_menu = Some(LegendMenu {
            selected,
            items,
            area: ratatui::layout::Rect::default(),
        });
    }
}

/// 模型芯片：停放计划审阅并打开模型选择器；选择器没开起来（如无模型）立即还原。
///
/// `control` 区分两个模型芯片：`Model` 改会话模型，`NewModel` 只写回停放的
/// 审阅状态 `execution_model`（新会话执行专用，不动当前会话）。
pub(super) fn park_plan_for_model_picker(app: &mut App, control: PlanControl) {
    // 记住芯片矩形：选择器锚到审阅卡上的芯片位置而不是输入框图例。
    let anchor = app
        .plan_control_hits
        .iter()
        .find(|(item, _)| *item == control)
        .map(|(_, area)| *area);
    let Some(Modal::Plan { state }) = app.take_modal() else {
        return;
    };
    app.parked_plan = Some(state);
    if control == PlanControl::NewModel {
        // 缺省沿用源会话当前模型；选过的话以已选值为当前项。
        let current = app
            .parked_plan
            .as_ref()
            .and_then(|state| state.execution_model.clone())
            .or_else(|| app.session.as_ref().map(|session| session.model.clone()))
            .unwrap_or_default();
        app.open_model_picker_for(current, &[], crate::modal::ModelTarget::PlanExecution);
    } else {
        app.open_model_picker();
    }
    if !matches!(app.modal, Some(Modal::Model { .. })) {
        restore_parked_plan(app);
    } else {
        app.model_picker_anchor = anchor;
    }
}

/// 选择器（含其后的切换确认卡等中间模态）全部关闭后，把停放的计划审阅还原；
/// 已离开计划模式（计划被执行 / 会话切换）则丢弃。
pub(crate) fn restore_parked_plan(app: &mut App) {
    if app.modal.is_some() || app.parked_plan.is_none() {
        return;
    }
    app.model_picker_anchor = None;
    if app
        .session
        .as_ref()
        .is_some_and(|session| session.plan_active)
    {
        let modal = app.parked_plan.take().map(|state| Modal::Plan { state });
        app.set_modal(modal);
    } else {
        app.parked_plan = None;
    }
}

/// 三个执行按钮统一走 `session/plan_approve`。
fn plan_approve(app: &mut App, connection: &EmbeddedConnection, control: PlanControl) {
    let action = match control {
        PlanControl::Run => moc_protocol::methods::PlanAction::Execute,
        PlanControl::RunCompacted => moc_protocol::methods::PlanAction::ExecuteCompacted,
        PlanControl::RunNew => moc_protocol::methods::PlanAction::ExecuteNewSession,
        _ => return,
    };
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let (plan_id, execution_engine, execution_model) = match app.modal.as_ref() {
        Some(Modal::Plan { state }) => (
            state.plan_id.clone(),
            state.execution_engine,
            // 只有新会话执行才带独立模型；其余动作始终用源会话当前模型。
            if control == PlanControl::RunNew {
                state.execution_model.clone()
            } else {
                None
            },
        ),
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
                execution_model,
            })
            .await
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::PlanApproval(result));
    });
}

/// 模态键盘路由。
///
/// `app.modal` 以 `as_mut` 借用；需要关闭模态或访问会话的分支通过
/// `close` 标志延后到借用结束后生效。
/// 计划审阅窗口的按键：编辑器 → 引擎下拉 → 标签视图逐级分派。
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
    // 引擎下拉打开时先给它；Passthrough 意味着菜单已自行关闭，按键照常处理。
    if matches!(&app.modal, Some(Modal::Plan { state }) if state.engine_menu.is_some()) {
        let action = match app.modal.as_mut() {
            Some(Modal::Plan { state }) => legend_menu_key(&mut state.engine_menu, modifiers, code),
            _ => LegendAction::Passthrough,
        };
        match action {
            LegendAction::Apply(engine) => {
                if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                    state.execution_engine = engine;
                }
                return;
            }
            LegendAction::Consumed => return,
            LegendAction::Passthrough => {}
        }
    }
    let tab = match &app.modal {
        Some(Modal::Plan { state }) => state.tab,
        _ => return,
    };
    match tab {
        PlanTab::Source => plan_annotate_key(app, connection, modifiers, code),
        PlanTab::Plan | PlanTab::Diff => plan_read_key(app, connection, modifiers, code),
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
            plan_switch_tab(app, PlanTab::Plan);
            return;
        }
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            let tab = match app.modal.as_ref() {
                Some(Modal::Plan { state }) if state.previous_plan_id.is_some() => PlanTab::Diff,
                _ => PlanTab::Plan,
            };
            plan_switch_tab(app, tab);
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

/// 计划/改动标签的按键：`d` 在改动与计划间切、`a` 进源码标签、`m`/`e` 开芯片
/// 下拉、`←→`/`tab` 在动作区焦点环移动、`Enter` 激活焦点项、`c` 批注选区。
pub(super) fn plan_read_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let submitted = matches!(
        &app.modal,
        Some(Modal::Plan { state }) if state.status == moc_protocol::methods::PlanStatus::Submitted
    );
    match (code, modifiers) {
        (KeyCode::Esc, _) => {
            // 有选区先清选区，再按一次才关闭（GUI 习惯）。
            let selected =
                matches!(&app.modal, Some(Modal::Plan { state }) if state.has_selection());
            if selected {
                if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                    state.anchor = None;
                }
            } else {
                app.set_modal(None);
            }
        }
        (KeyCode::Char('a'), KeyModifiers::NONE) => plan_toggle_annotate(app),
        (KeyCode::Char('d'), KeyModifiers::NONE) => {
            let tab = match app.modal.as_ref() {
                Some(Modal::Plan { state }) if state.previous_plan_id.is_some() => {
                    if state.tab == PlanTab::Diff {
                        PlanTab::Plan
                    } else {
                        PlanTab::Diff
                    }
                }
                _ => PlanTab::Plan,
            };
            plan_switch_tab(app, tab);
        }
        (KeyCode::Char('m'), KeyModifiers::NONE) if submitted => {
            park_plan_for_model_picker(app, PlanControl::Model);
        }
        (KeyCode::Char('n'), KeyModifiers::NONE) if submitted => {
            park_plan_for_model_picker(app, PlanControl::NewModel);
        }
        (KeyCode::Char('e'), KeyModifiers::NONE) if submitted => {
            plan_open_engine_menu(app);
        }
        (KeyCode::Char('s'), KeyModifiers::NONE) => submit_plan_review(app, connection),
        (KeyCode::Char('c'), KeyModifiers::NONE) => {
            // 有选区打开编辑器，否则切到源码标签逐行批注。
            let has_selection = matches!(
                &app.modal,
                Some(Modal::Plan { state }) if state.has_selection()
            );
            if has_selection {
                if let Some(Modal::Plan { state }) = app.modal.as_mut() {
                    state.open_editor();
                }
            } else {
                plan_toggle_annotate(app);
            }
        }
        (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => plan_scroll(app, -1),
        (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => plan_scroll(app, 1),
        (KeyCode::PageUp, _) => plan_scroll(app, -10),
        (KeyCode::PageDown, _) => plan_scroll(app, 10),
        (KeyCode::Left, _) | (KeyCode::Char('h'), KeyModifiers::NONE) => {
            plan_focus_move(app, -1);
        }
        (KeyCode::Right, _) | (KeyCode::Char('l'), KeyModifiers::NONE) => {
            plan_focus_move(app, 1);
        }
        (KeyCode::Tab, KeyModifiers::NONE) => plan_focus_move(app, 1),
        (KeyCode::BackTab, _) => plan_focus_move(app, -1),
        (KeyCode::Enter, KeyModifiers::NONE) if submitted => {
            let control = match app.modal.as_ref() {
                Some(Modal::Plan { state }) => state.focus,
                _ => return,
            };
            plan_activate_control(app, connection, control);
        }
        _ => {}
    }
}

/// 动作区焦点环移动一格。
fn plan_focus_move(app: &mut App, delta: isize) {
    if let Some(Modal::Plan { state }) = app.modal.as_mut() {
        state.focus_move(delta);
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
    state.tab = PlanTab::Source;
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
                let source_session_id = app.session.as_ref().map(|session| session.id.clone());
                let client = connection.client.clone();
                let events = app.events.clone();
                tokio::spawn(async move {
                    let loaded = load_display_session(&client, result.session_id, false)
                        .await
                        .map_err(anyhow::Error::from);
                    let _ = events.send(AppEvent::PlanExecutionLoaded {
                        result: loaded,
                        auto_prompt: result.auto_prompt,
                        source_session_id,
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
    source_session_id: Option<String>,
) {
    match result {
        Ok(loaded) => {
            if !app
                .session
                .as_ref()
                .is_some_and(|session| Some(session.id.as_str()) == source_session_id.as_deref())
            {
                return;
            }
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
            refresh_context(app, connection);
            refresh_usage(app, connection, false);
            load_subagents_for_parent(app, &connection.client, Some(id));
            start_auto_prompt(app, connection, auto_prompt);
            crate::refs::request_inspection(app, connection);
        }
        Err(error) => app
            .set_notice(t!("tui.plan.exec_load_failed_fmt", err = error.to_string()).into_owned()),
    }
}

//! 鼠标顶层路由与选择器命中。

use super::agents::focus_agent;
use super::agents::leave_focus;
use super::ask::handle_ask_mouse;
use super::commands::execute_command;
use super::completion::handle_completion_mouse;
use super::context::handle_context_content_mouse;
use super::context::open_context;
use super::keyboard::handle_key;
use super::keyboard::request_stop;
use super::legend::handle_legend_menu_mouse;
use super::modal_key::handle_modal_key;
use super::plan::handle_plan_mouse;
use super::rewind::handle_rewind_mouse;
use super::scrollback::handle_mouse;
use super::scrollback::handle_scrollbar_mouse;
use super::workflow::open_workflow_runs;
use super::workflow::take_next_approval;
use crate::app::App;
use crate::modal::Modal;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

/// 鼠标事件：上下文点击需要连接，其余交互保持无连接处理。
pub(super) fn handle_mouse_with_connection(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row);
    app.scrollbar_pointer = Some(position);
    // 已抓住滑块后，拖动与松开优先于 BTW、HUD 等区域，横向移出也不丢捕获。
    if matches!(app.modal, None | Some(Modal::Ask { .. }))
        && app.scrollbar_drag.is_some()
        && matches!(
            mouse.kind,
            MouseEventKind::Drag(MouseButton::Left) | MouseEventKind::Up(MouseButton::Left)
        )
        && handle_scrollbar_mouse(app, &mouse)
    {
        return;
    }
    let left_down = matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left));
    if app.modal.is_some() {
        // 提示条上的键位（footer、回退 dock、提问细条）：点中 = 按下该键。
        if left_down && let Some(hit) = key_hit_at(app, position) {
            handle_key(app, connection, hit.modifiers, hit.code);
            return;
        }
        // 浏览/选择类模态点卡片外即关闭（与 Esc 同义）；授权、提问、计划审阅、
        // 切换确认这类需要明确答复的卡片不因误点背景而消失。
        // 模型选择器的「外」是列表卡、详情卡、下拉卡三块浮层之外。
        let pos: ratatui::layout::Position = position.into();
        let outside = !app.modal_card_area.is_empty()
            && !app.modal_card_area.contains(pos)
            && !app.modal_detail_area.contains(pos)
            && !app.modal_menu_area.contains(pos);
        if left_down && outside && matches!(app.modal, Some(Modal::Model { .. })) {
            // popover 语义：一次点击即关——先收起下拉与详情焦点，再走列表层 Esc
            // （关闭；WorkflowRole 目标回到 workflow 窗）。
            if let Some(Modal::Model { detail, .. }) = app.modal.as_mut() {
                detail.menu = None;
                detail.focus = None;
            }
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Esc);
            // 点在触发标签上只收起（否则会立刻又打开）；Workflow 回程仍有模态
            // 时同样吞掉这次点击。
            if app.model_tag_area.contains(pos) || app.modal.is_some() || app.parked_plan.is_some()
            {
                return;
            }
            // 已彻底关闭：把这次点击当作无模态时的普通点击重新派发。
            handle_mouse_with_connection(app, connection, mouse);
            return;
        } else if left_down && outside && app.modal.as_ref().is_some_and(modal_dismissable) {
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Esc);
            return;
        }
        if matches!(app.modal, Some(Modal::ContextContent { .. })) {
            handle_context_content_mouse(app, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Rewind { .. })) {
            handle_rewind_mouse(app, connection, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Ask { .. })) {
            handle_ask_mouse(app, connection, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Plan { .. })) {
            handle_plan_mouse(app, connection, mouse);
            return;
        }
        if matches!(app.modal, Some(Modal::Model { .. })) {
            handle_model_picker_mouse(app, connection, mouse);
            return;
        }
        // 列表/面板类模态只记鼠标坐标：悬停高亮由渲染层按最终命中矩形计算，
        // 键盘导航时清空（handle_modal_key 开头），不与选中态抢视觉。
        {
            let pos: ratatui::layout::Position = position.into();
            match app.modal.as_mut() {
                Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
                    picker.pointer = Some(pos)
                }
                Some(Modal::Jobs { pointer, .. }) => *pointer = Some(pos),
                Some(Modal::WorkflowApproval { state }) => state.pointer = Some(pos),
                Some(Modal::WorkflowRun { state }) => state.pointer = Some(pos),
                _ => {}
            }
        }
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                // 命中矩形由渲染层按卡片正文行号每帧写入。
                if let Some((index, _)) = app
                    .modal_item_hits
                    .iter()
                    .find(|(_, area)| area.contains(position.into()))
                    .copied()
                {
                    click_modal_item(app, connection, index);
                }
            }
            MouseEventKind::Moved => {
                if let Some((index, _)) = app
                    .modal_item_hits
                    .iter()
                    .find(|(_, area)| area.contains(position.into()))
                    .copied()
                {
                    hover_modal_item(app, index);
                }
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                let up = matches!(mouse.kind, MouseEventKind::ScrollUp);
                let delta: isize = if up { -3 } else { 3 };
                // 列表/面板类模态：滚轮只滚 viewport，不移动选中项，也不滚背后的转写。
                let scrolled = match app.modal.as_mut() {
                    Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
                        picker.scroll.scroll_by(&mut picker.offset, delta);
                        true
                    }
                    Some(Modal::Jobs { offset, scroll, .. }) => {
                        scroll.scroll_by(offset, delta);
                        true
                    }
                    Some(Modal::WorkflowApproval { state }) => {
                        state.scroll.scroll_by(&mut state.body_scroll, delta);
                        true
                    }
                    Some(Modal::WorkflowRun { state }) => {
                        state.scroll.scroll_by(&mut state.body_scroll, delta);
                        true
                    }
                    // 占用窗与监控没有可滚内容，也不该滚背后的转写。
                    Some(Modal::Monitor) => true,
                    _ => false,
                };
                if !scrolled {
                    // 切换确认是二选一菜单：保留滚轮 = 换项的菜单语义。
                    if matches!(app.modal, Some(Modal::SwitchModel { .. })) {
                        handle_modal_key(
                            app,
                            connection,
                            KeyModifiers::NONE,
                            if up { KeyCode::Up } else { KeyCode::Down },
                        );
                    } else {
                        // 授权预览与帮助：滚各自的正文。
                        handle_mouse(app, mouse);
                    }
                }
            }
            _ => {}
        }
        return;
    }
    if crate::btw::handle_mouse(app, mouse) {
        return;
    }
    if handle_legend_menu_mouse(app, connection, &mouse) {
        return;
    }
    // 补全菜单等浮层提示条上的键位。
    if left_down && let Some(hit) = key_hit_at(app, position) {
        handle_key(app, connection, hit.modifiers, hit.code);
        return;
    }
    if handle_completion_mouse(app, &mouse) {
        return;
    }
    if app.queue_area.contains((mouse.column, mouse.row).into()) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                app.queue_offset = app.queue_offset.saturating_sub(1);
                return;
            }
            MouseEventKind::ScrollDown => {
                app.queue_offset = app.queue_offset.saturating_add(1);
                return;
            }
            _ => {}
        }
    }
    if app.subagent_area.contains((mouse.column, mouse.row).into()) {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left)
                if app
                    .subagent_close_area
                    .contains((mouse.column, mouse.row).into()) =>
            {
                app.subagent_sidebar = Some(false);
                return;
            }
            MouseEventKind::ScrollUp => {
                app.subagent_scroll = app.subagent_scroll.saturating_sub(1);
                return;
            }
            MouseEventKind::ScrollDown => {
                app.subagent_scroll = app.subagent_scroll.saturating_add(1);
                return;
            }
            _ => {}
        }
    }
    if handle_todo_hud_mouse(app, &mouse) {
        return;
    }
    if matches!(mouse.kind, MouseEventKind::Down(MouseButton::Left)) {
        let position = (mouse.column, mouse.row);
        if let Some((agent_id, _)) = app
            .subagent_hits
            .iter()
            .find(|(_, area)| area.contains(position.into()))
        {
            let agent_id = agent_id.clone();
            focus_agent(app, connection, agent_id);
            return;
        }
        if app.modal.is_none() && app.engine_area.contains(position.into()) {
            app.open_engine_menu();
            return;
        }
        if app.modal.is_none() && app.model_tag_area.contains(position.into()) {
            app.open_model_picker();
            return;
        }
        if app.modal.is_none() && app.permission_tag_area.contains(position.into()) {
            app.open_permission_menu();
            return;
        }
        // workflow 胶囊：最小化审批优先重开，其次弹出排队审批，否则打开 run 列表。
        if app.modal.is_none() && app.workflow_capsule_area.contains(position.into()) {
            if let Some(state) = take_next_approval(app) {
                app.modal = Some(Modal::WorkflowApproval { state });
            } else {
                open_workflow_runs(app, connection);
            }
            return;
        }
        // 输入框图例左端：运行中是「停止」，空闲是「快捷键」，点中与对应按键同义。
        if app.legend_tip_area.contains(position.into()) {
            if app.prompt_running {
                request_stop(app, connection);
            } else {
                app.modal = Some(Modal::Help { scroll: 0 });
            }
            return;
        }
        if app.focused.is_some() && app.header_back_area.contains(position.into()) {
            leave_focus(app, connection);
            return;
        }
        if let Some((id, _, cancel)) = app
            .queue_hits
            .iter()
            .find(|(_, row, cancel)| {
                row.contains(position.into()) || cancel.contains(position.into())
            })
            .cloned()
        {
            if cancel.contains(position.into()) {
                crate::queue::remove(app, connection, id);
            } else {
                crate::queue::edit(app, connection, id);
            }
            return;
        }
        let context = app.context_area;
        if position.0 >= context.x
            && position.0 < context.x.saturating_add(context.width)
            && position.1 >= context.y
            && position.1 < context.y.saturating_add(context.height)
        {
            open_context(app, connection);
            return;
        }
        // statusline 右侧用量统计段：点击与输入 /usage 同义。
        if app.usage_area.contains(position.into()) {
            execute_command(app, connection, "/usage".to_owned());
            return;
        }
    }
    handle_mouse(app, mouse);
}

/// 坐标命中的提示条键位（上一帧绘制登记）。
pub(super) fn key_hit_at(app: &App, position: (u16, u16)) -> Option<crate::render::labels::KeyHit> {
    app.key_hits
        .iter()
        .copied()
        .find(|hit| hit.area.contains(position.into()))
}

/// 点卡片外可关闭的模态：只浏览或选择、关闭不丢答复的那些。
pub(super) fn modal_dismissable(modal: &Modal) -> bool {
    matches!(
        modal,
        Modal::Help { .. }
            | Modal::Tools { .. }
            | Modal::ContextContent { .. }
            | Modal::Model { .. }
            | Modal::Sessions { .. }
            | Modal::Jobs { .. }
            | Modal::Rewind { .. }
            | Modal::Monitor
    )
}

/// 点中通用模态的第 `index` 项。
///
/// 按钮式选项（授权、切换确认）点中即执行；候选列表（会话、子代理）先选中、
/// 再点已选中的一项才确认。模型选择器有独立的三浮层处理，不走这里。
pub(super) fn click_modal_item(app: &mut App, connection: &EmbeddedConnection, index: usize) {
    let mut job_kill: Option<String> = None;
    let confirm = match app.modal.as_mut() {
        Some(Modal::Permission { selected, .. }) => {
            *selected = index;
            true
        }
        Some(Modal::SwitchModel { option, .. }) => {
            *option = index;
            true
        }
        Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
            let again = picker.selected == index;
            picker.selected = index;
            again
        }
        // workflow 审批窗：单击选中 role/ref 行，点已选行 = 打开模型选择器。
        Some(Modal::WorkflowApproval { state }) => {
            let again = state.focus == index;
            state.focus = index;
            again
        }
        // workflow run 面板：同上（点已选 role 行开选择器改模型）。
        Some(Modal::WorkflowRun { state }) => {
            let again = state.focus == index;
            state.focus = index;
            again
        }
        // /jobs：点行选中；点已选中行与连按两次 x 同义——第二次点击本身就是确认。
        Some(Modal::Jobs {
            jobs,
            selected,
            pending_kill,
            ..
        }) => {
            let again = *selected == index;
            *selected = index;
            *pending_kill = None;
            if again {
                job_kill = jobs.get(index).map(|job| job.job_id.clone());
            }
            false
        }
        _ => false,
    };
    if let Some(job_id) = job_kill {
        if let Some(session_id) = app.session.as_ref().map(|s| s.id.clone()) {
            let client = connection.client.clone();
            tokio::spawn(async move {
                let _ = client
                    .call::<moc_protocol::methods::JobKill>(moc_protocol::methods::JobTarget {
                        session_id,
                        job_id,
                    })
                    .await;
            });
        }
        // 与键盘路径一致：杀任务后关闭弹窗。
        app.modal = None;
        return;
    }
    if confirm {
        handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
    }
}

/// 鼠标悬浮移到按钮式选项（授权、切换确认）：与 ↑↓ 换行同语义，只改高亮不确认。
/// 列表类模态的悬停不写选中态——pointer 已记录，渲染层只铺悬停底色。
pub(super) fn hover_modal_item(app: &mut App, index: usize) {
    match app.modal.as_mut() {
        Some(Modal::Permission { selected, .. }) => *selected = index,
        Some(Modal::SwitchModel { option, .. }) => *option = index,
        _ => {}
    }
}

/// 模型选择器：滚轮只滚动光标所在列表；移动鼠标时焦点随指针切换而不改视口。
/// 列表行点中确认，详情行和下拉候选点中即执行。
pub(super) fn handle_model_picker_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: crossterm::event::MouseEvent,
) {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position: ratatui::layout::Position = (mouse.column, mouse.row).into();
    let Some(Modal::Model {
        entries,
        picker,
        level,
        detail,
        ..
    }) = app.modal.as_mut()
    else {
        return;
    };
    detail.pointer = Some(position);
    if matches!(
        mouse.kind,
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
    ) {
        let delta = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
            -3
        } else {
            3
        };
        if app.modal_menu_area.contains(position) {
            if let Some(menu) = detail.menu.as_mut() {
                menu.scroll.scroll_by(&mut menu.offset, delta);
            }
        } else if !app.modal_detail_area.contains(position)
            && app.modal_card_area.contains(position)
        {
            detail.list_scroll.scroll_by(&mut picker.offset, delta);
        }
        return;
    }
    if mouse.kind == MouseEventKind::Moved {
        if let Some(menu) = detail.menu.as_mut() {
            if let Some((index, _)) = app
                .modal_menu_hits
                .iter()
                .find(|(_, area)| area.contains(position))
            {
                menu.selected = *index;
            }
            return;
        }
        if let Some((row, _)) = app
            .modal_detail_hits
            .iter()
            .find(|(_, area)| area.contains(position))
        {
            detail.focus = Some(*row);
            return;
        }
        if app.modal_detail_area.contains(position) || app.modal_menu_area.contains(position) {
            return;
        }
        if let Some((index, _)) = app
            .modal_item_hits
            .iter()
            .find(|(_, area)| area.contains(position))
        {
            if picker.selected != *index {
                let filtered = crate::modal::filtered_entries(entries, &picker.query);
                level.change_entry(
                    filtered.get(picker.selected).map(|(_, entry)| *entry),
                    filtered.get(*index).map(|(_, entry)| *entry),
                );
                picker.selected = *index;
            }
            detail.focus = None;
        }
        return;
    }
    if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if let Some(menu) = detail.menu.as_mut() {
        if let Some((index, _)) = app
            .modal_menu_hits
            .iter()
            .find(|(_, area)| area.contains(position))
        {
            menu.selected = *index;
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
        } else {
            // 点下拉以外：收起下拉，再执行同一次点击。
            detail.menu = None;
            handle_model_picker_mouse(app, connection, mouse);
        }
        return;
    }
    if let Some((row, _)) = app
        .modal_detail_hits
        .iter()
        .find(|(_, area)| area.contains(position))
    {
        detail.focus = Some(*row);
        handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
    } else if app.modal_detail_area.contains(position) || app.modal_menu_area.contains(position) {
        // 详情卡/下拉卡内非控制行不穿透到候选列表。
    } else if let Some((index, _)) = app
        .modal_item_hits
        .iter()
        .find(|(_, area)| area.contains(position))
    {
        let again = picker.selected == *index;
        if !again {
            let filtered = crate::modal::filtered_entries(entries, &picker.query);
            level.change_entry(
                filtered.get(picker.selected).map(|(_, entry)| *entry),
                filtered.get(*index).map(|(_, entry)| *entry),
            );
            picker.selected = *index;
        }
        detail.focus = None;
        if again {
            handle_modal_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
        }
    }
}

/// todo HUD：单击在展开卡片与折叠胶囊间切换、按住卡片拖动。
pub(super) fn handle_todo_hud_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    let position = (mouse.column, mouse.row);
    match mouse.kind {
        MouseEventKind::Moved => {
            let inside = app.todo_hud.pointer_moved(position);
            if inside {
                // 卡片盖住图表入口时，别留下过期的行高亮。
                app.hover_chart = None;
            }
            inside
        }
        MouseEventKind::Down(MouseButton::Left) => {
            if app.todo_hud.pointer_down(position) {
                // 点击 HUD 不落到转写：不进入点击/拖选状态。
                app.pending_click = None;
                app.selection = None;
                true
            } else {
                false
            }
        }
        MouseEventKind::Drag(MouseButton::Left) => {
            app.todo_hud.pointer_drag(position, app.scrollback_area)
        }
        MouseEventKind::Up(MouseButton::Left) => app.todo_hud.pointer_up(position),
        _ => false,
    }
}

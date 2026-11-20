//! 终端键盘路由与输入提交。

use rust_i18n::t;

use super::agents::leave_focus;
use super::agents::open_subagents;
use super::agents::stop_agent_operation;
use super::clipboard::copy_selection;
use super::clipboard::spawn_clipboard_paste;
use super::commands::cycle_engine;
use super::commands::execute_command;
use super::commands::open_session_picker;
use super::completion::accept_completion;
use super::completion::handle_completion_key;
use super::completion::request_resource_completion;
use super::context::flush_context_loads;
use super::legend::handle_legend_menu_key;
use super::modal_key::handle_modal_key;
use super::model::cycle_thinking_level;
use super::mouse::handle_mouse_with_connection;
use super::rewind::open_rewind;
use super::scrollback::scroll;
use crate::app::App;
use crate::app::CtrlCPending;
use crate::app::Focus;
use crate::modal::Modal;
use crate::protocol::EmbeddedConnection;
use crate::widgets::prompt::clamp_cursor;
use crossterm::event::Event;
use crossterm::event::KeyCode;
use crossterm::event::KeyEventKind;
use crossterm::event::KeyModifiers;
use moc_protocol::ClientHandle;

/// 终端事件分派（键盘、鼠标、粘贴与窗口变化）。
pub(super) fn handle_terminal_event(app: &mut App, connection: &EmbeddedConnection, event: Event) {
    // 模态实际绘制前吞掉键盘、鼠标和粘贴；走过一帧后才放行普通 Esc，保证小终端也能退出。
    if app.modal.is_some()
        && !app.modal_drawn
        && matches!(event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_))
        && !matches!(event, Event::Key(key)
            if app.modal_frame_seen
                && key.kind == KeyEventKind::Press
                && key.code == KeyCode::Esc
                && key.modifiers.is_empty())
    {
        return;
    }
    if matches!(event, Event::FocusLost | Event::Resize(_, _)) {
        app.scrollbar_pointer = None;
        app.scrollbar_drag = None;
    }
    let previous_context = match &app.modal {
        Some(Modal::ContextContent { state }) => state
            .content
            .as_ref()
            .map(|c| (state.request, state.session_id.clone(), c.view_id.clone())),
        _ => None,
    };
    if app.settings.is_some() && app.modal.is_none() {
        match event {
            Event::Key(key) if key.kind == KeyEventKind::Press => {
                crate::settings::handle_key(app, connection, key.modifiers, key.code)
            }
            Event::Mouse(mouse) => crate::settings::mouse(app, connection, mouse),
            Event::Paste(text) => crate::settings::paste(app, &text),
            Event::FocusGained => app.terminal_focused = true,
            Event::FocusLost => app.terminal_focused = false,
            _ => {}
        }
        return;
    }
    match event {
        Event::Key(key) if key.kind == KeyEventKind::Press => {
            let previous_revision = app.prompt_revision;
            handle_key(app, connection, key.modifiers, key.code);
            if app.focus == Focus::Prompt && app.prompt_revision != previous_revision {
                request_resource_completion(app, connection);
            }
        }
        Event::Mouse(mouse) => handle_mouse_with_connection(app, connection, mouse),
        Event::Paste(text) if text.is_empty() => spawn_clipboard_paste(app.events.clone()),
        Event::Paste(text) => {
            // 提问的自定义答案正在编辑时，粘贴进那一行而不是主输入框——
            // 落到背后的输入框里，提交前用户根本看不见。
            let editing_ask = matches!(
                app.modal.as_mut(),
                Some(Modal::Ask {
                    custom,
                    question,
                    parked: false,
                    ..
                }) if custom
                    .get(*question)
                    .is_some_and(|answer| answer.editing)
            );
            if editing_ask {
                let Some(Modal::Ask {
                    custom, question, ..
                }) = app.modal.as_mut()
                else {
                    unreachable!("上面刚判过提问模态");
                };
                if let Some(answer) = custom.get_mut(*question) {
                    let pos = clamp_cursor(&answer.text, answer.cursor);
                    answer.text.insert_str(pos, &text);
                    answer.cursor = pos + text.len();
                }
            } else {
                app.prompt.insert_text(&text);
                app.prompt_changed();
                request_resource_completion(app, connection);
            }
        }
        Event::Resize(_, _) => {
            app.selection = None;
            app.pending_click = None;
            app.selection_copied_at = None;
            app.todo_hud.on_resize();
        }
        Event::FocusGained => app.terminal_focused = true,
        Event::FocusLost => {
            app.terminal_focused = false;
            app.todo_hud.on_focus_lost();
        }
        _ => {}
    }
    flush_context_loads(app, connection);
    if let Some((request, session_id, view_id)) = previous_context
        && !matches!(&app.modal,Some(Modal::ContextContent{state}) if state.request==request)
    {
        let client = connection.client.clone();
        tokio::spawn(async move {
            let _ = client
                .call::<moc_protocol::methods::SessionContextClose>(
                    moc_protocol::methods::SessionContextCloseParams {
                        session_id,
                        view_id,
                    },
                )
                .await;
        });
    }
}

/// 键盘处理：模态优先，其次全局开关，最后按焦点路由。
pub(super) fn handle_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    if !matches!(
        (code, modifiers),
        (KeyCode::Char('c'), KeyModifiers::CONTROL)
    ) {
        app.ctrl_c_pending = None;
    }
    if !matches!(code, KeyCode::Esc) {
        app.clear_pending = false;
    }
    if app.modal.is_some() {
        handle_modal_key(app, connection, modifiers, code);
        return;
    }
    if crate::btw::handle_key(app, modifiers, code) {
        return;
    }
    if handle_legend_menu_key(app, connection, modifiers, code) {
        return;
    }
    if code == KeyCode::Esc && app.selection.is_some() {
        app.selection = None;
        app.pending_click = None;
        return;
    }
    if matches!(code, KeyCode::Esc) && app.completion.is_some() {
        app.completion = None;
        return;
    }
    if handle_completion_key(app, modifiers, code) {
        return;
    }
    let translated = app.keymap.action(modifiers, code);
    match translated {
        Some("config") => {
            crate::settings::open(app, connection);
            return;
        }
        Some("agents") => {
            open_subagents(app, connection);
            return;
        }
        Some("sidebar") => {
            if app.subagents.is_empty() {
                app.set_notice(t!("tui.event.no_subagents").into_owned());
            } else {
                let visible = app.subagent_sidebar_visible();
                app.subagent_sidebar = Some(!visible);
            }
            return;
        }
        Some("engine") => {
            cycle_engine(app, connection);
            return;
        }
        Some("queue") => {
            crate::queue::submit(app, connection, true);
            return;
        }
        Some("queue_edit") => {
            let Some(session) = app.session.as_ref().map(|session| session.id.clone()) else {
                return;
            };
            let steer_id = app.steers.get(&session).and_then(|entries| {
                entries
                    .iter()
                    .rev()
                    .find(|entry| entry.withdrawal.is_none())
                    .map(|entry| entry.id)
            });
            let queue_id = app
                .queues
                .get(&session)
                .and_then(|queue| queue.front())
                .map(|entry| entry.id);
            if let Some(id) = steer_id.or(queue_id) {
                crate::queue::edit(app, connection, id);
            }
            return;
        }
        _ => {}
    }
    let (modifiers, code) = if let Some(key) = app.keymap.translated(modifiers, code) {
        key
    } else if app.keymap.reserved(modifiers, code) {
        return;
    } else {
        (modifiers, code)
    };
    // 原生终端语义：有选区时 Ctrl+C 先复制，没有选区才走取消/清空输入。
    if app.selection.is_some()
        && matches!(code, KeyCode::Char('c' | 'C'))
        && (modifiers == KeyModifiers::CONTROL
            || modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT))
        && copy_selection(app)
    {
        app.ctrl_c_pending = None;
        return;
    }
    if app.focus == Focus::Prompt
        && (matches!(
            code,
            KeyCode::Up
                | KeyCode::Down
                | KeyCode::Left
                | KeyCode::Right
                | KeyCode::Home
                | KeyCode::End
        ) || matches!(
            (code, modifiers),
            (KeyCode::Char('a' | 'e'), KeyModifiers::CONTROL)
                | (KeyCode::Char('b' | 'f'), KeyModifiers::ALT)
        ))
    {
        app.selection = None;
        app.pending_click = None;
        app.selection_copied_at = None;
    }
    if app.focus == Focus::Scrollback {
        match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                scroll(app, 3);
                return;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                scroll(app, -3);
                return;
            }
            (KeyCode::PageUp, _) | (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                scroll(app, 10);
                return;
            }
            (KeyCode::PageDown, _) | (KeyCode::Char('d'), KeyModifiers::CONTROL) => {
                scroll(app, -10);
                return;
            }
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                app.scroll_offset = usize::MAX;
                return;
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                app.scroll_offset = 0;
                return;
            }
            _ => {}
        }
    }
    match (code, modifiers) {
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
            if app.prompt_running {
                if app.take_ctrl_c_pending(CtrlCPending::Quit) {
                    app.should_quit = true;
                } else {
                    app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
                    app.cancel_requested = true;
                    app.set_notice(t!("tui.event.stopping_quit").into_owned());
                    cancel_prompt(&connection.client, app);
                }
            } else if !app.prompt_text().is_empty() {
                if app.take_ctrl_c_pending(CtrlCPending::ClearPrompt) {
                    app.prompt.clear();
                    app.prompt_changed();
                    app.notice = None;
                    app.notice_at = None;
                } else {
                    app.ctrl_c_pending =
                        Some((CtrlCPending::ClearPrompt, std::time::Instant::now()));
                    app.set_notice(t!("tui.event.ctrl_c_clear").into_owned());
                }
            } else if app.take_ctrl_c_pending(CtrlCPending::Quit) {
                app.should_quit = true;
            } else {
                app.ctrl_c_pending = Some((CtrlCPending::Quit, std::time::Instant::now()));
                app.set_notice(t!("tui.event.ctrl_c_quit").into_owned());
            }
        }
        (KeyCode::Char('h'), KeyModifiers::CONTROL) => {
            app.set_modal(Some(Modal::Help { scroll: 0 }))
        }
        (KeyCode::Char('t'), KeyModifiers::CONTROL) => {
            app.cycle_thinking_detail();
            app.set_notice(
                t!(
                    "tui.event.thinking_display_fmt",
                    label = app.thinking_detail.label().as_ref()
                )
                .into_owned(),
            );
        }
        (KeyCode::Char('o'), KeyModifiers::CONTROL) => {
            app.cycle_tool_detail();
            app.set_notice(
                t!(
                    "tui.event.tool_display_fmt",
                    label = app.tool_detail.label().as_ref()
                )
                .into_owned(),
            );
        }
        (KeyCode::Char('v'), KeyModifiers::CONTROL) => spawn_clipboard_paste(app.events.clone()),
        (KeyCode::Char('p'), KeyModifiers::CONTROL) => app.open_model_picker(),
        (KeyCode::Char('r'), KeyModifiers::CONTROL) => open_session_picker(app, connection),
        (KeyCode::Char('s'), KeyModifiers::CONTROL) if app.focused.is_some() => {
            // 聚焦时 ctrl+s 停止的就是眼前这个子代理。
            let agent_id = app.session.as_ref().map(|session| session.id.clone());
            if let Some(agent_id) = agent_id {
                stop_agent_operation(app, connection, agent_id);
            }
        }
        (KeyCode::Left, _)
            if app.focused.is_some()
                && app.focus == Focus::Prompt
                && app.prompt_text().is_empty() =>
        {
            // 双击 `←` 返回主会话（omp Hub 的手势）；单次按键不吞掉。
            if app.take_left_tap_pending(std::time::Instant::now()) {
                leave_focus(app, connection);
            } else {
                app.left_tap_pending = Some(std::time::Instant::now());
                app.prompt.left();
            }
        }
        (KeyCode::Char('y'), KeyModifiers::NONE) if app.selection.is_some() => {
            copy_selection(app);
        }
        (KeyCode::Esc, _) => {
            if app.focused.is_some() && app.focus == Focus::Prompt && app.prompt_text().is_empty() {
                // 聚焦子代理时的 `Esc` 只返回主会话，不打断子代理（omp Hub 的语义）；
                // 取消仍在运行的那个提示交给 `Ctrl+C`。
                leave_focus(app, connection);
            } else if app.prompt_running {
                request_stop(app, connection);
            } else if app.focus == Focus::Prompt && !app.prompt_text().is_empty() {
                if app.clear_pending {
                    app.prompt.clear();
                    app.prompt_changed();
                    app.clear_pending = false;
                } else {
                    app.clear_pending = true;
                    app.set_notice(t!("tui.event.esc_clear").into_owned());
                }
            } else if app.focus == Focus::Prompt && app.clear_pending {
                app.clear_pending = false;
                open_rewind(app, connection);
            } else if app.focus == Focus::Prompt {
                app.clear_pending = true;
                app.set_notice(t!("tui.event.esc_rewind").into_owned());
            }
        }
        (KeyCode::Tab, KeyModifiers::SHIFT) | (KeyCode::BackTab, _) => {
            cycle_thinking_level(app, connection)
        }
        (KeyCode::Tab, _) => {
            app.focus = if app.focus == Focus::Prompt && app.btw.expanded {
                app.selection = None;
                Focus::Btw
            } else {
                app.focus.toggle()
            };
        }
        (KeyCode::Enter, KeyModifiers::ALT | KeyModifiers::SHIFT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.insert_char('\n');
            app.prompt_changed();
        }
        (KeyCode::Enter, KeyModifiers::NONE) if app.focus == crate::app::Focus::Prompt => {
            commit_prompt(app, connection)
        }
        (KeyCode::Up, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.up();
        }
        (KeyCode::Down, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.down();
        }
        (KeyCode::Backspace, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.backspace();
            app.prompt_changed();
        }
        (KeyCode::Delete, _) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.delete();
            app.prompt_changed();
        }
        (KeyCode::Left, KeyModifiers::CONTROL)
        | (KeyCode::Left, KeyModifiers::ALT)
        | (KeyCode::Char('b'), KeyModifiers::ALT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_word_back();
        }
        (KeyCode::Right, KeyModifiers::CONTROL)
        | (KeyCode::Right, KeyModifiers::ALT)
        | (KeyCode::Char('f'), KeyModifiers::ALT)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_word_forward();
        }
        (KeyCode::Left, _) if app.focus == crate::app::Focus::Prompt => app.prompt.left(),
        (KeyCode::Right, _) if app.focus == crate::app::Focus::Prompt => app.prompt.right(),
        (KeyCode::Home, _) | (KeyCode::Char('a'), KeyModifiers::CONTROL)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_line_start()
        }
        (KeyCode::End, _) | (KeyCode::Char('e'), KeyModifiers::CONTROL)
            if app.focus == crate::app::Focus::Prompt =>
        {
            app.prompt.move_line_end()
        }
        (KeyCode::Char('k'), KeyModifiers::CONTROL) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.clear_line_to_end();
            app.prompt_changed();
        }
        (KeyCode::Char('u'), KeyModifiers::CONTROL) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.clear();
            app.prompt_changed();
        }
        (KeyCode::Char('w'), KeyModifiers::CONTROL) if app.focus == crate::app::Focus::Prompt => {
            app.prompt.delete_word_back();
            app.prompt_changed();
        }
        (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
            if app.focus == crate::app::Focus::Prompt && !ch.is_control() =>
        {
            app.prompt_insert_char(ch);
        }
        _ => {}
    }
}

/// 提交输入：补全确认 > 斜杠命令 > Bash 命令或普通发送。
pub(super) fn commit_prompt(app: &mut App, connection: &EmbeddedConnection) {
    if app.skills_compacting {
        app.set_notice(t!("tui.event.skills_compacting").into_owned());
        return;
    }
    if accept_completion(app, true) {
        return;
    }
    if crate::btw::submit(app, connection) {
        return;
    }
    let name = app
        .prompt_text()
        .split_whitespace()
        .next()
        .unwrap_or_default();
    // 只有已注册的命令才分流；绝对路径和其他斜杠文本按普通消息发送。
    if app.prompt_text().starts_with('/') && app.commands.resolve(name).is_some() {
        if app.prompt.has_attached_images() {
            app.set_notice(t!("tui.event.slash_no_images").into_owned());
            return;
        }
        let (line, history) = app.prompt.take_committed_text();
        if let Some(text) = history {
            app.record_input_history(&text);
        }
        execute_command(app, connection, line.trim().to_owned());
        return;
    }
    // `.` 独占整条输入时是 `/retry` 的快捷写法：走同一分派，忙时门控与提示完全复用。
    if app.prompt_text().trim() == "." {
        let (_, history) = app.prompt.take_committed_text();
        if let Some(text) = history {
            app.record_input_history(&text);
        }
        execute_command(app, connection, "/retry".to_owned());
        return;
    }
    if app.prompt.is_empty() {
        return;
    }
    crate::queue::submit(app, connection, false);
}

/// 请求停止当前提示（`Esc` 与图例「停止」共用）；已在停止中时不重复发送。
pub(super) fn request_stop(app: &mut App, connection: &EmbeddedConnection) {
    if !app.cancel_requested {
        app.cancel_requested = true;
        if app.skills_compacting {
            app.replace_skills_note(
                t!("tui.event.skills_cancel_wait").into_owned(),
                crate::transcript::NoteTone::Info,
            );
        }
        app.set_notice(t!("tui.event.stopping").into_owned());
        cancel_prompt(&connection.client, app);
    }
}

/// 发出取消；core 会终止本次提示并给未执行的工具调用补 Cancelled 结果。
pub(crate) fn cancel_prompt(client: &ClientHandle, app: &App) {
    if app.skills_compacting {
        let client = client.clone();
        tokio::spawn(async move {
            let _ = client
                .call::<moc_protocol::SkillsCancel>(moc_protocol::EmptyParams {})
                .await;
        });
        return;
    }
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let client = client.clone();
    tokio::spawn(async move {
        let _ = client
            .call::<moc_protocol::methods::SessionCancel>(
                moc_protocol::methods::SessionCancelParams { session_id },
            )
            .await;
    });
}

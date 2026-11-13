//! 只读面板与后台任务的键盘交互。

use super::Actions;
use crate::app::App;
use crate::event::HELP_PAGE;
use crate::modal::Modal;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

pub(super) fn handle(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
    actions: &mut Actions,
) -> bool {
    let Some(modal) = app.modal.as_mut() else {
        return false;
    };
    match modal {
        Modal::Help { scroll } | Modal::Tools { scroll, .. } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                *scroll = scroll.saturating_sub(1);
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                *scroll = scroll.saturating_add(1);
            }
            (KeyCode::PageUp, _) => *scroll = scroll.saturating_sub(HELP_PAGE),
            (KeyCode::PageDown, _) => *scroll = scroll.saturating_add(HELP_PAGE),
            (KeyCode::Home, _) => *scroll = 0,
            // 上界由渲染按可见行数收紧，这里只表达「到底」。
            (KeyCode::End, _) => *scroll = usize::MAX,
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => actions.close = true,
            _ => {}
        },
        Modal::Monitor => {
            if matches!(
                (code, modifiers),
                (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE)
            ) {
                actions.close = true;
            }
        }
        Modal::ContextContent { state } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                if state.is_body() {
                    state.scroll_by(-1);
                } else {
                    state.focus_step(-1);
                }
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                if state.is_body() {
                    state.scroll_by(1);
                } else {
                    state.focus_step(1);
                }
            }
            (KeyCode::PageUp, _) => state.scroll_by(-(state.viewport as i64)),
            (KeyCode::PageDown, _) => state.scroll_by(state.viewport as i64),
            (KeyCode::Home, _) | (KeyCode::Char('g'), KeyModifiers::NONE) => {
                state.scroll_to_edge(false);
            }
            (KeyCode::End, _) | (KeyCode::Char('G'), KeyModifiers::SHIFT) => {
                state.scroll_to_edge(true);
            }
            (KeyCode::Tab, KeyModifiers::NONE) => state.focus_step(1),
            (KeyCode::BackTab, _) | (KeyCode::Tab, KeyModifiers::SHIFT) => state.focus_step(-1),
            (KeyCode::Enter, KeyModifiers::NONE) | (KeyCode::Char(' '), KeyModifiers::NONE) => {
                if let Some(index) = state.focus {
                    state.enter(index);
                }
            }
            (KeyCode::Char('p'), KeyModifiers::NONE) => state.previous_page(),
            (KeyCode::Char('n'), KeyModifiers::NONE) => state.next_page(),
            (KeyCode::Char('r'), KeyModifiers::NONE) => state.refresh = true,
            (KeyCode::Char('v'), KeyModifiers::NONE) if state.is_body() => {
                state.raw = !state.raw;
                state.scroll = 0;
                state.layout = None;
            }
            (KeyCode::Char('y'), KeyModifiers::NONE) if state.is_body() => state.copy = true,
            (KeyCode::Left | KeyCode::Backspace | KeyCode::Esc, _) => actions.close = !state.back(),
            (KeyCode::Char('q'), KeyModifiers::NONE) => actions.close = true,
            _ => {}
        },
        Modal::Jobs {
            jobs,
            selected,
            pending_kill,
            scroll,
            ..
        } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                *selected = selected.saturating_sub(1);
                *pending_kill = None;
                scroll.reveal_selection = true;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                *selected = (*selected + 1).min(jobs.len().saturating_sub(1));
                *pending_kill = None;
                scroll.reveal_selection = true;
            }
            (KeyCode::Char('x'), KeyModifiers::NONE) => {
                if *pending_kill == Some(*selected) {
                    // 二次确认：杀任务
                    if let Some(job) = jobs.get(*selected) {
                        let job_id = job.job_id.clone();
                        let client = connection.client.clone();
                        let session_id = app.session.as_ref().map(|s| s.id.clone());
                        tokio::spawn(async move {
                            if let Some(session_id) = session_id {
                                let _ = client
                                    .call::<moc_protocol::methods::JobKill>(
                                        moc_protocol::methods::JobTarget { session_id, job_id },
                                    )
                                    .await;
                            }
                        });
                        actions.close = true;
                    }
                } else {
                    // 首次按 x：标记待确认
                    *pending_kill = Some(*selected);
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => actions.close = true,
            _ => {
                *pending_kill = None;
            }
        },
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

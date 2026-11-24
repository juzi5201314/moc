//! 会话与模型切换确认的键盘交互。

use moc_i18n::t;

use super::Actions;
use crate::app::App;
use crate::event::load_display_session;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::SessionModelOverflowAction;
use moc_protocol::SessionSetModel;
use moc_protocol::SessionSetModelParams;

pub(super) fn handle(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
    actions: &mut Actions,
) -> bool {
    let events = app.events.clone();
    let Some(modal) = app.modal.as_mut() else {
        return false;
    };
    match modal {
        Modal::Sessions {
            sessions,
            cwd,
            scope,
            picker,
        } => {
            let filtered = crate::modal::filtered_sessions(sessions, &picker.query, *scope, cwd);
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::PageUp, _) => picker.move_selection(-8, filtered.len(), 8),
                (KeyCode::PageDown, _) => picker.move_selection(8, filtered.len(), 8),
                (KeyCode::Tab, _) => {
                    *scope = scope.toggled();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Home, _) => {
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::End, _) => {
                    picker.selected = filtered.len().saturating_sub(1);
                    picker.keep_visible(8, filtered.len());
                }
                (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                    picker.query.clear();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char('w'), KeyModifiers::CONTROL) => {
                    while picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    while !picker.query.is_empty() && !picker.query.ends_with(' ') {
                        picker.query.pop();
                    }
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                    picker.offset = 0;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if picker.loading {
                        return false;
                    }
                    let Some((_, session)) = filtered.get(picker.selected) else {
                        return false;
                    };
                    picker.loading = true;
                    picker.error = None;
                    let client = connection.client.clone();
                    let session_id = session.id.clone();
                    let events = events.clone();
                    tokio::spawn(async move {
                        let first = load_display_session(&client, session_id.clone(), false).await;
                        // 被另一个 core 进程持有写租约时降级为只读打开，让用户至少能看历史。
                        let (loaded, read_only) = match first {
                            Ok(loaded) => (Ok(loaded), false),
                            Err(error)
                                if error.code()
                                    == Some(moc_protocol::RpcErrorCode::SessionLocked.code()) =>
                            {
                                let retry = load_display_session(&client, session_id, true).await;
                                (retry, true)
                            }
                            Err(error) => (Err(error), false),
                        };
                        match loaded {
                            Ok(loaded) => {
                                let before_entry_id =
                                    loaded.entries.first().map(|entry| entry.id.clone());
                                let _ = events.send(AppEvent::Loaded(
                                    loaded.session,
                                    crate::transcript_restore::blocks_from_entries_with_children(
                                        &loaded.entries,
                                        &loaded.child_calls,
                                    ),
                                    loaded.truncated,
                                    before_entry_id,
                                    loaded.todos,
                                    read_only,
                                    loaded.goal,
                                ));
                            }
                            Err(error) => {
                                let _ = events.send(AppEvent::PickerError(
                                    t!(
                                        "tui.event.session_load_failed_fmt",
                                        err = error.to_string()
                                    )
                                    .into_owned(),
                                ));
                            }
                        }
                    });
                }
                (KeyCode::Esc, _) => actions.close = true,
                _ => {}
            }
        }
        // 计划审阅在 `handle_modal_key` 开头整体分派，这里不会走到。
        Modal::SwitchModel {
            session_id: card_session,
            selector,
            option,
            loading,
            error,
            ..
        } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Left, _) => *option = option.saturating_sub(1),
            (KeyCode::Down, _) | (KeyCode::Right, _) => *option = (*option + 1).min(1),
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if *option == 1 {
                    actions.close = true;
                } else if !*loading {
                    *loading = true;
                    *error = None;
                    let session_id = card_session.clone();
                    let selector = selector.clone();
                    let client = connection.client.clone();
                    let events = events.clone();
                    app.inflight_session_ops.insert(session_id.clone());
                    tokio::spawn(async move {
                        let outcome = client
                            .call::<SessionSetModel>(SessionSetModelParams {
                                session_id: session_id.clone(),
                                selector,
                                overflow: SessionModelOverflowAction::Compact,
                            })
                            .await;
                        let _ = events.send(AppEvent::SessionControlSettled {
                            session_id,
                            notice: None,
                            result: outcome.map(|result| result.compaction_error).map_err(
                                |error| {
                                    t!("tui.event.model_switch_failed_fmt", err = error.to_string())
                                        .into_owned()
                                },
                            ),
                        });
                    });
                }
            }
            (KeyCode::Esc, _) => actions.close = true,
            _ => {}
        },
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

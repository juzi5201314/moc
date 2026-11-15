//! 回退选择的键盘交互。

use rust_i18n::t;

use super::Actions;
use crate::app::App;
use crate::event::load_mutation_page;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::SessionReask;
use moc_protocol::SessionReaskParams;
use moc_protocol::SessionRedo;
use moc_protocol::SessionRedoParams;
use moc_protocol::SessionRewind;
use moc_protocol::SessionRewindParams;

pub(super) fn handle(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
    actions: &mut Actions,
) -> bool {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let events = app.events.clone();
    let Some(modal) = app.modal.as_mut() else {
        return false;
    };
    match modal {
        Modal::Rewind { state } => {
            if state.filtering {
                match (code, modifiers) {
                    (KeyCode::Esc, _) => {
                        if state.query.is_empty() {
                            state.filtering = false;
                        } else {
                            state.clear_filter();
                        }
                    }
                    (KeyCode::Backspace, _) => {
                        state.backspace();
                    }
                    (KeyCode::Char(c), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        state.input_char(c);
                    }
                    (KeyCode::Up, _) => state.step(-1),
                    (KeyCode::Down, _) => state.step(1),
                    (KeyCode::Left, _) => state.jump_user(-1),
                    (KeyCode::Right, _) => state.jump_user(1),
                    (KeyCode::PageUp, _) => state.scroll_detail(-6),
                    (KeyCode::PageDown, _) => state.scroll_detail(6),
                    (KeyCode::Enter, KeyModifiers::NONE) if !state.loading => {
                        let is_redo = state.selected_item_ref().is_some_and(|item| item.future);
                        let Some(selected) = state.selected().cloned() else {
                            state.error = Some(t!("tui.rewind.none").into_owned());
                            return false;
                        };
                        let Some(session_id) = session_id.clone() else {
                            state.error = Some(t!("tui.common.no_session").into_owned());
                            return false;
                        };
                        state.loading = true;
                        state.error = None;
                        let client = connection.client.clone();
                        let events = events.clone();
                        tokio::spawn(async move {
                            let result = if is_redo {
                                client
                                    .call::<SessionRedo>(SessionRedoParams {
                                        omit_auxiliary: true,
                                        session_id,
                                        entry_id: selected.id.clone(),
                                        max_entries: Some(0),
                                    })
                                    .await
                            } else {
                                client
                                    .call::<SessionRewind>(SessionRewindParams {
                                        omit_auxiliary: true,
                                        session_id,
                                        entry_id: selected.id.clone(),
                                        max_entries: Some(0),
                                    })
                                    .await
                            };
                            let result = load_mutation_page(&client, result).await;
                            let _ = events.send(AppEvent::Rewound {
                                selected,
                                is_redo,
                                result: Box::new(result),
                            });
                        });
                    }
                    _ => {}
                }
            } else {
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k' | 'K'), KeyModifiers::NONE) => {
                        state.step(-1);
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j' | 'J'), KeyModifiers::NONE) => {
                        state.step(1);
                    }
                    (KeyCode::Left, _) | (KeyCode::Char('h' | 'H'), KeyModifiers::NONE) => {
                        state.jump_user(-1);
                    }
                    (KeyCode::Right, _) | (KeyCode::Char('l' | 'L'), KeyModifiers::NONE) => {
                        state.jump_user(1);
                    }
                    (KeyCode::Char('f' | 'F' | '/'), KeyModifiers::NONE) => {
                        state.start_filter();
                    }
                    // 选中含 ask 调用的条目时重新作答：core 截回未回答状态、按原参数重弹提问卡片。
                    (KeyCode::Char('r' | 'R'), KeyModifiers::NONE) => {
                        if !state.loading {
                            let reask_call = state.selected_item_ref().and_then(|item| {
                                (!item.future).then(|| {
                                    item.entry
                                        .message
                                        .tool_calls()
                                        .find(|call| call.name == "ask")
                                        .map(|call| call.id.clone())
                                })?
                            });
                            let Some(tool_call_id) = reask_call else {
                                state.error = Some(t!("tui.rewind.no_prompt").into_owned());
                                return false;
                            };
                            let Some(session_id) = session_id.clone() else {
                                state.error = Some(t!("tui.common.no_session").into_owned());
                                return false;
                            };
                            // 先关模态：重放的提问卡片要在主界面弹出作答。
                            // 作答期间会话对外表现为「操作在飞」，用户输入进队列
                            // 而不是撞 SessionBusy。
                            actions.close = true;
                            app.inflight_session_ops.insert(session_id.clone());
                            let client = connection.client.clone();
                            let events = events.clone();
                            tokio::spawn(async move {
                                let result = client
                                    .call::<SessionReask>(SessionReaskParams {
                                        omit_auxiliary: true,
                                        session_id,
                                        tool_call_id,
                                        max_entries: Some(0),
                                    })
                                    .await;
                                let result = load_mutation_page(&client, result).await;
                                let _ = events.send(AppEvent::Reasked {
                                    result: Box::new(result),
                                });
                            });
                        }
                    }
                    (KeyCode::PageUp, _) => state.scroll_detail(-6),
                    (KeyCode::PageDown, _) => state.scroll_detail(6),
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if !state.loading {
                            let is_redo = state.selected_item_ref().is_some_and(|item| item.future);
                            let Some(selected) = state.selected().cloned() else {
                                state.error = Some(t!("tui.rewind.none").into_owned());
                                return false;
                            };
                            let Some(session_id) = session_id.clone() else {
                                state.error = Some(t!("tui.common.no_session").into_owned());
                                return false;
                            };
                            state.loading = true;
                            state.error = None;
                            let client = connection.client.clone();
                            let events = events.clone();
                            tokio::spawn(async move {
                                let result = if is_redo {
                                    client
                                        .call::<SessionRedo>(SessionRedoParams {
                                            omit_auxiliary: true,
                                            session_id,
                                            entry_id: selected.id.clone(),
                                            max_entries: Some(0),
                                        })
                                        .await
                                } else {
                                    client
                                        .call::<SessionRewind>(SessionRewindParams {
                                            omit_auxiliary: true,
                                            session_id,
                                            entry_id: selected.id.clone(),
                                            max_entries: Some(0),
                                        })
                                        .await
                                };
                                let result = load_mutation_page(&client, result).await;
                                let _ = events.send(AppEvent::Rewound {
                                    selected,
                                    is_redo,
                                    result: Box::new(result),
                                });
                            });
                        }
                    }
                    (KeyCode::Esc, _) if state.loading && !state.items.is_empty() => {}
                    (KeyCode::Esc, _) => actions.close = true,
                    _ => {}
                }
            }
        }
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

//! 授权作答的键盘交互。

use moc_i18n::t;

use super::Actions;
use crate::app::App;
use crate::modal::Modal;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

pub(super) fn handle(
    app: &mut App,
    modifiers: KeyModifiers,
    code: KeyCode,
    actions: &mut Actions,
) -> bool {
    let Some(modal) = app.modal.as_mut() else {
        return false;
    };
    match modal {
        Modal::Permission {
            params,
            selected,
            scroll_offset,
            reply,
        } => {
            let option_count = params.options.len();
            match (code, modifiers) {
                (KeyCode::PageUp, _) => {
                    *scroll_offset = scroll_offset.saturating_sub(6);
                }
                (KeyCode::PageDown, _) => {
                    *scroll_offset = scroll_offset.saturating_add(6);
                }
                (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                    *selected = selected.saturating_sub(1);
                }
                (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                    *selected = (*selected + 1).min(option_count.saturating_sub(1));
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(option) = params.options.get(*selected)
                        && let Some(reply) = reply.take()
                    {
                        let _ = reply.send(Ok(moc_protocol::methods::PermissionResult {
                            option_id: option.id.clone(),
                        }));
                        actions.close = true;
                        app.notice = None;
                    }
                }
                (KeyCode::Char(digit), KeyModifiers::NONE)
                    if digit.is_ascii_digit() && digit != '0' =>
                {
                    let index = digit.to_digit(10).expect("ascii digit") as usize - 1;
                    if let Some(option) = params.options.get(index)
                        && let Some(reply) = reply.take()
                    {
                        let _ = reply.send(Ok(moc_protocol::methods::PermissionResult {
                            option_id: option.id.clone(),
                        }));
                        actions.close = true;
                    }
                }
                (KeyCode::Esc, _) | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    // 搁置：core 侧按取消处理（记为未选择 = 拒绝）
                    if let Some(reply) = reply.take() {
                        let _ = reply.send(Err(moc_protocol::RpcError::of(
                            moc_protocol::RpcErrorCode::RequestCancelled,
                        )));
                    }
                    actions.close = true;
                    app.set_notice(t!("tui.event.denied_unauthorized").into_owned());
                }
                _ => {}
            }
        }
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

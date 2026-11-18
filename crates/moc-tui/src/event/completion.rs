//! 输入资源与命令补全。

use crate::app::App;
use crate::app::Focus;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use moc_protocol::ResourceComplete;
use moc_protocol::ResourceCompleteParams;
use std::ops::Range;

pub(super) fn current_token_range(prompt: &crate::widgets::prompt::PromptState) -> Range<usize> {
    let line_start = prompt.text[..prompt.cursor]
        .rfind('\n')
        .map_or(0, |index| index + 1);
    let token_start = prompt.text[line_start..prompt.cursor]
        .rfind(char::is_whitespace)
        .map_or(line_start, |index| line_start + index + 1);
    let token_end = prompt.text[prompt.cursor..]
        .find(char::is_whitespace)
        .map_or(prompt.text.len(), |index| prompt.cursor + index);
    token_start..token_end
}

pub(super) fn resource_prefix(
    prompt: &crate::widgets::prompt::PromptState,
) -> Option<(String, Range<usize>)> {
    let range = current_token_range(prompt);
    let prefix = prompt.text[range.start..prompt.cursor].to_owned();
    let is_uri = prefix == "//" || prefix.ends_with(":/") || prefix.contains("://");
    if is_uri {
        return Some((prefix, range));
    }
    // `@` 工作区文件引用；`@@` 是字面转义，`!` bash 模式下 @ 无意义。
    let is_attachment =
        prefix.starts_with('@') && !prefix.starts_with("@@") && !prompt.text.starts_with('!');
    is_attachment.then_some((prefix, range))
}

pub(super) fn request_resource_completion(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.completion_prefix = None;
        return;
    };
    let Some((prefix, _range)) = resource_prefix(&app.prompt) else {
        app.completion_prefix = None;
        return;
    };
    let revision = app.prompt_revision;
    app.completion_prefix = Some(prefix.clone());
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        let result = client
            .call::<ResourceComplete>(ResourceCompleteParams {
                session_id: session_id.clone(),
                prefix: prefix.clone(),
            })
            .await
            .map(|result| result.candidates)
            .map_err(anyhow::Error::from);
        let _ = events.send(AppEvent::ResourceCompletion {
            session_id,
            revision,
            prefix,
            result,
        });
    });
}

/// 斜杠与资源补全菜单的鼠标处理：命中条目即选中并确认，点到外部关闭菜单。
pub(super) fn handle_completion_mouse(app: &mut App, mouse: &crossterm::event::MouseEvent) -> bool {
    use crossterm::event::{MouseButton, MouseEventKind};
    if app.completion.is_none() || app.modal.is_some() {
        return false;
    }
    let Some(completion) = &app.completion else {
        return false;
    };
    let in_area = completion.area.contains((mouse.column, mouse.row).into());
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            if in_area {
                if let Some(index) = completion.entry_at(mouse.column, mouse.row)
                    && let Some(comp) = app.completion.as_mut()
                {
                    comp.selected = index;
                    accept_completion(app, false);
                }
                true
            } else {
                app.completion = None;
                false
            }
        }
        MouseEventKind::Moved => {
            if in_area {
                if let Some(index) = completion.entry_at(mouse.column, mouse.row)
                    && let Some(comp) = app.completion.as_mut()
                {
                    comp.selected = index;
                }
                true
            } else {
                false
            }
        }
        MouseEventKind::ScrollUp if in_area => {
            if let Some(comp) = app.completion.as_mut() {
                comp.select_previous();
            }
            true
        }
        MouseEventKind::ScrollDown if in_area => {
            if let Some(comp) = app.completion.as_mut() {
                comp.select_next();
            }
            true
        }
        _ => false,
    }
}

pub(super) fn handle_completion_key(app: &mut App, modifiers: KeyModifiers, code: KeyCode) -> bool {
    if app.focus != Focus::Prompt || app.completion.is_none() {
        return false;
    }
    match (code, modifiers) {
        (KeyCode::Tab, KeyModifiers::SHIFT) => false,
        (KeyCode::Tab, KeyModifiers::NONE) | (KeyCode::Enter, KeyModifiers::NONE) => {
            accept_completion(app, code == KeyCode::Enter)
        }
        (KeyCode::Up, _) => {
            if let Some(completion) = app.completion.as_mut() {
                completion.select_previous();
            }
            true
        }
        (KeyCode::Down, _) => {
            if let Some(completion) = app.completion.as_mut() {
                completion.select_next();
            }
            true
        }
        _ => false,
    }
}

pub(super) fn accept_completion(app: &mut App, submit_exact: bool) -> bool {
    let Some(completion) = app.completion.take() else {
        return false;
    };
    let chosen = completion.chosen().clone();
    if submit_exact
        && chosen.source == crate::widgets::completion::CompletionSource::Slash
        && app.prompt.text.get(completion.replace_range.clone()) == Some(chosen.value.as_str())
    {
        return false;
    }
    app.prompt
        .replace_range(completion.replace_range, &chosen.value, chosen.append_space);
    app.prompt_changed();
    true
}

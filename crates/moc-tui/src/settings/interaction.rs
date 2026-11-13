use super::Hit;
use super::RowKind;
use super::SettingsPanel;
use super::View;
use super::forms;
use super::providers::ProviderAction;
use crate::app::App;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
use crossterm::event::MouseButton;
use crossterm::event::MouseEvent;
use crossterm::event::MouseEventKind;

pub fn handle_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    let Some(mut panel) = app.settings.take() else {
        return;
    };
    // 键盘接管后鼠标坐标作废，悬停底色不再与选中态抢视线。
    panel.pointer = None;
    let close = forms::key(&mut panel, app, connection, modifiers, code);
    if !close {
        app.settings = Some(panel);
    } else {
        crate::queue::dispatch(app, connection);
    }
}

pub fn paste(app: &mut App, text: &str) {
    if let Some(panel) = &mut app.settings {
        if let Some(edit) = &mut panel.edit {
            edit.editor.paste(text);
        } else if panel.provider_page()
            && let Some(draft) = &mut panel.providers.draft
            && draft.pending.is_none()
            && let Some(input) = draft.input()
        {
            input.input.insert_str_raw(text);
        }
    }
}

/// 滚轮只滚动指针所在子区的视口，不动任何光标；悬停交给绘制层按坐标重算。
pub(super) fn scroll(panel: &mut SettingsPanel, point: ratatui::layout::Position, delta: isize) {
    if let Some(edit) = &mut panel.edit
        && edit.popup
    {
        edit.editor.scroll_by(delta);
        return;
    }
    match &mut panel.view {
        View::Fields => {}
        View::Maintenance { report, offset } => {
            let last = report.removed.len().saturating_sub(1);
            *offset = offset.saturating_add_signed(delta).min(last);
            return;
        }
        View::Confirm { .. } => return,
    }
    // 供应商页按指针落在弹窗、侧栏还是详情区分流；行内编辑只占一行，照常滚动。
    if panel.provider_page() {
        if let Some(discovery) = &mut panel.providers.discovery {
            discovery.scroll.scroll_by(&mut discovery.offset, delta);
            return;
        }
        let sidebar = panel
            .hits
            .iter()
            .any(|(area, hit)| area.contains(point) && matches!(hit, Hit::Provider(_)));
        if sidebar {
            panel
                .providers
                .scroll
                .scroll_by(&mut panel.providers.offset, delta);
            return;
        }
        if let Some(draft) = &mut panel.providers.draft {
            if draft.pending.is_none() {
                draft.scroll.scroll_by(&mut draft.offset, delta);
            }
            return;
        }
    }
    panel.scroll.scroll_by(&mut panel.offset, delta);
}

/// 指针移动时让焦点跟随命中的列表行/侧栏项/弹窗项，不拉动手动滚动过的视口。
pub(super) fn hover(panel: &mut SettingsPanel, pointer: ratatui::layout::Position) {
    panel.pointer = Some(pointer);
    let hit = panel
        .hits
        .iter()
        .find(|(area, _)| area.contains(pointer))
        .map(|(_, hit)| hit.clone());
    if let Some(edit) = &mut panel.edit {
        if edit.popup
            && let Some(Hit::Item(index)) = hit
        {
            edit.editor.hover(index);
        }
    } else if panel.provider_page()
        && let Some(discovery) = &mut panel.providers.discovery
    {
        if let Some(Hit::DiscoveryItem(index)) = hit {
            discovery.cursor = index;
        }
    } else if matches!(panel.view, View::Fields) {
        match hit {
            Some(Hit::Row(index)) if index < panel.rows.len() => {
                panel.providers.sidebar = false;
                panel.selected = index;
            }
            Some(Hit::Provider(index)) => {
                panel.providers.sidebar = true;
                panel.providers.cursor = index;
            }
            Some(Hit::ProviderInput(index)) => {
                if let Some(draft) = &mut panel.providers.draft
                    && draft.pending.is_none()
                {
                    draft.focus = index;
                }
            }
            _ => {}
        }
    }
}

pub fn mouse(app: &mut App, connection: &EmbeddedConnection, event: MouseEvent) {
    let Some(panel) = &mut app.settings else {
        return;
    };
    let pointer = (event.column, event.row).into();
    panel.pointer = Some(pointer);
    if matches!(
        event.kind,
        MouseEventKind::ScrollDown | MouseEventKind::ScrollUp
    ) {
        let delta = if event.kind == MouseEventKind::ScrollDown {
            3
        } else {
            -3
        };
        scroll(panel, pointer, delta);
        return;
    }
    if event.kind == MouseEventKind::Moved {
        hover(panel, pointer);
        return;
    }
    let hit = panel
        .hits
        .iter()
        .find(|(area, _)| area.contains(pointer))
        .map(|(_, hit)| hit.clone());
    if event.kind != MouseEventKind::Down(MouseButton::Left) {
        return;
    }
    if panel.edit.as_ref().is_some_and(|edit| !edit.popup)
        && match &hit {
            Some(Hit::Row(index)) => *index != panel.selected,
            Some(Hit::Provider(_) | Hit::Tab(_) | Hit::ProviderAction(_)) => true,
            Some(Hit::Hint(code, _)) => *code != KeyCode::Esc,
            _ => false,
        }
    {
        handle_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
        if app
            .settings
            .as_ref()
            .is_some_and(|panel| panel.edit.is_none())
        {
            mouse(app, connection, event);
        }
        return;
    }
    if panel.edit.is_some() {
        match hit {
            Some(Hit::Item(index)) => {
                if let Some(code) = panel
                    .edit
                    .as_mut()
                    .and_then(|edit| edit.editor.click(index))
                {
                    handle_key(app, connection, KeyModifiers::NONE, code);
                }
            }
            Some(Hit::DismissPopup) | Some(Hit::Hint(KeyCode::Esc, _)) => {
                handle_key(app, connection, KeyModifiers::NONE, KeyCode::Esc);
            }
            Some(Hit::Hint(code, modifiers)) => handle_key(app, connection, modifiers, code),
            // 弹窗清空命中表收不到 Scroll；这里是行内编辑：滚视口但不提交。
            Some(Hit::Scroll(offset)) => {
                panel.offset = offset.min(panel.scroll.max_offset);
                panel.scroll.reveal_selection = false;
            }
            _ => {}
        }
        return;
    }
    if panel.provider_page() && panel.providers.discovery.is_some() {
        match hit {
            Some(Hit::DiscoveryItem(index)) => {
                if let Some(discovery) = &mut panel.providers.discovery {
                    discovery.cursor = index;
                }
                if let Some(&cand_idx) = panel.visible_candidates().get(index) {
                    panel.toggle_candidate(cand_idx);
                    panel.error = None;
                }
            }
            Some(Hit::DismissPopup) => {
                panel.providers.discovery = None;
                panel.error = None;
                panel.rebuild_rows();
                panel.select_action(ProviderAction::Fetch);
            }
            Some(Hit::Hint(code, modifiers)) => handle_key(app, connection, modifiers, code),
            Some(Hit::Scroll(offset)) => {
                if let Some(discovery) = &mut panel.providers.discovery {
                    discovery.offset = offset.min(discovery.scroll.max_offset);
                    discovery.scroll.reveal_selection = false;
                }
            }
            _ => {}
        }
        return;
    }
    let Some(hit) = hit else {
        return;
    };
    match hit {
        Hit::Tab(index) => panel.select_tab(index),
        // 滚动条是视口操作不是「点到别处」：只滚所在清单，不动光标。
        Hit::Scroll(offset) => match &mut panel.view {
            View::Maintenance {
                offset: view_offset,
                ..
            } => *view_offset = offset,
            _ => {
                panel.offset = offset.min(panel.scroll.max_offset);
                panel.scroll.reveal_selection = false;
            }
        },
        Hit::Row(index) => {
            let action = panel
                .rows
                .get(index)
                .is_some_and(|row| matches!(row.kind, RowKind::Action(_)));
            let was_selected = panel.selected == index;
            panel.providers.sidebar = false;
            panel.selected = index;
            panel.scroll.reveal_selection = true;
            if was_selected || action {
                handle_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
            }
        }
        Hit::Provider(index) => {
            panel.choose_provider(index);
            panel.providers.sidebar = false;
        }
        Hit::ProviderInput(index) => {
            if let Some(draft) = &mut panel.providers.draft {
                draft.focus = index;
                draft.scroll.reveal_selection = true;
                if index == 1 || index >= 4 {
                    handle_key(app, connection, KeyModifiers::NONE, KeyCode::Enter);
                }
            }
        }
        Hit::ProviderAction(action) => {
            let mut panel = app.settings.take().expect("设置页存在");
            panel.provider_action(action, app, connection);
            app.settings = Some(panel);
        }
        Hit::Hint(code, modifiers) => handle_key(app, connection, modifiers, code),
        _ => {}
    }
}

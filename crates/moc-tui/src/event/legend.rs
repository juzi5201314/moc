//! 引擎及权限菜单交互。

use super::commands::apply_permission;
use super::commands::set_engine;
use crate::app::App;
use crate::app::LegendMenu;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

/// 图例菜单对一个输入事件的处置：是否消费、是否带选中值应用。
pub(super) enum LegendAction<T> {
    /// 事件照常处理：无菜单，或菜单因普通键关闭（键位不丢）。
    Passthrough,
    /// 事件已被菜单吃掉。
    Consumed,
    /// 确认选中项：应用并关闭。
    Apply(T),
}

/// 图例菜单的键盘语义：`↑↓` 换高亮、`Enter` 应用、`Esc` 关闭、其他键关菜单后照常处理。
pub(super) fn legend_menu_key<T: Copy>(
    menu: &mut Option<LegendMenu<T>>,
    modifiers: KeyModifiers,
    code: KeyCode,
) -> LegendAction<T> {
    let Some(state) = menu.as_ref() else {
        return LegendAction::Passthrough;
    };
    let count = state.items.len();
    let selected = state.selected.min(count - 1);
    match (code, modifiers) {
        (KeyCode::Esc, _) => {
            *menu = None;
            LegendAction::Consumed
        }
        (KeyCode::Up, _) => {
            if let Some(state) = menu.as_mut() {
                state.selected = (selected + count - 1) % count;
            }
            LegendAction::Consumed
        }
        (KeyCode::Down, _) => {
            if let Some(state) = menu.as_mut() {
                state.selected = (selected + 1) % count;
            }
            LegendAction::Consumed
        }
        (KeyCode::Enter, KeyModifiers::NONE) => {
            let item = state.items.get(selected).copied();
            *menu = None;
            match item {
                Some(item) => LegendAction::Apply(item),
                None => LegendAction::Consumed,
            }
        }
        _ => {
            *menu = None;
            LegendAction::Passthrough
        }
    }
}

/// 图例菜单的鼠标语义：悬停换高亮、点条目应用、点菜单外（含再点一次图例）关闭。
pub(super) fn legend_menu_mouse<T: Copy>(
    menu: &mut Option<LegendMenu<T>>,
    mouse: &crossterm::event::MouseEvent,
) -> LegendAction<T> {
    use crossterm::event::{MouseButton, MouseEventKind};
    let Some(state) = menu.as_mut() else {
        return LegendAction::Passthrough;
    };
    match mouse.kind {
        MouseEventKind::Down(MouseButton::Left) => {
            let item = state
                .entry_at(mouse.column, mouse.row)
                .and_then(|index| state.items.get(index).copied());
            *menu = None;
            match item {
                Some(item) => LegendAction::Apply(item),
                None => LegendAction::Consumed,
            }
        }
        MouseEventKind::Moved => {
            if let Some(index) = state.entry_at(mouse.column, mouse.row) {
                state.selected = index;
            }
            LegendAction::Consumed
        }
        // 菜单内滚轮与 ↑↓ 同语义循环换高亮（同补全菜单约定）；菜单外照旧穿透滚转写。
        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
            if state.area.contains((mouse.column, mouse.row).into()) && !state.items.is_empty() =>
        {
            let count = state.items.len();
            let selected = state.selected.min(count - 1);
            state.selected = if matches!(mouse.kind, MouseEventKind::ScrollUp) {
                (selected + count - 1) % count
            } else {
                (selected + 1) % count
            };
            LegendAction::Consumed
        }
        _ => LegendAction::Passthrough,
    }
}

/// 就地图例菜单（引擎 / 权限）的键盘处理。
///
/// 与斜杠补全菜单同一套约定：只认方向键与确认键，其他按键先关掉菜单再照常处理，
/// 菜单不会吞掉正常输入与快捷键。
pub(super) fn handle_legend_menu_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) -> bool {
    if app.engine_menu.is_some() {
        return match legend_menu_key(&mut app.engine_menu, modifiers, code) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(engine) => {
                set_engine(app, connection, engine);
                true
            }
        };
    }
    if app.permission_menu.is_some() {
        return match legend_menu_key(&mut app.permission_menu, modifiers, code) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(mode) => {
                apply_permission(app, connection, mode, false);
                true
            }
        };
    }
    false
}

/// 就地图例菜单（引擎 / 权限）的鼠标处理：命中条目即应用，点到菜单外关闭。
/// 返回 `true` 表示事件已被菜单消费，不再落到队列、滚动区或输入框。
pub(super) fn handle_legend_menu_mouse(
    app: &mut App,
    connection: &EmbeddedConnection,
    mouse: &crossterm::event::MouseEvent,
) -> bool {
    if app.modal.is_some() {
        return false;
    }
    if app.engine_menu.is_some() {
        return match legend_menu_mouse(&mut app.engine_menu, mouse) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(engine) => {
                set_engine(app, connection, engine);
                true
            }
        };
    }
    if app.permission_menu.is_some() {
        return match legend_menu_mouse(&mut app.permission_menu, mouse) {
            LegendAction::Passthrough => false,
            LegendAction::Consumed => true,
            LegendAction::Apply(mode) => {
                apply_permission(app, connection, mode, false);
                true
            }
        };
    }
    false
}

//! 模型选择的键盘交互。

use rust_i18n::t;

use super::Actions;
use crate::app::App;
use crate::event::apply_fusion_pick;
use crate::modal::Modal;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;

pub(super) fn handle(
    app: &mut App,
    modifiers: KeyModifiers,
    code: KeyCode,
    actions: &mut Actions,
) -> bool {
    let session_id = app.session.as_ref().map(|session| session.id.clone());
    let session_fast = app
        .session
        .as_ref()
        .is_some_and(|session| session.fast_mode);
    let Some(modal) = app.modal.as_mut() else {
        return false;
    };
    match modal {
        Modal::Model {
            entries,
            current,
            picker,
            level,
            detail,
            target,
        } => {
            use crate::modal::{DetailRow, FUSION_ENTRY_SELECTOR, MenuItems, MenuState};
            detail.pointer = None;
            let filtered = crate::modal::filtered_entries(entries, &picker.query);
            let highlighted = |picker: &crate::modal::PickerState| {
                crate::modal::filtered_entries(entries, &picker.query)
                    .get(picker.selected)
                    .map(|(index, _)| *index)
            };
            let entry = filtered.get(picker.selected).map(|(_, entry)| *entry);
            let before = highlighted(picker);
            let rows = crate::modal::detail_rows(entry, entries, target, &detail.fusion);
            // 展开某详情行的下拉：selected 指向当前生效值，query 从头开始。
            let open_menu =
                |detail: &mut crate::modal::DetailState, row: DetailRow, current: Option<usize>| {
                    detail.menu = Some(MenuState {
                        row,
                        selected: current.unwrap_or(0),
                        offset: 0,
                        scroll: Default::default(),
                        query: String::new(),
                    });
                };
            if let Some(menu) = detail.menu.as_mut() {
                // 下拉打开：候选内导航；模型下拉支持输入过滤，等级下拉忽略输入。
                let items = crate::modal::menu_items(
                    menu.row,
                    entry,
                    entries,
                    level,
                    &detail.fusion,
                    &menu.query,
                );
                let count = items.len();
                let model_menu = matches!(menu.row, DetailRow::Lead | DetailRow::Sidekick);
                match (code, modifiers) {
                    (KeyCode::Up, _) => menu.move_selection(-1, count),
                    (KeyCode::Down, _) => menu.move_selection(1, count),
                    (KeyCode::PageUp, _) => menu.move_selection(-8, count),
                    (KeyCode::PageDown, _) => menu.move_selection(8, count),
                    (KeyCode::Home, _) => {
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    (KeyCode::End, _) => {
                        menu.selected = count.saturating_sub(1);
                        menu.keep_visible(count);
                    }
                    (KeyCode::Esc, _) | (KeyCode::Left, _) => detail.menu = None,
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        // 会话目标下切换进行中：Level 提交是 no-op，下拉保持打开。
                        if picker.loading
                            && menu.row == DetailRow::Level
                            && matches!(target, crate::modal::ModelTarget::Session)
                        {
                            return false;
                        }
                        let chosen = match &items {
                            MenuItems::Levels { levels, .. } => levels.get(menu.selected).cloned(),
                            MenuItems::Models {
                                entries: candidates,
                                ..
                            } => {
                                candidates
                                    .get(menu.selected)
                                    .map(|(_, candidate)| match menu.row {
                                        DetailRow::Lead => crate::modal::fusion_side_pick(
                                            &detail.fusion.lead,
                                            candidate,
                                        ),
                                        _ => crate::modal::fusion_side_pick(
                                            &detail.fusion.sidekick,
                                            candidate,
                                        ),
                                    })
                            }
                        };
                        if let Some(chosen) = chosen {
                            match menu.row {
                                DetailRow::Level => {
                                    level.manual = Some(chosen.clone());
                                    // 会话目标：改等级立即切换且不关闭选择器。
                                    if matches!(target, crate::modal::ModelTarget::Session)
                                        && let Some(entry) = entry
                                    {
                                        let selector = entry.selector_with(Some(&chosen));
                                        if selector != *current {
                                            picker.loading = true;
                                            picker.error = None;
                                            detail.pending = Some(selector.clone());
                                            actions.session_apply =
                                                Some((session_id.clone(), selector));
                                        }
                                    }
                                }
                                DetailRow::Lead => detail.fusion.lead = chosen,
                                DetailRow::Sidekick => detail.fusion.sidekick = chosen,
                                DetailRow::LeadLevel => {
                                    let next = crate::modal::fusion_side_level(
                                        &detail.fusion.lead,
                                        &chosen,
                                    );
                                    detail.fusion.lead = next;
                                }
                                DetailRow::SidekickLevel => {
                                    let next = crate::modal::fusion_side_level(
                                        &detail.fusion.sidekick,
                                        &chosen,
                                    );
                                    detail.fusion.sidekick = next;
                                }
                                _ => {}
                            }
                        }
                        detail.menu = None;
                    }
                    (KeyCode::Backspace, _) if model_menu => {
                        menu.query.pop();
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) if model_menu => {
                        menu.query.clear();
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                        if model_menu && !ch.is_control() =>
                    {
                        menu.query.push(ch);
                        menu.selected = 0;
                        menu.offset = 0;
                        menu.scroll.reveal_selection = true;
                    }
                    _ => {}
                }
            } else if let Some(focus) = detail.focus {
                // 详情卡焦点：↑↓ 在控制行间移动（夹紧不循环），Enter/Space 按行动作。
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Down, _) if !rows.is_empty() => {
                        let position = rows.iter().position(|row| *row == focus).unwrap_or(0);
                        let next = position
                            .saturating_add_signed(if matches!(code, KeyCode::Up) { -1 } else { 1 })
                            .min(rows.len() - 1);
                        detail.focus = Some(rows[next]);
                    }
                    (KeyCode::Left, _) | (KeyCode::Esc, _) => detail.focus = None,
                    (KeyCode::Enter, KeyModifiers::NONE)
                    | (KeyCode::Char(' '), KeyModifiers::NONE) => match focus {
                        DetailRow::Fast => actions.fast_mode_apply = Some(!session_fast),
                        DetailRow::Apply => {
                            if picker.loading {
                                return false;
                            }
                            if detail.fusion.lead.is_empty() || detail.fusion.sidekick.is_empty() {
                                picker.error =
                                    Some(t!("tui.event.pick_lead_sidekick").into_owned());
                            } else if apply_fusion_pick(
                                picker,
                                &detail.fusion,
                                current,
                                &session_id,
                                &mut actions.session_apply,
                            ) {
                                actions.close = true;
                            }
                        }
                        row => {
                            let items = crate::modal::menu_items(
                                row,
                                entry,
                                entries,
                                level,
                                &detail.fusion,
                                "",
                            );
                            open_menu(detail, row, items.current());
                        }
                    },
                    _ => {}
                }
            } else {
                match (code, modifiers) {
                    (KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown, _) => {
                        let delta = match code {
                            KeyCode::Up => -1,
                            KeyCode::Down => 1,
                            KeyCode::PageUp => -8,
                            _ => 8,
                        };
                        picker.selected = picker
                            .selected
                            .saturating_add_signed(delta)
                            .min(filtered.len().saturating_sub(1));
                        detail.list_scroll.reveal_selection = true;
                    }
                    // Tab：普通模型展开思考等级下拉（焦点仍在列表）；高亮 Fusion 项时等同 →。
                    (KeyCode::Tab, KeyModifiers::NONE) => {
                        if entry.is_some_and(|entry| entry.selector == FUSION_ENTRY_SELECTOR) {
                            if let Some(first) = rows.first() {
                                detail.focus = Some(*first);
                            }
                        } else if entry.is_some_and(|entry| !entry.levels.is_empty()) {
                            let items = crate::modal::menu_items(
                                DetailRow::Level,
                                entry,
                                entries,
                                level,
                                &detail.fusion,
                                "",
                            );
                            open_menu(detail, DetailRow::Level, items.current());
                        }
                    }
                    (KeyCode::Right, _) => {
                        if let Some(first) = rows.first() {
                            detail.focus = Some(*first);
                        }
                    }
                    (KeyCode::Home, _) => {
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
                    }

                    (KeyCode::End, _) => {
                        picker.selected = filtered.len().saturating_sub(1);
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Char('u'), KeyModifiers::CONTROL) => {
                        picker.query.clear();
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
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
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Backspace, _) => {
                        picker.query.pop();
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                        if !ch.is_control() =>
                    {
                        picker.query.push(ch);
                        picker.selected = 0;
                        picker.offset = 0;
                        detail.list_scroll.reveal_selection = true;
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        if picker.loading {
                            return false;
                        }
                        let Some((_, entry)) = filtered.get(picker.selected).copied() else {
                            return false;
                        };
                        if entry.selector == FUSION_ENTRY_SELECTOR {
                            // Fusion 特殊项：两侧都已选则应用；否则聚焦到第一个
                            // 空槽位行并直接展开它的模型下拉。
                            if detail.fusion.lead.is_empty() || detail.fusion.sidekick.is_empty() {
                                let row = if detail.fusion.lead.is_empty() {
                                    DetailRow::Lead
                                } else {
                                    DetailRow::Sidekick
                                };
                                detail.focus = Some(row);
                                let items = crate::modal::menu_items(
                                    row,
                                    Some(entry),
                                    entries,
                                    level,
                                    &detail.fusion,
                                    "",
                                );
                                open_menu(detail, row, items.current());
                            } else if apply_fusion_pick(
                                picker,
                                &detail.fusion,
                                current,
                                &session_id,
                                &mut actions.session_apply,
                            ) {
                                actions.close = true;
                            }
                        } else {
                            let selector =
                                entry.selector_with(level.resolve(&entry.levels).as_deref());
                            match target {
                                crate::modal::ModelTarget::Config { path, label } => {
                                    actions.config_pick =
                                        Some((path.clone(), label.clone(), selector));
                                }
                                crate::modal::ModelTarget::WorkflowRole { .. } => {
                                    // 先整体取出（借用期拿不走 back 箱），写回后统一还原。
                                    let owned = std::mem::replace(
                                        target,
                                        crate::modal::ModelTarget::Session,
                                    );
                                    if let crate::modal::ModelTarget::WorkflowRole { key, back } =
                                        owned
                                    {
                                        let (back, action) = crate::workflow::apply_workflow_pick(
                                            back, &key, selector,
                                        );
                                        actions.workflow_back = Some(back);
                                        actions.workflow_action = action;
                                    }
                                }
                                crate::modal::ModelTarget::Session => {
                                    picker.loading = true;
                                    picker.error = None;
                                    actions.session_apply = Some((session_id.clone(), selector));
                                }
                            }
                        }
                    }

                    (KeyCode::Esc, _) => {
                        if matches!(target, crate::modal::ModelTarget::WorkflowRole { .. }) {
                            // 取消选择：原样放回回程票，不写覆盖。
                            let owned =
                                std::mem::replace(target, crate::modal::ModelTarget::Session);
                            if let crate::modal::ModelTarget::WorkflowRole { back, .. } = owned {
                                actions.workflow_back = Some(*back);
                            }
                        } else {
                            actions.close = true;
                        }
                    }
                    _ => {}
                }
                // 高亮条目变化：等级跨条目记忆照旧，详情焦点与下拉一并复位。
                let after = highlighted(picker);
                if before != after {
                    detail.menu = None;
                    detail.focus = None;
                }
                level.change_entry(
                    before.and_then(|index| entries.get(index)),
                    after.and_then(|index| entries.get(index)),
                );
            }
        }
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

//! 工作流面板的键盘交互。

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
        Modal::WorkflowApproval { state } => {
            if let Some(feedback) = state.feedback.as_mut() {
                // 反馈输入态：全部键位归文本行。
                match (code, modifiers) {
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        let text = state.feedback.take().unwrap_or_default();
                        actions.workflow_decision = Some((
                            state.request.request_id.clone(),
                            moc_protocol::methods::WorkflowDecision::Reject {
                                feedback: (!text.trim().is_empty()).then_some(text),
                            },
                        ));
                        actions.close = true;
                    }
                    (KeyCode::Esc, _) => {
                        state.feedback = None;
                    }
                    (KeyCode::Backspace, _) => {
                        feedback.pop();
                    }
                    (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT) => {
                        feedback.push(ch);
                    }
                    _ => {}
                }
            } else {
                match (code, modifiers) {
                    (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                        state.focus = state.focus.saturating_sub(1);
                        state.scroll.reveal_selection = true;
                    }
                    (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                        state.focus = (state.focus + 1).min(state.rows().saturating_sub(1));
                        state.scroll.reveal_selection = true;
                    }
                    (KeyCode::PageUp, _) => {
                        state.body_scroll = state.body_scroll.saturating_sub(6);
                        state.scroll.reveal_selection = false;
                    }
                    (KeyCode::PageDown, _) => {
                        state.body_scroll = state.body_scroll.saturating_add(6);
                        state.scroll.reveal_selection = false;
                    }
                    (KeyCode::Enter, KeyModifiers::NONE) => {
                        // 选中 role/ref → 打开模型选择器；回程票带回整个审批状态。
                        if let Some(target) = state.focus_target() {
                            let current = match &target {
                                crate::workflow::FocusTarget::Role(id) => {
                                    state.role_models.get(id).cloned().unwrap_or_default()
                                }
                                crate::workflow::FocusTarget::ModelRef(id) => {
                                    state.model_ref_map.get(id).cloned().unwrap_or_default()
                                }
                            };
                            actions.workflow_picker = Some((target, current));
                        }
                    }
                    (KeyCode::Char('a'), KeyModifiers::NONE) => {
                        actions.workflow_decision = Some((
                            state.request.request_id.clone(),
                            moc_protocol::methods::WorkflowDecision::Approve {
                                role_models: Some(state.role_models.clone()),
                                model_ref_map: Some(state.model_ref_map.clone()),
                            },
                        ));
                        actions.close = true;
                    }
                    (KeyCode::Char('r'), KeyModifiers::NONE) => {
                        state.feedback = Some(String::new());
                    }
                    (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                        // 最小化：审批仍在等待，状态栏胶囊可重开。
                        state.parked = true;
                        actions.close = true;
                    }
                    _ => {}
                }
            }
        }
        Modal::WorkflowRuns { runs, picker } => {
            let filtered: Vec<_> = runs
                .iter()
                .filter(|run| {
                    let query = picker.query.to_lowercase();
                    run.run_id.to_lowercase().contains(&query)
                        || run
                            .title
                            .as_deref()
                            .unwrap_or_default()
                            .to_lowercase()
                            .contains(&query)
                })
                .collect();
            match (code, modifiers) {
                (KeyCode::Up, _) => picker.move_selection(-1, filtered.len(), 8),
                (KeyCode::Down, _) => picker.move_selection(1, filtered.len(), 8),
                (KeyCode::Backspace, _) => {
                    picker.query.pop();
                    picker.selected = 0;
                    picker.scroll.reveal_selection = true;
                }
                (KeyCode::Char(ch), KeyModifiers::NONE | KeyModifiers::SHIFT)
                    if !ch.is_control() =>
                {
                    picker.query.push(ch);
                    picker.selected = 0;
                    picker.scroll.reveal_selection = true;
                }
                (KeyCode::Enter, KeyModifiers::NONE) => {
                    if let Some(run) = filtered.get(picker.selected) {
                        actions.workflow_action =
                            Some(crate::workflow::WorkflowAction::OpenRun(run.run_id.clone()));
                    }
                    actions.close = true;
                }
                (KeyCode::Esc, _) => actions.close = true,
                _ => {}
            }
        }
        Modal::WorkflowRun { state } => match (code, modifiers) {
            (KeyCode::Up, _) | (KeyCode::Char('k'), KeyModifiers::NONE) => {
                state.focus = state.focus.saturating_sub(1);
                state.scroll.reveal_selection = true;
            }
            (KeyCode::Down, _) | (KeyCode::Char('j'), KeyModifiers::NONE) => {
                state.focus = (state.focus + 1).min(state.roles.len().saturating_sub(1));
                state.scroll.reveal_selection = true;
            }
            (KeyCode::PageUp, _) => {
                state.body_scroll = state.body_scroll.saturating_sub(6);
                state.scroll.reveal_selection = false;
            }
            (KeyCode::PageDown, _) => {
                state.body_scroll = state.body_scroll.saturating_add(6);
                state.scroll.reveal_selection = false;
            }
            (KeyCode::Enter, KeyModifiers::NONE) => {
                if let Some(role) = state.roles.get(state.focus) {
                    actions.workflow_picker = Some((
                        crate::workflow::FocusTarget::Role(role.role_id.clone()),
                        role.model.clone().unwrap_or_default(),
                    ));
                }
            }
            (KeyCode::Char('s'), KeyModifiers::NONE) => {
                actions.workflow_action =
                    Some(crate::workflow::WorkflowAction::Stop(state.run_id.clone()));
            }
            (KeyCode::Char('r'), KeyModifiers::NONE) => {
                actions.workflow_action = Some(crate::workflow::WorkflowAction::Resume(
                    state.run_id.clone(),
                ));
            }
            (KeyCode::Char('v'), KeyModifiers::NONE) => {
                // 多版本切换：所有有多版本的 artifact 各进一版（wrap）。
                let mut ids: Vec<String> = Vec::new();
                let mut counts: std::collections::BTreeMap<String, usize> =
                    std::collections::BTreeMap::new();
                for artifact in &state.artifacts {
                    if let Some(id) = artifact.get("artifact_id").and_then(|v| v.as_str()) {
                        *counts.entry(id.to_string()).or_insert(0) += 1;
                        if !ids.contains(&id.to_string()) {
                            ids.push(id.to_string());
                        }
                    }
                }
                for id in ids {
                    let n = counts[&id];
                    if n > 1 {
                        let cur = *state.artifact_versions.get(&id).unwrap_or(&n);
                        state.artifact_versions.insert(id, cur % n + 1);
                    }
                }
            }
            (KeyCode::Char('o'), KeyModifiers::NONE) => {
                if state.artifact_view.is_some() {
                    state.artifact_view = None;
                } else if let Some(id) = state
                    .artifacts
                    .iter()
                    .find(|a| {
                        matches!(
                            a.get("member").and_then(|v| v.as_str()),
                            Some("file") | Some("markdown")
                        )
                    })
                    .and_then(|a| a.get("artifact_id").and_then(|v| v.as_str()))
                {
                    actions.workflow_action = Some(crate::workflow::WorkflowAction::ViewArtifact(
                        state.run_id.clone(),
                        id.to_string(),
                    ));
                }
            }
            (KeyCode::Esc, _) | (KeyCode::Char('q'), KeyModifiers::NONE) => {
                if state.artifact_view.is_some() {
                    state.artifact_view = None;
                } else {
                    actions.close = true;
                }
            }
            _ => {}
        },
        _ => unreachable!("模态路由必须与处理器一致"),
    }
    true
}

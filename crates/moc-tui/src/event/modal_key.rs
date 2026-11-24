//! 模态键盘分派。

use moc_i18n::t;

use super::model::request_session_model;
use super::model::set_fast_mode;
use super::plan::plan_modal_key;
use crate::app::App;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crossterm::event::KeyCode;
use crossterm::event::KeyModifiers;
mod ask;
mod browse;
mod model;
mod permission;
mod rewind;
mod sessions;
mod workflow;

#[derive(Default)]
struct Actions {
    close: bool,
    config_pick: Option<(Vec<String>, String, String)>,
    session_apply: Option<(Option<String>, String)>,
    /// 计划审阅「新会话模型」芯片的选中结果：写回停放的审阅状态，不走会话通道。
    plan_model_pick: Option<String>,
    fast_mode_apply: Option<bool>,
    workflow_back: Option<crate::workflow::WorkflowRoleBack>,
    workflow_decision: Option<(String, moc_protocol::methods::WorkflowDecision)>,
    workflow_picker: Option<(crate::workflow::FocusTarget, String)>,
    workflow_action: Option<crate::workflow::WorkflowAction>,
}

pub(super) fn handle_modal_key(
    app: &mut App,
    connection: &EmbeddedConnection,
    modifiers: KeyModifiers,
    code: KeyCode,
) {
    if let Some(Modal::Plan { state }) = app.modal.as_ref() {
        if state.submitting {
            return;
        }
        // 计划审阅有自己的编辑/选区语义，整体交给 `plan_modal_key`。
        plan_modal_key(app, connection, modifiers, code);
        return;
    }
    let mut actions = Actions::default();
    let plan_review_open = matches!(app.modal, Some(Modal::Plan { .. }));
    // 键盘接管：丢掉悬停坐标，不与选中高亮抢视觉（模型选择器在其分支内自处理）。
    match app.modal.as_mut() {
        Some(Modal::Sessions { picker, .. } | Modal::WorkflowRuns { picker, .. }) => {
            picker.pointer = None
        }
        Some(Modal::Jobs { pointer, .. }) => *pointer = None,
        Some(Modal::WorkflowApproval { state }) => state.pointer = None,
        Some(Modal::WorkflowRun { state }) => state.pointer = None,
        Some(Modal::Rewind { state }) => state.pointer = None,
        _ => {}
    }
    if app.modal.is_none() {
        return;
    }

    app.ctrl_c_pending = None;
    let proceed = match app.modal.as_ref() {
        Some(Modal::Permission { .. }) => permission::handle(app, modifiers, code, &mut actions),
        Some(Modal::Ask { .. }) => ask::handle(app, modifiers, code, &mut actions),
        Some(Modal::Rewind { .. }) => {
            rewind::handle(app, connection, modifiers, code, &mut actions)
        }
        Some(Modal::Model { .. }) => model::handle(app, modifiers, code, &mut actions),
        Some(Modal::Sessions { .. }) | Some(Modal::SwitchModel { .. }) => {
            sessions::handle(app, connection, modifiers, code, &mut actions)
        }
        Some(Modal::Help { .. })
        | Some(Modal::Tools { .. })
        | Some(Modal::Monitor)
        | Some(Modal::ContextContent { .. })
        | Some(Modal::Jobs { .. }) => {
            browse::handle(app, connection, modifiers, code, &mut actions)
        }
        Some(Modal::WorkflowApproval { .. })
        | Some(Modal::WorkflowRuns { .. })
        | Some(Modal::WorkflowRun { .. }) => workflow::handle(app, modifiers, code, &mut actions),
        Some(Modal::Plan { .. }) | None => unreachable!("计划和空模态已提前处理"),
    };
    if !proceed {
        return;
    }
    let Actions {
        close,
        config_pick,
        session_apply,
        plan_model_pick,
        fast_mode_apply,
        workflow_back,
        workflow_decision,
        workflow_picker,
        workflow_action,
    } = actions;
    if let Some((id, selector)) = session_apply {
        request_session_model(app, connection, id, selector);
    }
    // 新会话模型：写回停放的审阅状态，模态关闭后由 restore_parked_plan 还原。
    if let Some(selector) = plan_model_pick
        && let Some(state) = app.parked_plan.as_mut()
    {
        state.execution_model = Some(selector);
    }
    // workflow：模型选择器回程（审批窗/run 面板）。
    if let Some(back) = workflow_back {
        app.set_modal(Some(match back {
            crate::workflow::WorkflowRoleBack::Approval(state) => Modal::WorkflowApproval { state },
            crate::workflow::WorkflowRoleBack::Run(state) => Modal::WorkflowRun { state },
        }));
    }
    // workflow：审批决策回传 core。
    if let Some((request_id, decision)) = workflow_decision
        && let Some(session_id) = app.session.as_ref().map(|session| session.id.clone())
    {
        let client = connection.client.clone();
        let events = app.events.clone();
        tokio::spawn(async move {
            let result = client
                .call::<moc_protocol::methods::SessionWorkflowApprove>(
                    moc_protocol::methods::SessionWorkflowApproveParams {
                        session_id,
                        request_id,
                        decision,
                    },
                )
                .await;
            if let Err(error) = result {
                let _ = events.send(AppEvent::Notice(
                    t!("tui.event.wf_approve_failed_fmt", err = error.to_string()).into_owned(),
                ));
            }
        });
    }
    // workflow：role/ref 焦点进模型选择器（带回程票）。
    if let Some((key, current)) = workflow_picker
        && let Some(modal) = app.take_modal()
    {
        let back = match modal {
            Modal::WorkflowApproval { state } => crate::workflow::WorkflowRoleBack::Approval(state),
            Modal::WorkflowRun { state } => crate::workflow::WorkflowRoleBack::Run(state),
            other => {
                app.set_modal(Some(other));
                return;
            }
        };
        app.open_model_picker_for(
            current,
            &[],
            crate::modal::ModelTarget::WorkflowRole {
                key,
                back: Box::new(back),
            },
        );
    }
    // workflow：面板动作 RPC。
    if let Some(action) = workflow_action
        && let Some(session_id) = app.session.as_ref().map(|session| session.id.clone())
    {
        let client = connection.client.clone();
        let events = app.events.clone();
        tokio::spawn(async move {
            match action {
                crate::workflow::WorkflowAction::Stop(run_id) => {
                    if let Err(error) = client
                        .call::<moc_protocol::methods::SessionWorkflowStop>(
                            moc_protocol::methods::SessionWorkflowRunParams { session_id, run_id },
                        )
                        .await
                    {
                        let _ = events.send(AppEvent::Notice(
                            t!("tui.event.wf_stop_failed_fmt", err = error.to_string())
                                .into_owned(),
                        ));
                    }
                }
                crate::workflow::WorkflowAction::Resume(run_id) => {
                    if let Err(error) = client
                        .call::<moc_protocol::methods::SessionWorkflowResume>(
                            moc_protocol::methods::SessionWorkflowRunParams { session_id, run_id },
                        )
                        .await
                    {
                        let _ = events.send(AppEvent::Notice(
                            t!("tui.event.wf_resume_failed_fmt", err = error.to_string())
                                .into_owned(),
                        ));
                    }
                }
                crate::workflow::WorkflowAction::OpenRun(run_id) => {
                    match client
                        .call::<moc_protocol::methods::SessionWorkflowRun>(
                            moc_protocol::methods::SessionWorkflowRunParams { session_id, run_id },
                        )
                        .await
                    {
                        Ok(snapshot) => {
                            let _ = events.send(AppEvent::OpenWorkflowRun(Box::new(snapshot)));
                        }
                        Err(error) => {
                            let _ = events.send(AppEvent::Notice(
                                t!("tui.event.wf_run_failed_fmt", err = error.to_string())
                                    .into_owned(),
                            ));
                        }
                    }
                }
                crate::workflow::WorkflowAction::SetRoleModel(run_id, role_id, model) => {
                    if let Err(error) = client
                        .call::<moc_protocol::methods::SessionWorkflowSetRoleModel>(
                            moc_protocol::methods::SessionWorkflowSetRoleModelParams {
                                session_id,
                                run_id,
                                role_id,
                                model,
                            },
                        )
                        .await
                    {
                        let _ = events.send(AppEvent::Notice(
                            t!("tui.event.role_model_failed_fmt", err = error.to_string())
                                .into_owned(),
                        ));
                    }
                }
                crate::workflow::WorkflowAction::ViewArtifact(run_id, artifact_id) => {
                    match client
                        .call::<moc_protocol::methods::SessionWorkflowArtifact>(
                            moc_protocol::methods::SessionWorkflowArtifactParams {
                                session_id,
                                run_id,
                                artifact_id,
                                version: None,
                            },
                        )
                        .await
                    {
                        Ok(result) => {
                            let _ = events.send(AppEvent::WorkflowArtifactView {
                                artifact_id: result.artifact_id,
                                content: result.content,
                            });
                        }
                        Err(error) => {
                            let _ = events.send(AppEvent::Notice(
                                t!("tui.event.artifact_failed_fmt", err = error.to_string())
                                    .into_owned(),
                            ));
                        }
                    }
                }
            }
        });
    }
    if let Some(enabled) = fast_mode_apply {
        set_fast_mode(app, connection, enabled);
    }

    if let Some((path, label, value)) = config_pick {
        app.set_modal(None);
        crate::settings::apply_model_role(app, connection, path, &label, value);
    }

    if close {
        // 最小化的 workflow 审批：状态移进状态栏胶囊（点击重开），不算已决。
        if let Some(Modal::WorkflowApproval { state }) = app.take_modal()
            && state.parked
        {
            app.workflow.parked_approval = Some(state);
        } else {
            app.set_modal(None);
        }
        app.show_next_pending_modal();
        if !plan_review_open {
            crate::queue::dispatch(app, connection);
        }
    }
}

//! 工作流通知与面板入口。

use moc_i18n::t;

use crate::app::App;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;

/// workflow 事件：审批弹窗（modal 占用时退化为胶囊计数）、run 面板实时投影、
/// 活跃 run 胶囊与终态通知。
pub(super) fn apply_workflow_update(
    app: &mut App,
    run_id: String,
    event: moc_protocol::methods::WorkflowEvent,
) {
    use moc_protocol::methods::WorkflowEvent as E;
    match event {
        E::ApprovalRequested { request } => {
            // modal 占用时完整载荷进队列（丢载荷该 run 的审批永远到不了，run 挂死）；
            // 空档时直接弹窗。
            if app.modal.is_some() {
                app.workflow.queued_approvals.push_back(request);
            } else {
                app.set_modal(Some(Modal::WorkflowApproval {
                    state: Box::new(crate::workflow::WorkflowApprovalState::new(request)),
                }));
            }
        }
        E::ApprovalResolved { request_id, .. } => {
            app.workflow
                .queued_approvals
                .retain(|a| a.request_id != request_id);
            // 打开的审批窗或最小化票正是该 request → 一并关掉/清掉。
            let modal_is_request = matches!(
                app.modal.as_ref(),
                Some(Modal::WorkflowApproval { state }) if state.request.request_id == request_id
            );
            if modal_is_request {
                app.set_modal(None);
            }
            if app
                .workflow
                .parked_approval
                .as_ref()
                .is_some_and(|s| s.request.request_id == request_id)
            {
                app.workflow.parked_approval = None;
            }
        }
        E::RunStatus { status } => {
            let active = matches!(
                status,
                moc_protocol::methods::WorkflowRunStatus::Running
                    | moc_protocol::methods::WorkflowRunStatus::Pending
            );
            if active {
                app.workflow
                    .active_runs
                    .entry(run_id.clone())
                    .or_insert_with(|| run_id.clone());
            } else {
                let title = app.workflow.active_runs.remove(&run_id);
                app.set_notice(format!(
                    "workflow run {} {}",
                    title.unwrap_or_else(|| run_id.clone()),
                    crate::workflow::status_label(status)
                ));
            }
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.status = status;
            }
        }
        E::PhaseEntered { phase } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.log_line(format!("phase → {phase}"));
            }
        }
        E::NodeUpdated { phases, roles } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.phases = phases;
                state.roles = roles;
            }
        }
        E::Log { level, message } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.log_line(format!("[{level}] {message}"));
            }
        }
        E::Report { item, .. } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.reports.push(item);
            }
        }
        E::Artifact { artifact } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.artifacts.push(artifact);
            }
        }
        E::HoleOpened { name, prompt, .. } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state
                    .holes
                    .push(serde_json::json!({"name": name, "prompt": prompt}));
            }
            app.set_notice(t!("tui.event.hole_opened_fmt", name = name.as_str()).into_owned());
        }
        E::HoleFilled { name } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.holes.retain(|hole| {
                    hole.get("name").and_then(|v| v.as_str()) != Some(name.as_str())
                });
            }
        }
        E::Usage { usage } => {
            if let Some(Modal::WorkflowRun { state }) = app.modal.as_mut()
                && state.run_id == run_id
            {
                state.usage = usage;
            }
        }
    }
}

/// 胶囊点击取下一个待开审批：最小化（parked）票优先，其次队首排队请求。
pub(super) fn take_next_approval(
    app: &mut App,
) -> Option<Box<crate::workflow::WorkflowApprovalState>> {
    if let Some(state) = app.workflow.parked_approval.take() {
        return Some(state);
    }
    app.workflow
        .queued_approvals
        .pop_front()
        .map(|request| Box::new(crate::workflow::WorkflowApprovalState::new(request)))
}

/// 打开 workflow run 列表：`session/workflow_runs` → `AppEvent::OpenWorkflowRuns`。
pub(super) fn open_workflow_runs(app: &mut App, connection: &EmbeddedConnection) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        app.set_notice(t!("tui.common.no_session").into_owned());
        return;
    };
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<moc_protocol::methods::SessionWorkflowRuns>(
                moc_protocol::methods::SessionWorkflowRunsParams { session_id },
            )
            .await
        {
            Ok(result) => {
                let _ = events.send(AppEvent::OpenWorkflowRuns(result.runs));
            }
            Err(error) => {
                let _ = events.send(AppEvent::Notice(
                    t!("tui.event.runs_failed_fmt", err = error.to_string()).into_owned(),
                ));
            }
        }
    });
}

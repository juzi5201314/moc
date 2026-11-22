//! workflow 动态 actor 的生命周期权限；公开消息和会话提示不能绕过 run。

use std::sync::Arc;

use moc_protocol::SessionInfo;

use crate::{CoreError, service::CoreInner};

pub(crate) async fn active_run(
    inner: &CoreInner,
    parent_id: &str,
    run_id: &str,
) -> Result<Arc<crate::workflow::runtime::RunHandle>, CoreError> {
    let runtime = inner.runtime(parent_id).await;
    runtime
        .workflows
        .get()
        .and_then(|manager| manager.running(run_id))
        .filter(|run| !run.cancel.is_cancelled() && run.view.lock().terminal.is_none())
        .ok_or_else(|| super::invalid("workflow actor can only run within an active run of its parent session"))
}

async fn run_id(inner: &CoreInner, session: &SessionInfo) -> Result<Option<String>, CoreError> {
    let record = inner.sessions.get(&session.id).await?;
    Ok(record
        .state
        .lock()
        .await
        .agent
        .as_ref()
        .and_then(|meta| meta.workflow_run.clone()))
}

pub(crate) async fn actor_run(
    inner: &CoreInner,
    session: &SessionInfo,
) -> Result<Option<Arc<crate::workflow::runtime::RunHandle>>, CoreError> {
    let Some(run_id) = run_id(inner, session).await? else {
        return Ok(None);
    };
    let parent = session
        .parent_session_id
        .as_deref()
        .ok_or_else(|| super::invalid("workflow actor is missing its parent session"))?;
    Ok(Some(active_run(inner, parent, &run_id).await?))
}

pub(crate) async fn authorize_message(
    inner: &CoreInner,
    source: &SessionInfo,
    target: &SessionInfo,
    driver_run: Option<&str>,
) -> Result<(), CoreError> {
    let source_run = run_id(inner, source).await?;
    let target_run = run_id(inner, target).await?;
    if let Some(run_id) = &source_run {
        let parent = source
            .parent_session_id
            .as_deref()
            .ok_or_else(|| super::invalid("workflow actor is missing its parent session"))?;
        active_run(inner, parent, run_id).await?;
        if target.id == parent {
            return Ok(());
        }
    }
    if let Some(run_id) = &target_run {
        let parent = target
            .parent_session_id
            .as_deref()
            .ok_or_else(|| super::invalid("workflow actor is missing its parent session"))?;
        active_run(inner, parent, run_id).await?;
        if (driver_run == Some(run_id.as_str()) && source.id == parent)
            || (source_run.as_ref() == Some(run_id)
                && source.parent_session_id == target.parent_session_id)
        {
            return Ok(());
        }
        return Err(super::invalid(
            "dynamic workflow actors can only be messaged by their workflow driver or same-run actors",
        ));
    }
    if source_run.is_some() || driver_run.is_some() {
        return Err(super::invalid("workflow messages cannot dispatch work across runs"));
    }
    Ok(())
}

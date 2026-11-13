//! 工作流协议操作。

use super::Core;
use super::errors::write_lease_error;
use super::workflow_manager;
use crate::error::CoreError;
use crate::service::to_rpc;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::methods::SessionInfo;

pub(super) async fn session_workflow_activate(
    core: &Core,
    ctx: RequestContext,
    params: moc_protocol::SessionWorkflowActivateParams,
) -> Result<SessionInfo, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    {
        let state = record.state.lock().await;
        let config = core
            .inner
            .project_config(std::path::Path::new(&state.session.cwd))
            .map_err(to_rpc)?;
        if !config.workflow.enabled {
            return Err(to_rpc(CoreError::Config(
                "workflow is disabled ([workflow] enabled = false)".into(),
            )));
        }
    }
    // 审批流依赖 ask 反向请求能力；无审批能力的连接（--print 等）不给激活。
    if !core.inner.client_supports_ask(ctx.connection_id()) {
        return Err(to_rpc(CoreError::Session(
            "workflows require approval capability; this connection cannot answer approvals".into(),
        )));
    }
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let already = record.state.lock().await.workflow_activated;
    if !already {
        core.inner
            .sessions
            .append(
                &params.session_id,
                crate::session::entries::EntryPayload::WorkflowActivated { active: true },
            )
            .await
            .map_err(to_rpc)?;
    }
    // 预热 run 目录的 interrupted 标记（首次激活即建管理器）。
    let _ = workflow_manager(&core.inner, &record).await;
    Ok(record.state.lock().await.session.clone())
}

pub(super) async fn session_workflow_approve(
    core: &Core,
    ctx: RequestContext,
    params: moc_protocol::SessionWorkflowApproveParams,
) -> Result<moc_protocol::WorkflowAck, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let manager = workflow_manager(&core.inner, &record).await;
    manager
        .resolve_approval(&params.request_id, params.decision, &ctx.updates())
        .map_err(to_rpc)?;
    Ok(moc_protocol::WorkflowAck {
        session_id: params.session_id,
    })
}

pub(super) async fn session_workflow_runs(
    core: &Core,
    _ctx: RequestContext,
    params: moc_protocol::SessionWorkflowRunsParams,
) -> Result<moc_protocol::SessionWorkflowRunsResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let manager = workflow_manager(&core.inner, &record).await;
    Ok(moc_protocol::SessionWorkflowRunsResult {
        runs: manager.list(),
    })
}

pub(super) async fn session_workflow_run(
    core: &Core,
    _ctx: RequestContext,
    params: moc_protocol::SessionWorkflowRunParams,
) -> Result<moc_protocol::WorkflowRunSnapshot, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let manager = workflow_manager(&core.inner, &record).await;
    let meta = manager.meta(&params.run_id).map_err(to_rpc)?;
    let running = manager.running(&params.run_id);
    let view = running.as_ref().map(|handle| handle.view.lock());
    Ok(crate::workflow::runtime::snapshot_of(
        &meta,
        view.as_deref(),
        &manager.run_dir(&params.run_id).map_err(to_rpc)?,
    ))
}

pub(super) async fn session_workflow_stop(
    core: &Core,
    ctx: RequestContext,
    params: moc_protocol::SessionWorkflowRunParams,
) -> Result<moc_protocol::WorkflowRunSummary, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let manager = workflow_manager(&core.inner, &record).await;
    manager.stop(&params.run_id).await.map_err(to_rpc)
}

pub(super) async fn session_workflow_resume(
    core: &Core,
    ctx: RequestContext,
    params: moc_protocol::SessionWorkflowRunParams,
) -> Result<moc_protocol::WorkflowRunSummary, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let manager = workflow_manager(&core.inner, &record).await;
    // resume 与 activate 同一 enabled 门禁。
    let cwd = record.state.lock().await.session.cwd.clone();
    let config = core
        .inner
        .project_config(std::path::Path::new(&cwd))
        .map_err(to_rpc)?;
    if !config.workflow.enabled {
        return Err(to_rpc(CoreError::Config(
            "workflow is disabled ([workflow] enabled = false)".into(),
        )));
    }
    // resume 与工具走同一条 prepare_resume：stopped-only + 命令集一致性校验。
    let (meta, compiled) = manager.prepare_resume(&params.run_id).map_err(to_rpc)?;
    let cwd_path = std::path::PathBuf::from(&cwd);
    let policy = {
        let state = record.state.lock().await;
        let runtime = core.inner.runtime(&params.session_id).await;
        let grants = runtime.sandbox_grants.read().clone();
        let permission = core.inner.effective_permission(state.permission);
        crate::sandbox::SandboxPolicy::new(
            permission,
            cwd_path.clone(),
            &config.sandbox,
            &grants,
            &crate::sandbox::SandboxGrants::default(),
            runtime.sandbox_scratch.clone(),
        )
    };
    manager
        .launch(
            meta,
            compiled,
            ctx.clone(),
            cwd_path,
            policy,
            ctx.updates(),
            core.inner.shutdown.child_token(),
        )
        .await
        .map_err(to_rpc)?;
    let summary = manager.meta(&params.run_id).map_err(to_rpc)?;
    Ok(crate::workflow::runtime::workflow_summary(&summary))
}

pub(super) async fn session_workflow_set_role_model(
    core: &Core,
    ctx: RequestContext,
    params: moc_protocol::SessionWorkflowSetRoleModelParams,
) -> Result<moc_protocol::WorkflowAck, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let manager = workflow_manager(&core.inner, &record).await;
    manager
        .set_role_model(&params.run_id, &params.role_id, &params.model)
        .map_err(to_rpc)?;
    Ok(moc_protocol::WorkflowAck {
        session_id: params.session_id,
    })
}

pub(super) async fn session_workflow_artifact(
    core: &Core,
    _ctx: RequestContext,
    params: moc_protocol::SessionWorkflowArtifactParams,
) -> Result<moc_protocol::WorkflowArtifactResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let manager = workflow_manager(&core.inner, &record).await;
    // artifact_id 参与拼路径：与编译期同一字符集校验，run_id 走 run_dir 校验。
    if !crate::workflow::compile::valid_artifact_id(&params.artifact_id) {
        return Err(to_rpc(CoreError::Session(format!(
            "invalid artifact id \"{}\"",
            params.artifact_id
        ))));
    }
    let dir = manager
        .run_dir(&params.run_id)
        .map_err(to_rpc)?
        .join("artifacts")
        .join(&params.artifact_id);
    let mut versions: Vec<(u32, std::path::PathBuf)> = std::fs::read_dir(&dir)
        .map_err(|e| to_rpc(CoreError::Session(format!("artifact dir unreadable: {e}"))))?
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let version = name
                .strip_prefix('v')
                .and_then(|s| s.split('_').next())
                .and_then(|s| s.parse::<u32>().ok())?;
            Some((version, e.path()))
        })
        .collect();
    versions.sort_by_key(|(v, _)| *v);
    let (version, path) = match params.version {
        Some(v) => versions
            .into_iter()
            .find(|(found, _)| *found == v)
            .ok_or_else(|| {
                to_rpc(CoreError::Session(format!(
                    "artifact {} version {v} not found",
                    params.artifact_id
                )))
            })?,
        None => versions.last().cloned().ok_or_else(|| {
            to_rpc(CoreError::Session(format!(
                "artifact {} has no versions",
                params.artifact_id
            )))
        })?,
    };
    let data = std::fs::read(&path)
        .map_err(|e| to_rpc(CoreError::Session(format!("artifact unreadable: {e}"))))?;
    let content = serde_json::from_slice::<serde_json::Value>(&data)
        .unwrap_or_else(|_| serde_json::Value::String(String::from_utf8_lossy(&data).to_string()));
    Ok(moc_protocol::WorkflowArtifactResult {
        artifact_id: params.artifact_id,
        version,
        content,
    })
}

//! 会话配置变更。

use super::Core;
use super::errors::write_lease_error;
use super::mode;
use super::model;
use crate::service::emit_session_config_changed;
use crate::service::invalid_params;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::features::flag_for_engine;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionSetAutonomyParams;
use moc_protocol::methods::SessionSetEngineParams;
use moc_protocol::methods::SessionSetFastModeParams;
use moc_protocol::methods::SessionSetModelParams;
use moc_protocol::methods::SessionSetModelResult;
use moc_protocol::methods::SessionSetPermissionParams;
use moc_protocol::methods::SessionUpdate;

pub(super) async fn session_set_permission(
    core: &Core,
    ctx: RequestContext,
    params: SessionSetPermissionParams,
) -> Result<SessionInfo, RpcError> {
    if !core.inner.sandbox_support.supports(params.permission) {
        let available = core
            .inner
            .sandbox_support
            .modes
            .iter()
            .map(|mode| mode.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        return Err(invalid_params(format!(
            "当前平台不支持 {} 权限；可用模式：{available}",
            params.permission.as_str()
        )));
    }
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let old_permission = record.state.lock().await.permission;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let mut info = core
        .inner
        .sessions
        .set_permission(&params.session_id, params.permission)
        .await
        .map_err(to_rpc)?;
    if params.clear_grants || old_permission != params.permission {
        core.inner
            .runtime(&params.session_id)
            .await
            .sandbox_grants
            .write()
            .clear();
    }
    info.lock = None;
    info.permission = core.inner.effective_permission(info.permission);
    ctx.updates()
        .emit(
            params.session_id,
            SessionUpdate::PermissionChanged {
                permission: info.permission,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    let state = record.state.lock().await.metadata();
    emit_session_config_changed(
        &core.inner,
        &state,
        "permission",
        old_permission.as_str(),
        info.permission.as_str(),
    )
    .await;
    Ok(info)
}

pub(super) async fn session_set_engine(
    core: &Core,
    ctx: RequestContext,
    params: SessionSetEngineParams,
) -> Result<SessionInfo, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    // 特性门控按会话所属项目判定；锁定状态下也要先挡住禁用引擎。
    if let Some(flag) = flag_for_engine(params.engine) {
        let cwd = record.state.lock().await.session.cwd.clone();
        let config = core
            .inner
            .project_config(std::path::Path::new(&cwd))
            .map_err(to_rpc)?;
        if !config.feature_enabled(flag) {
            return Err(invalid_params(format!(
                "引擎 {} 已被特性开关 [features] {} = false 禁用",
                serde_json::to_value(params.engine)
                    .ok()
                    .and_then(|value| value.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                flag.spec().key,
            )));
        }
    }
    let from = record.state.lock().await.engine;
    // 引擎锁在 `Sessions::set_engine` 里判定：有提示历史即拒绝。
    let mut info = core
        .inner
        .sessions
        .set_engine(&params.session_id, params.engine)
        .await
        .map_err(to_rpc)?;
    info.lock = None;
    info.permission = core.inner.effective_permission(info.permission);
    mode::emit_engine_change(&ctx, &params.session_id, info.engine).await?;
    let state = record.state.lock().await.metadata();
    emit_session_config_changed(
        &core.inner,
        &state,
        "engine",
        &serde_json::to_value(from)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
        &serde_json::to_value(info.engine)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
    )
    .await;
    Ok(info)
}

pub(super) async fn session_set_autonomy(
    core: &Core,
    ctx: RequestContext,
    params: SessionSetAutonomyParams,
) -> Result<SessionInfo, RpcError> {
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
    let from = record.state.lock().await.session.autonomy;
    let mut info = core
        .inner
        .sessions
        .set_autonomy(&params.session_id, params.autonomy)
        .await
        .map_err(to_rpc)?;
    info.lock = None;
    info.permission = core.inner.effective_permission(info.permission);
    mode::emit_autonomy_change(&ctx, &params.session_id, info.autonomy).await?;
    let state = record.state.lock().await.metadata();
    emit_session_config_changed(
        &core.inner,
        &state,
        "autonomy",
        &serde_json::to_value(from)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
        &serde_json::to_value(info.autonomy)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
    )
    .await;
    Ok(info)
}

pub(super) async fn session_set_fast_mode(
    core: &Core,
    ctx: RequestContext,
    params: SessionSetFastModeParams,
) -> Result<SessionInfo, RpcError> {
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
    let mut info = core
        .inner
        .sessions
        .set_fast_mode(&params.session_id, params.enabled)
        .await
        .map_err(to_rpc)?;
    info.lock = None;
    info.permission = core.inner.effective_permission(info.permission);
    ctx.updates()
        .emit(
            params.session_id,
            SessionUpdate::FastModeChanged {
                enabled: info.fast_mode,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(info)
}

pub(super) async fn session_set_model(
    core: &Core,
    ctx: RequestContext,
    params: SessionSetModelParams,
) -> Result<SessionSetModelResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    let (cwd, model_before) = {
        let state = record.state.lock().await;
        (state.session.cwd.clone(), state.model.clone())
    };
    let config = core
        .inner
        .project_config(std::path::Path::new(&cwd))
        .map_err(to_rpc)?;
    let model = config.resolve_model(&params.selector).map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let outcome = model::switch(
        &core.inner,
        &ctx,
        &params.session_id,
        &model,
        params.overflow,
    )
    .await
    .map_err(to_rpc)?;
    let mut session = outcome.session;
    session.lock = None;
    session.permission = core.inner.effective_permission(session.permission);
    let state = record.state.lock().await.metadata();
    emit_session_config_changed(&core.inner, &state, "model", &model_before, &session.model).await;
    if session.model == model.selector
        && !session.is_subagent()
        && matches!(ctx.client().frontend.as_str(), "tui" | "webui" | "acp")
    {
        crate::config::selection::save(&config.home, &model.selector).map_err(to_rpc)?;
    }
    Ok(SessionSetModelResult {
        session,
        overflow: outcome.overflow,
        compaction_error: outcome.compaction_error,
    })
}

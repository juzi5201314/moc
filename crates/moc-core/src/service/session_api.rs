//! 会话生命周期与历史操作。

use super::Core;
use super::errors::write_lease_error;
use super::goal_with_armed;
use super::mode;
use super::plugin_events;
use super::prompt;
use crate::service::invalid_params;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use crate::session::TranscriptEntryId;
use crate::session::entries::SessionOrigin;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::features::flag_for_engine;
use moc_protocol::methods::Empty;
use moc_protocol::methods::SessionDeleteParams;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionLoadPageParams;
use moc_protocol::methods::SessionLoadPageResult;
use moc_protocol::methods::SessionLoadParams;
use moc_protocol::methods::SessionLoadResult;
use moc_protocol::methods::SessionNewParams;
use moc_protocol::methods::SessionPlanApproveParams;
use moc_protocol::methods::SessionPlanApproveResult;
use moc_protocol::methods::SessionReaskParams;
use moc_protocol::methods::SessionRedoParams;
use moc_protocol::methods::SessionRenameParams;
use moc_protocol::methods::SessionRewindParams;
use moc_protocol::methods::SessionUnloadParams;
use moc_protocol::methods::SessionUnlockParams;
use moc_protocol::methods::SessionUpdate;
use rust_i18n::t;
use serde_json::json;
use std::path::PathBuf;

pub(super) async fn session_new(
    core: &Core,
    ctx: RequestContext,
    params: SessionNewParams,
) -> Result<SessionInfo, RpcError> {
    let cwd = params
        .cwd
        .as_deref()
        .map(PathBuf::from)
        .unwrap_or_else(|| core.inner.config().cwd.clone());
    let cwd = std::fs::canonicalize(&cwd)
        .map_err(|error| invalid_params(t!("core.err.cwd_invalid", error = error).into_owned()))?;
    let config = core.inner.project_config(&cwd).map_err(to_rpc)?;
    if let Some(flag) = flag_for_engine(params.engine)
        && !config.feature_enabled(flag)
    {
        return Err(invalid_params(format!(
            "引擎 {} 已被特性开关 [features] {} = false 禁用",
            serde_json::to_value(params.engine)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            flag.spec().key,
        )));
    }
    let model = match params.model.as_deref() {
        Some(selector) => config.resolve_model(selector),
        None => config.require_last_model(),
    }
    .map_err(|error| {
        let selectors: Vec<String> = config
            .available_models()
            .into_iter()
            .map(|model| model.selector)
            .take(10)
            .collect();
        RpcError::of(RpcErrorCode::InvalidParams).with_data(json!({
            "message": error.to_string(),
            "available_models": selectors,
        }))
    })?;
    let permission = config.sandbox.default_mode;
    if !core.inner.sandbox_support.supports(permission) {
        return Err(invalid_params(format!(
            "sandbox.default_mode={} 在当前平台不可用，请改用 full 或调整配置",
            permission.as_str()
        )));
    }
    let mut info = core
        .inner
        .sessions
        .create(
            &cwd,
            &model.selector,
            params.engine,
            permission,
            params.title.as_deref(),
            SessionOrigin::root(),
            config.session.compress,
        )
        .await
        .map_err(to_rpc)?;
    let record = core.inner.sessions.get(&info.id).await.map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&info.id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &info.id, &record, error))?;
    core.inner.warm_mcp(&cwd).await;
    if params.plan_active {
        info = core
            .inner
            .sessions
            .set_plan_active(&info.id, true)
            .await
            .map_err(to_rpc)?;
    }
    if params.fast_mode {
        info = core
            .inner
            .sessions
            .set_fast_mode(&info.id, true)
            .await
            .map_err(to_rpc)?;
    }
    info.permission = core.inner.effective_permission(info.permission);
    if let Some(host) = plugin_events::host_for(&core.inner, std::path::Path::new(&info.cwd)).await
    {
        let context = plugin_events::context_from_info(&info);
        for message in plugin_events::session_start(&host, &context, &info, "user", None).await {
            let role = if message.role == "system" {
                moc_llm::Role::System
            } else {
                moc_llm::Role::User
            };
            core.inner
                .sessions
                .append_message(
                    &info.id,
                    moc_llm::Message {
                        role,
                        content: vec![moc_llm::ContentPart::Text(message.text)].into(),
                    },
                    None,
                    None,
                )
                .await
                .map_err(to_rpc)?;
        }
    }
    if params.model.is_some() && matches!(ctx.client().frontend.as_str(), "tui" | "webui" | "acp") {
        crate::config::selection::save(&config.home, &model.selector).map_err(to_rpc)?;
    }
    Ok(info)
}

pub(super) async fn session_load(
    core: &Core,
    ctx: RequestContext,
    params: SessionLoadParams,
) -> Result<SessionLoadResult, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    if !params.read_only {
        core.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    }
    let mut result = core
        .inner
        .sessions
        .load_projected(
            &params.session_id,
            params.max_entries,
            params.omit_auxiliary,
        )
        .await
        .map_err(to_rpc)?;
    result.session.permission = core.inner.effective_permission(result.session.permission);
    if result.goal.is_some() {
        result.goal = goal_with_armed(&core.inner, &params.session_id, &record).await;
    }
    // 只读查看不会在这里发提示，不为它拉起 MCP。
    if !params.read_only {
        core.inner
            .warm_mcp(std::path::Path::new(&result.session.cwd))
            .await;
    }
    Ok(result)
}

pub(super) async fn session_load_page(
    core: &Core,
    _ctx: RequestContext,
    params: SessionLoadPageParams,
) -> Result<SessionLoadPageResult, RpcError> {
    let mut result = core
        .inner
        .sessions
        .load_pages(
            &params.session_id,
            params.before_entry_id.as_deref(),
            params.max_entries,
            params.max_message_bytes,
            params.redo_page.as_ref(),
            params.plan_page.as_ref(),
            params.full_messages,
        )
        .await
        .map_err(to_rpc)?;
    result.session.permission = core.inner.effective_permission(result.session.permission);
    if result.goal.is_some() {
        let record = core
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        result.goal = goal_with_armed(&core.inner, &params.session_id, &record).await;
    }
    Ok(result)
}

pub(super) async fn session_rewind(
    core: &Core,
    ctx: RequestContext,
    params: SessionRewindParams,
) -> Result<SessionLoadResult, RpcError> {
    let entry_id = TranscriptEntryId::new(params.entry_id);
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
    let mut loaded = core
        .inner
        .sessions
        .rewind_with_limit(
            &params.session_id,
            &entry_id,
            params.max_entries,
            params.omit_auxiliary,
        )
        .await
        .map_err(to_rpc)?;
    loaded.session.lock = None;
    loaded.session.permission = core.inner.effective_permission(loaded.session.permission);
    if loaded.goal.is_some() {
        loaded.goal = goal_with_armed(&core.inner, &params.session_id, &record).await;
    }
    // 原地回退不是新会话开始，不再注入插件 session_start 消息——
    // 那会向截断后的历史追加新条目，使刚压栈的前滚尾部立刻失效。
    // rewind 已把「尾部引用过、前缀不引用」的子代理标成孤儿，还在跑的要停掉。
    core.stop_orphaned_agents(&ctx, &params.session_id).await;
    Ok(loaded)
}

pub(super) async fn session_reask(
    core: &Core,
    ctx: RequestContext,
    params: SessionReaskParams,
) -> Result<SessionLoadResult, RpcError> {
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
    prompt::run_reask(
        &core.inner,
        &ctx,
        &params.session_id,
        &record,
        &params.tool_call_id,
    )
    .await?;
    // 与 rewind 相同：截断标记的孤儿子代理还在跑的要停掉。
    core.stop_orphaned_agents(&ctx, &params.session_id).await;
    let mut loaded = core
        .inner
        .sessions
        .load_projected(
            &params.session_id,
            params.max_entries,
            params.omit_auxiliary,
        )
        .await
        .map_err(to_rpc)?;
    loaded.session.lock = None;
    loaded.session.permission = core.inner.effective_permission(loaded.session.permission);
    if loaded.goal.is_some() {
        loaded.goal = goal_with_armed(&core.inner, &params.session_id, &record).await;
    }
    Ok(loaded)
}

pub(super) async fn session_redo(
    core: &Core,
    ctx: RequestContext,
    params: SessionRedoParams,
) -> Result<SessionLoadResult, RpcError> {
    let entry_id = TranscriptEntryId::new(params.entry_id);
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
    let mut loaded = core
        .inner
        .sessions
        .redo_with_limit(
            &params.session_id,
            &entry_id,
            params.max_entries,
            params.omit_auxiliary,
        )
        .await
        .map_err(to_rpc)?;
    loaded.session.lock = None;
    loaded.session.permission = core.inner.effective_permission(loaded.session.permission);
    if loaded.goal.is_some() {
        loaded.goal = goal_with_armed(&core.inner, &params.session_id, &record).await;
    }
    Ok(loaded)
}

pub(super) async fn session_unload(
    core: &Core,
    _ctx: RequestContext,
    params: SessionUnloadParams,
) -> Result<Empty, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    // unload_locked 要求调用方持有提示锁：进行中的提示会因运行时状态被移除而 panic 或写入失败，
    // 而且这里会先杀掉提示正在用的后台任务。和其它修改类 RPC 一样，拿不到锁就返回 SESSION_BUSY。
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    let state = record.state.lock().await.metadata();
    if let Some(host) =
        plugin_events::host_for(&core.inner, std::path::Path::new(&state.session.cwd)).await
    {
        let context = plugin_events::context(
            &params.session_id,
            &state.session.cwd,
            state.engine,
            crate::service::mode::Mode::effective(state.plan_active, None),
            state.permission,
            &state.model,
            state.session.is_subagent(),
        );
        plugin_events::session_end(&host, &context, "unloaded").await;
    }
    let runtime = core.inner.runtime(&params.session_id).await;
    if let Some(workflows) = runtime.workflows.get() {
        workflows.shutdown().await;
    }
    runtime.jobs.kill_all().await;
    core.inner
        .sessions
        .unload_locked(&params.session_id, &record);
    core.inner
        .context_views
        .lock()
        .remove_session(&params.session_id);
    core.inner.runtimes.lock().await.remove(&params.session_id);
    Ok(Empty {})
}

pub(super) async fn session_rename(
    core: &Core,
    ctx: RequestContext,
    params: SessionRenameParams,
) -> Result<SessionInfo, RpcError> {
    let session_id = params.session_id.clone();
    let record = core.inner.sessions.get(&session_id).await.map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&session_id, &record, &ctx.client().frontend)
        .await
        .map_err(|error| write_lease_error(core, &session_id, &record, error))?;
    let mut info = core
        .inner
        .sessions
        .rename(&session_id, params)
        .await
        .map_err(to_rpc)?;
    info.lock = None;
    info.permission = core.inner.effective_permission(info.permission);
    ctx.updates()
        .emit(
            session_id,
            SessionUpdate::TitleChanged {
                title: info.title.clone(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(info)
}

pub(super) async fn session_delete(
    core: &Core,
    _ctx: RequestContext,
    params: SessionDeleteParams,
) -> Result<Empty, RpcError> {
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    core.inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, "moc-core")
        .await
        .map_err(|error| write_lease_error(core, &params.session_id, &record, error))?;
    let state = record.state.lock().await.metadata();
    if let Some(host) =
        plugin_events::host_for(&core.inner, std::path::Path::new(&state.session.cwd)).await
    {
        let context = plugin_events::context(
            &params.session_id,
            &state.session.cwd,
            state.engine,
            crate::service::mode::Mode::effective(state.plan_active, None),
            state.permission,
            &state.model,
            state.session.is_subagent(),
        );
        plugin_events::session_end(&host, &context, "deleted").await;
    }
    let runtime = core.inner.runtime(&params.session_id).await;
    if let Some(workflows) = runtime.workflows.get() {
        workflows.shutdown().await;
    }
    runtime.jobs.kill_all().await;
    core.inner
        .sessions
        .delete_locked(&params.session_id, &record)
        .await
        .map_err(to_rpc)?;
    core.inner
        .context_views
        .lock()
        .remove_session(&params.session_id);
    core.inner.runtimes.lock().await.remove(&params.session_id);
    Ok(Empty {})
}

pub(super) async fn session_unlock(
    core: &Core,
    _ctx: RequestContext,
    params: SessionUnlockParams,
) -> Result<Empty, RpcError> {
    if !core.inner.sessions.locking_enabled() {
        return Err(RpcError::of(RpcErrorCode::NotSupported));
    }
    let record = core
        .inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    if !core.inner.sessions.release_lease(&record)
        && let Some(info) = core.inner.sessions.lease_info(&params.session_id, &record)
    {
        return Err(RpcError::new(
            RpcErrorCode::SessionLocked.code(),
            format!(
                "会话正被 pid {} ({}) 持有；请先让持有者卸载或退出",
                info.pid, info.frontend
            ),
        )
        .with_data(json!({ "lock": info })));
    }
    Ok(Empty {})
}

pub(super) async fn session_plan_approve(
    core: &Core,
    ctx: RequestContext,
    params: SessionPlanApproveParams,
) -> Result<SessionPlanApproveResult, RpcError> {
    let plan_id = params.plan_id.clone();
    let action = match params.action {
        moc_protocol::methods::PlanAction::Execute => "execute",
        moc_protocol::methods::PlanAction::ExecuteCompacted => "execute_compacted",
        moc_protocol::methods::PlanAction::ExecuteNewSession => "execute_new_session",
        moc_protocol::methods::PlanAction::Reject => "reject",
    };
    let result = mode::approve_plan(&core.inner, ctx, params).await?;
    let record = core
        .inner
        .sessions
        .get(&result.session_id)
        .await
        .map_err(to_rpc)?;
    let state = record.state.lock().await.metadata();
    if let Some(host) =
        plugin_events::host_for(&core.inner, std::path::Path::new(&state.session.cwd)).await
    {
        let context = plugin_events::context(
            &result.session_id,
            &state.session.cwd,
            state.engine,
            crate::service::mode::Mode::effective(state.plan_active, None),
            state.permission,
            &state.model,
            state.session.is_subagent(),
        );
        plugin_events::plan_decided(&host, &context, &plan_id, action).await;
    }
    Ok(result)
}

//! 用户手动执行的 Bash：会话加锁、结果持久化和前端事件；不触发模型请求。

use std::{path::PathBuf, sync::Arc, time::Instant};

use moc_llm::ToolResult;
use moc_protocol::{
    BashParams, BashResult, RequestContext, RpcError, SessionUpdate, ToolExecution, ToolStatus,
};

use super::{CoreInner, invalid_params, protocol_to_rpc, to_rpc};
use crate::session::entries::EntryPayload;

pub(super) async fn run(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: BashParams,
) -> Result<BashResult, RpcError> {
    if params.command.command.trim().is_empty() {
        return Err(invalid_params("Bash 命令不能为空"));
    }
    let session_id = &params.session_id;
    let record = inner.sessions.get(session_id).await.map_err(to_rpc)?;
    let _operation = record.try_prompt_lock().map_err(to_rpc)?;
    let _epoch = inner.config_epoch.read().await;
    inner
        .sessions
        .ensure_write_lease(session_id, &record, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
    let config = inner.project_config(&cwd).map_err(to_rpc)?;
    let call_id = format!("bash_{}", crate::ids::entry_id());
    let update = ctx.updates();
    update
        .emit(
            session_id.clone(),
            SessionUpdate::BashStarted {
                tool_call_id: call_id.clone(),
                command: params.command.clone(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    let started = Instant::now();
    let cancel = ctx.cancel();
    let skills = inner.project_skills(&cwd).map_err(to_rpc)?;
    let outcome = crate::tools::bash::run_manual(
        params.command.command.clone(),
        cwd,
        &record.paths,
        &record.state,
        &skills,
        &config.tools.bash,
        &cancel,
    )
    .await
    .unwrap_or_else(|error| error.into_outcome());
    // 用户命令造成的工作区改动不能算到下一次 agent 提示头上，所以重新采集基线。
    *inner.runtime(session_id).await.workspace.lock().await = None;
    let duration_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    let artifacts = crate::resources::ArtifactStore::new(record.paths.clone());
    let mut is_error = outcome.is_error;
    let content = match crate::tools::output::prepare(
        outcome.content,
        &config.tools.output,
        &artifacts,
    )
    .await
    {
        Ok(content) => content,
        Err(error) => {
            is_error = true;
            format!("[E_ARTIFACT] 命令已执行，但保存输出失败：{error}")
        }
    };
    let status = outcome.status.unwrap_or(if is_error {
        ToolStatus::Error
    } else {
        ToolStatus::Completed
    });
    let result = ToolResult {
        tool_call_id: call_id.clone(),
        content: content.clone(),
        is_error,
        images: Vec::new(),
    };
    let arguments = serde_json::to_string(&params.command).expect("Bash 命令参数可以序列化");
    let command_text = params.command.command.clone();
    inner
        .sessions
        .append(
            session_id,
            EntryPayload::Bash {
                command: params.command,
                output: content.clone(),
                tool: ToolExecution {
                    tool_call_id: call_id.clone(),
                    status,
                    duration_ms: Some(duration_ms),
                },
            },
        )
        .await
        .map_err(to_rpc)?;
    update
        .emit(
            session_id.clone(),
            SessionUpdate::ToolFinished {
                prompt_id: call_id.clone(),
                tool_call_id: call_id,
                name: "bash".to_owned(),
                parent_tool_call_id: None,
                arguments,
                status,
                result: result.clone(),
                duration_ms: Some(duration_ms),
                diff: None,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    let state = record.state.lock().await.clone();
    if let Some(host) =
        crate::service::plugin_events::host_for(&inner, std::path::Path::new(&state.session.cwd))
            .await
    {
        let context = crate::service::plugin_events::context(
            session_id,
            &state.session.cwd,
            state.mode,
            crate::service::mode::ModeContract::effective(state.plan_active, None),
            state.permission,
            &state.model,
            state.session.is_subagent(),
        );
        let status_name = serde_json::to_value(status)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        crate::service::plugin_events::user_command(
            &host,
            &context,
            &command_text,
            duration_ms,
            is_error,
            &status_name,
            &content,
        )
        .await;
    }
    Ok(BashResult {
        result,
        status,
        duration_ms,
    })
}

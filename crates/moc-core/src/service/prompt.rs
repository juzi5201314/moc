//! 流式提示编排、取消收尾、持久化与工具执行。

use crate::{
    compact,
    compact::replay::model_messages,
    service::{CoreInner, protocol_to_rpc, snapshot_workspace, to_rpc},
    session::store::now_ms,
    system_prompt::{build, merge_reminders, mode_reminder, reminder_message},
    tools::ToolCtx,
};
use moc_llm::{FinishReason, Message, Usage};
use moc_protocol::{
    RequestContext, RpcError, RpcErrorCode,
    methods::{PromptResult, SessionUpdate, ToolStatus},
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

pub(crate) mod request;
mod stream;
pub(crate) mod tools;
mod workspace;

use request::{build_request, project_input, upstream_mismatches};
use stream::forward_event;
use tools::{execute_tool_batch, persist_tool_result, repair_pending_calls};
use workspace::{WorkspaceScope, check_workspace};

pub async fn run_prompt(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
) -> Result<PromptResult, RpcError> {
    let session_id = params.session_id.clone();
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
    let _epoch = inner.config_epoch.read().await;
    let mut file_lock = inner
        .sessions
        .acquire_lock(&params.session_id, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let update = ctx.updates();
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::LockChanged {
                lock: file_lock
                    .as_ref()
                    .map(crate::session::lock::SessionLockGuard::info),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;

    repair_pending_calls(&inner, &session_id, &record)
        .await
        .map_err(to_rpc)?;
    inner
        .runtime(&session_id)
        .await
        .steering
        .lock()
        .await
        .accepting = true;
    let result = run_prompt_locked(Arc::clone(&inner), &ctx, params, Arc::clone(&record)).await;
    inner
        .runtime(&session_id)
        .await
        .steering
        .lock()
        .await
        .accepting = false;
    repair_pending_calls(&inner, &session_id, &record)
        .await
        .map_err(to_rpc)?;
    super::management::drain_steering(&inner, &session_id, true)
        .await
        .map_err(to_rpc)?;
    file_lock.take();
    let unlocked = update
        .emit(
            session_id.clone(),
            SessionUpdate::LockChanged { lock: None },
        )
        .await
        .map_err(protocol_to_rpc);
    match result {
        Err(error) => Err(error),
        Ok(outcome) => {
            unlocked?;
            Ok(outcome)
        }
    }
}

async fn run_prompt_locked(
    inner: Arc<CoreInner>,
    ctx: &RequestContext,
    params: moc_protocol::methods::PromptParams,
    record: Arc<crate::session::SessionRecord>,
) -> Result<PromptResult, RpcError> {
    let update = ctx.updates();
    let state_snapshot = record.state.lock().await.clone();
    let project_config = inner
        .project_config(std::path::Path::new(&state_snapshot.session.cwd))
        .map_err(to_rpc)?;
    let model = project_config
        .resolve_model(&state_snapshot.model)
        .map_err(to_rpc)?;
    let user_message = project_input(
        params.input,
        model.capabilities.image,
        std::path::Path::new(&state_snapshot.session.cwd),
    )
    .await
    .map_err(to_rpc)?;
    if user_message.content.is_empty() {
        return Err(RpcError::of(RpcErrorCode::InvalidParams));
    }
    let user_message =
        apply_skill_invocation(&inner, user_message, &state_snapshot.session.cwd).await?;
    inner
        .sessions
        .append_message(&params.session_id, user_message.clone(), None, None)
        .await
        .map_err(to_rpc)?;
    if project_config.openviking.capture
        && let Some(client) = inner
            .project_openviking(std::path::Path::new(&state_snapshot.session.cwd))
            .await
            .map_err(to_rpc)?
        && let Ok(identity) =
            inner.workspace_identity(std::path::Path::new(&state_snapshot.session.cwd))
    {
        let ov_session = openviking_session_id(&params.session_id);
        let _ = client.create_session(&ov_session, &identity.peer_id).await;
        let message = serde_json::json!({"role":"user","parts":[{"type":"text","text":truncate_memory(&user_message.text())}]});
        let _ = client
            .append_messages(&ov_session, &[message], &identity.peer_id)
            .await;
    }

    if state_snapshot.session.title == "新会话" {
        let title = user_message
            .text()
            .lines()
            .next()
            .unwrap_or_default()
            .chars()
            .filter(|character| !character.is_control())
            .take(60)
            .collect::<String>();
        if !title.trim().is_empty() {
            let info = inner
                .sessions
                .rename(
                    &params.session_id,
                    moc_protocol::methods::SessionRenameParams {
                        session_id: params.session_id.clone(),
                        title: title.trim().to_owned(),
                    },
                )
                .await
                .map_err(to_rpc)?;
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::TitleChanged { title: info.title },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
    }

    let runtime = inner.runtime(&params.session_id).await;
    let state = record.state.lock().await;
    let prompt_id = crate::ids::prompt_id(state.prompt_sequence);
    // `/ask` 等临时意图通过 `PromptParams::mode` 覆盖本条用户消息的模式：
    // 一次 `session/prompt` 内部的全部 turn（多次模型请求 + 各组工具调用）共用
    // 这一个值，且不落地为会话状态。`session/set_mode` 走 `try_prompt_lock`，
    // 提示在飞时无法调进来改写它。
    let mode = params.mode.unwrap_or(state.mode);
    let permission = inner.effective_permission(state.permission);
    let cwd = std::path::PathBuf::from(&state.session.cwd);
    drop(state);

    if project_config.workspace.watch && runtime.workspace.lock().await.is_none() {
        let baseline = snapshot_workspace(cwd.clone(), project_config.workspace.max_scan_entries)
            .await
            .map_err(to_rpc)?;
        *runtime.workspace.lock().await = Some(baseline);
    }
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::PromptStarted {
                prompt_id: prompt_id.clone(),
                model: model.selector.clone(),
                at_ms: now_ms(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;

    let cancel = ctx.cancel();
    // 一次提示固定发现与执行目录，服务器刷新留到下一次提示生效。
    let mcp = inner
        .project_mcp(std::path::Path::new(&state_snapshot.session.cwd))
        .await
        .map_err(to_rpc)?;
    mcp.start().await;
    for notice in mcp.drain_notices().await {
        record
            .state
            .lock()
            .await
            .notices
            .push(format!("MCP `{}`：{}", notice.server, notice.message));
    }
    let mcp_snapshot = mcp.snapshot().await;
    let catalog = Arc::new(
        inner
            .tools
            .catalog(&mcp_snapshot, Arc::clone(&mcp))
            .map_err(|error| to_rpc(crate::CoreError::Tool(error.to_string())))?,
    );
    let skills = inner
        .project_skills(std::path::Path::new(&state_snapshot.session.cwd))
        .map_err(to_rpc)?;
    let tools_available = inner.tools.definitions(
        inner.client_supports_ask(ctx.connection_id()),
        &project_config,
    );
    let mut last_message = Message::default();
    let mut final_reason = FinishReason::Stop;
    let mut usage = Usage::default();
    let mut length_continues = 0usize;
    let mut stream_overflows = 0usize;
    let summary = catalog.summary();
    let context_files = inner.project_context_files(&params.session_id, &cwd).await;
    let system = build(
        &project_config,
        &model,
        &cwd,
        &context_files,
        &skills,
        &summary,
    );

    'turns: for turn in 0..project_config.limits.max_turns_per_prompt {
        if cancel.is_cancelled() {
            final_reason = FinishReason::Cancelled;
            break;
        }
        super::management::drain_steering(&inner, &params.session_id, false)
            .await
            .map_err(to_rpc)?;
        let mut snapshot = record.state.lock().await.clone();
        let mut reminder_messages = vec![mode_reminder(mode)];
        if turn == 0 {
            if project_config.openviking.recall
                && !project_config.openviking.url.trim().is_empty()
                && let Some(client) = inner
                    .project_openviking(std::path::Path::new(&state_snapshot.session.cwd))
                    .await
                    .map_err(to_rpc)?
                && let Ok(identity) =
                    inner.workspace_identity(std::path::Path::new(&state_snapshot.session.cwd))
            {
                let key = CoreInner::openviking_failure_key(
                    &project_config.openviking.url,
                    &identity.peer_id,
                );
                let session_id = openviking_session_id(&params.session_id);
                match client
                    .search_context(
                        &user_message.text(),
                        Some(&session_id),
                        Some(&identity.peer_id),
                    )
                    .await
                {
                    Ok(context) => {
                        inner.openviking_mark_success(&key);
                        if let Some(rendered) = render_openviking_context(
                            &context,
                            project_config.openviking.max_context_tokens,
                        ) {
                            reminder_messages.push(reminder_message(rendered));
                        }
                    }
                    Err(error) => {
                        if inner.openviking_should_notice(&key) {
                            record
                                .state
                                .lock()
                                .await
                                .notices
                                .push(format!("OpenViking recall unavailable: {error}"));
                        }
                    }
                }
            }
            if project_config.openviking.skills
                && let Some(client) = inner
                    .project_openviking(std::path::Path::new(&state_snapshot.session.cwd))
                    .await
                    .map_err(to_rpc)?
                && let Ok(identity) =
                    inner.workspace_identity(std::path::Path::new(&state_snapshot.session.cwd))
                && let Ok(mut catalog) = client.list_skills(Some(&identity.peer_id)).await
            {
                inner.openviking_mark_success(&CoreInner::openviking_failure_key(
                    &project_config.openviking.url,
                    &identity.peer_id,
                ));
                let entries = if catalog.get("skills").is_some() {
                    catalog.get_mut("skills")
                } else {
                    Some(&mut catalog)
                };
                if let Some(entries) = entries.and_then(Value::as_array_mut) {
                    let mut seen = std::collections::HashSet::new();
                    entries.retain(|entry| {
                        entry
                            .get("name")
                            .and_then(Value::as_str)
                            .is_some_and(|name| {
                                skills.get(name).is_none() && seen.insert(name.to_owned())
                            })
                    });
                }
                let rendered = format!(
                    "<openviking-skills>{}</openviking-skills>",
                    catalog.get("skills").cloned().unwrap_or(catalog)
                );
                reminder_messages.push(reminder_message(rendered));
            }
            let mut reminder_state = runtime.reminders.lock().await;
            if let Some(reminder) = reminder_state.render(
                &crate::system_prompt::reminder::local_time(),
                &[],
                &[],
                false,
            ) {
                reminder_messages.push(reminder_message(reminder));
            }
            let notices = std::mem::take(&mut record.state.lock().await.notices);
            reminder_messages.extend(notices.into_iter().map(|notice| {
                reminder_message(format!("<system-reminder>{notice}</system-reminder>"))
            }));
        } else {
            let notices = std::mem::take(&mut record.state.lock().await.notices);
            reminder_messages.extend(notices.into_iter().map(|notice| {
                reminder_message(format!("<system-reminder>{notice}</system-reminder>"))
            }));
        }
        let reminder_messages = merge_reminders(reminder_messages);
        let mut messages = model_messages(&snapshot, Some(system.clone()), Some(&model));
        let context_tokens = compact::context_tokens_with_additional(
            &snapshot,
            &messages,
            &tools_available,
            compact::messages_tokens(&reminder_messages),
        );
        messages.extend(reminder_messages.iter().cloned());
        if project_config.compaction.enabled
            && project_config.compaction.auto
            && (turn == 0 || project_config.compaction.mid_prompt)
            && context_tokens > compact::threshold(&project_config, model.context_window)
        {
            match compact::compact(
                &inner,
                ctx,
                &params.session_id,
                compact::CompactReason::Threshold,
                None,
                compact::ContextBudget {
                    additional_tokens: compact::messages_tokens(&reminder_messages),
                },
            )
            .await
            {
                Ok(report) => {
                    if !report.created_headroom {
                        tracing::warn!(
                            session_id = %params.session_id,
                            tokens_after = report.result.tokens_after,
                            recovery_band = compact::recovery_band(&project_config, model.context_window),
                            "Compaction did not create enough context space; stopping automatic continuation to avoid repeated compaction"
                        );
                    }
                    if !report.created_headroom || !project_config.compaction.auto_continue {
                        final_reason = FinishReason::Other("compacted".into());
                        break 'turns;
                    }
                    snapshot = record.state.lock().await.clone();
                    messages = model_messages(&snapshot, Some(system.clone()), Some(&model));
                    messages.extend(reminder_messages.iter().cloned());
                }
                Err(error) => {
                    tracing::warn!(session_id = %params.session_id, %error, "自动上下文压缩失败，继续请求")
                }
            }
        }
        let mut request = build_request(
            &model,
            &project_config,
            messages,
            tools_available.clone(),
            snapshot.native_input.clone(),
            &params.session_id,
        );
        let mut overflow_attempts = 0;
        let (mut stream, request_permit) = loop {
            let mut llm_config = model.llm.clone();
            if project_config.compaction.enabled
                && project_config.compaction.auto
                && project_config.compaction.remote.enabled
                && project_config.compaction.remote.server_side
                && model.remote_compaction
                && !snapshot.server_compaction_disabled
                && model.llm.protocol == moc_llm::Protocol::OpenAiResponses
            {
                llm_config.server_compaction_threshold =
                    Some(compact::threshold(&project_config, model.context_window));
            }
            let mut permit = match inner.provider_permit(&model, &cancel).await {
                Ok(permit) => Some(permit),
                Err(crate::CoreError::Cancelled) => {
                    final_reason = FinishReason::Cancelled;
                    break 'turns;
                }
                Err(error) => return Err(to_rpc(error)),
            };
            let streamed = tokio::select! {
                _ = cancel.cancelled() => { final_reason = FinishReason::Cancelled; break 'turns; }
                result = inner.client.stream(&llm_config, &request) => result,
            };
            // 错误恢复可能再次请求同一 Provider，必须先归还失败请求的许可。
            if streamed.is_err() {
                permit.take();
            }
            match streamed {
                Ok(stream) => break (stream, permit),
                Err(error)
                    if llm_config.server_compaction_threshold.is_some()
                        && error.api_error().is_some_and(|api| api.status == Some(400))
                        && error.to_string().contains("context_management") =>
                {
                    record.state.lock().await.server_compaction_disabled = true;
                    snapshot.server_compaction_disabled = true;
                    tracing::warn!(session_id = %params.session_id, "上游不支持 context_management，本会话关闭服务端压缩");
                }
                Err(error)
                    if overflow_attempts < 2
                        && error.api_error().is_some_and(|api| {
                            api.kind == moc_llm::ApiErrorKind::ContextLength
                        }) =>
                {
                    overflow_attempts += 1;
                    match compact::compact(
                        &inner,
                        ctx,
                        &params.session_id,
                        compact::CompactReason::Overflow,
                        None,
                        compact::ContextBudget {
                            additional_tokens: compact::messages_tokens(&reminder_messages),
                        },
                    )
                    .await
                    {
                        Ok(report) if report.created_headroom => {}
                        Ok(_) => {
                            let message = format!(
                                "{error}; overflow recovery did not create enough context space"
                            );
                            emit_fatal(&update, &params.session_id, &prompt_id, &message).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), message));
                        }
                        Err(compact_error) => {
                            if cancel.is_cancelled() {
                                final_reason = FinishReason::Cancelled;
                                break 'turns;
                            }
                            let message =
                                format!("{error}; overflow recovery failed: {compact_error}");
                            emit_fatal(&update, &params.session_id, &prompt_id, &message).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), message));
                        }
                    }
                    if !project_config.compaction.auto_continue {
                        final_reason = FinishReason::Other("compacted".into());
                        break 'turns;
                    }
                    snapshot = record.state.lock().await.clone();
                    let mut retry_messages =
                        model_messages(&snapshot, Some(system.clone()), Some(&model));
                    retry_messages.extend(reminder_messages.iter().cloned());
                    request = build_request(
                        &model,
                        &project_config,
                        retry_messages,
                        tools_available.clone(),
                        snapshot.native_input.clone(),
                        &params.session_id,
                    );
                }
                Err(error) => {
                    emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string()).await?;
                    return Err(RpcError::new(
                        RpcErrorCode::InternalError.code(),
                        error.to_string(),
                    ));
                }
            }
        };
        let mut call_ids = HashMap::<usize, String>::new();
        let mut interrupted = false;
        let mut connection_error = None;
        let response = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    interrupted = true;
                    break Some(stream.interrupt());
                }
                event = stream.next_event() => {
                    match event {
                        Some(Ok(event)) => {
                            if let Err(error) = forward_event(&update, &params.session_id, &prompt_id, event, &mut call_ids).await {
                                interrupted = true;
                                connection_error = Some(protocol_to_rpc(error));
                                break Some(stream.interrupt());
                            }
                        }
                        Some(Err(error)) if error.api_error().is_some_and(|api|api.kind==moc_llm::ApiErrorKind::ContextLength)
                            && project_config.compaction.enabled && stream_overflows < 2 => {
                            let _ = stream.interrupt();
                            break None;
                        }
                        Some(Err(error)) => {
                            emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string()).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()));
                        }
                        None => match stream.finish().await {
                            Ok(response) => break Some(response),
                            Err(error) if error.api_error().is_some_and(|api|api.kind==moc_llm::ApiErrorKind::ContextLength) && project_config.compaction.enabled && stream_overflows < 2 => break None,
                            Err(error) => {
                                emit_fatal(&update, &params.session_id, &prompt_id, &error.to_string()).await?;
                                return Err(RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()));
                            }
                        },
                }
                }
            }
        };
        drop(request_permit);
        let Some(response) = response else {
            stream_overflows += 1;
            let compact_report = compact::compact(
                &inner,
                ctx,
                &params.session_id,
                compact::CompactReason::Overflow,
                None,
                compact::ContextBudget {
                    additional_tokens: compact::messages_tokens(&reminder_messages),
                },
            )
            .await
            .map_err(to_rpc)?;
            if !compact_report.created_headroom {
                final_reason = FinishReason::Length;
                break 'turns;
            }
            if !project_config.compaction.auto_continue {
                final_reason = FinishReason::Length;
                break 'turns;
            }
            continue 'turns;
        };
        let mismatches = upstream_mismatches(&model, &request, &response);
        if !mismatches.is_empty() {
            update
                .emit(
                    params.session_id.clone(),
                    SessionUpdate::UpstreamMismatch {
                        prompt_id: prompt_id.clone(),
                        fields: mismatches,
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
        usage.input_tokens = usage
            .input_tokens
            .saturating_add(response.usage.input_tokens);
        usage.output_tokens = usage
            .output_tokens
            .saturating_add(response.usage.output_tokens);
        usage.total_tokens = Some(usage.input_tokens.saturating_add(usage.output_tokens));
        final_reason = if interrupted {
            FinishReason::Cancelled
        } else {
            response.finish_reason.clone()
        };
        last_message = response.message.clone();
        inner
            .sessions
            .append_message(
                &params.session_id,
                response.message.clone(),
                None,
                Some(final_reason.clone()),
            )
            .await
            .map_err(to_rpc)?;
        {
            let mut state = record.state.lock().await;
            state.usage = response.usage;
            state.usage_at = state.transcript.len();
        }

        let server_compacted = response.compaction.is_some();
        if let Some(item) = response.compaction {
            let compact_state = record.state.lock().await.clone();
            let (first_kept_id, retained_tokens) = compact::remote::retained_boundary(
                &compact_state,
                project_config.compaction.remote.retained_message_tokens as u64,
            );
            let native = Value::Array(vec![*item]);
            let native_tokens = serde_json::to_string(&native)
                .map_or(0, |serialized| compact::estimate_tokens(&serialized));
            let tokens_after = compact::messages_tokens(std::slice::from_ref(&system))
                .saturating_add(compact::tools_tokens(&tools_available))
                .saturating_add(compact::messages_tokens(&reminder_messages))
                .saturating_add(retained_tokens)
                .saturating_add(native_tokens);
            compact::record_compaction(
                &inner,
                ctx,
                &params.session_id,
                compact::Checkpoint {
                    method: "remote",
                    summary: compact::remote::checkpoint_summary(&compact_state),
                    first_kept_id,
                    tokens_before: response.usage.input_tokens,
                    tokens_after,
                    instructions: None,
                    native: Some(Box::new(native)),
                },
            )
            .await
            .map_err(to_rpc)?;
        }

        let calls: Vec<_> = response.message.tool_calls().cloned().collect();
        if project_config.openviking.capture
            && let Some(client) = inner.project_openviking(&cwd).await.map_err(to_rpc)?
            && let Ok(identity) = inner.workspace_identity(&cwd)
        {
            let ov_session = openviking_session_id(&params.session_id);
            let tool_calls: Vec<_> = calls.iter().map(|call| serde_json::json!({"id": call.id, "name": call.name, "arguments": truncate_memory(&call.arguments)})).collect();
            let message = serde_json::json!({"role":"assistant","parts":[{"type":"text","text":truncate_memory(&response.message.text())}],"tool_calls":tool_calls});
            let _ = client
                .append_messages(&ov_session, &[message], &identity.peer_id)
                .await;
        }
        if calls.is_empty() || interrupted {
            if interrupted {
                for call in &calls {
                    persist_tool_result(
                        &inner,
                        &params.session_id,
                        call.id.clone(),
                        "Prompt cancelled; this call was not executed.".into(),
                        true,
                        ToolStatus::Cancelled,
                        0,
                    )
                    .await
                    .map_err(to_rpc)?;
                }
                if let Some(error) = connection_error {
                    return Err(error);
                }
            } else if final_reason == FinishReason::Length
                && length_continues < project_config.limits.max_length_continues
                && (!server_compacted || project_config.compaction.auto_continue)
            {
                length_continues += 1;
                inner
                    .sessions
                    .append_message(
                        &params.session_id,
                        Message::user_text(
                            "The previous output hit the length limit. Continue from the interruption without repeating completed text",
                        ),
                        None,
                        None,
                    )
                    .await
                    .map_err(to_rpc)?;
                continue;
            } else if final_reason == FinishReason::Length {
                tracing::warn!(session_id = %params.session_id, "模型连续输出达到长度上限");
            }
            if !interrupted {
                let mut steering = runtime.steering.lock().await;
                if !steering.messages.is_empty() {
                    continue;
                }
                steering.accepting = false;
            }
            break;
        }
        let openviking = inner.project_openviking(&cwd).await.map_err(to_rpc)?;
        let openviking_peer = inner
            .workspace_identity(&cwd)
            .ok()
            .map(|identity| identity.peer_id);
        let tool_ctx = ToolCtx {
            session_id: params.session_id.clone(),
            prompt_id: prompt_id.clone(),
            cwd: cwd.clone(),
            paths: record.paths.clone(),
            config: Arc::clone(&project_config),
            state: Arc::clone(&record.state),
            anchors: Arc::clone(&runtime.anchors),
            request: ctx.clone(),
            cancel: cancel.clone(),
            sessions: Arc::clone(&inner.sessions),
            core: Arc::clone(&inner),
            catalog: Arc::clone(&catalog),
            mode,
            permission,
            nested: false,
            jobs: Arc::clone(&runtime.jobs),
            openviking,
            openviking_peer,
            mcp: Arc::clone(&mcp),
            skills: Arc::clone(&skills),
            tool_call_id: String::new(),
            loaded_skills: Arc::clone(&runtime.loaded_skills),
        };
        let control_end =
            execute_tool_batch(&inner, &tool_ctx, &runtime, &record, mode, &calls).await?;
        if control_end {
            final_reason = FinishReason::Stop;
        }
        if control_end || cancel.is_cancelled() {
            final_reason = if control_end {
                FinishReason::Stop
            } else {
                FinishReason::Cancelled
            };
            break;
        }
        if server_compacted && !project_config.compaction.auto_continue {
            final_reason = FinishReason::Other("compacted".into());
            break;
        }
        if turn + 1 == project_config.limits.max_turns_per_prompt {
            final_reason = FinishReason::Other("max_turns".into());
        }
    }

    if project_config.openviking.capture
        && let Some(client) = inner.project_openviking(&cwd).await.map_err(to_rpc)?
        && let Ok(identity) = inner.workspace_identity(&cwd)
    {
        let ov_session = openviking_session_id(&params.session_id);
        let _ = client.commit_session(&ov_session, &identity.peer_id).await;
    }
    check_workspace(
        &update,
        &runtime,
        &record,
        WorkspaceScope {
            session_id: &params.session_id,
            cwd: &cwd,
            config: &project_config,
            mode,
            before_tools: None,
        },
    )
    .await?;
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::PromptFinished {
                prompt_id: prompt_id.clone(),
                message: last_message.clone(),
                finish_reason: final_reason.clone(),
                usage,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(PromptResult {
        session_id: params.session_id,
        prompt_id,
        message: last_message,
        finish_reason: final_reason,
        usage,
    })
}

fn openviking_session_id(session_id: &str) -> String {
    format!(
        "moc-{}",
        session_id.replace(
            |character: char| !character.is_ascii_alphanumeric()
                && character != '-'
                && character != '_',
            "-"
        )
    )
}

fn truncate_memory(value: &str) -> String {
    value.chars().take(4096).collect()
}

fn render_openviking_context(value: &Value, max_tokens: u32) -> Option<String> {
    let entries = value.get("entries")?.as_array()?;
    let mut ordered: Vec<(usize, &Value)> = entries.iter().enumerate().collect();
    ordered.sort_by(|(left_index, left), (right_index, right)| {
        let rank = |entry: &Value| match entry.get("origin").and_then(Value::as_str) {
            Some("actor_peer") => 0,
            Some("self") => 1,
            _ => 2,
        };
        rank(left)
            .cmp(&rank(right))
            .then_with(|| {
                right
                    .get("score")
                    .and_then(Value::as_f64)
                    .unwrap_or(0.0)
                    .partial_cmp(&left.get("score").and_then(Value::as_f64).unwrap_or(0.0))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then(left_index.cmp(right_index))
    });
    let mut seen = std::collections::HashSet::new();
    let mut rendered = String::from("<openviking-context>");
    let budget = u64::from(max_tokens);
    let mut used = compact::estimate_tokens(&rendered) + 8;
    for (_, entry) in ordered {
        let Some(uri) = entry.get("uri").and_then(Value::as_str) else {
            continue;
        };
        if !seen.insert(uri) {
            continue;
        }
        let content = entry
            .get("content")
            .or_else(|| entry.get("abstract"))
            .or_else(|| entry.get("text"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let memory = format!(
            "<memory uri=\"{}\">{}</memory>",
            escape_xml(uri),
            escape_xml(content)
        );
        let tokens = compact::estimate_tokens(&memory);
        if used.saturating_add(tokens) > budget {
            continue;
        }
        used += tokens;
        rendered.push_str(&memory);
    }
    (rendered.len() > "<openviking-context>".len()).then(|| {
        rendered.push_str("</openviking-context>");
        rendered
    })
}

fn escape_xml(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

async fn emit_fatal(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    prompt_id: &str,
    message: &str,
) -> Result<(), RpcError> {
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::Error {
                prompt_id: prompt_id.to_owned(),
                message: message.to_owned(),
                retryable: false,
            },
        )
        .await
        .map_err(protocol_to_rpc)
}

/// 解析用户输入中的 `/skill:<name> 参数` 显式调用。
///
/// 正文以 user 消息注入（用户指令优先级高于工具结果），`disable-model-invocation`
/// 的 skill 仅此路径可用；输入不以 `/skill:` 开头时原样返回。
async fn apply_skill_invocation(
    inner: &CoreInner,
    message: Message,
    cwd: &str,
) -> Result<Message, RpcError> {
    let text = message.text();
    let Some(rest) = text.strip_prefix("/skill:") else {
        return Ok(message);
    };
    let (name, arguments) = match rest.split_once([' ', '\n']) {
        Some((name, arguments)) => (name, arguments.trim()),
        None => (rest.trim_end(), ""),
    };
    if name.is_empty() {
        return Ok(message);
    }
    let skills = inner
        .project_skills(std::path::Path::new(cwd))
        .map_err(to_rpc)?;
    let mut loaded = std::collections::HashSet::new();
    let content = crate::skills::tool::render_for_invocation(&skills, name, &mut loaded)
        .map_err(crate::service::invalid_params)?;
    let mut injected = format!(
        "<system-reminder>The following content was explicitly injected by the user via /skill:{name}; follow it now.</system-reminder>\n{content}"
    );
    if !arguments.is_empty() {
        injected.push_str(&format!("\n\nUser-provided instructions: {arguments}"));
    }
    Ok(Message::user_text(injected))
}

#[cfg(test)]
mod openviking_tests {
    use super::render_openviking_context;

    #[test]
    fn actor_context_precedes_global_and_deduplicates_uri() {
        let value = serde_json::json!({"entries":[
            {"uri":"viking://user/u/a","origin":"self","score":0.99,"content":"global"},
            {"uri":"viking://resources/p/a","origin":"actor_peer","score":0.2,"content":"workspace"},
            {"uri":"viking://resources/p/a","origin":"actor_peer","score":0.1,"content":"duplicate"}
        ]});
        let rendered = render_openviking_context(&value, 1600).expect("context");
        assert!(
            rendered.find("workspace").expect("workspace")
                < rendered.find("global").expect("global")
        );
        assert!(!rendered.contains("duplicate"));
        assert_eq!(rendered.matches("viking://resources/p/a").count(), 1);
    }
}

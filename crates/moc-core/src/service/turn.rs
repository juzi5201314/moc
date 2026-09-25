//! 流式轮次编排、取消收尾、持久化与工具执行。

use crate::{
    compact,
    compact::replay::model_messages,
    prompt::{reminder_message, system_message},
    service::{CoreInner, protocol_to_rpc, snapshot_workspace, to_rpc},
    session::store::now_ms,
    tools::ToolCtx,
};
use moc_llm::{FinishReason, Message, Usage};
use moc_protocol::{
    RequestContext, RpcError, RpcErrorCode,
    methods::{SessionUpdate, ToolStatus, TurnResult},
};
use serde_json::Value;
use std::{collections::HashMap, sync::Arc};

mod request;
mod stream;
mod tools;
mod workspace;

use request::{build_request, project_input};
use stream::forward_event;
use tools::{execute_tool_batch, persist_tool_result, repair_pending_calls};
use workspace::check_workspace;

pub async fn run_turn(
    inner: &CoreInner,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
) -> Result<TurnResult, RpcError> {
    let session_id = params.session_id.clone();
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    let _turn = record.try_turn_lock().map_err(to_rpc)?;
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

    repair_pending_calls(inner, &session_id, &record)
        .await
        .map_err(to_rpc)?;
    let result = run_turn_locked(inner, &ctx, params, Arc::clone(&record)).await;
    repair_pending_calls(inner, &session_id, &record)
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
        Ok(turn) => {
            unlocked?;
            Ok(turn)
        }
    }
}

async fn run_turn_locked(
    inner: &CoreInner,
    ctx: &RequestContext,
    params: moc_protocol::methods::PromptParams,
    record: Arc<crate::session::SessionRecord>,
) -> Result<TurnResult, RpcError> {
    let update = ctx.updates();
    let state_snapshot = record.state.lock().await.clone();
    let project_config = inner
        .project_config(std::path::Path::new(&state_snapshot.session.cwd))
        .map_err(to_rpc)?;
    let model = project_config
        .resolve_model(&state_snapshot.model)
        .map_err(to_rpc)?;
    let user_message = project_input(params.input, model.capabilities.image);
    if user_message.content.is_empty() {
        return Err(RpcError::of(RpcErrorCode::InvalidParams));
    }
    inner
        .sessions
        .append_message(&params.session_id, user_message.clone(), None, None)
        .await
        .map_err(to_rpc)?;

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
    let turn_id = crate::ids::turn_id(state.turn_sequence);
    let mode = state.mode;
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
            SessionUpdate::TurnStarted {
                turn_id: turn_id.clone(),
                model: model.selector.clone(),
                at_ms: now_ms(),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;

    let cancel = ctx.cancel();
    let tools_available = inner.tools.definitions(
        mode,
        inner.client_supports_ask(ctx.connection_id()),
        &project_config.tools,
    );
    let mut last_message = Message::default();
    let mut final_reason = FinishReason::Stop;
    let mut usage = Usage::default();
    let mut length_continues = 0usize;
    let system = system_message(&project_config, &model, mode, &cwd);

    'steps: for step in 0..project_config.limits.max_steps_per_turn {
        if cancel.is_cancelled() {
            final_reason = FinishReason::Cancelled;
            break;
        }
        let mut snapshot = record.state.lock().await.clone();
        let mut reminder_messages = Vec::new();
        if step == 0 {
            let notices = std::mem::take(&mut record.state.lock().await.notices);
            let mut reminder_state = runtime.reminders.lock().await;
            if let Some(reminder) =
                reminder_state.render(&crate::prompt::reminder::local_time(), &[], &[], false)
            {
                reminder_messages.push(reminder_message(reminder));
            }
            reminder_messages.extend(notices.into_iter().map(|notice| {
                reminder_message(format!("<system-reminder>{notice}</system-reminder>"))
            }));
        } else {
            let notices = std::mem::take(&mut record.state.lock().await.notices);
            reminder_messages.extend(notices.into_iter().map(|notice| {
                reminder_message(format!("<system-reminder>{notice}</system-reminder>"))
            }));
        }
        let mut messages = model_messages(&snapshot, Some(system.clone()), Some(&model));
        messages.extend(reminder_messages.iter().cloned());
        if project_config.compaction.enabled
            && project_config.compaction.auto
            && (step == 0 || project_config.compaction.mid_turn)
            && compact::context_tokens(&snapshot, &messages, &tools_available)
                > compact::threshold(&project_config, model.context_window)
        {
            match compact::compact(
                inner,
                ctx,
                &params.session_id,
                compact::CompactReason::Threshold,
                None,
            )
            .await
            {
                Ok(_) => {
                    if !project_config.compaction.auto_continue {
                        final_reason = FinishReason::Other("compacted".into());
                        break 'steps;
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
        let mut stream = loop {
            let mut llm_config = model.llm.clone();
            if project_config.compaction.enabled
                && project_config.compaction.auto
                && project_config.compaction.remote.enabled
                && project_config.compaction.remote.server_side
                && !snapshot.server_compaction_disabled
                && model.llm.protocol == moc_llm::Protocol::OpenAiResponses
                && model.model_id.starts_with("gpt-")
            {
                llm_config.server_compaction_threshold =
                    Some(compact::threshold(&project_config, model.context_window));
            }
            let streamed = tokio::select! {
                _ = cancel.cancelled() => { final_reason = FinishReason::Cancelled; break 'steps; }
                result = inner.client.stream(&llm_config, &request) => result,
            };
            match streamed {
                Ok(stream) => break stream,
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
                    if let Err(compact_error) = compact::compact(
                        inner,
                        ctx,
                        &params.session_id,
                        compact::CompactReason::Overflow,
                        None,
                    )
                    .await
                    {
                        if cancel.is_cancelled() {
                            final_reason = FinishReason::Cancelled;
                            break 'steps;
                        }
                        let message = format!("{error}; 溢出恢复失败：{compact_error}");
                        emit_fatal(&update, &params.session_id, &turn_id, &message).await?;
                        return Err(RpcError::new(RpcErrorCode::InternalError.code(), message));
                    }
                    if !project_config.compaction.auto_continue {
                        final_reason = FinishReason::Other("compacted".into());
                        break 'steps;
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
                    emit_fatal(&update, &params.session_id, &turn_id, &error.to_string()).await?;
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
                    break stream.interrupt();
                }
                event = stream.next_event() => {
                    match event {
                        Some(Ok(event)) => {
                            if let Err(error) = forward_event(&update, &params.session_id, &turn_id, event, &mut call_ids).await {
                                interrupted = true;
                                connection_error = Some(protocol_to_rpc(error));
                                break stream.interrupt();
                            }
                        }
                        Some(Err(error)) => {
                            emit_fatal(&update, &params.session_id, &turn_id, &error.to_string()).await?;
                            return Err(RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()));
                        }
                        None => match stream.finish().await {
                            Ok(response) => break response,
                            Err(error) => {
                                emit_fatal(&update, &params.session_id, &turn_id, &error.to_string()).await?;
                                return Err(RpcError::new(RpcErrorCode::InternalError.code(), error.to_string()));
                            }
                        },
                }
                }
            }
        };
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
            let (first_kept_id, tokens_after) = compact::remote::retained_boundary(
                &compact_state,
                project_config.compaction.remote.retained_message_tokens as u64,
            );
            compact::record_compaction(
                inner,
                ctx,
                &params.session_id,
                compact::Checkpoint {
                    method: "remote",
                    summary: compact::remote::checkpoint_summary(&compact_state),
                    first_kept_id,
                    tokens_before: response.usage.input_tokens,
                    tokens_after,
                    instructions: None,
                    native: Some(Box::new(Value::Array(vec![*item]))),
                },
            )
            .await
            .map_err(to_rpc)?;
        }

        let calls: Vec<_> = response.message.tool_calls().cloned().collect();
        if calls.is_empty() || interrupted {
            if interrupted {
                for call in &calls {
                    persist_tool_result(
                        inner,
                        &params.session_id,
                        call.id.clone(),
                        "轮次被取消，该调用未执行。".into(),
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
                            "[上一条输出被长度上限截断，请从中断处继续，不要重复已写内容]",
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
            break;
        }
        let tool_ctx = ToolCtx {
            session_id: params.session_id.clone(),
            turn_id: turn_id.clone(),
            cwd: cwd.clone(),
            paths: record.paths.clone(),
            config: Arc::clone(&project_config),
            state: Arc::clone(&record.state),
            anchors: Arc::clone(&runtime.anchors),
            request: ctx.clone(),
            cancel: cancel.clone(),
            jobs: Arc::clone(&runtime.jobs),
            sessions: Arc::clone(&inner.sessions),
        };
        let control_end =
            execute_tool_batch(inner, &tool_ctx, &runtime, &record, mode, &calls).await?;
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
        if step + 1 == project_config.limits.max_steps_per_turn {
            final_reason = FinishReason::Other("max_steps".into());
        }
    }

    check_workspace(
        &update,
        &runtime,
        &record,
        &params.session_id,
        &cwd,
        &project_config,
        None,
    )
    .await?;
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::TurnFinished {
                turn_id: turn_id.clone(),
                message: last_message.clone(),
                finish_reason: final_reason.clone(),
                usage,
            },
        )
        .await
        .map_err(protocol_to_rpc)?;
    Ok(TurnResult {
        session_id: params.session_id,
        turn_id,
        message: last_message,
        finish_reason: final_reason,
        usage,
    })
}

async fn emit_fatal(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    turn_id: &str,
    message: &str,
) -> Result<(), RpcError> {
    update
        .emit(
            session_id.to_owned(),
            SessionUpdate::Error {
                turn_id: turn_id.to_owned(),
                message: message.to_owned(),
                retryable: false,
            },
        )
        .await
        .map_err(protocol_to_rpc)
}

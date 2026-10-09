//! 工具批次的有界并发、授权、取消与结果配对。

use super::workspace::{WorkspaceScope, check_workspace};
use crate::{
    error::CoreError,
    service::{CoreInner, SessionRuntime, protocol_to_rpc, snapshot_workspace, to_rpc},
    tools::{ToolControl, ToolCtx, ToolOutcome, ToolOutput},
};
use futures_util::{
    FutureExt,
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use moc_llm::{ContentPart, Message, Role};
use moc_protocol::{
    RpcError, RpcErrorCode,
    methods::{SessionUpdate, ToolExecution, ToolStatus},
};
use serde_json::Value;
use std::{panic::AssertUnwindSafe, sync::Arc, time::Instant};
type PendingTool<'a> = BoxFuture<'a, (usize, Result<ToolOutcome, CoreError>, u64)>;

pub(super) async fn execute_tool_batch(
    inner: &CoreInner,
    tool_ctx: &ToolCtx,
    runtime: &Arc<SessionRuntime>,
    record: &Arc<crate::session::SessionRecord>,
    mode: moc_protocol::methods::SessionMode,
    calls: &[moc_llm::ToolCall],
) -> Result<bool, RpcError> {
    let config = &tool_ctx.config;
    let parallel_limit = config.tools.max_concurrent_calls;
    debug_assert!(parallel_limit > 0);
    let session_id = tool_ctx.session_id.as_str();
    let prompt_id = tool_ctx.prompt_id.as_str();
    let cwd = &tool_ctx.cwd;
    let update = tool_ctx.request.updates();
    let tool_batch_before = if matches!(
        mode,
        moc_protocol::methods::SessionMode::Normal | moc_protocol::methods::SessionMode::Ptc
    ) && config.workspace.watch
    {
        Some(
            snapshot_workspace(cwd.clone(), config.workspace.max_scan_entries)
                .await
                .map_err(to_rpc)?,
        )
    } else {
        None
    };
    let mut results: Vec<Option<(Result<ToolOutcome, CoreError>, u64)>> =
        std::iter::repeat_with(|| None).take(calls.len()).collect();
    let mut started = vec![false; calls.len()];
    let mut pending: FuturesUnordered<PendingTool<'_>> = FuturesUnordered::new();
    let mut stop_reason = None;

    'calls: for (index, call) in calls.iter().enumerate() {
        if call.name.starts_with("mcp__") {
            results[index] = Some((
                Ok(ToolOutcome::failure(
                    "E_MODE",
                    "Call MCP tools through run_code",
                )),
                0,
            ));
            continue;
        }
        let Some(tool) = inner.tools.get(&call.name) else {
            let error = CoreError::Tool(format!(
                "E_TOOL_NOT_FOUND: tool `{}` is not registered",
                call.name
            ));
            results[index] = Some((Err(error), 0));
            continue;
        };
        let parallel = tool.parallel_safe();

        if !parallel {
            while let Some((done, result, elapsed)) = pending.next().await {
                results[done] = Some((result, elapsed));
            }
        }
        started[index] = true;
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolStarted {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                },
            )
            .await
        {
            stop_reason = Some(protocol_to_rpc(error));
            break 'calls;
        }
        let start = Instant::now();
        if parallel {
            pending.push(Box::pin(async move {
                let result = execute_tool(inner, tool_ctx, mode, call).await;
                (
                    index,
                    result,
                    start.elapsed().as_millis().min(u64::MAX as u128) as u64,
                )
            }));
            if pending.len() == parallel_limit
                && let Some((done, result, elapsed)) = pending.next().await
            {
                results[done] = Some((result, elapsed));
            }
        } else {
            let result = execute_tool(inner, tool_ctx, mode, call).await;
            let elapsed = start.elapsed().as_millis().min(u64::MAX as u128) as u64;
            if matches!(result, Err(CoreError::Protocol(_))) {
                stop_reason = Some(RpcError::of(RpcErrorCode::InternalError));
            }
            results[index] = Some((result, elapsed));
            if stop_reason.is_some() {
                break 'calls;
            }
            if results[index].as_ref().is_some_and(|(result, _)| {
                matches!(result, Ok(outcome) if outcome.control == ToolControl::EndTurn)
            }) {
                break 'calls;
            }
        }
    }

    if stop_reason.is_none() {
        while let Some((done, result, elapsed)) = pending.next().await {
            if matches!(result, Err(CoreError::Protocol(_))) {
                stop_reason = Some(RpcError::of(RpcErrorCode::InternalError));
            }
            results[done] = Some((result, elapsed));
        }
    }

    let mut end_prompt = false;
    for (index, call) in calls.iter().enumerate() {
        if !started[index]
            && stop_reason.is_none()
            && let Err(error) = update
                .emit(
                    session_id.to_owned(),
                    SessionUpdate::ToolStarted {
                        prompt_id: prompt_id.to_owned(),
                        tool_call_id: call.id.clone(),
                        name: call.name.clone(),
                    },
                )
                .await
        {
            stop_reason = Some(protocol_to_rpc(error));
        }
        let (result, duration_ms) = results[index].take().unwrap_or_else(|| {
            let (code, message) = if stop_reason.is_some() {
                ("E_CANCELLED", "Prompt aborted; this call was not executed.")
            } else {
                (
                    "E_TURN_ENDED",
                    "The plan was submitted; later calls were not executed.",
                )
            };
            let mut outcome = ToolOutcome::failure(code, message);
            outcome.status = Some(ToolStatus::Cancelled);
            (Ok(outcome), 0)
        });
        let mut outcome = match result {
            Ok(outcome) => outcome,
            Err(CoreError::Protocol(message)) => {
                if stop_reason.is_none() {
                    stop_reason = Some(RpcError::new(
                        RpcErrorCode::InternalError.code(),
                        message.clone(),
                    ));
                }
                ToolOutcome::failure("E_PROTOCOL", message)
            }
            Err(error) => ToolOutcome::failure("E_TOOL", error.to_string()),
        };
        let content = match crate::tools::output::prepare_outcome(
            &outcome,
            &config.tools.output,
            &tool_ctx.artifact_store(),
        )
        .await
        {
            Ok(content) => content,
            Err(error) => {
                outcome.is_error = true;
                outcome.error_code = Some("E_ARTIFACT".into());
                format!("[E_ARTIFACT] Tool ran, but its output could not be delivered: {error}")
            }
        };
        let status = outcome.status.unwrap_or(if outcome.is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Completed
        });
        let tool_result = moc_llm::ToolResult {
            tool_call_id: call.id.clone(),
            content,
            is_error: outcome.is_error,
            images: outcome.images.clone(),
        };
        inner
            .sessions
            .append_message(
                session_id,
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult(tool_result.clone())],
                },
                Some(ToolExecution {
                    tool_call_id: call.id.clone(),
                    status,
                    duration_ms: Some(duration_ms),
                }),
                None,
            )
            .await
            .map_err(to_rpc)?;
        if config.openviking.capture
            && let Some(client) = tool_ctx.openviking.as_ref()
            && let Some(peer) = tool_ctx.openviking_peer.as_deref()
        {
            let session = format!(
                "moc-{}",
                session_id.replace(
                    |character: char| !character.is_ascii_alphanumeric()
                        && character != '-'
                        && character != '_',
                    "-"
                )
            );
            let message = serde_json::json!({"role":"tool","tool_call_id":call.id,"tool_name":call.name,"parts":[{"type":"text","text":tool_result.content.chars().take(4096).collect::<String>()}]});
            let _ = client.append_messages(&session, &[message], peer).await;
        }
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolFinished {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: call.id.clone(),
                    status,
                    result: tool_result,
                    duration_ms: Some(duration_ms),
                    diff: outcome.diff,
                },
            )
            .await
            && stop_reason.is_none()
        {
            stop_reason = Some(protocol_to_rpc(error));
        }
        end_prompt |= outcome.control == ToolControl::EndTurn;
    }

    check_workspace(
        &update,
        runtime,
        record,
        WorkspaceScope {
            session_id,
            cwd,
            config,
            mode,
            before_tools: tool_batch_before.as_ref(),
        },
    )
    .await?;
    if let Some(error) = stop_reason {
        return Err(error);
    }
    Ok(end_prompt)
}
async fn execute_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    mode: moc_protocol::methods::SessionMode,
    call: &moc_llm::ToolCall,
) -> Result<ToolOutcome, CoreError> {
    let name = call.name.as_str();
    let arguments = call.arguments.as_str();
    let config = &ctx.config;
    let request = &ctx.request;
    if !ctx.nested && name.starts_with("mcp__") {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "Call MCP tools through run_code",
        ));
    }
    let selected = if ctx.nested {
        ctx.catalog.get(name)
    } else {
        inner.tools.get(name)
    };
    let Some(tool) = selected else {
        return Ok(ToolOutcome::failure(
            "E_TOOL_NOT_FOUND",
            format!("tool `{name}` is not registered"),
        ));
    };
    if !crate::service::mode::mode_allows(mode, name, tool.tier()) {
        return Ok(ToolOutcome::failure(
            "E_MODE",
            "This tool is not allowed in the current mode",
        ));
    }
    if !crate::service::mode::tool_enabled(
        name,
        inner.client_supports_ask(request.connection_id()),
        config,
    ) {
        return Ok(ToolOutcome::failure(
            "E_TOOL_DISABLED",
            "This tool is disabled",
        ));
    }
    let args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(_) => {
            return Ok(ToolOutcome::failure(
                "E_TOOL_ARGUMENTS",
                format!(
                    "Tool arguments are not valid JSON: {}",
                    arguments.chars().take(200).collect::<String>(),
                ),
            ));
        }
    };
    if name == "bash"
        && let Some(command) = args.get("command").and_then(Value::as_str)
        && let Err(error) =
            crate::tools::bash::reject_denied(&config.tools.bash.deny_patterns, command)
    {
        return Ok(error.into_outcome());
    }
    let execution_permission = match authorize(ctx, &call.id, name, &args).await? {
        Authorization::Denied => {
            return Ok(ToolOutcome {
                content: "[E_PERMISSION_DENIED] The user denied this command".into(),
                is_error: true,
                structured_content: None,
                error_code: Some("E_PERMISSION_DENIED".into()),
                images: Vec::new(),
                control: ToolControl::Continue,
                status: Some(ToolStatus::Denied),
                diff: None,
            });
        }
        Authorization::Allowed(permission) => permission,
    };
    if ctx.cancel.is_cancelled() {
        return Ok(cancelled_outcome());
    }
    let mut execution_ctx = ctx.clone();
    execution_ctx.permission = execution_permission;
    execution_ctx.tool_call_id = call.id.clone();
    let execution = AssertUnwindSafe(tool.run(&execution_ctx, args)).catch_unwind();
    let result = if tool.parallel_safe() || name == "ask" {
        tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => return Ok(cancelled_outcome()),
            result = execution => result,
        }
    } else {
        // 写入事务必须提交完整；进程工具自行响应取消并回收进程组。
        execution.await
    };
    match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(error)) if error.code == "E_UPDATE" => Err(CoreError::Protocol(error.to_string())),
        Ok(Err(error)) => Ok(error.into_outcome()),
        Err(_) => Ok(ToolOutcome::failure(
            "E_TOOL_PANIC",
            format!("Tool {name} panicked during execution"),
        )),
    }
}

/// 执行继承提示权限的 PTC 子工具，不写入模型消息。
pub(crate) async fn execute_nested_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    call: &moc_llm::ToolCall,
) -> Result<ToolOutcome, CoreError> {
    if call.name == "run_code" {
        return Ok(ToolOutcome::failure(
            "E_PTC_RECURSION",
            "Nested run_code calls are not allowed",
        ));
    }
    let mut nested = ctx.clone();
    nested.nested = true;
    let mode = if ctx.mode == moc_protocol::SessionMode::Ptc {
        moc_protocol::SessionMode::Normal
    } else {
        ctx.mode
    };
    execute_tool(inner, &nested, mode, call).await
}

enum Authorization {
    Allowed(moc_protocol::SessionPermission),
    Denied,
}

async fn authorize(
    ctx: &ToolCtx,
    tool_call_id: &str,
    tool_name: &str,
    arguments: &Value,
) -> Result<Authorization, CoreError> {
    if tool_name != "bash" || ctx.permission != moc_protocol::SessionPermission::Workspace {
        return Ok(Authorization::Allowed(ctx.permission));
    }
    let Some(command) = arguments.get("command").and_then(Value::as_str) else {
        return Ok(Authorization::Allowed(ctx.permission));
    };
    let requested_cwd = arguments.get("cwd").and_then(Value::as_str);
    let cwd = crate::tools::bash::resolve_cwd(&ctx.cwd, requested_cwd)
        .map_err(|error| CoreError::Tool(error.to_string()))?;
    if crate::tools::bash::allowed_in_sandbox(&ctx.config.tools.bash, command, &cwd, &ctx.cwd)
        .map_err(|error| CoreError::Tool(error.to_string()))?
    {
        return Ok(Authorization::Allowed(ctx.permission));
    }
    let request = &ctx.request;
    if !ctx
        .core
        .capabilities
        .read()
        .get(&request.connection_id())
        .is_some_and(|caps| caps.permission_requests)
    {
        return Ok(Authorization::Denied);
    }
    let request_id = crate::ids::entry_id();
    request
        .updates()
        .emit(
            ctx.session_id.clone(),
            SessionUpdate::PermissionRequested {
                prompt_id: ctx.prompt_id.clone(),
                request_id: request_id.clone(),
                tool_call_id: tool_call_id.to_owned(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    let answer = request
        .ask_permission(moc_protocol::methods::PermissionRequestParams {
            session_id: ctx.session_id.clone(),
            prompt_id: ctx.prompt_id.clone(),
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: "bash".into(),
            title: format!(
                "允许在沙盒外执行命令：{}",
                command.lines().next().unwrap_or(command)
            ),
            body: Some(command.to_owned()),
            diff: None,
            options: moc_protocol::methods::default_options(),
        })
        .await;
    let selected = answer.ok().map(|answer| answer.option_id);
    request
        .updates()
        .emit(
            ctx.session_id.clone(),
            SessionUpdate::PermissionResolved {
                prompt_id: ctx.prompt_id.clone(),
                request_id,
                option_id: selected.clone(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    if selected.as_deref() == Some("allow_once") {
        Ok(Authorization::Allowed(
            moc_protocol::SessionPermission::Full,
        ))
    } else {
        Ok(Authorization::Denied)
    }
}

pub(super) async fn persist_tool_result(
    inner: &CoreInner,
    session_id: &str,
    tool_call_id: String,
    content: String,
    is_error: bool,
    status: ToolStatus,
    duration_ms: u64,
) -> Result<(), CoreError> {
    inner
        .sessions
        .append_message(
            session_id,
            Message::tool_result(tool_call_id.clone(), content, is_error),
            Some(ToolExecution {
                tool_call_id,
                status,
                duration_ms: Some(duration_ms),
            }),
            None,
        )
        .await?;
    Ok(())
}

fn cancelled_outcome() -> ToolOutcome {
    let mut outcome = ToolOutcome::failure(
        "E_CANCELLED",
        "Prompt cancelled; this call was not executed or was terminated.",
    );
    outcome.status = Some(ToolStatus::Cancelled);
    outcome
}

pub(super) async fn repair_pending_calls(
    inner: &CoreInner,
    session_id: &str,
    record: &crate::session::SessionRecord,
) -> Result<(), CoreError> {
    let pending = {
        let state = record.state.lock().await;
        let mut pending = Vec::new();
        for entry in &state.transcript {
            pending.extend(entry.message.tool_calls().map(|call| call.id.clone()));
            for result in entry.message.tool_results() {
                pending.retain(|id| id != &result.tool_call_id);
            }
        }
        pending
    };
    for id in pending {
        let ToolOutput::Text(content) = cancelled_outcome().content else {
            unreachable!("取消结果必须是文本");
        };
        persist_tool_result(
            inner,
            session_id,
            id,
            content,
            true,
            ToolStatus::Cancelled,
            0,
        )
        .await?;
    }
    Ok(())
}

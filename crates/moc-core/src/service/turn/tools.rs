//! 工具批次的有界并发、授权、取消与结果配对。

use super::workspace::check_workspace;
use crate::{
    config::{ApprovalMode, ToolApproval},
    error::CoreError,
    service::{CoreInner, SessionRuntime, protocol_to_rpc, snapshot_workspace, to_rpc},
    tools::{Tier, ToolControl, ToolCtx, ToolOutcome},
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
    let session_id = tool_ctx.session_id.as_str();
    let turn_id = tool_ctx.turn_id.as_str();
    let cwd = &tool_ctx.cwd;
    let update = tool_ctx.request.updates();
    let tool_batch_before =
        if mode == moc_protocol::methods::SessionMode::Normal && config.workspace.watch {
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
        let Some(tool) = inner.tools.get(&call.name) else {
            let error = CoreError::Tool(format!("E_TOOL_NOT_FOUND: 未注册工具 `{}`", call.name));
            results[index] = Some((Err(error), 0));
            continue;
        };
        let approval = config.tool_approval.lookup(&call.name);
        let asks_permission = approval == Some(ToolApproval::Prompt)
            || (approval.is_none()
                && config.tools.approval_mode == ApprovalMode::Prompt
                && tool.tier() != Tier::Read);
        let parallel = tool.parallel_safe() && !asks_permission;

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
                    turn_id: turn_id.to_owned(),
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
                let result = execute_tool(inner, tool_ctx, runtime, mode, call).await;
                (
                    index,
                    result,
                    start.elapsed().as_millis().min(u64::MAX as u128) as u64,
                )
            }));
            if pending.len() == 8
                && let Some((done, result, elapsed)) = pending.next().await
            {
                results[done] = Some((result, elapsed));
            }
        } else {
            let result = execute_tool(inner, tool_ctx, runtime, mode, call).await;
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

    let mut end_turn = false;
    for (index, call) in calls.iter().enumerate() {
        if !started[index]
            && stop_reason.is_none()
            && let Err(error) = update
                .emit(
                    session_id.to_owned(),
                    SessionUpdate::ToolStarted {
                        turn_id: turn_id.to_owned(),
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
                ("E_CANCELLED", "轮次中止，该调用未执行。")
            } else {
                ("E_TURN_ENDED", "计划已提交，后续调用未执行。")
            };
            let mut outcome = ToolOutcome::failure(code, message);
            outcome.status = Some(ToolStatus::Cancelled);
            (Ok(outcome), 0)
        });
        let outcome = match result {
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
        let status = outcome.status.unwrap_or(if outcome.is_error {
            ToolStatus::Error
        } else {
            ToolStatus::Completed
        });
        let tool_result = moc_llm::ToolResult {
            tool_call_id: call.id.clone(),
            content: outcome.content.clone(),
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
            let message = serde_json::json!({"role":"tool","tool_call_id":call.id,"tool_name":call.name,"parts":[{"type":"text","text":outcome.content.chars().take(4096).collect::<String>()}]});
            let _ = client.append_messages(&session, &[message], peer).await;
        }
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolFinished {
                    turn_id: turn_id.to_owned(),
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
        end_turn |= outcome.control == ToolControl::EndTurn;
    }

    check_workspace(
        &update,
        runtime,
        record,
        session_id,
        cwd,
        config,
        tool_batch_before.as_ref(),
    )
    .await?;
    if let Some(error) = stop_reason {
        return Err(error);
    }
    Ok(end_turn)
}
async fn execute_tool(
    inner: &CoreInner,
    ctx: &ToolCtx,
    runtime: &SessionRuntime,
    mode: moc_protocol::methods::SessionMode,
    call: &moc_llm::ToolCall,
) -> Result<ToolOutcome, CoreError> {
    let name = call.name.as_str();
    let arguments = call.arguments.as_str();
    let config = &ctx.config;
    let request = &ctx.request;
    let Some(tool) = inner.tools.get(name) else {
        return Ok(ToolOutcome::failure(
            "E_TOOL_NOT_FOUND",
            format!("未注册工具 `{name}`"),
        ));
    };
    if !crate::service::mode::mode_allows(mode, name, tool.tier()) {
        return Ok(ToolOutcome::failure("E_MODE", "当前模式不允许调用该工具"));
    }
    if (name == "todo" && !config.tools.todo.enabled)
        || (name == "job" && !config.tools.job.enabled)
        || (name == "skill" && !config.skills.enabled)
        || (name == "ask" && !inner.client_supports_ask(request.connection_id()))
    {
        return Ok(ToolOutcome::failure("E_TOOL_DISABLED", "该工具未启用"));
    }
    let args: Value = match serde_json::from_str(arguments) {
        Ok(args) => args,
        Err(_) => {
            return Ok(ToolOutcome::failure(
                "E_TOOL_ARGUMENTS",
                format!(
                    "工具参数不是合法 JSON：{}",
                    arguments.chars().take(200).collect::<String>()
                ),
            ));
        }
    };
    match authorize(inner, ctx, runtime, &call.id, &tool, &args).await? {
        Authorization::Denied => {
            return Ok(ToolOutcome {
                content: "[E_PERMISSION_DENIED] 用户拒绝执行该工具".into(),
                is_error: true,
                images: Vec::new(),
                summary: None,
                control: ToolControl::Continue,
                status: Some(ToolStatus::Denied),
                diff: None,
            });
        }
        Authorization::Allowed => {}
    }
    if ctx.cancel.is_cancelled() {
        return Ok(cancelled_outcome());
    }
    let execution = AssertUnwindSafe(tool.run(ctx, args)).catch_unwind();
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
            format!("工具 {name} 执行发生 panic"),
        )),
    }
}

enum Authorization {
    Allowed,
    Denied,
}

async fn authorize(
    inner: &CoreInner,
    ctx: &ToolCtx,
    runtime: &SessionRuntime,
    tool_call_id: &str,
    tool: &Arc<dyn crate::tools::Tool>,
    arguments: &Value,
) -> Result<Authorization, CoreError> {
    let config = &ctx.config;
    let request = &ctx.request;
    let session_id = ctx.session_id.as_str();
    let turn_id = ctx.turn_id.as_str();
    let tool_name = tool.name();
    let policy = config.tool_approval.lookup(tool_name);
    if policy == Some(ToolApproval::Deny) {
        return Ok(Authorization::Denied);
    }
    if policy == Some(ToolApproval::Allow) {
        return Ok(Authorization::Allowed);
    }
    if let Some(allowed) = runtime.permissions.read().get(tool_name).copied() {
        return Ok(if allowed {
            Authorization::Allowed
        } else {
            Authorization::Denied
        });
    }
    let needs_prompt = policy == Some(ToolApproval::Prompt)
        || (policy.is_none()
            && config.tools.approval_mode == ApprovalMode::Prompt
            && tool.tier() != Tier::Read);
    if !needs_prompt {
        return Ok(Authorization::Allowed);
    }
    if !inner
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
            session_id.to_owned(),
            SessionUpdate::PermissionRequested {
                turn_id: turn_id.to_owned(),
                request_id: request_id.clone(),
                tool_call_id: tool_call_id.to_owned(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    let command = arguments
        .get("command")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let diff = match tool_name {
        "edit" => crate::tools::edit::preview_edit_diff(ctx, arguments),
        "write" => {
            let path = arguments.get("path").and_then(Value::as_str);
            let content = arguments.get("content").and_then(Value::as_str);
            match (path, content) {
                (Some(path), Some(content)) => {
                    crate::tools::write::preview_write_diff(
                        &ctx.cwd,
                        path,
                        content,
                        ctx.config.tools.output.max_file_bytes,
                    )
                    .await
                }
                _ => None,
            }
        }
        _ => None,
    };
    let answer = request
        .ask_permission(moc_protocol::methods::PermissionRequestParams {
            session_id: session_id.to_owned(),
            turn_id: turn_id.to_owned(),
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.to_owned(),
            tool_name: tool_name.to_owned(),
            title: if command.is_empty() {
                format!("允许工具 {tool_name}")
            } else {
                format!(
                    "允许执行命令：{}",
                    command.lines().next().unwrap_or(command)
                )
            },
            body: (!command.is_empty()).then(|| command.to_owned()),
            diff,
            options: [
                (
                    "allow_once",
                    "允许一次",
                    moc_protocol::PermissionOptionKind::AllowOnce,
                ),
                (
                    "allow_always",
                    "本会话允许",
                    moc_protocol::PermissionOptionKind::AllowAlways,
                ),
                (
                    "reject_once",
                    "拒绝一次",
                    moc_protocol::PermissionOptionKind::RejectOnce,
                ),
                (
                    "reject_always",
                    "本会话拒绝",
                    moc_protocol::PermissionOptionKind::RejectAlways,
                ),
            ]
            .into_iter()
            .map(|(id, label, kind)| moc_protocol::PermissionOption {
                id: id.into(),
                label: label.into(),
                kind,
            })
            .collect(),
        })
        .await;
    let selected = answer.ok().map(|answer| answer.option_id);
    request
        .updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::PermissionResolved {
                turn_id: turn_id.to_owned(),
                request_id,
                option_id: selected.clone(),
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    match selected.as_deref() {
        Some("allow_once") => Ok(Authorization::Allowed),
        Some("allow_always") => {
            runtime
                .permissions
                .write()
                .insert(tool_name.to_owned(), true);
            Ok(Authorization::Allowed)
        }
        Some("reject_always") => {
            runtime
                .permissions
                .write()
                .insert(tool_name.to_owned(), false);
            Ok(Authorization::Denied)
        }
        _ => Ok(Authorization::Denied),
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
    let mut outcome = ToolOutcome::failure("E_CANCELLED", "轮次被取消，该调用未执行或已终止。");
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
        persist_tool_result(
            inner,
            session_id,
            id,
            cancelled_outcome().content,
            true,
            ToolStatus::Cancelled,
            0,
        )
        .await?;
    }
    Ok(())
}

//! 请求按提交顺序进入有界队列；非并行操作作为屏障，取消后排空事务。
use super::{
    result::{script_result, status},
    runtime::Guest,
    state::error,
    wire::*,
};
use crate::{
    session::entries::EntryPayload,
    tools::{ToolControl, ToolCtx, ToolOutcome},
};
use futures_util::{
    future::BoxFuture,
    stream::{FuturesUnordered, StreamExt},
};
use moc_protocol::{SessionUpdate, ToolStatus, methods::session::TranscriptChildCall};
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

type Work<'a> = BoxFuture<'a, (u32, Result<Value, ScriptError>, Option<ToolOutcome>)>;

fn parallel(ctx: &ToolCtx, operation: &Operation) -> bool {
    match operation {
        Operation::Store { .. } | Operation::Remove { .. } => false,
        Operation::Tool { name, .. } => ctx.catalog.get(name).is_some_and(|tool| {
            !(name == "bash" && ctx.permission == moc_protocol::SessionPermission::Workspace)
                && tool.parallel_safe()
        }),
        _ => true,
    }
}

async fn perform(
    ctx: &ToolCtx,
    request: HostRequest,
    cancelled: Option<&str>,
) -> (u32, Result<Value, ScriptError>, Option<ToolOutcome>) {
    let id = request.id;
    if let Operation::Tool { name, arguments } = request.operation {
        let call_id = format!("{}:{id}", ctx.tool_call_id);
        let arguments = arguments.to_string();
        let started = Instant::now();
        let update = ctx.request.updates();
        // 沙盒内的嵌套调用与模型直发的调用走同一对更新：`parent_tool_call_id` 标明它挂在
        // 发起它的 `run_code` 下，参数就是即将执行的那一份。
        let start_result = update
            .emit(
                ctx.session_id.clone(),
                SessionUpdate::ToolStarted {
                    prompt_id: ctx.prompt_id.clone(),
                    tool_call_id: call_id.clone(),
                    name: name.clone(),
                    parent_tool_call_id: Some(ctx.tool_call_id.clone()),
                    arguments: arguments.clone(),
                },
            )
            .await;
        let call = moc_llm::ToolCall {
            id: call_id.clone(),
            name: name.clone(),
            arguments,
        };
        let mut outcome = if let Some(code) = cancelled {
            let mut outcome =
                ToolOutcome::failure(code, "code execution terminated; this call was cancelled");
            outcome.status = Some(ToolStatus::Cancelled);
            outcome
        } else if let Err(err) = start_result {
            ToolOutcome::failure("E_UPDATE", err.to_string())
        } else {
            crate::service::prompt::tools::execute_nested_tool(&ctx.core, ctx, &call)
                .await
                .unwrap_or_else(|err| ToolOutcome::failure("E_TOOL", err.to_string()))
        };
        let content = match crate::tools::output::prepare_outcome(
            &outcome,
            &ctx.config.tools.output,
            &ctx.artifact_store(),
        )
        .await
        {
            Ok(content) => content,
            Err(err) => {
                outcome.is_error = true;
                outcome.error_code = Some("E_ARTIFACT".into());
                format!("[E_ARTIFACT] {err}")
            }
        };
        let duration_ms = Some(started.elapsed().as_millis() as u64);
        let status = status(&outcome);
        let audit = ctx
            .sessions
            .append(
                &ctx.session_id,
                EntryPayload::ChildTool {
                    call: TranscriptChildCall {
                        id: call_id.clone(),
                        parent_tool_call_id: ctx.tool_call_id.clone(),
                        name: name.clone(),
                        arguments: call.arguments.clone(),
                        result: content.clone(),
                        status,
                        duration_ms,
                    },
                },
            )
            .await;
        let sent = update
            .emit(
                ctx.session_id.clone(),
                SessionUpdate::ToolFinished {
                    prompt_id: ctx.prompt_id.clone(),
                    tool_call_id: call_id.clone(),
                    name,
                    parent_tool_call_id: Some(ctx.tool_call_id.clone()),
                    arguments: call.arguments,
                    status,
                    result: moc_llm::ToolResult {
                        tool_call_id: call_id,
                        content,
                        is_error: outcome.is_error,
                        images: outcome.images.clone(),
                    },
                    duration_ms,
                    diff: outcome.diff.clone(),
                },
            )
            .await;
        let result = match (audit, sent) {
            (Err(err), _) => Err(error("E_STATE_WRITE", err)),
            (_, Err(err)) => Err(error("E_UPDATE", err)),
            _ => script_result(ctx, &outcome).await.and_then(|result| {
                serde_json::to_value(result).map_err(|err| error("E_SERIALIZE", err))
            }),
        };
        (id, result, Some(outcome))
    } else {
        let result = if cancelled.is_some() {
            Err(error("E_CANCELLED", "code execution terminated"))
        } else {
            let ask = ctx.core.client_supports_ask(ctx.request.connection_id());
            match request.operation {
                Operation::Servers => Ok(ctx.catalog.servers(ctx.mode, ask, &ctx.config)),
                Operation::Search {
                    query,
                    server,
                    offset,
                    limit,
                } => ctx
                    .catalog
                    .search(
                        ctx.mode,
                        ask,
                        &ctx.config,
                        query.as_deref().unwrap_or(""),
                        server.as_deref(),
                        offset.unwrap_or(0),
                        limit.unwrap_or(20),
                    )
                    .map_err(|err| error(err.code, err.message)),
                Operation::Describe { name } => ctx
                    .catalog
                    .describe(ctx.mode, ask, &ctx.config, &name)
                    .map_err(|err| error(err.code, err.message)),
                operation => super::state::execute(ctx, operation).await,
            }
        };
        let result = result.and_then(|value| {
            if serde_json::to_vec(&value).expect("JSON 可序列化").len()
                > ctx.config.tools.ptc.max_result_bytes
            {
                Err(error(
                    "E_RESULT_LIMIT",
                    "result exceeds the code-read limit",
                ))
            } else {
                Ok(value)
            }
        });
        (id, result, None)
    }
}

pub(super) async fn run(
    ctx: &ToolCtx,
    mut guest: Guest,
    input: Vec<u8>,
    deadline: tokio::time::Instant,
) -> ToolOutcome {
    let mut child = ctx.clone();
    child.cancel = ctx.cancel.child_token();
    let config = &ctx.config.tools.ptc;
    let mut queue = VecDeque::<HostRequest>::new();
    let mut active = FuturesUnordered::<Work<'_>>::new();
    // 存活 ID 稀疏，数量始终受 max_pending_calls 限制；完成交付后立即删除。
    let mut pending = BTreeMap::<u32, usize>::new();
    let mut pending_bytes = 0usize;
    let mut next_id = 0u64;
    let mut input = input;
    let mut first = true;
    let mut images = Vec::new();
    let mut outcome;
    'drive: loop {
        let step = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => { outcome = ToolOutcome::failure("E_CANCELLED", "script cancelled"); break; }
            _ = tokio::time::sleep_until(deadline) => { outcome = ToolOutcome::failure("E_TIMEOUT", "script timed out"); break; }
            step = guest.step(&input, first, if first { config.max_output_bytes } else { config.max_result_bytes }) => match step {
                Ok(step) => step,
                Err(err) => { outcome = err.into_outcome(); break; }
            }
        };
        first = false;
        for request in step.requests {
            if u64::from(request.id) != next_id {
                outcome = ToolOutcome::failure(
                    "E_PTC_PROTOCOL",
                    "request IDs must increase and be unique",
                );
                break 'drive;
            }
            next_id += 1;
            let size = serde_json::to_vec(&request).expect("请求可序列化").len();
            if pending.len() >= config.max_pending_calls
                || size > config.max_output_bytes.saturating_sub(pending_bytes)
            {
                active.push(Box::pin(async move {
                    (
                        request.id,
                        Err(error(
                            "E_QUEUE_LIMIT",
                            "pending requests exceed the count or byte limit",
                        )),
                        None,
                    )
                }));
            } else {
                pending.insert(request.id, size);
                pending_bytes += size;
                queue.push_back(request);
            }
        }
        debug_assert!(pending.len() <= config.max_pending_calls);
        match step.state {
            GuestState::Done { value } => {
                outcome = if !pending.is_empty() || !active.is_empty() {
                    ToolOutcome::failure(
                        "E_UNAWAITED_CALLS",
                        "await all host calls before returning",
                    )
                } else {
                    ToolOutcome::success(value.to_string())
                };
                break;
            }
            GuestState::Failed { error } => {
                outcome = ToolOutcome::failure(&error.code, error.message);
                break;
            }
            GuestState::Pending => {}
        }
        while active.len() < config.max_concurrent_calls {
            let Some(front) = queue.front() else {
                break;
            };
            let safe = parallel(&child, &front.operation);
            if !safe && !active.is_empty() {
                break;
            }
            let request = queue.pop_front().expect("队首存在");
            active.push(Box::pin(perform(&child, request, None)));
            if !safe {
                break;
            }
        }
        if active.is_empty() {
            outcome = ToolOutcome::failure("E_PTC_PENDING", "the script has no completable event");
            break;
        }
        let completed = tokio::select! {
            biased;
            _ = ctx.cancel.cancelled() => { outcome = ToolOutcome::failure("E_CANCELLED", "script cancelled"); break; }
            _ = tokio::time::sleep_until(deadline) => { outcome = ToolOutcome::failure("E_TIMEOUT", "script timed out"); break; }
            completed = active.next() => completed.expect("活动队列非空"),
        };
        let (id, result, tool) = completed;
        if let Some(size) = pending.remove(&id) {
            pending_bytes -= size;
        }
        if let Some(mut tool) = tool {
            images.append(&mut tool.images);
            if tool.control == ToolControl::EndTurn {
                outcome = tool;
                break;
            }
        }
        input = serde_json::to_vec(&[HostCompletion { id, result }]).expect("完成项可序列化");
    }
    // 取消尚在执行的只读／进程子工具；写入事务按已有执行出口完整提交。
    child.cancel.cancel();
    let code = if outcome.error_code.as_deref() == Some("E_TURN_ENDED") {
        "E_TURN_ENDED"
    } else if outcome.error_code.as_deref() == Some("E_TIMEOUT") {
        "E_TIMEOUT"
    } else {
        "E_CANCELLED"
    };
    while let Some(request) = queue.pop_front() {
        let (_, _, tool) = perform(&child, request, Some(code)).await;
        if let Some(mut tool) = tool {
            images.append(&mut tool.images);
        }
    }
    while let Some((_, _, tool)) = active.next().await {
        if let Some(mut tool) = tool {
            images.append(&mut tool.images);
        }
    }
    outcome.images.extend(images);
    outcome
}

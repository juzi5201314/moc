//! 调度脚本发起的宿主调用：按提交顺序进入有上限的队列；不能并行的操作充当屏障；取消后让进行中的事务完整结束。
use super::{
    process::Guest,
    result::{script_result, status},
    state::error,
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
use moc_ptc_protocol::wire::*;
use serde_json::Value;
use std::{
    collections::{BTreeMap, VecDeque},
    time::Instant,
};

type Work<'a> = BoxFuture<'a, (u32, Result<Value, ScriptError>, Option<ToolOutcome>)>;

/// 只扣除用户提问/授权等待的并集；普通工具等待仍受整次 execute 期限约束。
struct ExecutionClock {
    deadline: tokio::time::Instant,
    waiting: usize,
    paused_at: Option<tokio::time::Instant>,
    events: tokio::sync::mpsc::UnboundedReceiver<(bool, tokio::time::Instant)>,
}

impl ExecutionClock {
    async fn wait<F: Future>(
        &mut self,
        future: F,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<F::Output, Box<ToolOutcome>> {
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Box::new(ToolOutcome::failure("E_CANCELLED", "script cancelled"))),
                Some((started, at)) = self.events.recv() => {
                    if started {
                        if self.waiting == 0 { self.paused_at = Some(at); }
                        self.waiting += 1;
                    } else {
                        debug_assert!(self.waiting > 0);
                        self.waiting -= 1;
                        if self.waiting == 0 {
                            self.deadline += at - self.paused_at.take().expect("存在用户等待起点");
                        }
                    }
                }
                _ = tokio::time::sleep_until(self.deadline), if self.waiting == 0 => {
                    return Err(Box::new(ToolOutcome::failure("E_TIMEOUT", "script timed out")));
                }
                result = &mut future => return Ok(result),
            }
        }
    }
}

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
        // 沙盒内的嵌套调用和模型直接发起的调用推送同样的开始/结束更新：`parent_tool_call_id`
        // 指明它属于哪个 `execute`，参数就是马上要执行的那一份。
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
            crate::service::prompt::tools::execute_nested_tool(&ctx.core, ctx, &call, true)
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
                        diff: outcome.diff.clone(),
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
            let ask = ctx.core.ask_available(
                ctx.request.connection_id(),
                ctx.state.lock().await.session.autonomy,
            );
            match request.operation {
                Operation::Servers => {
                    Ok(ctx.catalog.servers(ctx.engine, ctx.mode, ask, &ctx.config))
                }
                Operation::Search {
                    query,
                    server,
                    offset,
                    limit,
                } => ctx
                    .catalog
                    .search(
                        ctx.engine,
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
                    .describe(ctx.engine, ctx.mode, ask, &ctx.config, &name)
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
    deadline: tokio::time::Instant,
) -> ToolOutcome {
    let mut child = ctx.clone();
    child.cancel = ctx.cancel.child_token();
    let (interactions, events) = tokio::sync::mpsc::unbounded_channel();
    child.request = ctx
        .request
        .with_cancel(child.cancel.clone())
        .with_interaction_observer(interactions);
    let mut clock = ExecutionClock {
        deadline,
        waiting: 0,
        paused_at: None,
        events,
    };
    let config = &ctx.config.tools.ptc;
    let mut queue = VecDeque::<HostRequest>::new();
    let mut active = FuturesUnordered::<Work<'_>>::new();
    // 未完成调用的 ID 是稀疏的，数量受 max_pending_calls 限制；结果交付后立即删除。
    let mut pending = BTreeMap::<u32, usize>::new();
    let mut pending_bytes = 0usize;
    let mut next_id = 0u64;
    let mut input = Vec::new();
    let mut images = Vec::new();
    let mut recorded_tools = false;
    let mut outcome;
    'drive: loop {
        let step = match clock
            .wait(guest.step(std::mem::take(&mut input)), &ctx.cancel)
            .await
        {
            Ok(Ok(step)) => step,
            Ok(Err(error)) => {
                outcome = error.into_outcome();
                break;
            }
            Err(error) => {
                outcome = *error;
                break;
            }
        };
        for request in step.requests {
            if u64::from(request.id) != next_id {
                outcome = ToolOutcome::failure(
                    "E_PTC_PROTOCOL",
                    "request IDs must increase and be unique",
                );
                break 'drive;
            }
            if next_id >= config.max_total_calls as u64 {
                outcome = ToolOutcome::failure("E_CALL_LIMIT", "total host call limit exceeded");
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
                    ToolOutcome::success(match value {
                        Value::String(text) => text,
                        value => value.to_string(),
                    })
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
            let request = queue.pop_front().expect("队列非空");
            active.push(Box::pin(perform(&child, request, None)));
            if !safe {
                break;
            }
        }
        if active.is_empty() {
            outcome = ToolOutcome::failure("E_PTC_PENDING", "the script has no completable event");
            break;
        }
        let completed = clock.wait(async {
            tokio::select! {
                biased;
                step = guest.receive() => Err(match step {
                    Ok(GuestStep { state: GuestState::Failed { error }, .. }) => ToolOutcome::failure(&error.code, error.message),
                    Ok(_) => ToolOutcome::failure("E_PTC_PROTOCOL", "unexpected execution step while waiting for host calls"),
                    Err(error) => error.into_outcome(),
                }),
                completed = active.next() => Ok(completed.expect("活动队列非空")),
            }
        }, &ctx.cancel).await;
        let completed = match completed {
            Ok(Ok(completed)) => completed,
            Ok(Err(error)) => {
                outcome = error;
                break;
            }
            Err(error) => {
                outcome = *error;
                break;
            }
        };
        let (id, result, tool) = completed;
        if let Some(size) = pending.remove(&id) {
            pending_bytes -= size;
        }
        if let Some(mut tool) = tool {
            recorded_tools = true;
            images.append(&mut tool.images);
            if tool.control == ToolControl::EndTurn {
                outcome = tool;
                break;
            }
        }
        input.push(HostCompletion { id, result });
    }
    // 取消还在执行的只读和进程类子工具；写入事务照常完整提交。
    child.cancel.cancel();
    let code = if outcome.error_code.as_deref() == Some("E_TURN_ENDED") {
        "E_TURN_ENDED"
    } else if outcome.error_code.as_deref() == Some("E_TIMEOUT") {
        "E_TIMEOUT"
    } else {
        "E_CANCELLED"
    };
    guest.cancel(code);
    while let Some(request) = queue.pop_front() {
        let (_, _, tool) = perform(&child, request, Some(code)).await;
        if let Some(mut tool) = tool {
            recorded_tools = true;
            images.append(&mut tool.images);
        }
    }
    while let Some((_, _, tool)) = active.next().await {
        if let Some(mut tool) = tool {
            recorded_tools = true;
            images.append(&mut tool.images);
        }
    }
    guest.shutdown().await;
    if recorded_tools
        && outcome.is_error
        && !matches!(
            outcome.error_code.as_deref(),
            Some("E_CANCELLED" | "E_TURN_ENDED")
        )
    {
        super::result::recover_children(ctx, &mut outcome).await;
    }
    outcome.images.extend(images);
    outcome
}

#[cfg(test)]
mod clock_tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn only_interaction_union_extends_deadline_and_cancel_still_wins() {
        let now = tokio::time::Instant::now();
        let (tx, events) = tokio::sync::mpsc::unbounded_channel();
        let mut clock = ExecutionClock {
            deadline: now - Duration::from_secs(5),
            waiting: 0,
            paused_at: None,
            events,
        };
        for (started, seconds) in [(true, 10), (true, 8), (false, 6), (false, 1)] {
            tx.send((started, now - Duration::from_secs(seconds)))
                .unwrap();
        }
        let cancel = tokio_util::sync::CancellationToken::new();
        assert_eq!(clock.wait(std::future::ready(7), &cancel).await.unwrap(), 7);
        assert_eq!(clock.deadline, now + Duration::from_secs(4));
        clock.deadline = now - Duration::from_secs(1);
        let error = clock
            .wait(std::future::pending::<()>(), &cancel)
            .await
            .unwrap_err();
        assert_eq!(error.error_code.as_deref(), Some("E_TIMEOUT"));
        tx.send((true, now)).unwrap();
        cancel.cancel();
        let error = clock
            .wait(std::future::pending::<()>(), &cancel)
            .await
            .unwrap_err();
        assert_eq!(error.error_code.as_deref(), Some("E_CANCELLED"));
    }
}

use super::super::workspace::WorkspaceScope;
use super::super::workspace::check_workspace;
use super::CommittedTool;
use super::PendingTool;
use super::ToolBatchResult;
use super::commit_outcome;
use super::note_protocol_fault;
use super::run_step;
use crate::service::CoreInner;
use crate::service::SessionRuntime;
use crate::service::flush_plugin_notices;
use crate::service::protocol_to_rpc;
use crate::service::snapshot_workspace;
use crate::service::to_rpc;
use crate::tools::ToolCtx;
use crate::tools::ToolOutcome;
use futures_util::StreamExt;
use futures_util::stream::FuturesUnordered;
use moc_llm::ContentPart;
use moc_llm::Message;
use moc_llm::Role;
use moc_protocol::RpcError;
use moc_protocol::methods::SessionUpdate;
use moc_protocol::methods::ToolStatus;
use std::sync::Arc;
use std::time::Instant;

pub(in super::super) async fn execute_tool_batch(
    inner: &CoreInner,
    tool_ctx: &ToolCtx,
    runtime: &Arc<SessionRuntime>,
    record: &Arc<crate::session::SessionRecord>,
    calls: &[moc_llm::ToolCall],
) -> Result<ToolBatchResult, RpcError> {
    let config = &tool_ctx.config;
    let parallel_limit = config.tools.max_concurrent_calls;
    debug_assert!(parallel_limit > 0);
    let session_id = tool_ctx.session_id.as_str();
    let prompt_id = tool_ctx.prompt_id.as_str();
    let cwd = &tool_ctx.cwd;
    let update = tool_ctx.request.updates();
    let mut observations = Vec::with_capacity(calls.len());
    let progress_before = tool_ctx.guard.progress();
    if calls.len() > 1 {
        update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolBatchStarted {
                    prompt_id: prompt_id.to_owned(),
                    batch_id: crate::ids::entry_id(),
                    calls: calls
                        .iter()
                        .map(|call| moc_protocol::methods::ToolBatchCall {
                            tool_call_id: call.id.clone(),
                            name: call.name.clone(),
                        })
                        .collect(),
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    // 只读模式不跟踪工作区写入；CLM 引擎走自己的实时上下文视图。
    let tool_batch_before = if tool_ctx.mode == crate::service::mode::Mode::Default
        && matches!(
            tool_ctx.engine,
            moc_protocol::methods::SessionEngine::Standard
                | moc_protocol::methods::SessionEngine::Ptc
        )
        && config.workspace.watch
    {
        Some(
            snapshot_workspace(cwd.clone(), config.workspace.max_scan_entries)
                .await
                .map_err(to_rpc)?,
        )
    } else {
        None
    };
    // 每一步在自身完成时就提交到磁盘：并行批次里先完成的工具不再等慢工具，
    // 崩溃最多丢失尚未完成的那一步。完整结果只在通知前端时短暂保留。
    let mut slots: Vec<Option<CommittedTool>> =
        std::iter::repeat_with(|| None).take(calls.len()).collect();
    let mut started = vec![false; calls.len()];
    let mut pending: FuturesUnordered<PendingTool<'_>> = FuturesUnordered::new();
    let mut stop_reason = None;

    'calls: for (index, call) in calls.iter().enumerate() {
        // 调度只查询并行性；工具解析、入口限制与执行许可统一由 execute_tool 判定。
        let parallel = tool_ctx
            .catalog
            .get(&call.name)
            .is_some_and(|tool| tool.parallel_safe());

        if !parallel {
            while let Some((done, committed)) = pending.next().await {
                let committed = committed.map_err(to_rpc)?;
                note_protocol_fault(&mut stop_reason, &committed);
                slots[done] = Some(committed);
            }
        }
        started[index] = true;
        // 子代理的最近一次工具调用记进注册表，wait/sidekick 的进度行据此展示。
        crate::agents::note_activity(inner, session_id, call).await;
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolStarted {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    parent_tool_call_id: None,
                    arguments: call.arguments.clone(),
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
                (index, run_step(inner, tool_ctx, call, start).await)
            }));
            if pending.len() == parallel_limit
                && let Some((done, committed)) = pending.next().await
            {
                let committed = committed.map_err(to_rpc)?;
                note_protocol_fault(&mut stop_reason, &committed);
                slots[done] = Some(committed);
            }
        } else {
            let committed = run_step(inner, tool_ctx, call, start)
                .await
                .map_err(to_rpc)?;
            note_protocol_fault(&mut stop_reason, &committed);
            let end_turn = committed.end_turn;
            slots[index] = Some(committed);
            if stop_reason.is_some() || end_turn {
                break 'calls;
            }
        }
    }

    if stop_reason.is_none() {
        while let Some((done, committed)) = pending.next().await {
            let committed = committed.map_err(to_rpc)?;
            note_protocol_fault(&mut stop_reason, &committed);
            slots[done] = Some(committed);
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
                        parent_tool_call_id: None,
                        arguments: call.arguments.clone(),
                    },
                )
                .await
        {
            stop_reason = Some(protocol_to_rpc(error));
        }
        // 没执行到的调用在这里补一条结果并提交，保证每个工具调用都有配对结果。
        let committed = match slots[index].take() {
            Some(committed) => committed,
            None => {
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
                commit_outcome(inner, tool_ctx, call, outcome, 0)
                    .await
                    .map_err(to_rpc)?
            }
        };
        if let Some(observation) = committed.observation {
            observations.push(observation);
        }
        let CommittedTool {
            status,
            duration_ms,
            diff,
            end_turn,
            result: tool_result,
            ..
        } = committed;
        if tool_ctx.engine == moc_protocol::methods::SessionEngine::ClmExperimental {
            let message = Message {
                role: Role::Tool,
                content: vec![ContentPart::ToolResult(tool_result.clone())].into(),
            };
            crate::tools::clm::append_message(
                &inner.sessions,
                &tool_ctx.state,
                session_id,
                &message,
            )
            .await
            .map_err(to_rpc)?;
        }
        // todo 停滞提醒的计数：除 todo 外，每次工具调用都加一。
        if call.name != "todo" {
            tool_ctx.state.lock().await.tool_calls_since_todo += 1;
        }
        if let Err(error) = update
            .emit(
                session_id.to_owned(),
                SessionUpdate::ToolFinished {
                    prompt_id: prompt_id.to_owned(),
                    tool_call_id: call.id.clone(),
                    name: call.name.clone(),
                    parent_tool_call_id: None,
                    arguments: call.arguments.clone(),
                    status,
                    result: tool_result,
                    duration_ms: Some(duration_ms),
                    diff,
                },
            )
            .await
            && stop_reason.is_none()
        {
            stop_reason = Some(protocol_to_rpc(error));
        }
        end_prompt |= end_turn;
    }

    check_workspace(
        &update,
        runtime,
        record,
        WorkspaceScope {
            session_id,
            cwd,
            config,
            mode: tool_ctx.mode,
            before_tools: tool_batch_before.as_ref(),
        },
        &tool_ctx.plugins,
        &tool_ctx.plugin_call_context(),
    )
    .await?;
    // 批次里刚被停用的插件要立刻让模型与前端看到，不能等到下一次提示。
    flush_plugin_notices(&tool_ctx.plugins, &record.state, &update, session_id).await?;
    if let Some(error) = stop_reason {
        return Err(error);
    }
    if tool_ctx.guard.progress() != progress_before {
        for (_, changed, _) in &mut observations {
            *changed = true;
        }
    }
    // 全是只读调用的批次不必在意完成顺序；含写入屏障的批次必须按顺序执行。
    if calls.iter().all(|call| {
        inner
            .tools
            .get(&call.name)
            .or_else(|| {
                tool_ctx
                    .catalog
                    .get(&call.name)
                    .filter(|tool| tool.origin() == crate::tools::ToolOrigin::Plugin)
            })
            .is_some_and(|tool| tool.parallel_safe())
    }) {
        observations.sort_unstable_by_key(|(fingerprint, _, _)| *fingerprint);
    }
    Ok(ToolBatchResult {
        control_end: end_prompt,
        observations,
    })
}

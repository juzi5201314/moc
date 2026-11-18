use super::execution::run_prompt_locked;
use super::guard::PromptGuard;
use super::tools::repair_pending_calls;
use crate::service::CoreInner;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use moc_llm::FinishReason;
use moc_llm::Message;
use moc_llm::Usage;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::methods::PromptResult;
use moc_protocol::methods::SessionUpdate;
use std::sync::Arc;
use std::time::Instant;

/// 主会话的后台任务完成时唤醒新一轮。
///
/// 子代理会话无空闲唤醒：notices 留在原地，下次提示自然送达。前台提示仍在运行时也不唤醒，
/// 一次提示结束后才重新检查是否有任务完成待通知。
pub(crate) async fn wake_for_jobs(
    inner: Arc<CoreInner>,
    request: RequestContext,
    session_id: String,
) {
    // 只唤醒顶层会话；运行期间再完成的任务，由结尾再检。
    let Ok(record) = inner.sessions.get(&session_id).await else {
        return;
    };
    if record
        .state
        .lock()
        .await
        .session
        .parent_session_id
        .is_some()
    {
        return;
    }
    let runtime = inner.runtime(&session_id).await;
    if !runtime.jobs.wants_wake() {
        return;
    }
    // 会话空闲时才唤醒，正跑着的提示结束后自己检查 wants_wake 不再轮询。
    let Ok(prompt_lock) = record.try_prompt_lock() else {
        return;
    };
    crate::service::CoreInner::spawn_detached_prompt(
        &inner,
        request,
        moc_protocol::methods::PromptParams {
            session_id: session_id.clone(),
            input: Vec::new(),
            mode: None,
            agent_message: None,
            goal_round: None,
            max_turns: None,
        },
        record,
        prompt_lock,
    )
    .await;
}

pub(super) async fn run_prompt_entry(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
    retry: bool,
) -> Result<PromptResult, RpcError> {
    let record = inner
        .sessions
        .get(&params.session_id)
        .await
        .map_err(to_rpc)?;
    if record
        .state
        .lock()
        .await
        .agent
        .as_ref()
        .is_some_and(|meta| meta.workflow_run.is_some())
    {
        return Err(to_rpc(crate::CoreError::Session(
            "workflow actor 不能通过 session/prompt 或 session/retry 单独执行；请继续所属 workflow run".into(),
        )));
    }
    let prompt = record.try_prompt_lock().map_err(to_rpc)?;
    run_prompt_with_lock(inner, ctx, params, record, prompt, retry).await
}

/// 用调用方已经拿到的提示锁跑一次提示。
///
/// 代理唤醒会在派发前先抢提示锁：抢不到说明该会话还有提示没结束，调用方就把消息
/// 放进收件箱，而不是和那个提示争同一个会话。`retry` 为真时不写用户消息（见
/// [`run_retry`]）。
pub(crate) async fn run_prompt_with_lock(
    inner: Arc<CoreInner>,
    ctx: RequestContext,
    params: moc_protocol::methods::PromptParams,
    record: Arc<crate::session::SessionRecord>,
    prompt: tokio::sync::OwnedMutexGuard<()>,
    retry: bool,
) -> Result<PromptResult, RpcError> {
    let session_id = params.session_id.clone();
    let _prompt = prompt;
    let _epoch = inner.config_epoch.read().await;
    let started = Instant::now();
    inner
        .sessions
        .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
        .await
        .map_err(to_rpc)?;
    let update = ctx.updates();
    update
        .emit(
            params.session_id.clone(),
            SessionUpdate::LockChanged {
                lock: inner.sessions.lease_info(&params.session_id, &record),
            },
        )
        .await
        .map_err(protocol_to_rpc)?;

    repair_pending_calls(&inner, &session_id, &record)
        .await
        .map_err(to_rpc)?;
    let runtime = inner.runtime(&session_id).await;
    runtime.steering.lock().await.accepting = true;
    runtime.agent_inbox.lock().await.accepting = true;
    let parent_id = record.state.lock().await.session.parent_session_id.clone();
    let parent_guard = match parent_id {
        Some(parent_id) => inner.runtime(&parent_id).await.guard.lock().clone(),
        None => None,
    };
    let guard = PromptGuard::new(ctx.cancel().child_token(), parent_guard);
    *runtime.guard.lock() = Some(Arc::clone(&guard));
    // `/goal pause` 按这个标记识别该不该中止当前提示；仅续跑轮有值。
    *runtime.goal_active_round.lock() = params.goal_round.clone();
    // 本轮期间的 armed 代际：收尾摘 armed 只在没别的目标动作插过手时生效，
    // 否则 pause 取消后轮还没收完就 resume，会被晚到的摘位盖掉续跑状态。
    let goal_armed_snapshot = runtime.goal_armed.snapshot();
    let scoped_ctx = ctx.with_cancel(guard.cancel.clone());
    let result = run_prompt_locked(
        Arc::clone(&inner),
        &scoped_ctx,
        params,
        Arc::clone(&record),
        retry,
    )
    .await;
    // 用户取消 lead 的提示时，后台（block=false）仍在跑的 sidekick 一并停掉；
    // 这里必须用外层 ctx 的取消令牌（guard/scoped 令牌不代表用户取消）。
    // 普通 spawn_agent 子代理行为不变。
    if ctx.cancel().is_cancelled() {
        crate::agents::sidekick::stop_running(&inner, &session_id).await;
    }
    let result = if let Some(violation) = guard.violation() {
        async {
            let prompt_id = match &result {
                Ok(result) => result.prompt_id.clone(),
                Err(_) => crate::ids::prompt_id(record.state.lock().await.prompt_sequence),
            };
            let reason = FinishReason::LoopDetected;
            let message = Message::assistant_text(format!(
                "已停止本次提示：{violation}。已完成的工具操作会保留，不会自动重试。"
            ));
            inner
                .sessions
                .append_message(&session_id, message.clone(), None, Some(reason.clone()))
                .await
                .map_err(to_rpc)?;
            update
                .emit(
                    session_id.clone(),
                    SessionUpdate::GuardNotice {
                        prompt_id: prompt_id.clone(),
                        message: message.text(),
                        terminal: true,
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
            let usage = result
                .as_ref()
                .map_or(Usage::default(), |result| result.usage);
            Ok(PromptResult {
                session_id: session_id.clone(),
                prompt_id,
                message,
                finish_reason: reason,
                usage,
            })
        }
        .await
    } else {
        result
    };
    *runtime.guard.lock() = None;
    *runtime.goal_active_round.lock() = None;
    // 推送失败只影响前端能否看到，不能跳过后面的清理：steering 复位、修复工具调用配对、
    // 写入补充消息和广播解锁都必须执行。
    let finished = match &result {
        Ok(result) => update
            .emit(
                session_id.clone(),
                SessionUpdate::PromptFinished {
                    prompt_id: result.prompt_id.clone(),
                    message: result.message.clone(),
                    finish_reason: result.finish_reason.clone(),
                    usage: result.usage,
                },
            )
            .await
            .map_err(protocol_to_rpc)
            .err(),
        Err(_) => update
            .emit(
                session_id.clone(),
                SessionUpdate::PromptFinished {
                    prompt_id: crate::ids::prompt_id(record.state.lock().await.prompt_sequence),
                    message: Message::default(),
                    finish_reason: FinishReason::Other("error".into()),
                    usage: Usage::default(),
                },
            )
            .await
            .map_err(protocol_to_rpc)
            .err(),
    };
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
    for entry in super::super::management::drain_steering(&inner, &session_id, true)
        .await
        .map_err(to_rpc)?
    {
        // 收尾阶段的推送失败只影响前端能否看到，不能跳过后面的清理与解锁。
        let _ = update
            .emit(
                session_id.clone(),
                SessionUpdate::SteeringApplied {
                    id: entry.id,
                    text: entry.text,
                },
            )
            .await;
    }
    // 结束阶段收到的代理消息照样写入会话：模型这轮看不到，但界面和下次唤醒要能看到。
    crate::agents::drain_inbox(&inner, &update, &session_id, true)
        .await
        .map_err(to_rpc)?;
    // 写租约不随提示结束释放：它属于「这个 core 进程打开了该会话」，由 unload 或进程
    // 退出释放，否则另一个进程可以在两次提示之间抢走会话。
    let result = match result {
        Err(error) => Err(error),
        Ok(outcome) => finished.map_or(Ok(outcome), Err),
    };
    // `afterPrompt` 在清理之后触发：失败、取消与循环终止的路径同样能被插件观察到。
    let plugin_state = record.state.lock().await.metadata();
    if let Some(host) = crate::service::plugin_events::host_for(
        &inner,
        std::path::Path::new(&plugin_state.session.cwd),
    )
    .await
    {
        let context = crate::service::plugin_events::context(
            &session_id,
            &plugin_state.session.cwd,
            plugin_state.engine,
            crate::service::mode::Mode::effective(plugin_state.plan_active, None),
            plugin_state.permission,
            &plugin_state.model,
            plugin_state.session.is_subagent(),
        );
        let (prompt_id, finish_reason, usage, error) = match &result {
            Ok(outcome) => (
                outcome.prompt_id.clone(),
                Some(outcome.finish_reason.clone()),
                outcome.usage,
                None,
            ),
            Err(error) => (
                crate::ids::prompt_id(plugin_state.prompt_sequence),
                None,
                Usage::default(),
                Some(format!("{error}")),
            ),
        };
        let loop_detected = matches!(finish_reason, Some(FinishReason::LoopDetected));
        crate::service::plugin_events::after_prompt(
            &host,
            &context,
            &prompt_id,
            finish_reason.as_ref(),
            usage,
            u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX),
            loop_detected,
            error.as_deref(),
        )
        .await;
    }
    // 提示成功结束：如果期间又有任务完成，再次唤醒；失败则不再自动重试，避免循环。
    // 用户取消时抑制唤醒，等用户下次输入重新激活。
    if result.is_ok() && ctx.cancel().is_cancelled() {
        runtime.jobs.suppress_wake(true);
    } else if result.is_ok() {
        runtime.jobs.suppress_wake(false);
        drop(plugin_state);
        if runtime.jobs.wants_wake() {
            wake_for_jobs(Arc::clone(&inner), ctx.clone(), session_id.clone()).await;
        }
    }
    // 目标续跑只在一切正常时接力：用户取消、循环终止、RPC 错误、length/cancelled
    // 收场一律摘掉 armed，需要 `/goal resume`（或工具的 update/resume）才恢复。
    let keep_armed = result.as_ref().is_ok_and(|outcome| {
        !ctx.cancel().is_cancelled()
            && !matches!(
                outcome.finish_reason,
                FinishReason::Cancelled | FinishReason::LoopDetected | FinishReason::Length
            )
    });
    if !keep_armed {
        runtime.goal_armed.disarm_if_unchanged(goal_armed_snapshot);
    }
    // 必须先释放提示锁再评估续跑：本轮收尾持有 `_prompt` 时 try_prompt_lock 必失败，
    // 续跑会被自己挡掉。
    drop(_prompt);
    crate::goal::maybe_continue(&inner, &ctx, &session_id).await;
    result
}

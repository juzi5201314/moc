use super::RESULT_PREVIEW_CHARS;
use super::Wakeup;
use super::info_from_session;
use super::parent_snapshot;
use super::preview_text;
use super::prune_terminal_agents;
use super::roster::current_info;
use super::spawn::prepare_agent;
use super::workflow_scope;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::service::prompt::PromptRun;
use crate::session::store::now_ms;
use moc_llm::FinishReason;
use moc_llm::Message;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::UpdateSink;
use moc_protocol::methods::AgentStatus;
use moc_protocol::methods::InputPart;
use moc_protocol::methods::PromptParams;
use moc_protocol::methods::SessionInfo;
use moc_protocol::methods::SessionUpdate;
use std::path::Path;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// 把子代理最近一次工具调用记进注册表：wait_agent/sidekick 的进度行据此
/// 显示「正在做什么」。注册表里没有条目（顶层会话）时是空操作。
///
/// 摘要 = 工具名 + 空格 + 参数里第一个常见字符串字段的首行；参数不是合法
/// JSON 或没有这些字段就只写工具名。整体用 [`preview_text`] 截到 80 字符。
pub(crate) async fn note_activity(inner: &CoreInner, session_id: &str, call: &moc_llm::ToolCall) {
    const ARG_KEYS: [&str; 7] = [
        "command",
        "path",
        "file_path",
        "pattern",
        "query",
        "url",
        "target",
    ];
    let mut summary = call.name.clone();
    if let Ok(args) = serde_json::from_str::<serde_json::Value>(&call.arguments)
        && let Some(arg) = ARG_KEYS
            .iter()
            .find_map(|key| args.get(*key).and_then(serde_json::Value::as_str))
        && let Some(line) = arg.lines().next()
    {
        summary.push(' ');
        summary.push_str(line);
    }
    let (summary, _) = preview_text(&summary, 80);
    if let Some(runtime) = inner.agents.lock().await.items.get_mut(session_id) {
        runtime.activity = Some(summary);
    }
}

/// 子代理的取消令牌挂在「父代理的运行令牌」下：父是注册过的子代理时挂它的
/// `runtime.cancel`，`stop_agent` 停父代理后代级联终止；父是顶层会话（不在
/// 注册表）时挂进程 shutdown。
///
/// 不能挂父的 prompt guard：`guard.cancel` 是请求级 token，父的那次 prompt 一
/// 返回即取消，会把还活着的子代理误杀（经 `spawn_agent` 工具 spawn 必踩）。
pub(crate) async fn child_cancel(inner: &CoreInner, parent_id: &str) -> CancellationToken {
    let registry = inner.agents.lock().await;
    match registry.items.get(parent_id) {
        Some(runtime) => runtime.cancel.child_token(),
        None => inner.shutdown.child_token(),
    }
}

/// 启动一次子代理提示。
///
/// `wakeup` 只对「代理消息唤醒」给出。
pub(super) async fn activate(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_id: String,
    agent_id: String,
    task: String,
    cancel: CancellationToken,
    wakeup: Wakeup,
) -> Result<(), CoreError> {
    let Wakeup {
        agent,
        prompt_lock,
        retry,
    } = wakeup;
    let record = inner.sessions.get(&agent_id).await?;
    let session = record.state.lock().await.session.clone();
    workflow_scope::actor_run(&inner, &session).await?;
    // 锁被占说明该会话还有提示没结束，唤醒只能放弃（调用方据此改走收件箱）。
    let prompt_lock = match prompt_lock {
        Some(lock) => lock,
        None => record.try_prompt_lock()?,
    };
    {
        let mut registry = inner.agents.lock().await;
        let runtime = registry
            .items
            .get_mut(&agent_id)
            .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")))?;
        runtime.info.status = AgentStatus::Running;
        runtime.info.updated_at_ms = now_ms();
    }
    let _ = request
        .updates()
        .emit(
            parent_id.clone(),
            SessionUpdate::AgentStatusChanged {
                agent_id: agent_id.clone(),
                status: AgentStatus::Running,
                last_message: None,
            },
        )
        .await;
    let child_request = request.with_cancel(cancel.clone());
    let task_inner = Arc::clone(&inner);
    let task_request = request.clone();
    let mut tasks = inner.agent_tasks.lock().await;
    // 回收已完成的任务和输出，免得 JoinSet 随会话时间无限增长。
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            tracing::error!(%error, "子代理任务失败");
        }
    }
    tasks.spawn(async move {
        let result = crate::service::prompt::run_prompt_with_lock(
            Arc::clone(&task_inner),
            child_request,
            PromptParams {
                session_id: agent_id.clone(),
                // retry 唤醒不带新输入；剪尾重放由 run_prompt_locked 负责。
                input: if retry {
                    Vec::new()
                } else {
                    vec![InputPart::Text { text: task }]
                },
                mode: None,
                agent_message: agent,
                goal_round: None,
                max_turns: None,
            },
            record,
            prompt_lock,
            if retry {
                PromptRun::Retry
            } else {
                PromptRun::Normal
            },
            false,
        )
        .await;
        finish(
            task_inner,
            task_request,
            parent_id,
            agent_id,
            cancel,
            result,
        )
        .await;
    });
    Ok(())
}

pub(super) async fn finish(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_id: String,
    agent_id: String,
    cancel: CancellationToken,
    result: Result<moc_protocol::methods::PromptResult, RpcError>,
) {
    let (status, message, finish_reason) = match result {
        Ok(result) => {
            let status = if cancel.is_cancelled()
                || result.finish_reason == FinishReason::Cancelled
                || result.finish_reason.is_guard_stop()
            {
                AgentStatus::Stopped
            } else {
                AgentStatus::Completed
            };
            (status, Some(result.message), Some(result.finish_reason))
        }
        Err(error) if cancel.is_cancelled() => {
            tracing::debug!(agent_id = %agent_id, %error, "子代理已取消");
            (AgentStatus::Stopped, None, Some(FinishReason::Cancelled))
        }
        Err(error) => {
            // 父会话只看到 failed 状态时看不出原因，所以失败消息和日志都要留下。
            tracing::warn!(agent_id = %agent_id, %error, "子代理提示失败");
            (
                AgentStatus::Failed,
                Some(Message::assistant_text(format!(
                    "Subagent prompt failed: {error}"
                ))),
                None,
            )
        }
    };
    let finish_for_event = finish_reason.clone();
    let (message, last_message, title, notify, awaited) = {
        let mut registry = inner.agents.lock().await;
        let Some(runtime) = registry.items.get_mut(&agent_id) else {
            return;
        };
        // `yield` 报告是子代理自己交付的最终结果，会覆盖模型最后一条正文；
        // 结构化结果留在 runtime 里给 wait 查询。
        let yielded = runtime.yielded.take();
        if let Some(yielded) = &yielded {
            runtime.structured = yielded.result.clone();
        }
        let message = yielded
            .map(|yielded| Message::assistant_text(yielded.report))
            .or(message);
        let last_message = message.as_ref().map(|message| {
            let text = message.text();
            if let Some((index, _)) = text.char_indices().nth(2_000) {
                text[..index].to_owned()
            } else {
                text
            }
        });
        runtime.info.last_message = last_message.clone();
        runtime.message = message.clone();
        runtime.finish_reason = finish_reason;
        let title = runtime.info.title.clone();
        let notify = Arc::clone(&runtime.notify);
        let awaited = runtime.awaited;
        (message, last_message, title, notify, awaited)
    };
    // 阻塞中的 `sidekick` 调用已把报告放在工具结果里，完成通知不再重复投递。
    if !awaited
        && let Err(error) = deliver_result(
            &inner,
            &request.updates(),
            &parent_id,
            &agent_id,
            &title,
            status,
            message.as_ref(),
        )
        .await
    {
        tracing::error!(agent_id, %error, "子代理结果投递失败");
    }
    // 先提交报告与模型提醒，再公开终态；wait_agent 返回时结果已经可读取。
    {
        let mut registry = inner.agents.lock().await;
        if let Some(runtime) = registry.items.get_mut(&agent_id) {
            runtime.info.status = status;
            runtime.info.updated_at_ms = now_ms();
        }
        prune_terminal_agents(&mut registry);
    }
    notify.notify_waiters();
    let _ = request
        .updates()
        .emit(
            parent_id.clone(),
            SessionUpdate::AgentStatusChanged {
                agent_id: agent_id.clone(),
                status,
                last_message,
            },
        )
        .await;
    if let Ok(parent) = parent_snapshot(&inner, &parent_id).await
        && let Some(host) =
            crate::service::plugin_events::host_for(&inner, Path::new(&parent.session.cwd)).await
    {
        let context = crate::service::plugin_events::context(
            &parent_id,
            &parent.session.cwd,
            parent.engine,
            crate::service::mode::Mode::effective(parent.plan_active, None),
            parent.permission,
            &parent.model,
            parent.session.is_subagent(),
        );
        if let Ok(agent) = current_info(&inner, &agent_id).await {
            crate::service::plugin_events::agent_finished(
                &host,
                &context,
                &agent,
                finish_for_event.as_ref(),
            )
            .await;
        }
    }
}

/// 保存完整展示结果并推送卡片；模型仍只接收原有的有界结果提醒。
pub(super) async fn deliver_result(
    inner: &CoreInner,
    updates: &UpdateSink,
    parent_id: &str,
    agent_id: &str,
    title: &str,
    status: AgentStatus,
    message: Option<&Message>,
) -> Result<(), CoreError> {
    let label = match status {
        AgentStatus::Completed => "completed",
        AgentStatus::Stopped => "stopped",
        AgentStatus::Failed => "failed",
        _ => return Ok(()),
    };
    // 回退孤儿的收尾通知不进父会话 notice 队列：它在 transcript 里的痕迹已被
    // 截断，"已停止"通知只会成为指向死时间线的噪音。
    if inner
        .sessions
        .orphan_marks(parent_id)
        .await
        .contains(agent_id)
    {
        return Ok(());
    }
    let parent = inner.sessions.get(parent_id).await?;
    let mut notice = format!("Subagent \"{title}\" ({agent_id}) {label}");
    let body = message.map_or_else(String::new, Message::text);
    if !body.trim().is_empty() {
        let (preview, truncated) = preview_text(&body, RESULT_PREVIEW_CHARS);
        notice.push_str(&format!(". Result preview:\n{preview}"));
        if truncated {
            notice.push_str("\n(truncated; wait_agent returns the full result)");
        }
    }
    let meta = moc_protocol::methods::AgentResultMeta {
        agent_id: agent_id.to_owned(),
        title: title.to_owned(),
        status,
    };
    let entry = inner
        .sessions
        .append(
            parent_id,
            crate::session::entries::EntryPayload::AgentResult {
                meta: meta.clone(),
                report: body,
            },
        )
        .await?;
    parent
        .state
        .lock()
        .await
        .notices
        .push(crate::session::state::SessionNotice::new(notice));
    updates
        .emit(
            parent_id,
            SessionUpdate::AgentResult {
                entry_id: entry.id().to_owned(),
                at_ms: entry.at_ms(),
                meta,
                body: match entry {
                    crate::session::entries::Entry::Message { message, .. } => message.text(),
                    _ => unreachable!("结果载荷只生成消息条目"),
                },
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))
}

/// sidekick `mode="retry"` 的唤醒：失败/停止态的代理会话不带新消息重跑一轮。
///
/// 与 `deliver` 的唤醒路径同构：重建注册表条目（收件箱重开、cancel 重新挂
/// 父令牌），预占的提示锁转交给 `activate`；`retry = true` 让
/// `run_prompt_locked` 走剪尾+重放分支而不是追加输入。
pub(crate) async fn retry_wakeup(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_id: String,
    target: &SessionInfo,
    prompt_lock: tokio::sync::OwnedMutexGuard<()>,
) -> Result<(), CoreError> {
    let info = info_from_session(target, AgentStatus::Queued);
    let cancel = child_cancel(&inner, &parent_id).await;
    prepare_agent(&inner, &info, cancel.clone()).await;
    activate(
        inner,
        request,
        parent_id,
        target.id.clone(),
        String::new(),
        cancel,
        Wakeup {
            agent: None,
            prompt_lock: Some(prompt_lock),
            retry: true,
        },
    )
    .await
}

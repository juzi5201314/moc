use super::AgentEntry;
use super::AgentInbox;
use super::BROADCAST_TARGET;
use super::MAX_PENDING_AGENT_MESSAGES;
use super::MessageTarget;
use super::PARENT_TARGET;
use super::Peers;
use super::PendingAgentMessage;
use super::Wakeup;
use super::Yielded;
use super::info_from_session;
use super::invalid;
use super::lifecycle::activate;
use super::live;
use super::preset;
use super::roster::peers;
use super::spawn::prepare_agent;
use super::validate_text;
use super::workflow_scope;
use crate::agents::child_cancel;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::service::prompt::PromptRun;
use moc_llm::Message;
use moc_protocol::RequestContext;
use moc_protocol::UpdateSink;
use moc_protocol::methods::AgentMessageMeta;
use moc_protocol::methods::AgentMessageOutcome;
use moc_protocol::methods::AgentMessageReceipt;
use moc_protocol::methods::AgentMessageResult;
use moc_protocol::methods::AgentStatus;
use moc_protocol::methods::InputPart;
use moc_protocol::methods::PromptParams;
use moc_protocol::methods::SessionUpdate;
use std::path::Path;
use std::sync::Arc;

/// sidekick 只能经 `sidekick` 工具驱动：`message_agent` 绕开 handoff 的模型
/// 同步，非 Fusion 顶层会话唤醒它还会在旧模型上继续计费。
/// `from_subagent`：sidekick 自己或兄弟子代理不受此限（如 sidekick 发给 parent）。
pub(super) fn resolve_targets(
    peers: &Peers,
    target: &str,
    from_subagent: bool,
) -> Result<Vec<MessageTarget>, CoreError> {
    let is_sidekick = |entry: &&AgentEntry| {
        entry.session.agent_preset.as_deref() == Some(preset::SIDEKICK_PRESET)
    };
    if target == BROADCAST_TARGET {
        let targets: Vec<MessageTarget> = peers
            .siblings_or_children()
            .iter()
            .filter(|entry| live(entry.status))
            .filter(|entry| from_subagent || !is_sidekick(entry))
            .map(|entry| MessageTarget {
                session: entry.session.clone(),
            })
            .collect();
        if targets.is_empty() {
            return Err(invalid("no running agents available to message"));
        }
        return Ok(targets);
    }
    if target == PARENT_TARGET {
        let Some(parent) = &peers.parent else {
            return Err(invalid("top-level session has no parent agent"));
        };
        return Ok(vec![MessageTarget {
            session: parent.session.clone(),
        }]);
    }
    if target == peers.me.id {
        return Err(invalid("cannot message self"));
    }
    let matched = peers
        .parent
        .iter()
        .chain(peers.siblings.iter())
        .chain(peers.children.iter())
        .find(|entry| entry.session.id == target);
    match matched {
        Some(entry) if !from_subagent && is_sidekick(&entry) => Err(invalid(
            "Address the sidekick through the sidekick tool (available only in Fusion sessions).",
        )),
        Some(entry) => Ok(vec![MessageTarget {
            session: entry.session.clone(),
        }]),
        None => Err(invalid(format!("target `{target}` is not deliverable"))),
    }
}

/// `agent/message`：给其它代理投递消息；目标在运行就注入，空闲就唤醒。
pub(crate) async fn message(
    inner: Arc<CoreInner>,
    request: RequestContext,
    session_id: String,
    target: String,
    text: String,
) -> Result<AgentMessageResult, CoreError> {
    message_scoped(inner, request, session_id, target, text, None).await
}

/// 仅 workflow driver 持有此入口，run 身份不会暴露为模型可传参数。
pub(crate) async fn message_workflow(
    inner: Arc<CoreInner>,
    request: RequestContext,
    session_id: String,
    run_id: &str,
    target: String,
    text: String,
) -> Result<AgentMessageResult, CoreError> {
    message_scoped(inner, request, session_id, target, text, Some(run_id)).await
}

pub(super) async fn message_scoped(
    inner: Arc<CoreInner>,
    request: RequestContext,
    session_id: String,
    target: String,
    text: String,
    workflow_run: Option<&str>,
) -> Result<AgentMessageResult, CoreError> {
    let text = validate_text(&text, "message")?;
    let target = validate_text(&target, "target")?;
    let peers = peers(&inner, &session_id).await?;
    // 调用方自己已被父会话回退孤立：它发出的一切消息都属于死时间线。
    if let Some(parent_id) = peers.me.parent_session_id.as_deref()
        && inner
            .sessions
            .orphan_marks(parent_id)
            .await
            .contains(&peers.me.id)
    {
        return Err(CoreError::Session(format!(
            "AGENT_ORPHANED: {} was orphaned by parent rewind and cannot send messages",
            peers.me.id
        )));
    }
    let from = AgentMessageMeta {
        from_id: peers.me.id.clone(),
        from_title: peers.me.title.clone(),
        delivery: AgentMessageOutcome::Injected,
    };
    let targets = resolve_targets(&peers, &target, peers.is_subagent())?;
    for target in &targets {
        workflow_scope::authorize_message(&inner, &peers.me, &target.session, workflow_run).await?;
    }
    let mut receipts = Vec::with_capacity(targets.len());
    for target in targets {
        receipts.push(deliver(&inner, &request, &from, &target, &text).await);
    }
    Ok(AgentMessageResult { receipts })
}

pub(super) fn receipt(
    target: &MessageTarget,
    outcome: AgentMessageOutcome,
    error: Option<String>,
) -> AgentMessageReceipt {
    AgentMessageReceipt {
        agent_id: target.session.id.clone(),
        title: target.session.title.clone(),
        outcome,
        error,
    }
}

/// 投递一条消息：运行中的目标进收件箱，空闲目标唤醒一个新回合。
pub(super) async fn deliver(
    inner: &Arc<CoreInner>,
    request: &RequestContext,
    from: &AgentMessageMeta,
    target: &MessageTarget,
    text: &str,
) -> AgentMessageReceipt {
    let session_runtime = inner.runtime(&target.session.id).await;
    {
        let mut inbox = session_runtime.agent_inbox.lock().await;
        if inbox.accepting {
            return push_inbox(&mut inbox, from, text, target);
        }
    }
    let record = match inner.sessions.get(&target.session.id).await {
        Ok(record) => record,
        Err(error) => {
            return receipt(target, AgentMessageOutcome::Failed, Some(error.to_string()));
        }
    };
    // 提示锁被占，说明该会话还有提示没结束，此刻唤醒必然和它抢锁。
    let Ok(prompt_lock) = record.try_prompt_lock() else {
        return match session_runtime.agent_inbox.lock().await {
            mut inbox if inbox.accepting => push_inbox(&mut inbox, from, text, target),
            _ => receipt(
                target,
                AgentMessageOutcome::Failed,
                Some("target session is finishing; retry shortly".into()),
            ),
        };
    };
    let woken = AgentMessageMeta {
        delivery: AgentMessageOutcome::Woken,
        ..from.clone()
    };
    match &target.session.parent_session_id {
        Some(parent_id) => {
            // 提示锁可能先于旧 finish 释放；等收尾结束后才能重建条目，
            // 否则旧结果、终态和通知会落到新一轮上。
            session_runtime.wait_agent_finish().await;
            let info = info_from_session(&target.session, AgentStatus::Queued);
            let cancel = match workflow_scope::actor_run(inner, &target.session).await {
                Ok(Some(run)) => run.cancel.child_token(),
                Ok(None) => child_cancel(inner, parent_id).await,
                Err(error) => {
                    return receipt(target, AgentMessageOutcome::Failed, Some(error.to_string()));
                }
            };
            prepare_agent(inner, &info, cancel.clone()).await;
            if let Err(error) = activate(
                Arc::clone(inner),
                request.clone(),
                parent_id.clone(),
                target.session.id.clone(),
                text.to_owned(),
                cancel,
                Wakeup {
                    agent: Some(woken),
                    prompt_lock: Some(prompt_lock),
                    retry: false,
                },
            )
            .await
            {
                return receipt(target, AgentMessageOutcome::Failed, Some(error.to_string()));
            }
        }
        None => {
            CoreInner::spawn_detached_prompt(
                inner,
                request.clone(),
                PromptParams {
                    session_id: target.session.id.clone(),
                    input: vec![InputPart::Text {
                        text: text.to_owned(),
                    }],
                    mode: None,
                    agent_message: Some(woken),
                    goal_round: None,
                    max_turns: None,
                },
                record,
                prompt_lock,
                PromptRun::Normal,
            )
            .await;
        }
    }
    receipt(target, AgentMessageOutcome::Woken, None)
}

/// 放入收件箱；队列满了就明确回报失败。
pub(super) fn push_inbox(
    inbox: &mut AgentInbox,
    from: &AgentMessageMeta,
    text: &str,
    target: &MessageTarget,
) -> AgentMessageReceipt {
    if inbox.messages.len() >= MAX_PENDING_AGENT_MESSAGES {
        return receipt(
            target,
            AgentMessageOutcome::Failed,
            Some("inbox full (64 pending); retry shortly".into()),
        );
    }
    inbox.messages.push_back(PendingAgentMessage {
        meta: from.clone(),
        message: Message::user_text(text),
    });
    inbox.notify.notify_waiters();
    receipt(target, from.delivery, None)
}

/// 把收件箱里的消息写入会话，并推送 `SessionUpdate::AgentMessage`。
pub(crate) async fn drain_inbox(
    inner: &CoreInner,
    updates: &UpdateSink,
    session: &str,
    closing: bool,
) -> Result<usize, CoreError> {
    let runtime = inner.runtime(session).await;
    let pending = {
        let mut inbox = runtime.agent_inbox.lock().await;
        if closing {
            inbox.accepting = false;
        }
        std::mem::take(&mut inbox.messages)
    };
    // 回退孤儿在标记生效前塞进收件箱的滞留消息属于死时间线：落盘就是新历史，
    // 会污染回退后的 transcript 并作废前滚尾部，直接丢弃。
    let orphans = inner.sessions.orphan_marks(session).await;
    let pending: Vec<PendingAgentMessage> = pending
        .into_iter()
        .filter(|queued| !orphans.contains(&queued.meta.from_id))
        .collect();
    let count = pending.len();
    if count == 0 {
        return Ok(0);
    }
    let plugin_state = inner
        .sessions
        .get(session)
        .await?
        .state
        .lock()
        .await
        .clone();
    let plugin_host =
        crate::service::plugin_events::host_for(inner, Path::new(&plugin_state.session.cwd)).await;
    let plugin_context = plugin_host.as_ref().map(|_| {
        crate::service::plugin_events::context(
            session,
            &plugin_state.session.cwd,
            plugin_state.engine,
            crate::service::mode::Mode::effective(plugin_state.plan_active, None),
            plugin_state.permission,
            &plugin_state.model,
            plugin_state.session.is_subagent(),
        )
    });
    for PendingAgentMessage { meta, message } in pending {
        let mut message = message;
        if let (Some(host), Some(context)) = (&plugin_host, &plugin_context) {
            let images = message
                .content
                .iter()
                .filter(|part| matches!(part, moc_llm::ContentPart::Image { .. }))
                .count();
            let rewrite = crate::service::plugin_events::message_received(
                host,
                context,
                "agent",
                &message.text(),
                images,
                Some(&meta),
            )
            .await;
            if let Some(text) = rewrite.text {
                message = crate::service::plugin_events::rewrite_message_text(message, text);
            }
        }
        inner
            .sessions
            .append_agent_message(session, message.clone(), meta.clone())
            .await?;
        if let Err(error) = updates
            .emit(
                session,
                SessionUpdate::AgentMessage {
                    meta,
                    body: message.text(),
                },
            )
            .await
        {
            tracing::warn!(session = %session, %error, "代理消息卡片推送失败");
        }
    }
    Ok(count)
}

/// 提示自然收尾前调用：收件箱还有积压消息就返回 `false`（调用方继续跑一轮把
/// 消息答掉）；为空则原子关闭投递窗口并返回 `true`，此后新消息只能走唤醒路径。
///
/// 与「结束阶段再 drain 一次」不同：那时消息虽落盘但永远拿不到应答，这里保证
/// 不留下无人应答的投递。
pub(crate) async fn close_inbox_if_empty(inner: &CoreInner, session: &str) -> bool {
    let runtime = inner.runtime(session).await;
    let mut inbox = runtime.agent_inbox.lock().await;
    if !inbox.messages.is_empty() {
        return false;
    }
    inbox.accepting = false;
    true
}

/// `yield`：记录子代理交付的最终报告；声明了 `output_schema` 的子代理
/// 必须同时交付通过校验的结构化结果。
pub(crate) async fn record_yield(
    inner: &CoreInner,
    session_id: &str,
    report: String,
    result: Option<serde_json::Value>,
) -> Result<(), CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let schema = record
        .state
        .lock()
        .await
        .agent
        .as_ref()
        .and_then(|meta| meta.output_schema.clone());
    let result = match (&schema, result) {
        (Some(schema), Some(result)) => {
            let validator = jsonschema::validator_for(schema)
                .map_err(|error| invalid(format!("invalid output_schema: {error}")))?;
            if let Err(error) = validator.validate(&result) {
                return Err(invalid(format!(
                    "yield result failed output_schema validation: {error}"
                )));
            }
            Some(result)
        }
        (Some(_), None) => {
            return Err(invalid(
                "this subagent declares output_schema; yield requires `result`",
            ));
        }
        (None, Some(_)) => {
            return Err(invalid(
                "this subagent has no output_schema; omit `result`",
            ));
        }
        (None, None) => None,
    };
    let report = validate_text(&report, "report")?;
    let mut registry = inner.agents.lock().await;
    let Some(runtime) = registry.items.get_mut(session_id) else {
        return Err(invalid("yield can only be called while running as a subagent"));
    };
    if !live(runtime.info.status) {
        return Err(invalid("yield can only be called while running as a subagent"));
    }
    runtime.yielded = Some(Yielded { report, result });
    Ok(())
}

/// 子代理本轮是否已交付 `yield` 报告。
pub(crate) async fn has_yielded(inner: &CoreInner, session_id: &str) -> bool {
    inner
        .agents
        .lock()
        .await
        .items
        .get(session_id)
        .is_some_and(|runtime| runtime.yielded.is_some())
}

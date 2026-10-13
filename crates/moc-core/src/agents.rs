//! 子代理运行时 registry、持久子会话控制与生命周期收尾。

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moc_llm::{FinishReason, Message, Role};
use moc_protocol::methods::{
    AgentInfo, AgentListResult, AgentSendResult, AgentSpawnResult, AgentStatus, AgentStopResult,
    AgentWaitResult, InputPart, PromptParams, SessionUpdate,
};
use moc_protocol::{RequestContext, RpcError, SteerResult};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::store::now_ms;

pub(crate) struct AgentRuntime {
    pub info: AgentInfo,
    pub cancel: CancellationToken,
    pub notify: Arc<Notify>,
    pub message: Option<Message>,
    pub finish_reason: Option<FinishReason>,
}

#[derive(Default)]
pub(crate) struct AgentRegistry {
    pub items: BTreeMap<String, AgentRuntime>,
}

fn terminal(status: AgentStatus) -> bool {
    matches!(
        status,
        AgentStatus::Completed | AgentStatus::Stopped | AgentStatus::Failed | AgentStatus::Idle
    )
}

/// 常驻注册表保留的已完成子代理条目上限；更早的终态条目按更新时间回收。
///
/// 终态信息仍可从 transcript 复原（`list_agents` 会回退到会话信息），
/// 这里只需为最近的 `wait_agent` 查询保留一段窗口。
const MAX_TERMINAL_AGENTS: usize = 64;

/// 回收超出上限的终态条目；运行中的条目永远保留。
fn prune_terminal_agents(registry: &mut AgentRegistry) {
    let terminal_count = registry
        .items
        .values()
        .filter(|runtime| terminal(runtime.info.status))
        .count();
    let excess = terminal_count.saturating_sub(MAX_TERMINAL_AGENTS);
    if excess == 0 {
        return;
    }
    let mut finished: Vec<(u64, String)> = registry
        .items
        .iter()
        .filter(|(_, runtime)| terminal(runtime.info.status))
        .map(|(id, runtime)| (runtime.info.updated_at_ms, id.clone()))
        .collect();
    finished.sort_unstable();
    for (_, id) in finished.into_iter().take(excess) {
        registry.items.remove(&id);
    }
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::Config(format!("INVALID_PARAMS: {}", message.into()))
}

fn validate_text(value: &str, label: &str) -> Result<String, CoreError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(invalid(format!("{label} 不能为空")));
    }
    Ok(value.to_owned())
}

async fn parent_snapshot(
    inner: &CoreInner,
    parent_id: &str,
) -> Result<crate::session::state::SessionState, CoreError> {
    let record = inner.sessions.get(parent_id).await?;
    Ok(record.state.lock().await.clone())
}

fn resolve_model(
    config: &crate::ResolvedConfig,
    parent_model: &str,
    requested: Option<&str>,
) -> Result<String, CoreError> {
    let selector = match requested.unwrap_or("inherit") {
        "inherit" => parent_model,
        role @ ("default" | "explore" | "plan") => {
            return Ok(config.resolve_role(role)?.selector);
        }
        other => {
            return Err(CoreError::Config(format!(
                "INVALID_PARAMS: 子代理 model 只允许 inherit、default、explore、plan，收到 {other}"
            )));
        }
    };
    Ok(config.resolve_model(selector)?.selector)
}

fn info_from_session(
    session: &moc_protocol::methods::SessionInfo,
    status: AgentStatus,
) -> AgentInfo {
    AgentInfo {
        id: session.id.clone(),
        parent_id: session.parent_session_id.clone().unwrap_or_default(),
        title: session.title.clone(),
        model: session.model.clone(),
        cwd: session.cwd.clone(),
        status,
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        last_message: None,
    }
}

pub(crate) async fn spawn(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_session_id: String,
    task: String,
    title: Option<String>,
    model: Option<String>,
) -> Result<AgentSpawnResult, CoreError> {
    let task = validate_text(&task, "task")?;
    let parent = parent_snapshot(&inner, &parent_session_id).await?;
    let config = inner.project_config(Path::new(&parent.session.cwd))?;
    let model = resolve_model(&config, &parent.model, model.as_deref())?;
    let session = inner
        .sessions
        .create(
            &parent.session.cwd,
            &model,
            moc_protocol::SessionMode::Normal,
            parent.permission,
            title.as_deref(),
            Some(parent_session_id.clone()),
        )
        .await?;
    let info = info_from_session(&session, AgentStatus::Queued);
    let cancel = match inner.runtime(&parent_session_id).await.guard.lock().clone() {
        Some(guard) => guard.cancel.child_token(),
        None => inner.shutdown.child_token(),
    };
    let runtime = AgentRuntime {
        info: info.clone(),
        cancel: cancel.clone(),
        notify: Arc::new(Notify::new()),
        message: None,
        finish_reason: None,
    };
    inner
        .agents
        .lock()
        .await
        .items
        .insert(info.id.clone(), runtime);
    let _ = request
        .updates()
        .emit(
            parent_session_id.clone(),
            SessionUpdate::AgentStarted {
                agent: info.clone(),
            },
        )
        .await;
    activate(
        Arc::clone(&inner),
        request,
        parent_session_id,
        info.id.clone(),
        task,
        cancel,
    )
    .await?;
    let info = current_info(&inner, &info.id).await?;
    Ok(AgentSpawnResult { agent: info })
}

async fn activate(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_id: String,
    agent_id: String,
    task: String,
    cancel: CancellationToken,
) -> Result<(), CoreError> {
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
    let child_request = request.as_subagent(cancel.clone());
    let task_inner = Arc::clone(&inner);
    let task_request = request.clone();
    let mut tasks = inner.agent_tasks.lock().await;
    // 已完成的任务连同输出一起回收，避免 JoinSet 随会话时长无界增长。
    while let Some(result) = tasks.try_join_next() {
        if let Err(error) = result {
            tracing::error!(%error, "子代理任务失败");
        }
    }
    tasks.spawn(async move {
        let result = crate::service::prompt::run_prompt(
            Arc::clone(&task_inner),
            child_request,
            PromptParams {
                session_id: agent_id.clone(),
                input: vec![InputPart::Text { text: task }],
                mode: None,
            },
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

async fn finish(
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
            // 父会话只看到 failed 状态时无从判断原因：失败消息与日志都要留下。
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
    let last_message = message.as_ref().map(|message| {
        let text = message.text();
        if let Some((index, _)) = text.char_indices().nth(2_000) {
            text[..index].to_owned()
        } else {
            text
        }
    });
    let notify = {
        let mut registry = inner.agents.lock().await;
        let Some(runtime) = registry.items.get_mut(&agent_id) else {
            return;
        };
        runtime.info.status = status;
        runtime.info.updated_at_ms = now_ms();
        runtime.info.last_message = last_message.clone();
        runtime.message = message;
        runtime.finish_reason = finish_reason;
        let notify = Arc::clone(&runtime.notify);
        prune_terminal_agents(&mut registry);
        notify
    };
    notify.notify_waiters();
    let _ = request
        .updates()
        .emit(
            parent_id,
            SessionUpdate::AgentStatusChanged {
                agent_id,
                status,
                last_message,
            },
        )
        .await;
}

async fn current_info(inner: &CoreInner, agent_id: &str) -> Result<AgentInfo, CoreError> {
    inner
        .agents
        .lock()
        .await
        .items
        .get(agent_id)
        .map(|runtime| runtime.info.clone())
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")))
}

pub(crate) async fn list(
    inner: &CoreInner,
    parent_id: String,
) -> Result<AgentListResult, CoreError> {
    let sessions = inner.sessions.list().await?;
    let mut agents = Vec::new();
    for session in sessions {
        if session.parent_session_id.as_deref() != Some(parent_id.as_str()) {
            continue;
        }
        let mut info = info_from_session(&session, AgentStatus::Idle);
        if let Ok(loaded) = inner.sessions.load(&session.id, Some(128)).await {
            info.last_message = loaded
                .entries
                .iter()
                .rev()
                .find(|entry| entry.message.role == Role::Assistant)
                .map(|entry| entry.message.text());
        }
        let registry = inner.agents.lock().await;
        if let Some(runtime) = registry.items.get(&session.id) {
            info = runtime.info.clone();
        }
        drop(registry);
        agents.push(info);
    }
    agents.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(AgentListResult { agents })
}

fn find_parent<'a>(
    sessions: &'a [moc_protocol::methods::SessionInfo],
    id: &str,
    parent_id: &str,
) -> Result<&'a moc_protocol::methods::SessionInfo, CoreError> {
    let session = sessions
        .iter()
        .find(|session| session.id == id)
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {id}")))?;
    if session.parent_session_id.as_deref() != Some(parent_id) {
        return Err(invalid("目标不是调用方的直接子代理"));
    }
    Ok(session)
}

pub(crate) async fn send(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_id: String,
    agent_id: String,
    message: String,
) -> Result<AgentSendResult, CoreError> {
    let message = validate_text(&message, "message")?;
    let sessions = inner.sessions.list().await?;
    let session = find_parent(&sessions, &agent_id, &parent_id)?;
    let info = info_from_session(session, AgentStatus::Queued);
    let cancel = match inner.runtime(&parent_id).await.guard.lock().clone() {
        Some(guard) => guard.cancel.child_token(),
        None => inner.shutdown.child_token(),
    };
    {
        let mut registry = inner.agents.lock().await;
        if let Some(runtime) = registry.items.get(&agent_id)
            && matches!(
                runtime.info.status,
                AgentStatus::Queued | AgentStatus::Running
            )
        {
            return Ok(AgentSendResult {
                accepted: false,
                status: runtime.info.status,
            });
        }
        registry.items.insert(
            agent_id.clone(),
            AgentRuntime {
                info,
                cancel: cancel.clone(),
                notify: Arc::new(Notify::new()),
                message: None,
                finish_reason: None,
            },
        );
    }
    activate(inner, request, parent_id, agent_id, message, cancel).await?;
    Ok(AgentSendResult {
        accepted: true,
        status: AgentStatus::Running,
    })
}

pub(crate) async fn steer(
    inner: &CoreInner,
    parent_id: String,
    agent_id: String,
    message: String,
) -> Result<SteerResult, CoreError> {
    let message = validate_text(&message, "message")?;
    let runtime = inner
        .agents
        .lock()
        .await
        .items
        .get(&agent_id)
        .map(|runtime| (runtime.info.parent_id.clone(), runtime.info.status));
    let Some((runtime_parent, status)) = runtime else {
        return Err(CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")));
    };
    if runtime_parent != parent_id {
        return Err(invalid("目标不是调用方的直接子代理"));
    }
    if status != AgentStatus::Running {
        return Ok(SteerResult { accepted: false });
    }
    let runtime = inner.runtime(&agent_id).await;
    let mut steering = runtime.steering.lock().await;
    if !steering.accepting {
        return Ok(SteerResult { accepted: false });
    }
    steering.messages.push_back(Message::user_text(message));
    Ok(SteerResult { accepted: true })
}

pub(crate) async fn stop(
    inner: &CoreInner,
    parent_id: String,
    agent_id: String,
    _reason: Option<String>,
) -> Result<AgentStopResult, CoreError> {
    let mut registry = inner.agents.lock().await;
    let runtime = registry
        .items
        .get_mut(&agent_id)
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")))?;
    if runtime.info.parent_id != parent_id {
        return Err(invalid("目标不是调用方的直接子代理"));
    }
    if matches!(
        runtime.info.status,
        AgentStatus::Queued | AgentStatus::Running
    ) {
        runtime.cancel.cancel();
        return Ok(AgentStopResult {
            stopped: true,
            status: AgentStatus::Stopped,
        });
    }
    Ok(AgentStopResult {
        stopped: false,
        status: runtime.info.status,
    })
}

pub(crate) async fn wait(
    inner: &CoreInner,
    parent_id: String,
    agent_id: String,
    timeout_ms: Option<u64>,
) -> Result<AgentWaitResult, CoreError> {
    if timeout_ms == Some(0) {
        return Err(invalid("timeout_ms 必须大于 0"));
    }
    let notify = {
        let registry = inner.agents.lock().await;
        if let Some(runtime) = registry.items.get(&agent_id) {
            if runtime.info.parent_id != parent_id {
                return Err(invalid("目标不是调用方的直接子代理"));
            }
            if terminal(runtime.info.status) {
                return Ok(AgentWaitResult {
                    agent: runtime.info.clone(),
                    completed: true,
                    message: runtime.message.clone(),
                    finish_reason: runtime.finish_reason.clone(),
                });
            }
            Some(Arc::clone(&runtime.notify))
        } else {
            None
        }
    };
    let Some(notify) = notify else {
        let sessions = inner.sessions.list().await?;
        let session = find_parent(&sessions, &agent_id, &parent_id)?;
        return Ok(AgentWaitResult {
            agent: info_from_session(session, AgentStatus::Idle),
            completed: true,
            message: None,
            finish_reason: None,
        });
    };
    let completed = if let Some(timeout_ms) = timeout_ms {
        tokio::time::timeout(Duration::from_millis(timeout_ms), notify.notified())
            .await
            .is_ok()
    } else {
        notify.notified().await;
        true
    };
    let registry = inner.agents.lock().await;
    let runtime = registry
        .items
        .get(&agent_id)
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")))?;
    Ok(AgentWaitResult {
        agent: runtime.info.clone(),
        completed,
        message: runtime.message.clone(),
        finish_reason: runtime.finish_reason.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::AgentStatus;

    /// 与 `spawn` 登记的运行态一致，只保留等待语义需要的字段。
    fn running_agent(notify: Arc<Notify>) -> AgentRuntime {
        AgentRuntime {
            info: AgentInfo {
                id: "ses_agent00001".into(),
                parent_id: "ses_parent0001".into(),
                title: "子代理".into(),
                model: "p/m".into(),
                cwd: "/tmp/project".into(),
                status: AgentStatus::Running,
                created_at_ms: 1,
                updated_at_ms: 1,
                last_message: None,
            },
            cancel: CancellationToken::new(),
            notify,
            message: None,
            finish_reason: None,
        }
    }

    #[test]
    fn pruning_keeps_running_agents_and_the_newest_finished_entries() {
        let mut registry = AgentRegistry::default();
        for index in 0..(MAX_TERMINAL_AGENTS + 5) {
            let mut runtime = running_agent(Arc::new(Notify::new()));
            runtime.info.id = format!("ses_done{index:05}");
            runtime.info.status = AgentStatus::Completed;
            runtime.info.updated_at_ms = u64::try_from(index).expect("索引可表示为时间戳");
            registry.items.insert(runtime.info.id.clone(), runtime);
        }
        let mut active = running_agent(Arc::new(Notify::new()));
        active.info.id = "ses_running0001".into();
        registry.items.insert(active.info.id.clone(), active);

        prune_terminal_agents(&mut registry);

        let terminal_count = registry
            .items
            .values()
            .filter(|runtime| terminal(runtime.info.status))
            .count();
        assert_eq!(terminal_count, MAX_TERMINAL_AGENTS);
        assert!(
            registry.items.contains_key("ses_running0001"),
            "运行中的条目必须保留"
        );
        assert!(
            !registry.items.contains_key("ses_done00000"),
            "最旧的终态条目被回收"
        );
        assert!(
            registry
                .items
                .contains_key(&format!("ses_done{:05}", MAX_TERMINAL_AGENTS + 4)),
            "最新的终态条目必须保留"
        );
        // 上限之内重复回收不得再删除条目。
        prune_terminal_agents(&mut registry);
        assert_eq!(registry.items.len(), MAX_TERMINAL_AGENTS + 1);
    }
}

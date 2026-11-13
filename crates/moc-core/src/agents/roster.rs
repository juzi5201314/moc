use super::AgentEntry;
use super::Peers;
use super::ROSTER_LIMIT;
use super::SubagentPrompt;
use super::info_from_session;
use super::invalid;
use super::live;
use super::roles;
use crate::error::CoreError;
use crate::service::CoreInner;
use moc_llm::Role;
use moc_protocol::methods::AgentInfo;
use moc_protocol::methods::AgentListResult;
use moc_protocol::methods::AgentStatus;
use moc_protocol::methods::AgentStopResult;
use std::path::Path;

pub(super) async fn current_info(
    inner: &CoreInner,
    agent_id: &str,
) -> Result<AgentInfo, CoreError> {
    inner
        .agents
        .lock()
        .await
        .items
        .get(agent_id)
        .map(|runtime| runtime.info.clone())
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")))
}

/// 从会话表和注册表构建调用方看到的代理关系。
///
/// 注册表里没有条目时按 `Idle` 处理（以前跑过，当前进程里没有运行）。
pub(super) async fn peers(inner: &CoreInner, session_id: &str) -> Result<Peers, CoreError> {
    let sessions = inner.sessions.known().await?;
    let registry = inner.agents.lock().await;
    let status_of = |id: &str| {
        registry
            .items
            .get(id)
            .map_or(AgentStatus::Idle, |runtime| runtime.info.status)
    };
    let me = sessions
        .iter()
        .find(|session| session.id == session_id)
        .cloned()
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {session_id}")))?;
    let parent_id = me.parent_session_id.clone();
    let mut children = Vec::new();
    let mut siblings = Vec::new();
    let mut parent = None;
    // children/siblings 的可见性以「未被父会话标记为孤儿」为准：回退把 spawn
    // 记录截掉的子代理被标记，不再进名册也不能被寻址；redo 接回引用时标记摘除，
    // 名册随之恢复。标记集合按首个命中的方向惰性取一次。
    let mut own_orphans: Option<std::collections::BTreeSet<String>> = None;
    let mut parent_orphans: Option<std::collections::BTreeSet<String>> = None;
    for session in sessions {
        if session.id == session_id {
            continue;
        }
        if session.parent_session_id.as_deref() == Some(session_id) {
            if own_orphans.is_none() {
                own_orphans = Some(inner.sessions.orphan_marks(session_id).await);
            }
            if own_orphans
                .as_ref()
                .expect("上一行刚填充")
                .contains(&session.id)
            {
                continue;
            }
            let status = status_of(&session.id);
            children.push(AgentEntry { session, status });
            continue;
        }
        let Some(parent_id) = parent_id.as_deref() else {
            continue;
        };
        if session.id == parent_id {
            let status = status_of(&session.id);
            parent = Some(AgentEntry { session, status });
            continue;
        }
        if session.parent_session_id.as_deref() == Some(parent_id) {
            if parent_orphans.is_none() {
                parent_orphans = Some(inner.sessions.orphan_marks(parent_id).await);
            }
            if parent_orphans
                .as_ref()
                .expect("上一行刚填充")
                .contains(&session.id)
            {
                continue;
            }
            let status = status_of(&session.id);
            siblings.push(AgentEntry { session, status });
        }
    }
    Ok(Peers {
        me,
        children,
        parent,
        siblings,
    })
}

pub(crate) async fn list(
    inner: &CoreInner,
    session_id: String,
) -> Result<AgentListResult, CoreError> {
    let peers = peers(inner, &session_id).await?;
    // 子代理的可通信面是兄弟；能再委托的子代理还要能看到自己的直接子代理。
    let related: Vec<&AgentEntry> = if peers.is_subagent() {
        peers.siblings.iter().chain(peers.children.iter()).collect()
    } else {
        peers.children.iter().collect()
    };
    let mut agents = Vec::with_capacity(related.len());
    for entry in related {
        let mut info = info_from_session(&entry.session, entry.status);
        if let Ok(loaded) = inner.sessions.load(&entry.session.id, Some(128)).await {
            info.last_message = loaded
                .entries
                .iter()
                .rev()
                .find(|entry| entry.message.role == Role::Assistant)
                .map(|entry| entry.message.text());
        }
        if let Some(runtime) = inner.agents.lock().await.items.get(&entry.session.id) {
            info = runtime.info.clone();
        }
        agents.push(info);
    }
    agents.sort_by(|left, right| {
        right
            .updated_at_ms
            .cmp(&left.updated_at_ms)
            .then_with(|| left.id.cmp(&right.id))
    });
    let config = inner.project_config(Path::new(&peers.me.cwd))?;
    let plugins = inner.project_plugins(Path::new(&peers.me.cwd)).await?;
    let record = inner.sessions.get(&session_id).await?;
    let state = record.state.lock().await.metadata();
    let scope = crate::tools::ToolScope::for_metadata(
        &state,
        &config,
        roles::descriptions(inner, &config, &plugins),
    );
    let roles = if scope.permits("spawn_agent") {
        scope
            .roles
            .into_iter()
            .map(|(name, description)| moc_protocol::methods::AgentRole { name, description })
            .collect()
    } else {
        Vec::new()
    };
    Ok(AgentListResult { agents, roles })
}

pub(crate) async fn stop(
    inner: &CoreInner,
    session_id: String,
    agent_id: String,
    _reason: Option<String>,
) -> Result<AgentStopResult, CoreError> {
    let mut registry = inner.agents.lock().await;
    let runtime = registry
        .items
        .get_mut(&agent_id)
        .ok_or_else(|| CoreError::Session(format!("SESSION_NOT_FOUND: {agent_id}")))?;
    if runtime.info.parent_id != session_id {
        return Err(invalid("目标不是调用方的直接子代理"));
    }
    if live(runtime.info.status) {
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

/// 子代理提示：固定角色契约与尾部通信名册。
pub(crate) async fn subagent_prompt(inner: &CoreInner, session_id: &str) -> Option<SubagentPrompt> {
    let peers = peers(inner, session_id).await.ok()?;
    let parent = peers.parent.as_ref()?;
    let meta = inner
        .sessions
        .get(session_id)
        .await
        .ok()?
        .state
        .lock()
        .await
        .agent
        .clone();
    let mut siblings: Vec<&AgentEntry> = peers.siblings.iter().collect();
    siblings.sort_by(|left, right| {
        live(right.status)
            .cmp(&live(left.status))
            .then_with(|| right.session.updated_at_ms.cmp(&left.session.updated_at_ms))
            .then_with(|| left.session.id.cmp(&right.session.id))
    });
    let mut block = String::from(
        "<subagent>\nYou are a subagent. When the task is done, call `yield` with your complete report; the parent receives exactly that text. Never end with a plain message instead, unless the task is impossible.\nCoordinate via `message_agent`: target \"parent\", a sibling id, or \"all\" (running siblings). Incoming messages arrive as <agent-message from=\"...\" id=\"...\">. Message the parent only for questions, blockers, or decisions; progress and completion reports go through `yield`.\n",
    );
    if let Some(meta) = &meta {
        if let Some(prompt) = &meta.prompt {
            block.push('\n');
            block.push_str(prompt);
            block.push('\n');
        }
        if let Some(schema) = &meta.output_schema {
            // schema 是会话级静态内容，附在系统块里不影响前缀缓存。
            block.push_str(
                "\nOutput contract: this run declares an `output_schema`. `yield` must include a `result` value conforming to it, alongside the readable `report`.\nSchema: ",
            );
            block.push_str(&schema.to_string());
            block.push('\n');
        }
    }
    block.push_str("</subagent>");

    let mut roster = String::from("<agent-roster>\n");
    roster.push_str(&format!(
        "Parent: {parent_title} ({parent_id})\n",
        parent_title = parent.session.title,
        parent_id = parent.session.id
    ));
    if siblings.is_empty() {
        roster.push_str("Siblings: (none)\n");
    } else {
        roster.push_str("Siblings (running first):\n");
        for entry in siblings.into_iter().take(ROSTER_LIMIT) {
            roster.push_str(&format!(
                "- {id} \"{title}\" [{status}]\n",
                id = entry.session.id,
                title = entry.session.title,
                status = status_word(entry.status)
            ));
        }
    }
    roster.push_str("</agent-roster>");
    Some(SubagentPrompt { block, roster })
}

/// 状态词，子代理系统提示和工具结果共用；模型可见的文本一律用英文。
pub(crate) fn status_word(status: AgentStatus) -> &'static str {
    match status {
        AgentStatus::Queued => "queued",
        AgentStatus::Running => "running",
        AgentStatus::Completed => "completed",
        AgentStatus::Stopped => "stopped",
        AgentStatus::Failed => "failed",
        AgentStatus::Idle => "idle",
    }
}

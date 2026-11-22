//! 子代理运行时注册表、持久化子会话、代理间消息和生命周期收尾。

pub mod preset;
pub(crate) mod roles;
pub(crate) mod sidekick;
mod workflow_scope;

pub use preset::{AgentPreset, PresetRegistry, ToolPolicy};

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;

use moc_llm::{FinishReason, Message};
use moc_protocol::UpdateSink;
use moc_protocol::methods::{
    AgentInfo, AgentMessageMeta, AgentStatus, AgentWaitEntry, SessionInfo,
};
use tokio::sync::Notify;
use tokio::sync::futures::OwnedNotified;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::service::CoreInner;

/// `yield` 交付物：报告文本 + 可选的结构化结果（按 `output_schema` 校验过）。
#[derive(Debug, Clone)]
pub(crate) struct Yielded {
    pub report: String,
    pub result: Option<serde_json::Value>,
}

pub(crate) struct AgentRuntime {
    pub info: AgentInfo,
    pub cancel: CancellationToken,
    pub notify: Arc<Notify>,
    pub message: Option<Message>,
    pub finish_reason: Option<FinishReason>,
    /// `yield` 交付的最终报告；父代理查询结果时优先用它。
    pub yielded: Option<Yielded>,
    /// `yield` 交付的结构化结果；完成后由 `wait_agent` 查询。
    pub structured: Option<serde_json::Value>,
    /// `sidekick` 工具的阻塞等待中：报告随工具结果直达 lead，
    /// 结束通知不再重复投递。
    pub awaited: bool,
    /// 最近一次工具调用的一行摘要；wait/sidekick 的进度行据此显示
    /// 「正在做什么」。每次提示前重建条目时清为 `None`。
    pub activity: Option<String>,
}

/// 待投递的代理消息。
pub(crate) struct PendingAgentMessage {
    pub meta: AgentMessageMeta,
    pub message: Message,
}

/// 会话收件箱：运行中的提示在两轮之间取走消息并写入会话。
#[derive(Default)]
pub(crate) struct AgentInbox {
    /// 提示运行期间为 true；结束时置 false，之后的新消息改为唤醒或明确失败。
    pub accepting: bool,
    pub messages: VecDeque<PendingAgentMessage>,
    pub notify: Arc<Notify>,
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

pub(crate) fn live(status: AgentStatus) -> bool {
    matches!(status, AgentStatus::Queued | AgentStatus::Running)
}

/// 收件箱积压上限；超过就明确回报失败，免得父代理一直投递、子代理却永远不消费。
const MAX_PENDING_AGENT_MESSAGES: usize = 64;

/// 子代理缺 `yield` 时的提醒次数上限。
pub(crate) const MAX_YIELD_REMINDERS: usize = 3;

/// `wait_agent` 省略 `timeout_secs` 时的无事件窗口。
pub(crate) const WAIT_DEFAULT_WINDOW_SECS: u64 = 180;

/// `wait_agent` 的进度行刷新间隔。
const WAIT_PROGRESS_INTERVAL: Duration = Duration::from_millis(500);

/// 自动投递 notice 的结果预览上限（字符）。
const RESULT_PREVIEW_CHARS: usize = 500;

/// 子代理系统提示里的兄弟清单上限。
const ROSTER_LIMIT: usize = 32;

/// 注册表最多保留多少个已结束的子代理；更早的按更新时间回收。
///
/// 终态信息仍然可以从 transcript 复原（`list_agents` 会回退到会话信息），这里只为
/// 最近的 `wait_agent` 查询保留一段窗口。
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
        return Err(invalid(format!("{label} must not be empty")));
    }
    Ok(value.to_owned())
}

/// 结果预览：按字符截断到 limit，超出时补 `…` 并报告截断。
fn preview_text(text: &str, limit: usize) -> (String, bool) {
    for (count, (index, _)) in text.char_indices().enumerate() {
        if count == limit {
            return (format!("{}…", &text[..index]), true);
        }
    }
    (text.to_owned(), false)
}

async fn parent_snapshot(
    inner: &CoreInner,
    parent_id: &str,
) -> Result<crate::session::state::SessionState, CoreError> {
    let record = inner.sessions.get(parent_id).await?;
    Ok(record.state.lock().await.clone())
}

mod spawn;
pub(crate) use spawn::spawn;
mod lifecycle;
pub(crate) use lifecycle::child_cancel;
pub(crate) use lifecycle::note_activity;
pub(crate) use lifecycle::retry_wakeup;
mod messaging;
pub(crate) use messaging::close_inbox_if_empty;
use messaging::deliver;
pub(crate) use messaging::drain_inbox;
pub(crate) use messaging::has_yielded;
pub(crate) use messaging::message;
pub(crate) use messaging::message_workflow;
pub(crate) use messaging::record_yield;
mod waiting;
pub(crate) use waiting::wait;
mod roster;
pub(crate) use roster::list;
use roster::peers;
pub(crate) use roster::status_word;
pub(crate) use roster::stop;
pub(crate) use roster::subagent_prompt;

fn info_from_session(session: &SessionInfo, status: AgentStatus) -> AgentInfo {
    AgentInfo {
        id: session.id.clone(),
        parent_id: session.parent_session_id.clone().unwrap_or_default(),
        title: session.title.clone(),
        model: session.model.clone(),
        cwd: session.cwd.clone(),
        preset: session.agent_preset.clone(),
        status,
        created_at_ms: session.created_at_ms,
        updated_at_ms: session.updated_at_ms,
        last_message: None,
    }
}

/// 固定角色与两条受控内部通道分开，普通调用不能构造动态角色。
#[derive(Debug)]
pub(crate) enum SpawnProfile {
    FixedRole(String),
    Sidekick {
        model: String,
    },
    Workflow {
        run_id: String,
        model: String,
        system: Option<String>,
        tools: Option<ToolPolicy>,
        output_schema: Option<serde_json::Value>,
    },
}

#[derive(Debug)]
pub(crate) struct SpawnSpec {
    pub task: String,
    pub title: Option<String>,
    pub profile: SpawnProfile,
    /// 仅 workflow amend 导入前驱 transcript，不含会话头。
    pub seed_entries: Vec<crate::session::entries::Entry>,
}

/// 唤醒空闲代理所需的交付信息。
///
/// `spawn` 的首轮任务两者都为空；被代理消息唤醒时则带着来源信息和已占用的提示锁。
#[derive(Default)]
struct Wakeup {
    /// 来源信息；写入会话后显示为 `IRC ◀` 卡片。
    agent: Option<AgentMessageMeta>,
    /// 派发前已抢占的提示锁。
    prompt_lock: Option<tokio::sync::OwnedMutexGuard<()>>,
    /// `sidekick` 工具的 `mode="retry"`：不带新消息重跑，`run_prompt_locked`
    /// 的 retry 分支负责剪尾并重放未执行的调用。
    retry: bool,
}

/// 会话与其注册表状态的组合。
struct AgentEntry {
    session: SessionInfo,
    status: AgentStatus,
}

/// 调用方可见的代理关系。
struct Peers {
    /// 调用方自身。
    me: SessionInfo,
    /// 直接子会话。
    children: Vec<AgentEntry>,
    /// 父会话（仅子代理有）。
    parent: Option<AgentEntry>,
    /// 兄弟会话（不含自身，仅子代理有）。
    siblings: Vec<AgentEntry>,
}

impl Peers {
    fn is_subagent(&self) -> bool {
        self.parent.is_some()
    }

    /// 广播覆盖的平级代理：顶层是子会话，子代理是兄弟。
    fn siblings_or_children(&self) -> &[AgentEntry] {
        if self.is_subagent() {
            &self.siblings
        } else {
            &self.children
        }
    }
}

/// 一次代理间消息投递的目标。
struct MessageTarget {
    session: SessionInfo,
}

const BROADCAST_TARGET: &str = "all";
const PARENT_TARGET: &str = "parent";

/// 一张等待快照行：协议条目 + 运行时长。
struct WaitRow {
    entry: AgentWaitEntry,
    elapsed_ms: u64,
    /// 最近一次工具调用摘要；仅运行中的代理有值。
    activity: Option<String>,
}

/// 在注册表锁内取等待快照：必须先建好等待者，再读状态。
struct WaitSnapshot {
    rows: Vec<WaitRow>,
    watched: Vec<std::pin::Pin<Box<OwnedNotified>>>,
    settled: bool,
}

/// 运行中 `wait_agent` 的进度推送目标。
pub(crate) struct WaitProgress {
    pub updates: UpdateSink,
    pub session_id: String,
    pub prompt_id: String,
    pub tool_call_id: String,
}

/// 子代理提示的组装结果。
pub(crate) struct SubagentPrompt {
    /// 角色定义与交付约定；任务正文保留为用户消息，不提升为系统指令。
    pub block: String,
    /// 运行期列表：父代理和兄弟代理。兄弟状态随时会变，所以作为尾部提醒注入，
    /// 不参与系统前缀缓存。
    pub roster: String,
}

#[cfg(test)]
mod tests;

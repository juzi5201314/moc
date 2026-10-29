//! 子代理运行时注册表、持久化子会话、代理间消息和生命周期收尾。

pub mod preset;

pub use preset::{AgentPreset, PresetRegistry, ToolPolicy};

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use moc_llm::{FinishReason, Message, Role};
use moc_protocol::methods::{
    AgentInfo, AgentListResult, AgentMessageMeta, AgentMessageOutcome, AgentMessageReceipt,
    AgentMessageResult, AgentSpawnResult, AgentStatus, AgentStopResult, AgentWaitEntry,
    AgentWaitResult, InputPart, PromptParams, SessionInfo, SessionUpdate, ToolProgressRow,
};
use moc_protocol::{RequestContext, RpcError, UpdateSink};
use tokio::sync::Notify;
use tokio::sync::futures::OwnedNotified;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::entries::SessionOrigin;
use crate::session::store::now_ms;

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

fn live(status: AgentStatus) -> bool {
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
        return Err(invalid(format!("{label} 不能为空")));
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
                "INVALID_PARAMS: 子代理 model 只能是 inherit、default、explore、plan，收到 {other}"
            )));
        }
    };
    Ok(config.resolve_model(selector)?.selector)
}

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

/// 登记即将启动的子代理，并先打开收件箱。
///
/// 顺序不能反：`accepting = true` 必须在注册表条目出现之前生效。投递消息时先看收件箱
/// 再看状态，否则在「已登记、提示还没拿到锁」的窗口里，投递会走唤醒路径，和即将启动的
/// 提示抢同一个会话。
async fn prepare_agent(inner: &CoreInner, info: &AgentInfo, cancel: CancellationToken) {
    inner
        .runtime(&info.id)
        .await
        .agent_inbox
        .lock()
        .await
        .accepting = true;
    inner.agents.lock().await.items.insert(
        info.id.clone(),
        AgentRuntime {
            info: info.clone(),
            cancel,
            notify: Arc::new(Notify::new()),
            message: None,
            finish_reason: None,
            yielded: None,
            structured: None,
        },
    );
}

/// 子代理的取消令牌挂在「父代理的运行令牌」下：父是注册过的子代理时挂它的
/// `runtime.cancel`，`stop_agent` 停父代理后代级联终止；父是顶层会话（不在
/// 注册表）时挂进程 shutdown。
///
/// 不能挂父的 prompt guard：`guard.cancel` 是请求级 token，父的那次 prompt 一
/// 返回即取消，会把还活着的子代理误杀（经 `spawn_agent` 工具 spawn 必踩）。
async fn child_cancel(inner: &CoreInner, parent_id: &str) -> CancellationToken {
    let registry = inner.agents.lock().await;
    match registry.items.get(parent_id) {
        Some(runtime) => runtime.cancel.child_token(),
        None => inner.shutdown.child_token(),
    }
}

/// 一次 spawn 的完整描述；批量 spawn 时逐项构造。
#[derive(Debug)]
pub(crate) struct SpawnSpec {
    pub task: String,
    pub title: Option<String>,
    pub model: Option<String>,
    /// 选用的 preset 名；`None` 为普通子代理。
    pub preset: Option<String>,
    /// 共享上下文：排在任务原文之前注入首条消息（批量 spawn 的公共前缀）。
    pub context: Option<String>,
    /// `yield` 交付物必须满足的 JSON Schema；省略为自由文本交付。
    pub output_schema: Option<serde_json::Value>,
}

/// 解析 preset 名 → 快照契约：先查内置注册表，再查该工作区插件声明。
///
/// 插件 preset 与内置同名时内置优先；未知名报错并列出可选项。
async fn resolve_agent_meta(
    inner: &CoreInner,
    cwd: &str,
    name: Option<&str>,
    output_schema: Option<serde_json::Value>,
) -> Result<Option<crate::session::entries::AgentSessionMeta>, CoreError> {
    let mut meta = crate::session::entries::AgentSessionMeta::default();
    if let Some(name) = name {
        let host = crate::service::plugin_events::host_for(inner, Path::new(cwd)).await;
        let declared = plugin_presets(host.as_ref())?;
        let preset = inner
            .agent_presets
            .get(name)
            .cloned()
            .or_else(|| declared.get(name).cloned());
        let Some(preset) = preset else {
            let mut names: Vec<String> = inner
                .agent_presets
                .names()
                .map(str::to_owned)
                .chain(declared.keys().cloned())
                .collect();
            names.sort();
            names.dedup();
            return Err(invalid(format!(
                "未知 agent preset `{name}`；可选：{}",
                names.join(", ")
            )));
        };
        meta.preset = Some(preset.name);
        meta.prompt = Some(preset.system_prompt);
        meta.tool_policy = Some(preset.tools);
    }
    if let Some(schema) = output_schema {
        // schema 在 spawn 时就编译校验，把坏契约挡在子代理创建之前。
        validate_output_schema(&schema)?;
        meta.output_schema = Some(schema);
    }
    Ok((!meta.is_empty()).then_some(meta))
}

/// 插件宿主声明的 preset → `AgentPreset`：allow/deny 互斥，省略则不限。
fn plugin_presets(
    host: Option<&Arc<moc_plugin::PluginHost>>,
) -> Result<BTreeMap<String, AgentPreset>, CoreError> {
    let mut presets = BTreeMap::new();
    let Some(host) = host else {
        return Ok(presets);
    };
    for decl in host.agent_preset_declarations().into_values() {
        let tools = match (decl.allow, decl.deny) {
            (Some(allow), None) => ToolPolicy::Allow(allow.into_iter().collect()),
            (None, deny) => ToolPolicy::Deny(deny.unwrap_or_default().into_iter().collect()),
            (Some(_), Some(_)) => {
                return Err(invalid(format!(
                    "agent preset `{}` 的 allow 与 deny 互斥",
                    decl.name
                )));
            }
        };
        let preset = preset::sanitize_preset(AgentPreset {
            name: decl.name,
            system_prompt: decl.system_prompt,
            tools,
        });
        presets.insert(preset.name.clone(), preset);
    }
    Ok(presets)
}

/// spawn 参数里的 `output_schema` 必须是合法的 JSON Schema 对象。
fn validate_output_schema(schema: &serde_json::Value) -> Result<(), CoreError> {
    jsonschema::validator_for(schema).map_err(|error| {
        invalid(format!("output_schema 不是合法的 JSON Schema：{error}"))
    })?;
    Ok(())
}

pub(crate) async fn spawn(
    inner: Arc<CoreInner>,
    request: RequestContext,
    session_id: String,
    spec: SpawnSpec,
) -> Result<AgentSpawnResult, CoreError> {
    let task = validate_text(&spec.task, "task")?;
    let parent = parent_snapshot(&inner, &session_id).await?;
    let config = inner.project_config(Path::new(&parent.session.cwd))?;
    let depth = parent.session.delegation_depth + 1;
    if depth > config.agents.max_depth {
        return Err(invalid(format!(
            "已达到最大委托深度 {}：该会话不能再 spawn 子代理",
            config.agents.max_depth
        )));
    }
    let model = resolve_model(&config, &parent.model, spec.model.as_deref())?;
    let agent_meta =
        resolve_agent_meta(&inner, &parent.session.cwd, spec.preset.as_deref(), spec.output_schema)
            .await?;
    let task = match spec.context {
        Some(context) if !context.trim().is_empty() => {
            format!("{}\n\n{}", context.trim(), task)
        }
        _ => task,
    };
    let session = inner
        .sessions
        .create(
            &parent.session.cwd,
            &model,
            parent.mode,
            parent.permission,
            spec.title.as_deref(),
            SessionOrigin::spawned(session_id.clone(), depth, agent_meta),
            config.session.compress,
        )
        .await?;
    let record = inner.sessions.get(&session.id).await?;
    inner
        .sessions
        .ensure_write_lease(&session.id, &record, &request.client().frontend)
        .await?;
    let info = info_from_session(&session, AgentStatus::Queued);
    let cancel = child_cancel(&inner, &session_id).await;
    prepare_agent(&inner, &info, cancel.clone()).await;
    let plugin_host =
        crate::service::plugin_events::host_for(&inner, Path::new(&parent.session.cwd)).await;
    if let Some(host) = &plugin_host {
        // 子代理会话同样是「会话」：`sessionStart` 注入的初始消息进入它的 transcript。
        let child_context = crate::service::plugin_events::context_from_info(&session);
        for message in crate::service::plugin_events::session_start(
            host,
            &child_context,
            &session,
            "subagent",
            Some(&session_id),
        )
        .await
        {
            let role = if message.role == "system" {
                Role::System
            } else {
                Role::User
            };
            inner
                .sessions
                .append_message(
                    &session.id,
                    Message {
                        role,
                        content: vec![moc_llm::ContentPart::Text(message.text)],
                    },
                    None,
                    None,
                )
                .await?;
        }
    }
    let mut task = task;
    if let Some(host) = &plugin_host {
        let parent_context = crate::service::plugin_events::context(
            &session_id,
            &parent.session.cwd,
            parent.mode,
            crate::service::mode::ModeContract::effective(parent.plan_active, None),
            parent.permission,
            &parent.model,
            parent.session.is_subagent(),
        );
        if let Some(rewritten) =
            crate::service::plugin_events::agent_started(host, &parent_context, &info, &task).await
        {
            task = rewritten;
        }
    }
    let _ = request
        .updates()
        .emit(
            session_id.clone(),
            SessionUpdate::AgentStarted {
                agent: info.clone(),
            },
        )
        .await;
    activate(
        Arc::clone(&inner),
        request,
        session_id,
        info.id.clone(),
        task,
        cancel,
        Wakeup::default(),
    )
    .await?;
    let info = current_info(&inner, &info.id).await?;
    Ok(AgentSpawnResult { agent: info })
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
}

/// 启动一次子代理提示。
///
/// `wakeup` 只对「代理消息唤醒」给出。
async fn activate(
    inner: Arc<CoreInner>,
    request: RequestContext,
    parent_id: String,
    agent_id: String,
    task: String,
    cancel: CancellationToken,
    wakeup: Wakeup,
) -> Result<(), CoreError> {
    let Wakeup { agent, prompt_lock } = wakeup;
    let record = inner.sessions.get(&agent_id).await?;
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
                input: vec![InputPart::Text { text: task }],
                contract: None,
                agent_message: agent,
            },
            record,
            prompt_lock,
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
    let (message, last_message, title, notify) = {
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
        runtime.info.status = status;
        runtime.info.updated_at_ms = now_ms();
        runtime.info.last_message = last_message.clone();
        runtime.message = message.clone();
        runtime.finish_reason = finish_reason;
        let title = runtime.info.title.clone();
        let notify = Arc::clone(&runtime.notify);
        prune_terminal_agents(&mut registry);
        (message, last_message, title, notify)
    };
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
            parent.mode,
            crate::service::mode::ModeContract::effective(parent.plan_active, None),
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
    deliver_result_notice(
        &inner,
        &parent_id,
        &agent_id,
        &title,
        status,
        message.as_ref(),
    )
    .await;
}

/// 子代理结束时把结果作为 notice 投进父会话，父模型下一轮就能看到。
async fn deliver_result_notice(
    inner: &CoreInner,
    parent_id: &str,
    agent_id: &str,
    title: &str,
    status: AgentStatus,
    message: Option<&Message>,
) {
    let label = match status {
        AgentStatus::Completed => "completed",
        AgentStatus::Stopped => "stopped",
        AgentStatus::Failed => "failed",
        _ => return,
    };
    let Ok(parent) = inner.sessions.get(parent_id).await else {
        return;
    };
    let mut notice = format!("Subagent \"{title}\" ({agent_id}) {label}");
    if let Some(text) = message
        .map(Message::text)
        .filter(|text| !text.trim().is_empty())
    {
        let (preview, truncated) = preview_text(&text, RESULT_PREVIEW_CHARS);
        notice.push_str(&format!(". Result preview:\n{preview}"));
        if truncated {
            notice.push_str("\n(truncated; wait_agent returns the full result)");
        }
    }
    parent.state.lock().await.notices.push(notice);
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

/// 从会话表和注册表构建调用方看到的代理关系。
///
/// 注册表里没有条目时按 `Idle` 处理（以前跑过，当前进程里没有运行）。
async fn peers(inner: &CoreInner, session_id: &str) -> Result<Peers, CoreError> {
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
    for session in sessions {
        if session.id == session_id {
            continue;
        }
        if session.parent_session_id.as_deref() == Some(session_id) {
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

/// 一次代理间消息投递的目标。
struct MessageTarget {
    session: SessionInfo,
}

const BROADCAST_TARGET: &str = "all";
const PARENT_TARGET: &str = "parent";

fn resolve_targets(peers: &Peers, target: &str) -> Result<Vec<MessageTarget>, CoreError> {
    if target == BROADCAST_TARGET {
        let targets: Vec<MessageTarget> = peers
            .siblings_or_children()
            .iter()
            .filter(|entry| live(entry.status))
            .map(|entry| MessageTarget {
                session: entry.session.clone(),
            })
            .collect();
        if targets.is_empty() {
            return Err(invalid("没有正在运行的可投递代理"));
        }
        return Ok(targets);
    }
    if target == PARENT_TARGET {
        let Some(parent) = &peers.parent else {
            return Err(invalid("顶层会话没有父代理"));
        };
        return Ok(vec![MessageTarget {
            session: parent.session.clone(),
        }]);
    }
    if target == peers.me.id {
        return Err(invalid("不能向会话自身投递消息"));
    }
    let matched = peers
        .parent
        .iter()
        .chain(peers.siblings.iter())
        .chain(peers.children.iter())
        .find(|entry| entry.session.id == target);
    match matched {
        Some(entry) => Ok(vec![MessageTarget {
            session: entry.session.clone(),
        }]),
        None => Err(invalid(format!("目标 {target} 不是可投递的代理"))),
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
    let text = validate_text(&text, "message")?;
    let target = validate_text(&target, "target")?;
    let peers = peers(&inner, &session_id).await?;
    let from = AgentMessageMeta {
        from_id: peers.me.id.clone(),
        from_title: peers.me.title.clone(),
        delivery: AgentMessageOutcome::Injected,
    };
    let targets = resolve_targets(&peers, &target)?;
    let mut receipts = Vec::with_capacity(targets.len());
    for target in targets {
        receipts.push(deliver(&inner, &request, &from, &target, &text).await);
    }
    Ok(AgentMessageResult { receipts })
}

fn receipt(
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
async fn deliver(
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
                Some("目标会话正在收尾，请稍后重试".into()),
            ),
        };
    };
    let woken = AgentMessageMeta {
        delivery: AgentMessageOutcome::Woken,
        ..from.clone()
    };
    match &target.session.parent_session_id {
        Some(parent_id) => {
            // 收件箱不收消息、提示锁也空闲，说明旧条目（如果有）已经作废：
            // 重建条目，清掉上一轮的交付物和等待者。
            let info = info_from_session(&target.session, AgentStatus::Queued);
            let cancel = child_cancel(inner, parent_id).await;
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
                    contract: None,
                    agent_message: Some(woken),
                },
                record,
                prompt_lock,
            )
            .await;
        }
    }
    receipt(target, AgentMessageOutcome::Woken, None)
}

/// 放入收件箱；队列满了就明确回报失败。
fn push_inbox(
    inbox: &mut AgentInbox,
    from: &AgentMessageMeta,
    text: &str,
    target: &MessageTarget,
) -> AgentMessageReceipt {
    if inbox.messages.len() >= MAX_PENDING_AGENT_MESSAGES {
        return receipt(
            target,
            AgentMessageOutcome::Failed,
            Some("收件箱已满（64 条待投递），请稍后重试".into()),
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
            plugin_state.mode,
            crate::service::mode::ModeContract::effective(plugin_state.plan_active, None),
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
                .map_err(|error| invalid(format!("output_schema 非法：{error}")))?;
            if let Err(error) = validator.validate(&result) {
                return Err(invalid(format!("yield 的 result 不满足 output_schema：{error}")));
            }
            Some(result)
        }
        (Some(_), None) => {
            return Err(invalid("该子代理声明了 output_schema，yield 必须提供 result"));
        }
        (None, Some(_)) => {
            return Err(invalid("该子代理未声明 output_schema，不能交付结构化结果"));
        }
        (None, None) => None,
    };
    let report = validate_text(&report, "report")?;
    let mut registry = inner.agents.lock().await;
    let Some(runtime) = registry.items.get_mut(session_id) else {
        return Err(invalid("yield 仅能在子代理运行中调用"));
    };
    if !live(runtime.info.status) {
        return Err(invalid("yield 仅能在子代理运行中调用"));
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
    Ok(AgentListResult { agents })
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

/// 一张等待快照行：协议条目 + 运行时长。
struct WaitRow {
    entry: AgentWaitEntry,
    elapsed_ms: u64,
}

/// 在注册表锁内取等待快照：必须先建好等待者，再读状态。
struct WaitSnapshot {
    rows: Vec<WaitRow>,
    watched: Vec<std::pin::Pin<Box<OwnedNotified>>>,
    settled: bool,
}

/// 在注册表锁内建立等待者并读取快照。
///
/// 完成路径先写终态、再 `notify_waiters`；这两步如果和这里的读取交错，等待者会错过
/// 唤醒并一直挂起（`notify_waiters` 不保存许可）。所以锁内先建等待者再读状态，无论怎么
/// 交错，结果都会是「立即返回终态」或「等待者已就绪」。
fn wait_snapshot(registry: &AgentRegistry, targets: &[SessionInfo]) -> WaitSnapshot {
    let mut rows = Vec::with_capacity(targets.len());
    let mut watched = Vec::with_capacity(targets.len());
    let mut settled = false;
    let now = now_ms();
    for session in targets {
        let Some(runtime) = registry.items.get(&session.id) else {
            // 注册表里没有条目：进程内没有在跑，按 `Idle` 回报。
            settled = true;
            rows.push(WaitRow {
                entry: AgentWaitEntry {
                    id: session.id.clone(),
                    title: session.title.clone(),
                    status: AgentStatus::Idle,
                    message: None,
                    finish_reason: None,
                    structured_result: None,
                },
                elapsed_ms: 0,
            });
            continue;
        };
        let info = &runtime.info;
        if terminal(info.status) {
            settled = true;
        } else {
            watched.push(Box::pin(Arc::clone(&runtime.notify).notified_owned()));
        }
        rows.push(WaitRow {
            entry: AgentWaitEntry {
                id: info.id.clone(),
                title: info.title.clone(),
                status: info.status,
                message: runtime.message.clone(),
                finish_reason: runtime.finish_reason.clone(),
                structured_result: runtime.structured.clone(),
            },
            elapsed_ms: if live(info.status) {
                now.saturating_sub(info.updated_at_ms)
            } else {
                0
            },
        });
    }
    WaitSnapshot {
        rows,
        watched,
        settled,
    }
}

/// 逐个轮询一组等待者，任一就绪即返回。
///
/// 等价于「select 全部 `Notified`」，避免为每个目标各起一个任务。
async fn await_any(watchers: &mut [std::pin::Pin<Box<OwnedNotified>>]) {
    std::future::poll_fn(|context| {
        for watcher in watchers.iter_mut() {
            if watcher.as_mut().poll(context).is_ready() {
                return std::task::Poll::Ready(());
            }
        }
        std::task::Poll::Pending
    })
    .await
}

/// 运行中 `wait_agent` 的进度推送目标。
pub(crate) struct WaitProgress {
    pub updates: UpdateSink,
    pub session_id: String,
    pub prompt_id: String,
    pub tool_call_id: String,
}

async fn push_progress(progress: Option<&WaitProgress>, rows: &[WaitRow]) {
    let Some(progress) = progress else {
        return;
    };
    let rows = rows
        .iter()
        .map(|row| ToolProgressRow {
            agent_id: row.entry.id.clone(),
            title: row.entry.title.clone(),
            status: row.entry.status,
            elapsed_ms: row.elapsed_ms,
        })
        .collect();
    if let Err(error) = progress
        .updates
        .emit(
            progress.session_id.clone(),
            SessionUpdate::ToolProgress {
                prompt_id: progress.prompt_id.clone(),
                tool_call_id: progress.tool_call_id.clone(),
                rows,
            },
        )
        .await
    {
        tracing::warn!(%error, "子代理等待进度推送失败");
    }
}

fn wait_result(
    rows: Vec<WaitRow>,
    settled: bool,
    messages_pending: bool,
    timed_out: bool,
) -> AgentWaitResult {
    AgentWaitResult {
        agents: rows.into_iter().map(|row| row.entry).collect(),
        settled,
        messages_pending,
        timed_out,
    }
}

/// 等待目标集：显式 id 校验为直接子代理，省略则取全部运行中的直接子代理。
async fn resolve_wait_targets(
    inner: &CoreInner,
    session_id: &str,
    agent_ids: Option<Vec<String>>,
) -> Result<Vec<SessionInfo>, CoreError> {
    let peers = peers(inner, session_id).await?;
    match agent_ids.filter(|ids| !ids.is_empty()) {
        Some(ids) => ids
            .iter()
            .map(|id| {
                peers
                    .children
                    .iter()
                    .find(|entry| entry.session.id == *id)
                    .map(|entry| entry.session.clone())
                    .ok_or_else(|| invalid(format!("目标 {id} 不是调用方的直接子代理")))
            })
            .collect(),
        None => {
            let live_targets: Vec<SessionInfo> = peers
                .children
                .iter()
                .filter(|entry| live(entry.status))
                .map(|entry| entry.session.clone())
                .collect();
            if !live_targets.is_empty() {
                return Ok(live_targets);
            }
            // 子代理全都结束了：直接回报它们的终态。否则「刚 spawn 就 wait」这种很快
            // 结束的子代理会撞上空目标错误，而这恰恰是最常见的收集结果时机。
            if peers.children.is_empty() {
                return Err(invalid("没有可等待的子代理"));
            }
            Ok(peers
                .children
                .iter()
                .map(|entry| entry.session.clone())
                .collect())
        }
    }
}

/// `agent/wait`：等待任一子代理进入终态，或被消息唤醒，或窗口到期。
pub(crate) async fn wait(
    inner: &CoreInner,
    session_id: &str,
    agent_ids: Option<Vec<String>>,
    timeout_secs: Option<u64>,
    progress: Option<&WaitProgress>,
    cancel: &CancellationToken,
) -> Result<AgentWaitResult, CoreError> {
    let targets = resolve_wait_targets(inner, session_id, agent_ids).await?;
    let session_runtime = inner.runtime(session_id).await;
    let window = Duration::from_secs(timeout_secs.unwrap_or(WAIT_DEFAULT_WINDOW_SECS));
    let deadline = tokio::time::Instant::now() + window;
    // 首个 tick 立即就绪：先消费掉，进度节奏由下面的循环控制。
    let mut ticker = tokio::time::interval(WAIT_PROGRESS_INTERVAL);
    ticker.tick().await;
    loop {
        let WaitSnapshot {
            rows,
            mut watched,
            settled,
        } = {
            let registry = inner.agents.lock().await;
            wait_snapshot(&registry, &targets)
        };
        let (inbox_notified, inbox_pending) = {
            let inbox = session_runtime.agent_inbox.lock().await;
            (
                Arc::clone(&inbox.notify).notified_owned(),
                !inbox.messages.is_empty(),
            )
        };
        let (steering_notified, steering_pending) = {
            let steering = session_runtime.steering.lock().await;
            (
                Arc::clone(&steering.notify).notified_owned(),
                !steering.messages.is_empty(),
            )
        };
        let mut inbox_notified = std::pin::pin!(inbox_notified);
        let mut steering_notified = std::pin::pin!(steering_notified);
        let messages_pending = inbox_pending || steering_pending;
        // 每轮都推一次：既是最新进度行，也是返回前的最终快照。
        push_progress(progress, &rows).await;
        if settled || messages_pending || cancel.is_cancelled() {
            return Ok(wait_result(rows, settled, messages_pending, false));
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Ok(wait_result(rows, false, false, false));
            }
            _ = tokio::time::sleep_until(deadline) => {
                return Ok(wait_result(rows, false, false, true));
            }
            _ = ticker.tick() => {}
            _ = await_any(&mut watched) => {}
            _ = inbox_notified.as_mut() => {}
            _ = steering_notified.as_mut() => {}
        }
    }
}

/// 子代理提示的组装结果。
pub(crate) struct SubagentPrompt {
    /// 系统提示块：交付约定和任务本身。任务放在这里，优先级高于通用仓库规则。
    pub block: String,
    /// 运行期列表：父代理和兄弟代理。兄弟状态随时会变，所以作为尾部提醒注入，
    /// 不参与系统前缀缓存。
    pub roster: String,
}

/// 子代理提示：系统块（交付契约 + preset 片段 + 任务）与尾部提醒（可通信代理清单）。
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
        if meta.output_schema.is_some() {
            block.push_str(
                "\nOutput contract: this run declares an `output_schema`. `yield` must include a `result` value conforming to it, alongside the readable `report`.\n",
            );
        }
    }
    if let Some(task) = assignment(inner, session_id).await {
        block.push_str(
            "\nTask (authoritative; outranks general repository conventions on conflict):\n",
        );
        block.push_str(&task);
        block.push('\n');
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

/// 子代理会话里的第一条用户消息：spawn 投递的任务原文。
///
/// 跳过 moc 自己注入的上下文消息（计划、todo、压缩摘要）：它们已经随 transcript 进入
/// 上下文，再抄进系统块只会重复一遍。
async fn assignment(inner: &CoreInner, session_id: &str) -> Option<String> {
    let record = inner.sessions.get(session_id).await.ok()?;
    let state = record.state.lock().await;
    state
        .transcript
        .iter()
        .filter_map(|entry| entry.context_message())
        .find(|message| {
            message.role == Role::User && !crate::compact::replay::is_injected_context(message)
        })
        .map(|message| message.text())
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
                preset: None,
                status: AgentStatus::Running,
                created_at_ms: 1,
                updated_at_ms: 1,
                last_message: None,
            },
            cancel: CancellationToken::new(),
            notify,
            message: None,
            finish_reason: None,
            yielded: None,
            structured: None,
        }
    }

    fn target(id: &str) -> SessionInfo {
        SessionInfo {
            id: id.into(),
            parent_session_id: Some("ses_parent0001".into()),
            forked_from: None,
            agent_preset: None,
            delegation_depth: 1,
            title: "子代理".into(),
            model: "p/m".into(),
            cwd: "/tmp/project".into(),
            mode: moc_protocol::methods::SessionMode::Normal,
            plan_active: false,
            mode_locked: false,
            permission: moc_protocol::methods::SessionPermission::Workspace,
            created_at_ms: 1,
            updated_at_ms: 1,
            lock: None,
        }
    }

    #[tokio::test]
    async fn completion_between_status_check_and_wait_is_not_lost() {
        let notify = Arc::new(Notify::new());
        let mut registry = AgentRegistry::default();
        registry
            .items
            .insert("ses_agent00001".into(), running_agent(Arc::clone(&notify)));
        // 等待者和终态读取在同一临界区内完成；此时代理还在运行。
        let snapshot = wait_snapshot(&registry, &[target("ses_agent00001")]);
        assert!(!snapshot.settled);
        assert_eq!(snapshot.watched.len(), 1);
        assert_eq!(snapshot.rows[0].entry.status, AgentStatus::Running);
        let mut watched = snapshot.watched;
        // 完成路径按 `finish` 的顺序执行：先写终态，再唤醒等待者。
        {
            let runtime = registry
                .items
                .get_mut("ses_agent00001")
                .expect("代理已登记");
            runtime.info.status = AgentStatus::Completed;
            runtime.info.last_message = Some("子代理完成".into());
            runtime.message = Some(Message::assistant_text("子代理完成"));
        }
        notify.notify_waiters();
        // 唤醒发生在等待者建立之后：必须立即就绪，不能依赖后续通知。
        // 上限取 5s，只用来兜住「永远不会被唤醒」这种缺陷，避免并行测试的调度延迟造成误报。
        tokio::time::timeout(Duration::from_secs(5), await_any(&mut watched))
            .await
            .expect("完成通知不得丢失");
    }

    #[tokio::test]
    async fn terminal_agent_returns_its_completion_snapshot() {
        let notify = Arc::new(Notify::new());
        let mut registry = AgentRegistry::default();
        registry
            .items
            .insert("ses_agent00001".into(), running_agent(notify));
        {
            let runtime = registry
                .items
                .get_mut("ses_agent00001")
                .expect("代理已登记");
            runtime.info.status = AgentStatus::Failed;
            runtime.message = Some(Message::assistant_text("上游失败"));
        }
        let snapshot = wait_snapshot(&registry, &[target("ses_agent00001")]);
        assert!(snapshot.settled);
        assert!(snapshot.watched.is_empty());
        let entry = &snapshot.rows[0].entry;
        assert_eq!(entry.status, AgentStatus::Failed);
        assert_eq!(entry.message.as_ref().expect("失败消息").text(), "上游失败");
        // 注册表里没有条目：按 `Idle` 回报。
        let absent = wait_snapshot(&registry, &[target("ses_missing0001")]);
        assert!(absent.settled);
        assert_eq!(absent.rows[0].entry.status, AgentStatus::Idle);
    }

    #[test]
    fn preview_text_truncates_on_char_boundary() {
        assert_eq!(preview_text("abc", 3), ("abc".to_owned(), false));
        assert_eq!(preview_text("abcd", 3), ("abc…".to_owned(), true));
        assert_eq!(preview_text("你好世界", 2), ("你好…".to_owned(), true));
    }

    #[test]
    fn broadcast_and_explicit_targets_resolve_against_peers() {
        let peers = Peers {
            me: SessionInfo {
                parent_session_id: None,
                ..target("ses_parent0001")
            },
            children: vec![
                AgentEntry {
                    session: target("ses_child00001"),
                    status: AgentStatus::Running,
                },
                AgentEntry {
                    session: target("ses_child00002"),
                    status: AgentStatus::Completed,
                },
            ],
            parent: None,
            siblings: Vec::new(),
        };
        let broadcast = resolve_targets(&peers, BROADCAST_TARGET).expect("广播解析");
        assert_eq!(broadcast.len(), 1, "广播只覆盖运行中的直接子代理");
        assert_eq!(broadcast[0].session.id, "ses_child00001");
        assert!(resolve_targets(&peers, PARENT_TARGET).is_err());
        assert!(resolve_targets(&peers, "ses_parent0001").is_err());
        assert!(resolve_targets(&peers, "ses_unknown0001").is_err());
        let explicit = resolve_targets(&peers, "ses_child00002").expect("显式目标");
        assert_eq!(explicit[0].session.id, "ses_child00002");
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
        // 没超过上限时，再回收也不应删除条目。
        prune_terminal_agents(&mut registry);
        assert_eq!(registry.items.len(), MAX_TERMINAL_AGENTS + 1);
    }
}

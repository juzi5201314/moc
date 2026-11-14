//! core 的公开入口、生命周期管理和 JSON-RPC 服务实现。

pub(crate) mod plugin_events;
mod plugin_persistence;

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;

use moc_llm::Client;
use moc_protocol::methods::ClientCapabilities;
use moc_protocol::{RpcError, UpdateSink};
use parking_lot::RwLock;
use tokio::sync::Mutex;

use crate::CoreConfig;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::session::SessionTable;
use crate::tools::{JobManager, ToolRegistry};

/// 会话期缓存的指令文件与尚未吐出的截断提醒。
#[derive(Clone)]
pub(crate) struct CachedContextFiles {
    pub files: Arc<Vec<crate::system_prompt::context_files::ContextFile>>,
    pub warning: Option<String>,
}

/// 已加载会话的运行时状态，只在内存里，不写入 JSONL。
pub(crate) struct SessionRuntime {
    pub anchors: Arc<crate::hashline::AnchorStore>,
    pub jobs: Arc<JobManager>,
    pub context_files: Mutex<Option<CachedContextFiles>>,
    pub reminders: tokio::sync::Mutex<crate::system_prompt::reminder::ReminderState>,
    pub loaded_skills: Arc<RwLock<crate::skills::LoadedSkills>>,
    pub steering: Mutex<management::Steering>,
    /// 代理间消息的收件箱。运行中的提示在两轮之间取走消息并写入会话。
    pub agent_inbox: Mutex<crate::agents::AgentInbox>,
    pub guard: parking_lot::Mutex<Option<Arc<prompt::guard::PromptGuard>>>,
    /// 本进程是否允许该会话的目标自动续跑；不落库，会话加载时恒为 `false`，
    /// 由 `session/goal` 的 create/resume 或 `create_goal`/`update_goal` 工具置位。
    /// armed 位与代际打包在同一个字，见 [`crate::goal::GoalArmed`]。
    pub goal_armed: crate::goal::GoalArmed,
    /// 正在跑的 goal 续跑轮的来源标记；`/goal pause` 用它识别该不该中止当前提示。
    pub goal_active_round: parking_lot::Mutex<Option<moc_protocol::methods::GoalRoundMeta>>,
    /// 目标操作串行化：RPC 与工具侧的读-改-写互斥。
    pub goal_op: tokio::sync::Mutex<()>,
    /// Grok Build 两遍压缩中提前摘要（prefire）的状态；用同步原语实现，不跨任务持有异步锁。
    pub grok_prefire: crate::compact::grok_build::prefire::PrefireState,
    /// 本会话累积的沙盒授权；模式切换或 `clear_grants` 时清空。
    pub sandbox_grants: Arc<RwLock<crate::sandbox::SandboxGrants>>,
    /// 会话级临时目录与 HOME overlay；runner 不可用时为空。
    pub sandbox_scratch: Option<Arc<crate::sandbox::SandboxScratch>>,
    /// workflow run 管理器（激活后惰性建立）。
    pub workflows: tokio::sync::OnceCell<Arc<crate::workflow::runtime::WorkflowManager>>,
}

mod plugin_support;
pub(crate) use plugin_support::emit_session_config_changed;
pub(crate) use plugin_support::flush_plugin_notices;
pub(crate) use plugin_support::plugin_context;
pub(crate) use plugin_support::plugin_tools_of;
mod bootstrap;
mod errors;
mod runtime;
pub(crate) use errors::invalid_params;
pub(crate) use errors::protocol_to_rpc;
pub(crate) use errors::to_rpc;

/// core 进程内共享状态。
pub(crate) struct CoreInner {
    pub config: RwLock<Arc<ResolvedConfig>>,
    pub boot: CoreConfig,
    pub config_write: Mutex<()>,
    pub config_epoch: tokio::sync::RwLock<()>,
    pub provider_gates: RwLock<HashMap<String, Arc<crate::concurrency::Gate>>>,
    pub client: Client,
    pub context_views: parking_lot::Mutex<context::Views>,
    pub history_transfers: parking_lot::Mutex<history::Transfers>,
    pub sessions: Arc<SessionTable>,
    pub tools: ToolRegistry,
    pub capabilities: RwLock<HashMap<u64, ClientCapabilities>>,
    pub runtimes: Mutex<HashMap<String, Arc<SessionRuntime>>>,
    pub project_configs: RwLock<HashMap<PathBuf, Arc<ResolvedConfig>>>,
    /// 每个项目（cwd）一个 MCP 管理器，键与 `project_configs` 相同。
    pub mcp_managers: Mutex<HashMap<PathBuf, Arc<moc_mcp::McpManager>>>,
    pub skill_indexes: RwLock<HashMap<PathBuf, Arc<crate::skills::SkillIndex>>>,
    /// 每个项目一个插件宿主，键与 `project_configs` 相同。
    pub plugin_hosts: Mutex<HashMap<PathBuf, Arc<moc_plugin::PluginHost>>>,
    pub ptc_host: Mutex<Option<Arc<crate::tools::ptc::PtcHost>>>,
    /// 本进程内子代理的运行状态。历史以持久化的 transcript 为准。
    pub agents: Mutex<crate::agents::AgentRegistry>,
    /// 内置 agent preset 注册表；插件 preset 按工作区在 spawn 时并入。
    pub agent_presets: crate::agents::PresetRegistry,
    pub agent_tasks: Mutex<tokio::task::JoinSet<()>>,
    pub prompts: Mutex<tokio::task::JoinSet<()>>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub sandbox_support: crate::sandbox::SandboxSupport,
    /// 读过 `web/state` 的连接的 `web/state_updated` 扇出表；键为连接号。
    /// 连接断开没有回调，死连接在下次扇出发送失败时剔除。
    pub web_state_sinks: parking_lot::Mutex<HashMap<u64, UpdateSink>>,
}

/// 代理运行时，通过协议层的 [`moc_protocol::AgentService`] 对外提供服务。
#[derive(Clone)]
pub struct Core {
    pub(crate) inner: Arc<CoreInner>,
}

/// 各前端共用的具体服务端，避免跨 crate 重复实例化 `Server<Core>` 的异步调用链。
#[derive(Clone)]
pub struct CoreServer {
    server: Arc<moc_protocol::Server<Core>>,
}

/// 把一次完成的模型请求计入会话用量统计；统计 entry 不会出现在模型看到的历史里。
///
/// `ttft_ms` 是首 token 等待；非流式请求与失败请求没有首 token，传 `None`，
/// 这样平均值不会被吞掉一次请求的整段等待。
pub(crate) async fn record_model_usage(
    inner: &CoreInner,
    session_id: &str,
    model: &str,
    usage: moc_llm::Usage,
    active_ms: u64,
    ttft_ms: Option<u64>,
) -> Result<(), CoreError> {
    inner
        .sessions
        .append(
            session_id,
            crate::session::entries::EntryPayload::Usage {
                model: model.to_owned(),
                usage,
                active_ms,
                ttft_ms,
            },
        )
        .await
        .map(|_| ())
}

/// `session_usage` 的聚合范围：从根会话按 `parent_session_id` 广度优先遍历全部
/// 后代；被各层父会话标记为孤儿的子树整体排除（与回退后自身用量收缩同口径）。
/// 返回 (来源, session_id)，不含根自身。
async fn usage_descendants(
    inner: &CoreInner,
    root: &str,
) -> Result<Vec<(moc_protocol::methods::UsageSource, String)>, RpcError> {
    use moc_protocol::methods::UsageSource;
    let known = inner.sessions.known().await.map_err(to_rpc)?;
    let mut found = Vec::new();
    let mut queue: VecDeque<String> = VecDeque::from([root.to_owned()]);
    let mut seen: BTreeSet<String> = BTreeSet::from([root.to_owned()]);
    while let Some(parent) = queue.pop_front() {
        let orphans = inner.sessions.orphan_marks(&parent).await;
        for info in &known {
            if info.parent_session_id.as_deref() != Some(parent.as_str())
                || orphans.contains(&info.id)
                || !seen.insert(info.id.clone())
            {
                continue;
            }
            let source =
                if info.agent_preset.as_deref() == Some(crate::agents::preset::SIDEKICK_PRESET) {
                    UsageSource::Sidekick
                } else {
                    UsageSource::Subagent
                };
            found.push((source, info.id.clone()));
            queue.push_back(info.id.clone());
        }
    }
    Ok(found)
}

mod config_api;
mod debug_info;
mod inspect;
mod plugin_api;
mod resource_api;
mod session_api;
mod session_config;
mod usage;
mod workflow_api;

mod protocol;

/// 会话的 workflow 管理器（未激活也会惰性创建：run 查询与 stopped 列表不依赖激活位）。
async fn workflow_manager(
    inner: &Arc<CoreInner>,
    record: &crate::session::SessionRecord,
) -> Arc<crate::workflow::runtime::WorkflowManager> {
    let session_id = record.state.lock().await.session.id.clone();
    let runtime = inner.runtime(&session_id).await;
    Arc::clone(
        runtime
            .workflows
            .get_or_init(|| async {
                crate::workflow::runtime::WorkflowManager::new(
                    Arc::clone(inner),
                    &session_id,
                    record.paths.clone(),
                )
            })
            .await,
    )
}

/// load 系结果的 armed 补丁：`SessionTable` 读不到本进程 runtime，这里统一补；
/// 冷加载会话没有 runtime 记录，armed 天然为 `false`（续跑不自行复活）。
async fn goal_with_armed(
    inner: &Arc<CoreInner>,
    session_id: &str,
    record: &Arc<crate::session::SessionRecord>,
) -> Option<moc_protocol::methods::GoalView> {
    let runtime = inner.runtime(session_id).await;
    let state = record.state.lock().await;
    crate::goal::view(
        state.goal.as_ref(),
        state.goal_rounds_started,
        runtime.goal_armed.is_armed(),
    )
}

#[cfg(test)]
mod compression_tests;

mod bash;
mod btw;
mod context;
mod history;
mod management;
pub(crate) mod mode;
pub(crate) mod model;
pub(crate) mod prompt;

//! core 的公开入口、生命周期管理和 JSON-RPC 服务实现。

pub(crate) mod plugin_events;
mod plugin_persistence;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moc_llm::Client;
use moc_protocol::methods::{
    AgentListParams, AgentListResult, AgentMessageParams, AgentMessageResult, AgentProfile,
    AgentSpawnParams, AgentSpawnResult, AgentStopParams, AgentStopResult, AgentWaitParams,
    AgentWaitResult, ClientCapabilities, CompactResult, ConfigGetParams, ConfigGetResult,
    ContextContentResult, ContextUsageResult, Empty, InitializeParams, InitializeResult, ModelInfo,
    ModelInput, ModelUsageSummary, PluginCommandInfo, PluginInfo, PluginListParams,
    PluginListResult, PluginReloadParams, PluginReloadResult, PluginRunCommandParams,
    PluginRunCommandResult, PluginToolInfo, PromptResult, ResourceCompleteParams,
    ResourceCompleteResult, ServerCapabilities, ServerInfo, SessionCompactParams,
    SessionContextContentParams, SessionContextParams, SessionDeleteParams, SessionInfo,
    SessionListParams, SessionListResult, SessionLoadPageParams, SessionLoadPageResult,
    SessionLoadParams, SessionLoadResult, SessionMode, SessionNewParams, SessionPermission,
    SessionPlanApproveParams, SessionPlanApproveResult, SessionRenameParams, SessionRewindParams,
    SessionSetModeParams, SessionSetModelParams, SessionSetModelResult, SessionSetPermissionParams,
    SessionUnloadParams, SessionUnlockParams, SessionUpdate, SessionUsageParams,
    SessionUsageResult, UnsteerParams, UnsteerResult,
};
use moc_protocol::{AgentService, RequestContext, RpcError, RpcErrorCode};
use parking_lot::RwLock;
use serde_json::json;
use tokio::sync::Mutex;

use crate::CoreConfig;
use crate::compact;
use crate::config::{ResolvedConfig, resolve_models};
use crate::error::CoreError;
use crate::session::entries::SessionOrigin;
use crate::session::{SessionTable, TranscriptEntryId};
use crate::tools::{JobManager, ToolRegistry};
use crate::workspace::watch::WorkspaceSnapshot;

/// 已加载会话的运行时状态，只在内存里，不写入 JSONL。
pub(crate) struct SessionRuntime {
    pub anchors: Arc<crate::hashline::AnchorStore>,
    pub jobs: Arc<JobManager>,
    pub workspace: Mutex<Option<WorkspaceSnapshot>>,
    pub context_files: Mutex<Option<Arc<Vec<crate::system_prompt::context_files::ContextFile>>>>,
    pub reminders: tokio::sync::Mutex<crate::system_prompt::reminder::ReminderState>,
    pub loaded_skills: Arc<RwLock<crate::skills::LoadedSkills>>,
    pub steering: Mutex<management::Steering>,
    /// 代理间消息的收件箱。运行中的提示在两轮之间取走消息并写入会话。
    pub agent_inbox: Mutex<crate::agents::AgentInbox>,
    pub guard: parking_lot::Mutex<Option<Arc<prompt::guard::PromptGuard>>>,
    /// Grok Build 两遍压缩中提前摘要（prefire）的状态；用同步原语实现，不跨任务持有异步锁。
    pub grok_prefire: crate::compact::grok_build::prefire::PrefireState,
    /// 本会话累积的沙盒授权；模式切换或 `clear_grants` 时清空。
    pub sandbox_grants: Arc<RwLock<crate::sandbox::SandboxGrants>>,
    /// 会话级临时目录与 HOME overlay；runner 不可用时为空。
    pub sandbox_scratch: Option<Arc<crate::sandbox::SandboxScratch>>,
}

impl SessionRuntime {
    fn new(slots: Arc<crate::concurrency::Gate>, support: &crate::sandbox::SandboxSupport) -> Self {
        let scratch = support
            .runner
            .and_then(|_| crate::sandbox::SandboxScratch::new().ok())
            .map(Arc::new);
        Self {
            anchors: Arc::new(crate::hashline::AnchorStore::new()),
            jobs: Arc::new(JobManager::with_slots(slots)),
            workspace: Mutex::new(None),
            context_files: Mutex::new(None),
            reminders: tokio::sync::Mutex::new(
                crate::system_prompt::reminder::ReminderState::default(),
            ),
            loaded_skills: Arc::new(RwLock::new(std::collections::HashSet::new())),
            steering: Mutex::new(management::Steering::default()),
            agent_inbox: Mutex::new(crate::agents::AgentInbox::default()),
            guard: parking_lot::Mutex::new(None),
            grok_prefire: crate::compact::grok_build::prefire::PrefireState::default(),
            sandbox_grants: Arc::new(RwLock::new(crate::sandbox::SandboxGrants::default())),
            sandbox_scratch: scratch,
        }
    }
}

/// core 进程内共享状态。
pub(crate) struct CoreInner {
    pub config: RwLock<Arc<ResolvedConfig>>,
    pub boot: CoreConfig,
    pub config_write: Mutex<()>,
    pub config_epoch: tokio::sync::RwLock<()>,
    pub provider_gates: RwLock<HashMap<String, Arc<crate::concurrency::Gate>>>,
    pub job_slots: Arc<crate::concurrency::Gate>,
    pub client: Client,
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
    pub remote_unavailable: RwLock<std::collections::HashSet<String>>,
    /// 本进程内子代理的运行状态。历史以持久化的 transcript 为准。
    pub agents: Mutex<crate::agents::AgentRegistry>,
    pub agent_tasks: Mutex<tokio::task::JoinSet<()>>,
    pub prompts: Mutex<tokio::task::JoinSet<()>>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub sandbox_support: crate::sandbox::SandboxSupport,
}

/// 会话状态里的插件调用上下文；构建提示词时还没有 `ToolCtx`，先按会话头取一份。
pub(crate) fn plugin_context(
    state: &crate::session::state::SessionState,
    model: &str,
) -> moc_plugin::CallContext {
    moc_plugin::CallContext {
        session_id: state.session.id.clone(),
        cwd: state.session.cwd.clone(),
        mode: serde_json::to_value(state.mode)
            .ok()
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default(),
        permission: state.permission.as_str().to_owned(),
        model: model.to_owned(),
        is_subagent: state.session.is_subagent(),
        writable: true,
    }
}

/// 会话配置变更事件：模式 / 模型 / 权限切换成功后广播给插件。
pub(crate) async fn emit_session_config_changed(
    inner: &CoreInner,
    state: &crate::session::state::SessionState,
    field: &str,
    from: &str,
    to: &str,
) {
    if from == to {
        return;
    }
    if let Some(host) = plugin_events::host_for(inner, std::path::Path::new(&state.session.cwd)).await
    {
        let context = plugin_events::context(
            &state.session.id,
            &state.session.cwd,
            state.mode,
            state.permission,
            &state.model,
            state.session.is_subagent(),
        );
        plugin_events::session_config_changed(&host, &context, field, from, to).await;
    }
}

/// 把插件声明适配成工具实例；调用方已经拿到宿主时用它，免得重复查询缓存。
pub(crate) fn plugin_tools_of(host: &moc_plugin::PluginHost) -> Vec<Arc<dyn crate::tools::Tool>> {
    host.tools()
        .into_iter()
        .map(|(plugin, declaration)| {
            Arc::new(crate::tools::plugin::PluginTool::new(plugin, &declaration))
                as Arc<dyn crate::tools::Tool>
        })
        .collect()
}

/// 插件清单的协议投影；`plugin/list` 与 `plugin/reload` 用同一份映射。
fn plugin_infos(host: &moc_plugin::PluginHost) -> Vec<PluginInfo> {
    host.info()
        .into_iter()
        .map(|info| PluginInfo {
            name: info.name,
            version: info.version,
            description: info.description,
            source: info.source,
            dir: info.dir.to_string_lossy().into_owned(),
            tools: info
                .tools
                .into_iter()
                .map(|tool| PluginToolInfo {
                    name: tool.name,
                    description: tool.description,
                    modes: tool.modes.names().map(str::to_owned).collect(),
                })
                .collect(),
            commands: info
                .commands
                .into_iter()
                .map(|command| PluginCommandInfo {
                    name: command.name,
                    description: command.description,
                    usage: command.usage,
                })
                .collect(),
            error: info.error,
        })
        .collect()
}

/// 插件失败提醒的前缀；`plugin_reload` 按同一前缀撤掉过时的提醒。
fn plugin_notice_prefix(plugin: &str) -> String {
    format!("插件 `{plugin}`：")
}

/// 把宿主积压的插件失败通知送进会话提醒与前端。
///
/// 调用方在加载、工具批次与命令失败这些边界上主动抽干通道：通知攒到下一轮再报，
/// 模型可能已经在用一把刚失去插件的工具表。
pub(crate) async fn flush_plugin_notices(
    host: &moc_plugin::PluginHost,
    state: &Mutex<crate::session::state::SessionState>,
    updates: &moc_protocol::UpdateSink,
    session_id: &str,
) -> Result<(), RpcError> {
    let notices = host.drain_notices();
    if notices.is_empty() {
        return Ok(());
    }
    {
        // 锁只圈住 push：通知里可能带多行 JS 栈，不能让格式化或 emit 拖住会话状态。
        let mut state = state.lock().await;
        for notice in &notices {
            state.notices.push(format!(
                "{}{}（本进程内已停用；`plugin/reload` 或重启后恢复）",
                plugin_notice_prefix(&notice.plugin),
                notice.message
            ));
        }
    }
    for notice in notices {
        updates
            .emit(
                session_id.to_owned(),
                SessionUpdate::PluginNotice {
                    plugin: notice.plugin,
                    code: notice.code,
                    message: notice.message,
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    Ok(())
}

impl CoreInner {
    pub(crate) fn effective_permission(&self, permission: SessionPermission) -> SessionPermission {
        permission
    }
    pub(crate) fn config(&self) -> Arc<ResolvedConfig> {
        Arc::clone(&self.config.read())
    }

    pub(crate) async fn provider_permit(
        &self,
        model: &crate::ResolvedModel,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> Result<crate::concurrency::Permit, CoreError> {
        let gate = {
            let mut gates = self.provider_gates.write();
            Arc::clone(gates.entry(model.provider_id.clone()).or_default())
        };
        gate.acquire(model.max_concurrency, cancel)
            .await
            .ok_or(CoreError::Cancelled)
    }
    pub(crate) fn project_config(&self, cwd: &Path) -> Result<Arc<ResolvedConfig>, CoreError> {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        if let Some(config) = self.project_configs.read().get(&cwd).cloned() {
            return Ok(config);
        }
        let config = Arc::new(self.config().for_project(&cwd)?);
        self.project_configs
            .write()
            .insert(cwd, Arc::clone(&config));
        Ok(config)
    }
    pub(crate) async fn runtime(&self, session_id: &str) -> Arc<SessionRuntime> {
        let mut runtimes = self.runtimes.lock().await;
        Arc::clone(runtimes.entry(session_id.to_owned()).or_insert_with(|| {
            Arc::new(SessionRuntime::new(
                Arc::clone(&self.job_slots),
                &self.sandbox_support,
            ))
        }))
    }
    pub(crate) async fn reset_loaded_skills(&self, session_id: &str) {
        if let Some(runtime) = self.runtimes.lock().await.get(session_id).cloned() {
            runtime.loaded_skills.write().clear();
        }
    }

    /// 在后台跑一次提示：任务挂在 `prompts` JoinSet 上，结果只记日志。
    ///
    /// 用于代理消息唤醒顶层会话。和 [`Core::run_operation`] 的区别：不返回结果（前端
    /// 通过更新通知观察），并且调用方已经持有该会话的提示锁。
    pub(crate) async fn spawn_detached_prompt(
        inner: &Arc<CoreInner>,
        ctx: RequestContext,
        params: moc_protocol::methods::PromptParams,
        record: Arc<crate::session::SessionRecord>,
        prompt_lock: tokio::sync::OwnedMutexGuard<()>,
    ) {
        let session_id = params.session_id.clone();
        let cancel = inner.shutdown.child_token();
        let ctx = ctx.with_cancel(cancel.clone());
        let task_inner = Arc::clone(inner);
        let mut prompts = inner.prompts.lock().await;
        if inner.shutdown.is_cancelled() {
            return;
        }
        while let Some(result) = prompts.try_join_next() {
            if let Err(error) = result {
                tracing::error!(%error, "会话操作任务失败");
            }
        }
        prompts.spawn(async move {
            let running = prompt::run_prompt_with_lock(
                Arc::clone(&task_inner),
                ctx,
                params,
                record,
                prompt_lock,
            );
            tokio::pin!(running);
            let result = tokio::select! {
                biased;
                _ = task_inner.shutdown.cancelled() => {
                    cancel.cancel();
                    running.await
                }
                result = &mut running => result,
            };
            if let Err(error) = result {
                tracing::warn!(session_id = %session_id, %error, "代理消息唤醒的提示执行失败");
            }
        });
    }

    pub(crate) fn client_supports_ask(&self, connection_id: u64) -> bool {
        self.capabilities
            .read()
            .get(&connection_id)
            .is_some_and(|caps| caps.ask_requests)
    }

    /// 获取该项目的 MCP 管理器，没有就创建。这里不建立连接，连接在执行提示时才启动。
    pub(crate) async fn project_mcp(
        &self,
        cwd: &Path,
    ) -> Result<Arc<moc_mcp::McpManager>, CoreError> {
        let config = self.project_config(cwd)?;
        let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        if let Some(manager) = self.mcp_managers.lock().await.get(&key).cloned() {
            return Ok(manager);
        }
        let servers = match config.mcp_servers() {
            Ok(servers) => servers,
            Err(error) => {
                tracing::error!(%error, "MCP 配置解析失败，跳过全部 server");
                Vec::new()
            }
        };
        let manager = moc_mcp::McpManager::new(servers, self.shutdown.child_token());
        self.mcp_managers
            .lock()
            .await
            .insert(key, Arc::clone(&manager));
        Ok(manager)
    }

    pub(crate) async fn mcp_summary(&self, config: &ResolvedConfig) -> Result<String, CoreError> {
        let manager = self.project_mcp(&config.cwd).await?;
        let snapshot = manager.snapshot().await;
        let plugins = self.plugin_tools(&config.cwd).await;
        let catalog = self
            .tools
            .catalog(&snapshot, manager, plugins)
            .map_err(|err| CoreError::Tool(err.to_string()))?;
        Ok(catalog.summary())
    }

    /// 渲染 PTC 模式的工具声明；非 Ptc 模式返回空串。提示主路径直接用当轮
    /// catalog 快照调用 `declarations::render`，本方法供压缩与 /context 预览复用。
    pub(crate) async fn tool_declarations(
        &self,
        mode: SessionMode,
        config: &ResolvedConfig,
        context_window: u64,
        ask_supported: bool,
    ) -> Result<String, CoreError> {
        let manager = self.project_mcp(&config.cwd).await?;
        let snapshot = manager.snapshot().await;
        let plugins = self.plugin_tools(&config.cwd).await;
        let catalog = self
            .tools
            .catalog(&snapshot, manager, plugins)
            .map_err(|err| CoreError::Tool(err.to_string()))?;
        Ok(crate::tools::declarations::render(
            &catalog,
            mode,
            ask_supported,
            config,
            context_window,
        )
        .unwrap_or_default())
    }

    /// 获取该项目的插件宿主：没有就发现并加载，目录内容变了就重建。
    ///
    /// 缓存键是工作目录；命中之后还要比对目录指纹，插件目录增删或清单/模块改动都会
    /// 让旧宿主失效。配置改动改不到文件，由保存配置时清空缓存。
    pub(crate) async fn project_plugins(
        &self,
        cwd: &Path,
    ) -> Result<Arc<moc_plugin::PluginHost>, CoreError> {
        let config = self.project_config(cwd)?;
        let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        let settings = config.plugins.settings();
        let stamp = moc_plugin::fingerprint(&key, &config.home, &settings);
        if let Some(host) = self.plugin_hosts.lock().await.get(&key).cloned()
            && host.stamp() == stamp
        {
            return Ok(host);
        }
        // 加载期间不持锁：模块求值可能耗到 load_timeout，不能挡住其它会话。
        let reserved: Vec<&str> = self.tools.all().iter().map(|tool| tool.name()).collect();
        let mut boot = self.boot.clone();
        boot.home = config.home.clone();
        let bridge = Arc::new(plugin_persistence::Bridge {
            boot,
            sessions: Arc::clone(&self.sessions),
            cwd: key.clone(),
        });
        let host = moc_plugin::PluginHost::start_with_persistence(
            &key,
            &config.home,
            &settings,
            &reserved,
            bridge,
        )
        .await;
        // 必须覆盖：命中的旧宿主指纹过期了，`or_insert` 会把它留下、丢掉刚加载的这份。
        self.plugin_hosts
            .lock()
            .await
            .insert(key, Arc::clone(&host));
        Ok(host)
    }

    /// 丢弃该项目的宿主缓存并重建，忽略指纹；插件在本进程内被停用后靠它恢复。
    pub(crate) async fn reload_plugins(
        &self,
        cwd: &Path,
    ) -> Result<Arc<moc_plugin::PluginHost>, CoreError> {
        // 键必须与 `project_plugins` 一致：先按 canonicalize 后的 cwd 移除，再走正常加载。
        let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        self.plugin_hosts.lock().await.remove(&key);
        self.project_plugins(&key).await
    }

    /// 该项目的插件工具；插件宿主不可用时退化成空列表。
    pub(crate) async fn plugin_tools(&self, cwd: &Path) -> Vec<Arc<dyn crate::tools::Tool>> {
        match self.project_plugins(cwd).await {
            Ok(host) => plugin_tools_of(&host),
            Err(error) => {
                tracing::warn!(%error, "插件宿主不可用，按无插件继续");
                Vec::new()
            }
        }
    }

    /// 获取该项目的 skill 索引，没有就扫描生成。
    pub(crate) fn project_skills(
        &self,
        cwd: &Path,
    ) -> Result<Arc<crate::skills::SkillIndex>, CoreError> {
        let config = self.project_config(cwd)?;
        let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        if let Some(index) = self.skill_indexes.read().get(&key).cloned() {
            return Ok(index);
        }
        let index = Arc::new(crate::skills::discovery::discover_index(
            cwd,
            &config.home,
            crate::paths::home_dir().as_deref(),
            &config.skills,
        ));
        self.skill_indexes.write().insert(key, Arc::clone(&index));
        Ok(index)
    }

    pub(crate) async fn project_context_files(
        &self,
        session_id: &str,
        cwd: &Path,
    ) -> Arc<Vec<crate::system_prompt::context_files::ContextFile>> {
        let runtime = self.runtime(session_id).await;
        if let Some(files) = runtime.context_files.lock().await.clone() {
            return files;
        }
        let config = self.project_config(cwd).expect("project config is loaded");
        let files = Arc::new(if config.context_files.enabled {
            crate::system_prompt::context_files::load_context_files(
                cwd,
                &config.home,
                &config.context_files,
            )
        } else {
            Vec::new()
        });
        *runtime.context_files.lock().await = Some(Arc::clone(&files));
        files
    }
}

/// 代理运行时，通过协议层的 [`AgentService`] 对外提供服务。
#[derive(Clone)]
pub struct Core {
    pub(crate) inner: Arc<CoreInner>,
}

impl Core {
    pub fn config_home(&self) -> PathBuf {
        self.inner.config().home.clone()
    }

    /// 当前平台的沙盒能力；前端据此只展示可用的权限模式。
    pub fn sandbox_support(&self) -> crate::sandbox::SandboxSupport {
        self.inner.sandbox_support.clone()
    }

    /// 加载配置、校验供应商，并创建会话存储目录。
    pub fn new(config: CoreConfig) -> anyhow::Result<Arc<Self>> {
        Self::new_with_sandbox_support(config, crate::sandbox::detect())
    }

    /// 以显式沙盒能力创建 core；测试或嵌入方可注入，避免依赖宿主机 runner。
    pub fn new_with_sandbox_support(
        config: CoreConfig,
        sandbox_support: crate::sandbox::SandboxSupport,
    ) -> anyhow::Result<Arc<Self>> {
        let mut resolved = ResolvedConfig::load(&config)?;
        if resolved.sandbox.default_mode != moc_protocol::SessionPermission::Full
            && !sandbox_support.supports(resolved.sandbox.default_mode)
        {
            anyhow::bail!(
                "sandbox.default_mode={} 在当前平台不可用；请改用 full，或在 WSL2/macOS 中提供 sandbox runner",
                resolved.sandbox.default_mode.as_str()
            );
        }
        if let Some(session_dir) = &config.session_dir {
            resolved.session.dir = session_dir.clone();
        }
        let resolved = Arc::new(resolved);
        std::fs::create_dir_all(&resolved.session.dir)?;
        let sessions = Arc::new(SessionTable::new(resolved.session.dir.clone(), true));
        Ok(Arc::new(Self {
            inner: Arc::new(CoreInner {
                config: RwLock::new(resolved),
                boot: config.clone(),
                config_write: Mutex::new(()),
                config_epoch: tokio::sync::RwLock::new(()),
                provider_gates: RwLock::new(HashMap::new()),
                job_slots: Arc::new(crate::concurrency::Gate::default()),
                client: config.client,
                sessions,
                tools: ToolRegistry::new(),
                capabilities: RwLock::new(HashMap::new()),
                runtimes: Mutex::new(HashMap::new()),
                project_configs: RwLock::new(HashMap::new()),
                mcp_managers: Mutex::new(HashMap::new()),
                skill_indexes: RwLock::new(HashMap::new()),
                plugin_hosts: Mutex::new(HashMap::new()),
                ptc_host: Mutex::new(None),
                remote_unavailable: RwLock::new(std::collections::HashSet::new()),
                agents: Mutex::new(crate::agents::AgentRegistry::default()),
                agent_tasks: Mutex::new(tokio::task::JoinSet::new()),
                prompts: Mutex::new(tokio::task::JoinSet::new()),
                shutdown: tokio_util::sync::CancellationToken::new(),
                sandbox_support,
            }),
        }))
    }

    /// 取消并等待所有提示结束，终止后台进程组和 MCP 连接，释放运行时状态。
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let mut agents = std::mem::take(&mut *self.inner.agent_tasks.lock().await);
        while let Some(result) = agents.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "子代理任务异常退出");
            }
        }
        let mut prompts = std::mem::take(&mut *self.inner.prompts.lock().await);
        while let Some(result) = prompts.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "提示任务异常退出");
            }
        }
        let managers: Vec<_> = self
            .inner
            .mcp_managers
            .lock()
            .await
            .values()
            .cloned()
            .collect();
        for manager in managers {
            manager.shutdown().await;
        }
        let runtimes: Vec<_> = self.inner.runtimes.lock().await.values().cloned().collect();
        for runtime in runtimes {
            runtime.jobs.kill_all().await;
        }
        if let Some(host) = self.inner.ptc_host.lock().await.take() {
            host.shutdown().await;
        }
        self.inner.runtimes.lock().await.clear();
        // 丢掉宿主句柄：插件线程收到退出信号后自己走 deactivate 并释放 Runtime。
        self.inner.plugin_hosts.lock().await.clear();
    }

    /// 返回配置中的模型列表，供 CLI 展示与诊断。
    pub fn models(config: &CoreConfig) -> anyhow::Result<Vec<crate::ResolvedModel>> {
        resolve_models(config)
    }
    /// 撤销当前会话最近一次文件编辑。
    pub async fn undo(&self, session_id: &str) -> anyhow::Result<String> {
        let record = self.inner.sessions.get(session_id).await?;
        let _prompt = record.try_prompt_lock()?;
        self.inner
            .sessions
            .ensure_write_lease(session_id, &record, "moc-cli")
            .await?;
        let cwd = record.state.lock().await.session.cwd.clone();
        let config = self.inner.project_config(Path::new(&cwd))?;
        let runtime = self.inner.runtime(session_id).await;
        let path = runtime
            .anchors
            .latest_undo_path()
            .ok_or_else(|| anyhow::anyhow!("[E_NO_UNDO] 当前会话没有可撤销的编辑"))?;
        Ok(crate::hashline::apply::undo(
            &path,
            &runtime.anchors,
            config.tools.output.max_file_bytes,
        )
        .await?)
    }
    /// 把长操作挂在 Core 上运行，前端断开后也能完成进程回收和会话提交。
    async fn run_operation<T: Send + 'static>(
        &self,
        cancel: tokio_util::sync::CancellationToken,
        running: impl Future<Output = Result<T, RpcError>> + Send + 'static,
    ) -> Result<T, RpcError> {
        let _cancel_on_drop = cancel.clone().drop_guard();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let inner = Arc::clone(&self.inner);
        {
            let mut prompts = self.inner.prompts.lock().await;
            if self.inner.shutdown.is_cancelled() {
                return Err(RpcError::of(RpcErrorCode::RequestCancelled));
            }
            while let Some(result) = prompts.try_join_next() {
                if let Err(error) = result {
                    tracing::error!(%error, "会话操作任务失败");
                }
            }
            prompts.spawn(async move {
                tokio::pin!(running);
                let result = tokio::select! {
                    biased;
                    _ = inner.shutdown.cancelled() => { cancel.cancel(); running.await },
                    result = &mut running => result,
                };
                let _ = sender.send(result);
            });
        }
        receiver.await.map_err(protocol_to_rpc)?
    }
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

impl AgentService for Core {
    async fn initialize(
        &self,
        ctx: RequestContext,
        params: InitializeParams,
    ) -> Result<InitializeResult, RpcError> {
        self.inner
            .capabilities
            .write()
            .insert(ctx.connection_id(), params.capabilities);
        Ok(InitializeResult {
            protocol_version: moc_protocol::PROTOCOL_VERSION.into(),
            server: ServerInfo {
                name: "moc-core".into(),
                version: env!("CARGO_PKG_VERSION").into(),
            },
            capabilities: ServerCapabilities {
                permission_requests: true,
                session_locking: self.inner.sessions.locking_enabled(),
                ask_requests: true,
            },
        })
    }

    async fn config_manage(
        &self,
        ctx: RequestContext,
        params: moc_protocol::ConfigAction,
    ) -> Result<moc_protocol::ConfigReply, RpcError> {
        management::manage(&self.inner, ctx, params)
            .await
            .map_err(to_rpc)
    }

    async fn session_steer(
        &self,
        _ctx: RequestContext,
        params: moc_protocol::SteerParams,
    ) -> Result<moc_protocol::SteerResult, RpcError> {
        management::steer(&self.inner, params).await.map_err(to_rpc)
    }

    async fn session_unsteer(
        &self,
        _ctx: RequestContext,
        params: UnsteerParams,
    ) -> Result<UnsteerResult, RpcError> {
        management::unsteer(&self.inner, params)
            .await
            .map_err(to_rpc)
    }

    async fn agent_spawn(
        &self,
        ctx: RequestContext,
        params: AgentSpawnParams,
    ) -> Result<AgentSpawnResult, RpcError> {
        crate::agents::spawn(
            Arc::clone(&self.inner),
            ctx,
            params.session_id,
            params.task,
            params.title,
            params.model,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_list(
        &self,
        _ctx: RequestContext,
        params: AgentListParams,
    ) -> Result<AgentListResult, RpcError> {
        crate::agents::list(&self.inner, params.session_id)
            .await
            .map_err(to_rpc)
    }

    async fn agent_message(
        &self,
        ctx: RequestContext,
        params: AgentMessageParams,
    ) -> Result<AgentMessageResult, RpcError> {
        crate::agents::message(
            Arc::clone(&self.inner),
            ctx,
            params.session_id,
            params.target,
            params.message,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_stop(
        &self,
        _ctx: RequestContext,
        params: AgentStopParams,
    ) -> Result<AgentStopResult, RpcError> {
        crate::agents::stop(
            &self.inner,
            params.session_id,
            params.agent_id,
            params.reason,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_wait(
        &self,
        ctx: RequestContext,
        params: AgentWaitParams,
    ) -> Result<AgentWaitResult, RpcError> {
        crate::agents::wait(
            &self.inner,
            &params.session_id,
            params.agent_ids,
            params.timeout_secs,
            None,
            &ctx.cancel(),
        )
        .await
        .map_err(to_rpc)
    }

    async fn config_get(
        &self,
        _ctx: RequestContext,
        _params: ConfigGetParams,
    ) -> Result<ConfigGetResult, RpcError> {
        let config = self.inner.config();
        let models = config
            .available_models()
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                display_name: model.display_name,
                thinking_levels: Some(
                    std::iter::once("off".to_owned())
                        .chain(
                            model
                                .thinking_levels
                                .iter()
                                .map(|level| level.as_str().to_owned()),
                        )
                        .collect(),
                ),
                context_window: Some(model.context_window),
                max_output_tokens: Some(model.max_output_tokens),
                pricing: model.pricing,
                inputs: [
                    (model.capabilities.text, ModelInput::Text),
                    (model.capabilities.image, ModelInput::Image),
                ]
                .into_iter()
                .filter_map(|(supported, input)| supported.then_some(input))
                .collect(),
                remote_compaction: model.remote_compaction,
            })
            .collect::<Vec<_>>();
        Ok(ConfigGetResult {
            models,
            default_model: if config.roles.default.is_empty() {
                None
            } else {
                Some(config.resolve_role("default").map_err(to_rpc)?.selector)
            },
            agents: vec![AgentProfile {
                id: "moc".into(),
                display_name: "moc".into(),
                description: Some("通用编码代理".into()),
            }],
            default_agent: Some("moc".into()),
            cwd: config.cwd.to_string_lossy().into_owned(),
        })
    }
    async fn plugin_list(
        &self,
        _ctx: RequestContext,
        params: PluginListParams,
    ) -> Result<PluginListResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let state = record.state.lock().await;
        let cwd = PathBuf::from(&state.session.cwd);
        let host = self.inner.project_plugins(&cwd).await.map_err(to_rpc)?;
        Ok(PluginListResult {
            plugins: plugin_infos(&host),
        })
    }

    async fn plugin_reload(
        &self,
        _ctx: RequestContext,
        params: PluginReloadParams,
    ) -> Result<PluginReloadResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
        let host = self.inner.reload_plugins(&cwd).await.map_err(to_rpc)?;
        // 重载是用户主动发起的：新宿主若仍有加载失败，返回清单的 error 已经写明，
        // 这里丢掉通知，免得下一轮把刚展示过的失败再报一次。
        host.drain_notices();
        let plugins = plugin_infos(&host);
        // 重载的目的就是把插件修好：撤掉本次会话里对已恢复插件的「已停用」提醒，
        // 否则下一轮模型还会被告知一个其实已经可用的插件坏了。仍失败的保留原因。
        {
            let mut state = record.state.lock().await;
            for plugin in plugins.iter().filter(|plugin| plugin.error.is_none()) {
                let prefix = plugin_notice_prefix(&plugin.name);
                state.notices.retain(|notice| !notice.starts_with(&prefix));
            }
        }
        Ok(PluginReloadResult { plugins })
    }

    async fn plugin_run_command(
        &self,
        ctx: RequestContext,
        params: PluginRunCommandParams,
    ) -> Result<PluginRunCommandResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _epoch = self.inner.config_epoch.read().await;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, "moc-plugin")
            .await
            .map_err(to_rpc)?;
        let (cwd, context) = {
            let state = record.state.lock().await;
            (
                PathBuf::from(&state.session.cwd),
                plugin_context(&state, &state.model),
            )
        };
        // JS 可同步访问持久化 state；等待插件返回时不能占用会话状态锁。
        let host = self.inner.project_plugins(&cwd).await.map_err(to_rpc)?;
        let text = match host.run_command(&params.name, &params.args, &context).await {
            Ok(text) => text,
            Err(error) => {
                // 命令失败通常意味着插件刚被停用：先把失败通知送到前端与下一轮提醒再报错。
                flush_plugin_notices(&host, &record.state, &ctx.updates(), &params.session_id)
                    .await?;
                return Err(RpcError::new(
                    RpcErrorCode::InvalidParams.code(),
                    error.to_string(),
                ));
            }
        };
        Ok(PluginRunCommandResult { text })
    }

    async fn resource_complete(
        &self,
        _ctx: RequestContext,
        params: ResourceCompleteParams,
    ) -> Result<ResourceCompleteResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
        let skills = self.inner.project_skills(&cwd).map_err(to_rpc)?;
        let context = crate::resources::ResourceContext {
            session_id: &params.session_id,
            cwd: &cwd,
            paths: &record.paths,
            skills: &skills,
            sessions: &self.inner.sessions,
        };
        let candidates = crate::resources::ResourceRouter::complete(&context, &params.prefix)
            .map_err(|error| RpcError::new(RpcErrorCode::InvalidParams.code(), error.to_string()))?
            .into_iter()
            .map(|candidate| moc_protocol::ResourceCandidate {
                uri: candidate.uri,
                label: candidate.label,
                description: candidate.description,
                kind: candidate.kind,
                append_space: candidate.append_space,
            })
            .collect();
        Ok(ResourceCompleteResult { candidates })
    }

    async fn session_new(
        &self,
        ctx: RequestContext,
        params: SessionNewParams,
    ) -> Result<SessionInfo, RpcError> {
        let cwd = params
            .cwd
            .as_deref()
            .map(PathBuf::from)
            .unwrap_or_else(|| self.inner.config().cwd.clone());
        let cwd = std::fs::canonicalize(&cwd)
            .map_err(|error| invalid_params(format!("cwd 无效：{error}")))?;
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        let selector = params.model.as_deref().unwrap_or("default");
        let model = config.resolve_model(selector).map_err(|error| {
            let selectors: Vec<String> = config
                .available_models()
                .into_iter()
                .map(|model| model.selector)
                .take(10)
                .collect();
            RpcError::of(RpcErrorCode::InvalidParams).with_data(json!({
                "message": error.to_string(),
                "available_models": selectors,
            }))
        })?;
        let permission = config.sandbox.default_mode;
        if !self.inner.sandbox_support.supports(permission) {
            return Err(invalid_params(format!(
                "sandbox.default_mode={} 在当前平台不可用，请改用 full 或调整配置",
                permission.as_str()
            )));
        }
        let mut info = self
            .inner
            .sessions
            .create(
                &cwd,
                &model.selector,
                params.mode,
                permission,
                params.title.as_deref(),
                SessionOrigin::root(),
                config.session.compress,
            )
            .await
            .map_err(to_rpc)?;
        let record = self.inner.sessions.get(&info.id).await.map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&info.id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &info.id, &record, error))?;
        // 以计划模式创建会话：header 记下会话本来的模型，随后立即切到计划模型，
        // 退出计划模式时再切回。
        if params.mode == SessionMode::Plan
            && let Some(plan_model) = config
                .resolve_plan_mode_model()
                .map_err(to_rpc)?
                .map(|model| model.selector)
                .filter(|plan_model| *plan_model != model.selector)
        {
            info = self
                .inner
                .sessions
                .set_model(&info.id, plan_model)
                .await
                .map_err(to_rpc)?;
        }
        info.permission = self.inner.effective_permission(info.permission);
        if matches!(params.mode, SessionMode::Ask | SessionMode::Plan) && config.workspace.watch {
            let snapshot = snapshot_workspace(cwd, config.workspace.max_scan_entries)
                .await
                .map_err(to_rpc)?;
            *self.inner.runtime(&info.id).await.workspace.lock().await = Some(snapshot);
        }
        if let Some(host) =
            plugin_events::host_for(&self.inner, std::path::Path::new(&info.cwd)).await
        {
            let context = plugin_events::context_from_info(&info);
            for message in plugin_events::session_start(&host, &context, &info, "user", None).await
            {
                let role = if message.role == "system" {
                    moc_llm::Role::System
                } else {
                    moc_llm::Role::User
                };
                self.inner
                    .sessions
                    .append_message(
                        &info.id,
                        moc_llm::Message {
                            role,
                            content: vec![moc_llm::ContentPart::Text(message.text)],
                        },
                        None,
                        None,
                    )
                    .await
                    .map_err(to_rpc)?;
            }
        }
        Ok(info)
    }

    async fn session_list(
        &self,
        _ctx: RequestContext,
        _params: SessionListParams,
    ) -> Result<SessionListResult, RpcError> {
        let mut sessions = self.inner.sessions.list().await.map_err(to_rpc)?;
        for session in &mut sessions {
            session.permission = self.inner.effective_permission(session.permission);
        }
        Ok(SessionListResult { sessions })
    }

    async fn session_load(
        &self,
        ctx: RequestContext,
        params: SessionLoadParams,
    ) -> Result<SessionLoadResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        if !params.read_only {
            self.inner
                .sessions
                .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
                .await
                .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        }
        let mut result = self
            .inner
            .sessions
            .load(&params.session_id, params.max_entries)
            .await
            .map_err(to_rpc)?;
        result.session.permission = self.inner.effective_permission(result.session.permission);
        Ok(result)
    }

    async fn session_load_page(
        &self,
        _ctx: RequestContext,
        params: SessionLoadPageParams,
    ) -> Result<SessionLoadPageResult, RpcError> {
        let mut result = self
            .inner
            .sessions
            .load_page(
                &params.session_id,
                params.before_entry_id.as_deref(),
                params.max_entries,
                params.max_message_bytes,
            )
            .await
            .map_err(to_rpc)?;
        result.session.permission = self.inner.effective_permission(result.session.permission);
        Ok(result)
    }
    async fn session_rewind(
        &self,
        ctx: RequestContext,
        params: SessionRewindParams,
    ) -> Result<SessionLoadResult, RpcError> {
        let entry_id = TranscriptEntryId::new(params.entry_id);
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        let mut loaded = self
            .inner
            .sessions
            .rewind(&params.session_id, &entry_id, config.session.compress)
            .await
            .map_err(to_rpc)?;
        let child_id = loaded.session.id.clone();
        let child = self.inner.sessions.get(&child_id).await.map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&child_id, &child, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &child_id, &child, error))?;
        loaded.session.lock = None;
        loaded.session.permission = self.inner.effective_permission(loaded.session.permission);
        if let Some(host) = plugin_events::host_for(
            &self.inner,
            std::path::Path::new(&loaded.session.cwd),
        )
        .await
        {
            let context = plugin_events::context_from_info(&loaded.session);
            let parent = loaded.session.forked_from.clone();
            for message in plugin_events::session_start(
                &host,
                &context,
                &loaded.session,
                "rewind",
                parent.as_deref(),
            )
            .await
            {
                let role = if message.role == "system" {
                    moc_llm::Role::System
                } else {
                    moc_llm::Role::User
                };
                self.inner
                    .sessions
                    .append_message(
                        &loaded.session.id,
                        moc_llm::Message {
                            role,
                            content: vec![moc_llm::ContentPart::Text(message.text)],
                        },
                        None,
                        None,
                    )
                    .await
                    .map_err(to_rpc)?;
            }
        }
        Ok(loaded)
    }

    async fn session_unload(
        &self,
        _ctx: RequestContext,
        params: SessionUnloadParams,
    ) -> Result<Empty, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        // unload_locked 要求调用方持有提示锁：进行中的提示会因运行时状态被移除而 panic 或写入失败，
        // 而且这里会先杀掉提示正在用的后台任务。和其它修改类 RPC 一样，拿不到锁就返回 SESSION_BUSY。
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        let state = record.state.lock().await.clone();
        if let Some(host) =
            plugin_events::host_for(&self.inner, std::path::Path::new(&state.session.cwd)).await
        {
            let context = plugin_events::context(
                &params.session_id,
                &state.session.cwd,
                state.mode,
                state.permission,
                &state.model,
                state.session.is_subagent(),
            );
            plugin_events::session_end(&host, &context, "unloaded").await;
        }
        let runtime = self.inner.runtime(&params.session_id).await;
        runtime.jobs.kill_all().await;
        self.inner
            .sessions
            .unload_locked(&params.session_id, &record);
        self.inner.runtimes.lock().await.remove(&params.session_id);
        Ok(Empty {})
    }

    async fn session_rename(
        &self,
        ctx: RequestContext,
        params: SessionRenameParams,
    ) -> Result<SessionInfo, RpcError> {
        let session_id = params.session_id.clone();
        let record = self.inner.sessions.get(&session_id).await.map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &session_id, &record, error))?;
        let mut info = self
            .inner
            .sessions
            .rename(&session_id, params)
            .await
            .map_err(to_rpc)?;
        info.lock = None;
        info.permission = self.inner.effective_permission(info.permission);
        ctx.updates()
            .emit(
                session_id,
                SessionUpdate::TitleChanged {
                    title: info.title.clone(),
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
        Ok(info)
    }

    async fn session_delete(
        &self,
        _ctx: RequestContext,
        params: SessionDeleteParams,
    ) -> Result<Empty, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, "moc-core")
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let state = record.state.lock().await.clone();
        if let Some(host) =
            plugin_events::host_for(&self.inner, std::path::Path::new(&state.session.cwd)).await
        {
            let context = plugin_events::context(
                &params.session_id,
                &state.session.cwd,
                state.mode,
                state.permission,
                &state.model,
                state.session.is_subagent(),
            );
            plugin_events::session_end(&host, &context, "deleted").await;
        }
        let runtime = self.inner.runtime(&params.session_id).await;
        runtime.jobs.kill_all().await;
        self.inner
            .sessions
            .delete_locked(&params.session_id, &record)
            .await
            .map_err(to_rpc)?;
        self.inner.runtimes.lock().await.remove(&params.session_id);
        Ok(Empty {})
    }

    async fn session_unlock(
        &self,
        _ctx: RequestContext,
        params: SessionUnlockParams,
    ) -> Result<Empty, RpcError> {
        if !self.inner.sessions.locking_enabled() {
            return Err(RpcError::of(RpcErrorCode::NotSupported));
        }
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        if !self.inner.sessions.release_lease(&record)
            && let Some(info) = self.inner.sessions.lease_info(&params.session_id, &record)
        {
            return Err(RpcError::new(
                RpcErrorCode::SessionLocked.code(),
                format!(
                    "会话正被 pid {} ({}) 持有；请先让持有者卸载或退出",
                    info.pid, info.frontend
                ),
            )
            .with_data(json!({ "lock": info })));
        }
        Ok(Empty {})
    }

    async fn session_set_permission(
        &self,
        ctx: RequestContext,
        params: SessionSetPermissionParams,
    ) -> Result<SessionInfo, RpcError> {
        if !self.inner.sandbox_support.supports(params.permission) {
            let available = self
                .inner
                .sandbox_support
                .modes
                .iter()
                .map(|mode| mode.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            return Err(invalid_params(format!(
                "当前平台不支持 {} 权限；可用模式：{available}",
                params.permission.as_str()
            )));
        }
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let old_permission = record.state.lock().await.permission;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let mut info = self
            .inner
            .sessions
            .set_permission(&params.session_id, params.permission)
            .await
            .map_err(to_rpc)?;
        if params.clear_grants || old_permission != params.permission {
            self.inner
                .runtime(&params.session_id)
                .await
                .sandbox_grants
                .write()
                .clear();
        }
        info.lock = None;
        info.permission = self.inner.effective_permission(info.permission);
        ctx.updates()
            .emit(
                params.session_id,
                SessionUpdate::PermissionChanged {
                    permission: info.permission,
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
        let state = record.state.lock().await.clone();
        emit_session_config_changed(
            &self.inner,
            &state,
            "permission",
            old_permission.as_str(),
            info.permission.as_str(),
        )
        .await;
        Ok(info)
    }

    async fn session_set_mode(
        &self,
        ctx: RequestContext,
        params: SessionSetModeParams,
    ) -> Result<SessionInfo, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let (cwd, from, model_before) = {
            let state = record.state.lock().await;
            (
                PathBuf::from(state.session.cwd.clone()),
                state.mode,
                state.model.clone(),
            )
        };
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        // 进入计划模式前先解析计划模型，避免解析失败时模式已切换而模型没切换。
        let plan_model = if params.mode == SessionMode::Plan && from != SessionMode::Plan {
            config
                .resolve_plan_mode_model()
                .map_err(to_rpc)?
                .map(|model| model.selector)
        } else {
            None
        };
        let mut info = self
            .inner
            .sessions
            .set_mode(&params.session_id, params.mode)
            .await
            .map_err(to_rpc)?;
        let switched = mode::sync_plan_model(
            &self.inner,
            &params.session_id,
            from,
            params.mode,
            plan_model,
        )
        .await
        .map_err(to_rpc)?;
        if switched.is_some() {
            info = record.state.lock().await.session_info();
        }
        info.lock = None;
        info.permission = self.inner.effective_permission(info.permission);
        let runtime = self.inner.runtime(&params.session_id).await;
        if matches!(params.mode, SessionMode::Ask | SessionMode::Plan) && config.workspace.watch {
            let snapshot = snapshot_workspace(cwd, config.workspace.max_scan_entries)
                .await
                .map_err(to_rpc)?;
            *runtime.workspace.lock().await = Some(snapshot);
        }
        mode::emit_mode_change(&ctx, &params.session_id, info.mode, switched.clone()).await?;
        let state = record.state.lock().await.clone();
        emit_session_config_changed(
            &self.inner,
            &state,
            "mode",
            &serde_json::to_value(from)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
            &serde_json::to_value(info.mode)
                .ok()
                .and_then(|value| value.as_str().map(str::to_owned))
                .unwrap_or_default(),
        )
        .await;
        if let Some(model) = &switched {
            emit_session_config_changed(&self.inner, &state, "model", &model_before, model).await;
        }
        Ok(info)
    }

    async fn session_set_model(
        &self,
        ctx: RequestContext,
        params: SessionSetModelParams,
    ) -> Result<SessionSetModelResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        let (cwd, model_before) = {
            let state = record.state.lock().await;
            (state.session.cwd.clone(), state.model.clone())
        };
        let config = self
            .inner
            .project_config(std::path::Path::new(&cwd))
            .map_err(to_rpc)?;
        let model = config.resolve_model(&params.selector).map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let outcome = model::switch(
            &self.inner,
            &ctx,
            &params.session_id,
            &model,
            params.overflow,
        )
        .await
        .map_err(to_rpc)?;
        let mut session = outcome.session;
        session.lock = None;
        session.permission = self.inner.effective_permission(session.permission);
        let state = record.state.lock().await.clone();
        emit_session_config_changed(&self.inner, &state, "model", &model_before, &session.model)
            .await;
        Ok(SessionSetModelResult {
            session,
            overflow: outcome.overflow,
            compaction_error: outcome.compaction_error,
        })
    }

    async fn session_plan_approve(
        &self,
        ctx: RequestContext,
        params: SessionPlanApproveParams,
    ) -> Result<SessionPlanApproveResult, RpcError> {
        let plan_id = params.plan_id.clone();
        let action = match params.action {
            moc_protocol::methods::PlanAction::Execute => "execute",
            moc_protocol::methods::PlanAction::ExecuteCompacted => "execute_compacted",
            moc_protocol::methods::PlanAction::ExecuteNewSession => "execute_new_session",
            moc_protocol::methods::PlanAction::Reject => "reject",
        };
        let result = mode::approve_plan(&self.inner, ctx, params).await?;
        let record = self
            .inner
            .sessions
            .get(&result.session_id)
            .await
            .map_err(to_rpc)?;
        let state = record.state.lock().await.clone();
        if let Some(host) =
            plugin_events::host_for(&self.inner, std::path::Path::new(&state.session.cwd)).await
        {
            let context = plugin_events::context(
                &result.session_id,
                &state.session.cwd,
                state.mode,
                state.permission,
                &state.model,
                state.session.is_subagent(),
            );
            plugin_events::plan_decided(&host, &context, &plan_id, action).await;
        }
        Ok(result)
    }

    async fn session_compact(
        &self,
        ctx: RequestContext,
        params: SessionCompactParams,
    ) -> Result<CompactResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        self.inner
            .sessions
            .ensure_write_lease(&params.session_id, &record, &ctx.client().frontend)
            .await
            .map_err(|error| write_lease_error(self, &params.session_id, &record, error))?;
        let result = compact::manual(
            &self.inner,
            &ctx,
            &params.session_id,
            params.instructions.as_deref(),
            None,
            None,
        )
        .await
        .map_err(to_rpc)?;
        Ok(result)
    }

    async fn session_context(
        &self,
        ctx: RequestContext,
        params: SessionContextParams,
    ) -> Result<ContextUsageResult, RpcError> {
        context::usage(&self.inner, ctx, params).await
    }

    async fn session_context_content(
        &self,
        ctx: RequestContext,
        params: SessionContextContentParams,
    ) -> Result<ContextContentResult, RpcError> {
        context::content(&self.inner, ctx, params).await
    }
    async fn session_usage(
        &self,
        _ctx: RequestContext,
        params: SessionUsageParams,
    ) -> Result<SessionUsageResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let state = record.state.lock().await;
        let mut models = Vec::with_capacity(state.model_usage.len());
        let mut total = crate::session::state::ModelUsage::default();
        for (model, summary) in &state.model_usage {
            total.calls = total.calls.saturating_add(summary.calls);
            crate::session::state::add_usage(&mut total.usage, summary.usage);
            total.active_ms = total.active_ms.saturating_add(summary.active_ms);
            total.ttft_ms = total.ttft_ms.saturating_add(summary.ttft_ms);
            total.ttft_calls = total.ttft_calls.saturating_add(summary.ttft_calls);
            total.gen_ms = total.gen_ms.saturating_add(summary.gen_ms);
            models.push(ModelUsageSummary {
                model: model.clone(),
                calls: summary.calls,
                input_tokens: summary.usage.input_tokens,
                output_tokens: summary.usage.output_tokens,
                cached_input_tokens: summary.usage.cached_input_tokens,
                cache_creation_input_tokens: summary.usage.cache_creation_input_tokens,
                reasoning_tokens: summary.usage.reasoning_tokens,
                active_ms: summary.active_ms,
                ttft_ms: summary.ttft_ms,
                ttft_calls: summary.ttft_calls,
                gen_ms: summary.gen_ms,
            });
        }
        Ok(SessionUsageResult {
            models,
            total: ModelUsageSummary {
                model: "total".into(),
                calls: total.calls,
                input_tokens: total.usage.input_tokens,
                output_tokens: total.usage.output_tokens,
                cached_input_tokens: total.usage.cached_input_tokens,
                cache_creation_input_tokens: total.usage.cache_creation_input_tokens,
                reasoning_tokens: total.usage.reasoning_tokens,
                active_ms: total.active_ms,
                ttft_ms: total.ttft_ms,
                ttft_calls: total.ttft_calls,
                gen_ms: total.gen_ms,
            },
            tool_ms: state.tool_ms,
        })
    }

    async fn session_bash(
        &self,
        ctx: RequestContext,
        params: moc_protocol::BashParams,
    ) -> Result<moc_protocol::BashResult, RpcError> {
        self.run_operation(
            ctx.cancel(),
            bash::run(Arc::clone(&self.inner), ctx, params),
        )
        .await
    }
    async fn session_btw(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::BtwParams,
    ) -> Result<moc_protocol::methods::BtwResult, RpcError> {
        self.run_operation(ctx.cancel(), btw::run(Arc::clone(&self.inner), ctx, params))
            .await
    }

    async fn session_prompt(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::PromptParams,
    ) -> Result<PromptResult, RpcError> {
        self.run_operation(
            ctx.cancel(),
            prompt::run_prompt(Arc::clone(&self.inner), ctx, params),
        )
        .await
    }
}
/// 写租约失败时把持有者投影一并返回，前端不用再发一次 `session/list` 才能提示。
fn write_lease_error(
    core: &Core,
    session_id: &str,
    record: &Arc<crate::session::SessionRecord>,
    error: CoreError,
) -> RpcError {
    let message = error.to_string();
    if message.contains("SESSION_LOCKED") {
        return RpcError::new(RpcErrorCode::SessionLocked.code(), message).with_data(json!({
            "lock": core.inner.sessions.lease_info(session_id, record),
        }));
    }
    to_rpc(error)
}

fn to_rpc(error: CoreError) -> RpcError {
    let message = error.to_string();
    let code = if message.contains("SESSION_NOT_FOUND") {
        RpcErrorCode::SessionNotFound
    } else if message.contains("SESSION_BUSY") {
        RpcErrorCode::SessionBusy
    } else if message.contains("SESSION_LOCKED") {
        RpcErrorCode::SessionLocked
    } else if matches!(error, CoreError::Config(_)) || message.contains("INVALID_PARAMS") {
        RpcErrorCode::InvalidParams
    } else if message.contains("NotSupported") {
        RpcErrorCode::NotSupported
    } else if matches!(error, CoreError::Cancelled) {
        RpcErrorCode::RequestCancelled
    } else {
        RpcErrorCode::InternalError
    };
    RpcError::new(code.code(), message)
}

pub(crate) fn invalid_params(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidParams.code(), message)
}

pub(crate) fn protocol_to_rpc(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
}

pub(crate) async fn snapshot_workspace(
    cwd: PathBuf,
    max_entries: usize,
) -> Result<WorkspaceSnapshot, CoreError> {
    tokio::task::spawn_blocking(move || crate::workspace::watch::snapshot(&cwd, max_entries))
        .await
        .map_err(|error| CoreError::Session(format!("工作区快照任务失败：{error}")))?
        .map_err(|error| CoreError::Session(error.to_string()))
}

#[cfg(test)]
mod compression_tests {
    use super::*;
    use moc_protocol::{
        ClientBuilder, ClientInfo, ConfigAction, ConfigManage, ConfigReply, ConfigScope, Message,
        PlanAction, Server, SessionNew, SessionPlanApprove, SessionRewind, transport,
    };

    #[tokio::test]
    async fn project_compression_is_selected_per_creation_and_after_config_save() {
        for global in [false, true] {
            let root = tempfile::tempdir().expect("临时目录");
            let home = root.path().join("home");
            let plain = root.path().join("plain");
            let compressed = root.path().join("compressed");
            std::fs::create_dir_all(&home).unwrap();
            for (cwd, enabled) in [(&plain, false), (&compressed, true)] {
                std::fs::create_dir_all(cwd.join(".moc")).unwrap();
                std::fs::write(
                    cwd.join(".moc/config.toml"),
                    format!("[session]\ncompress={enabled}\n"),
                )
                .unwrap();
            }
            std::fs::write(home.join("config.toml"), format!(
                "[models.roles]\ndefault='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n[providers.p.models.m]\n[session]\ncompress={global}\n"
            )).unwrap();
            let core = Core::new_with_sandbox_support(
                CoreConfig {
                    home,
                    cwd: plain.clone(),
                    session_dir: Some(root.path().join("sessions")),
                    ..Default::default()
                },
                crate::sandbox::SandboxSupport::full_only(),
            )
            .unwrap();
            let server = Server::new((*core).clone());
            let (server_side, client_side) = transport::duplex(65536);
            let connection = server.spawn(server_side);
            let (client, _) = ClientBuilder::new(ClientInfo {
                name: "压缩回归".into(),
                version: "1".into(),
                frontend: "test".into(),
            })
            .connect(client_side)
            .await
            .unwrap();
            let new = |cwd: &Path| SessionNewParams {
                cwd: Some(cwd.to_string_lossy().into_owned()),
                ..Default::default()
            };
            let (plain_info, compressed_info) = tokio::join!(
                client.call::<SessionNew>(new(&plain)),
                client.call::<SessionNew>(new(&compressed))
            );
            for (info, enabled) in [
                (plain_info.unwrap(), false),
                (compressed_info.unwrap(), true),
            ] {
                let record = core.inner.sessions.get(&info.id).await.unwrap();
                assert_eq!(
                    record.paths.compressed(),
                    enabled,
                    "全局={global}，项目={enabled}"
                );
                let entry = core
                    .inner
                    .sessions
                    .append_message(&info.id, Message::user_text("回退目标"), None, None)
                    .await
                    .unwrap();
                let rewind = client
                    .call::<SessionRewind>(SessionRewindParams {
                        session_id: info.id.clone(),
                        entry_id: entry.id().to_owned(),
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    core.inner
                        .sessions
                        .get(&rewind.session.id)
                        .await
                        .unwrap()
                        .paths
                        .compressed(),
                    enabled
                );

                core.inner
                    .sessions
                    .append(
                        &info.id,
                        crate::session::entries::EntryPayload::Plan {
                            plan_id: "plan_test".into(),
                            title: "执行计划".into(),
                            path: "local://plan_test.md".into(),
                            markdown: "测试计划".into(),
                            status: moc_protocol::PlanStatus::Submitted,
                        },
                    )
                    .await
                    .unwrap();
                let plan = client
                    .call::<SessionPlanApprove>(SessionPlanApproveParams {
                        session_id: info.id.clone(),
                        plan_id: "plan_test".into(),
                        action: PlanAction::ExecuteNewSession,
                        execution_model: None,
                    })
                    .await
                    .unwrap();
                assert_eq!(
                    core.inner
                        .sessions
                        .get(&plan.session_id)
                        .await
                        .unwrap()
                        .paths
                        .compressed(),
                    enabled
                );
            }
            // 保存项目配置后只影响以后创建的会话；全局与其它工作区不应被改写。
            let ConfigReply::Settings(view) = client
                .call::<ConfigManage>(ConfigAction::Read {
                    scope: ConfigScope::Project,
                    cwd: compressed.to_string_lossy().into_owned(),
                })
                .await
                .unwrap()
            else {
                panic!("配置视图");
            };
            client
                .call::<ConfigManage>(ConfigAction::Save {
                    scope: ConfigScope::Project,
                    cwd: compressed.to_string_lossy().into_owned(),
                    document: json!({"session":{"compress":false}}),
                    revision: view.revision,
                })
                .await
                .unwrap();
            let next = client.call::<SessionNew>(new(&compressed)).await.unwrap();
            assert!(
                !core
                    .inner
                    .sessions
                    .get(&next.id)
                    .await
                    .unwrap()
                    .paths
                    .compressed()
            );
            client.close().await;
            connection.close();
            connection.wait().await;
            core.shutdown().await;
        }
    }
}

mod bash;
mod btw;
mod context;
mod management;
pub(crate) mod mode;
mod model;
pub(crate) mod prompt;

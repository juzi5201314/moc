//! Core 公开入口、生命周期与 JSON-RPC 服务实现。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moc_llm::Client;
use moc_protocol::methods::{
    AgentListParams, AgentListResult, AgentProfile, AgentSendParams, AgentSendResult,
    AgentSpawnParams, AgentSpawnResult, AgentSteerParams, AgentStopParams, AgentStopResult,
    AgentWaitParams, AgentWaitResult, ClientCapabilities, CompactResult, ConfigGetParams,
    ConfigGetResult, ContextUsageResult, ContextUsageSection, Empty, InitializeParams,
    InitializeResult, ModelInfo, ModelUsageSummary, OpenVikingDeleteParams, OpenVikingDeleteResult,
    OpenVikingListParams, OpenVikingListResult, OpenVikingReadParams, OpenVikingReadResult,
    OpenVikingWriteParams, OpenVikingWriteResult, PromptResult, ResourceCompleteParams,
    ResourceCompleteResult, ServerCapabilities, ServerInfo, SessionCompactParams,
    SessionContextParams, SessionDeleteParams, SessionInfo, SessionListParams, SessionListResult,
    SessionLoadPageParams, SessionLoadPageResult, SessionLoadParams, SessionLoadResult,
    SessionMode, SessionNewParams, SessionPermission, SessionPlanApproveParams,
    SessionPlanApproveResult, SessionRenameParams, SessionRewindParams, SessionSetModeParams,
    SessionSetModelParams, SessionSetModelResult, SessionSetPermissionParams, SessionUnloadParams,
    SessionUnlockParams, SessionUpdate, SessionUsageParams, SessionUsageResult,
};
use moc_protocol::{AgentService, RequestContext, RpcError, RpcErrorCode};
use parking_lot::RwLock;
use serde_json::json;
use tokio::sync::Mutex;

use crate::CoreConfig;
use crate::compact;
use crate::config::{ResolvedConfig, resolve_models};
use crate::error::CoreError;
use crate::session::{SessionTable, TranscriptEntryId};
use crate::tools::{JobManager, ToolRegistry};
use crate::workspace::watch::WorkspaceSnapshot;

/// 已加载会话的临时状态，不写入 JSONL。
pub(crate) struct SessionRuntime {
    pub anchors: Arc<crate::hashline::AnchorStore>,
    pub jobs: Arc<JobManager>,
    pub workspace: Mutex<Option<WorkspaceSnapshot>>,
    pub context_files: Mutex<Option<Arc<Vec<crate::system_prompt::context_files::ContextFile>>>>,
    pub reminders: tokio::sync::Mutex<crate::system_prompt::reminder::ReminderState>,
    pub loaded_skills: Arc<RwLock<crate::skills::LoadedSkills>>,
    pub steering: Mutex<management::Steering>,
    pub guard: parking_lot::Mutex<Option<Arc<prompt::guard::PromptGuard>>>,
}

impl SessionRuntime {
    fn new(slots: Arc<crate::concurrency::Gate>) -> Self {
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
            guard: parking_lot::Mutex::new(None),
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
    pub ptc: tokio::sync::OnceCell<Arc<crate::tools::ptc::runtime::PtcRuntime>>,
    pub capabilities: RwLock<HashMap<u64, ClientCapabilities>>,
    pub runtimes: Mutex<HashMap<String, Arc<SessionRuntime>>>,
    pub project_configs: RwLock<HashMap<PathBuf, Arc<ResolvedConfig>>>,
    /// 每个 cwd（项目）一个 MCP 管理器，与 project_configs 同键。
    pub mcp_managers: Mutex<HashMap<PathBuf, Arc<moc_mcp::McpManager>>>,
    /// 按 canonical workspace 缓存 OpenViking client，空 URL 时保持为空。
    pub openviking_clients: Mutex<HashMap<PathBuf, Option<Arc<moc_openviking::OpenVikingClient>>>>,
    pub skill_indexes: RwLock<HashMap<PathBuf, Arc<crate::skills::SkillIndex>>>,
    pub remote_unavailable: RwLock<std::collections::HashSet<String>>,
    /// 当前进程中的子代理运行状态；持久 transcript 仍是历史真相源。
    pub agents: Mutex<crate::agents::AgentRegistry>,
    pub agent_tasks: Mutex<tokio::task::JoinSet<()>>,
    pub prompts: Mutex<tokio::task::JoinSet<()>>,
    pub shutdown: tokio_util::sync::CancellationToken,
    pub bwrap_available: bool,
}

impl CoreInner {
    pub(crate) fn workspace_sandbox_available(&self) -> bool {
        self.bwrap_available
    }

    pub(crate) fn effective_permission(&self, permission: SessionPermission) -> SessionPermission {
        permission
    }
    pub(crate) fn config(&self) -> Arc<ResolvedConfig> {
        Arc::clone(&self.config.read())
    }

    pub(crate) fn workspace_identity(
        &self,
        cwd: &Path,
    ) -> Result<moc_openviking::WorkspaceIdentity, CoreError> {
        crate::workspace::registry::resolve(self.project_config(cwd)?.as_ref(), cwd)
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
    pub(crate) async fn project_openviking(
        &self,
        cwd: &Path,
    ) -> Result<Option<Arc<moc_openviking::OpenVikingClient>>, CoreError> {
        let key = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        if let Some(client) = self.openviking_clients.lock().await.get(&key).cloned() {
            return Ok(client);
        }
        let config = self.project_config(&key)?.openviking.clone();
        let client = moc_openviking::OpenVikingClient::new(config)
            .map_err(|error| CoreError::Config(error.to_string()))?
            .map(Arc::new);
        self.openviking_clients
            .lock()
            .await
            .insert(key, client.clone());
        Ok(client)
    }
    pub(crate) fn openviking_failure_key(url: &str, peer: &str) -> String {
        format!("{url}\n{peer}")
    }

    pub(crate) fn openviking_should_notice(&self, key: &str) -> bool {
        self.remote_unavailable.write().insert(key.to_owned())
    }

    pub(crate) fn openviking_mark_success(&self, key: &str) {
        self.remote_unavailable.write().remove(key);
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
        Arc::clone(
            runtimes
                .entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(SessionRuntime::new(Arc::clone(&self.job_slots)))),
        )
    }
    pub(crate) async fn reset_loaded_skills(&self, session_id: &str) {
        if let Some(runtime) = self.runtimes.lock().await.get(session_id).cloned() {
            runtime.loaded_skills.write().clear();
        }
    }

    pub(crate) fn client_supports_ask(&self, connection_id: u64) -> bool {
        self.capabilities
            .read()
            .get(&connection_id)
            .is_some_and(|caps| caps.ask_requests)
    }

    /// 取得（或惰性创建）该项目的 MCP 管理器；仅提示执行显式启动连接。
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
        let catalog = self
            .tools
            .catalog(&snapshot, manager)
            .map_err(|err| CoreError::Tool(err.to_string()))?;
        Ok(catalog.summary())
    }

    /// 取得（或惰性扫描）该项目的 skill 索引。
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

fn probe_bwrap() -> bool {
    #[cfg(unix)]
    {
        std::process::Command::new("bwrap")
            .args([
                "--die-with-parent",
                "--new-session",
                "--unshare-user",
                "--unshare-pid",
                "--unshare-ipc",
                "--unshare-uts",
                "--disable-userns",
                "--ro-bind",
                "/",
                "/",
                "--",
                "true",
            ])
            .status()
            .is_ok_and(|status| status.success())
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// 通过协议 [`AgentService`] 暴露的完整代理运行时。
#[derive(Clone)]
pub struct Core {
    pub(crate) inner: Arc<CoreInner>,
}

impl Core {
    pub fn config_home(&self) -> PathBuf {
        self.inner.config().home.clone()
    }
    /// 加载配置、验证供应商并建立会话存储目录。
    pub fn new(config: CoreConfig) -> anyhow::Result<Arc<Self>> {
        let mut resolved = ResolvedConfig::load(&config)?;
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
                ptc: tokio::sync::OnceCell::new(),
                capabilities: RwLock::new(HashMap::new()),
                runtimes: Mutex::new(HashMap::new()),
                project_configs: RwLock::new(HashMap::new()),
                mcp_managers: Mutex::new(HashMap::new()),
                openviking_clients: Mutex::new(HashMap::new()),
                skill_indexes: RwLock::new(HashMap::new()),
                remote_unavailable: RwLock::new(std::collections::HashSet::new()),
                agents: Mutex::new(crate::agents::AgentRegistry::default()),
                agent_tasks: Mutex::new(tokio::task::JoinSet::new()),
                prompts: Mutex::new(tokio::task::JoinSet::new()),
                shutdown: tokio_util::sync::CancellationToken::new(),
                bwrap_available: probe_bwrap(),
            }),
        }))
    }

    /// 等待提示取消收尾，终止后台进程组与 MCP 连接并释放临时状态。
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let mut agents = std::mem::take(&mut *self.inner.agent_tasks.lock().await);
        while let Some(result) = agents.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "子代理收尾任务失败");
            }
        }
        let mut prompts = std::mem::take(&mut *self.inner.prompts.lock().await);
        while let Some(result) = prompts.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "提示收尾任务失败");
            }
        }
        let sessions = match self.inner.sessions.list().await {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::debug!(%error, "读取会话列表失败，跳过 OpenViking commit");
                Vec::new()
            }
        };
        for session in sessions {
            self.commit_openviking_for(Path::new(&session.cwd), &session.id)
                .await;
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
        self.inner.runtimes.lock().await.clear();
    }

    /// 返回配置中的模型列表，供 CLI 展示与诊断。
    pub fn models(config: &CoreConfig) -> anyhow::Result<Vec<crate::ResolvedModel>> {
        resolve_models(config)
    }
    /// 撤销当前会话最近一次文件编辑。
    pub async fn undo(&self, session_id: &str) -> anyhow::Result<String> {
        let record = self.inner.sessions.get(session_id).await?;
        let _prompt = record.try_prompt_lock()?;
        let _file_lock = self
            .inner
            .sessions
            .acquire_lock(session_id, "moc-cli")
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
    async fn openviking_client_for(
        &self,
        cwd: &Path,
    ) -> Result<Option<Arc<moc_openviking::OpenVikingClient>>, RpcError> {
        self.inner.project_openviking(cwd).await.map_err(to_rpc)
    }

    async fn commit_openviking_for(&self, cwd: &Path, session_id: &str) {
        let Ok(Some(client)) = self.openviking_client_for(cwd).await else {
            return;
        };
        let Ok(identity) = self.inner.workspace_identity(cwd) else {
            return;
        };
        let _ = client
            .commit_session(&openviking_session_id(session_id), &identity.peer_id)
            .await;
    }

    async fn openviking_client(&self) -> Result<Arc<moc_openviking::OpenVikingClient>, RpcError> {
        self.openviking_client_for(&self.inner.config().cwd)
            .await?
            .ok_or_else(|| {
                RpcError::new(RpcErrorCode::NotSupported.code(), "未配置 OpenViking URL")
            })
    }

    fn openviking_identity(&self) -> Result<moc_openviking::WorkspaceIdentity, RpcError> {
        self.inner
            .workspace_identity(&self.inner.config().cwd)
            .map_err(to_rpc)
    }
    /// 由 Core 持有长操作，前端断线后仍完成进程回收与会话提交。
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

/// 把一次已完成的模型请求写入会话统计；统计 entry 不进入模型 transcript。
pub(crate) async fn record_model_usage(
    inner: &CoreInner,
    session_id: &str,
    model: &str,
    usage: moc_llm::Usage,
    active_ms: u64,
) -> Result<(), CoreError> {
    if let Some(guard) = inner.runtime(session_id).await.guard.lock().clone() {
        guard.tokens(usage.output_tokens);
    }
    inner
        .sessions
        .append(
            session_id,
            crate::session::entries::EntryPayload::Usage {
                model: model.to_owned(),
                usage,
                active_ms,
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

    async fn agent_spawn(
        &self,
        ctx: RequestContext,
        params: AgentSpawnParams,
    ) -> Result<AgentSpawnResult, RpcError> {
        crate::agents::spawn(
            Arc::clone(&self.inner),
            ctx,
            params.parent_session_id,
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
        crate::agents::list(&self.inner, params.parent_session_id)
            .await
            .map_err(to_rpc)
    }

    async fn agent_send(
        &self,
        ctx: RequestContext,
        params: AgentSendParams,
    ) -> Result<AgentSendResult, RpcError> {
        crate::agents::send(
            Arc::clone(&self.inner),
            ctx,
            params.parent_session_id,
            params.agent_id,
            params.message,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_steer(
        &self,
        _ctx: RequestContext,
        params: AgentSteerParams,
    ) -> Result<moc_protocol::SteerResult, RpcError> {
        crate::agents::steer(
            &self.inner,
            params.parent_session_id,
            params.agent_id,
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
            params.parent_session_id,
            params.agent_id,
            params.reason,
        )
        .await
        .map_err(to_rpc)
    }

    async fn agent_wait(
        &self,
        _ctx: RequestContext,
        params: AgentWaitParams,
    ) -> Result<AgentWaitResult, RpcError> {
        crate::agents::wait(
            &self.inner,
            params.parent_session_id,
            params.agent_id,
            params.timeout_ms,
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
    async fn openviking_list(
        &self,
        _ctx: RequestContext,
        params: OpenVikingListParams,
    ) -> Result<OpenVikingListResult, RpcError> {
        let client = self.openviking_client().await?;
        let identity = self.openviking_identity()?;
        let uri = moc_openviking::OpenVikingUri::parse(&params.uri).map_err(openviking_rpc)?;
        let entries = client
            .list(
                uri.as_str(),
                Some(&identity.peer_id),
                params.recursive,
                params.limit,
            )
            .await
            .map_err(openviking_rpc)?;
        Ok(OpenVikingListResult {
            uri: uri.as_str().to_owned(),
            entries: entries
                .into_iter()
                .map(|entry| moc_protocol::methods::OpenVikingEntry {
                    name: entry.name,
                    uri: entry.uri,
                    is_dir: entry.is_dir,
                    abstract_text: entry.abstract_text,
                })
                .collect(),
        })
    }

    async fn openviking_read(
        &self,
        _ctx: RequestContext,
        params: OpenVikingReadParams,
    ) -> Result<OpenVikingReadResult, RpcError> {
        let client = self.openviking_client().await?;
        let identity = self.openviking_identity()?;
        let uri = moc_openviking::OpenVikingUri::parse(&params.uri).map_err(openviking_rpc)?;
        let document = client
            .read(
                uri.as_str(),
                Some(&identity.peer_id),
                params.offset,
                params.limit,
            )
            .await
            .map_err(openviking_rpc)?;
        Ok(OpenVikingReadResult {
            uri: document.uri,
            content: document.content,
        })
    }

    async fn openviking_write(
        &self,
        _ctx: RequestContext,
        params: OpenVikingWriteParams,
    ) -> Result<OpenVikingWriteResult, RpcError> {
        if params.mode != "replace" {
            return Err(invalid_params("OpenViking 仅支持 mode=replace"));
        }
        let client = self.openviking_client().await?;
        let identity = self.openviking_identity()?;
        let uri = moc_openviking::OpenVikingUri::parse(&params.uri).map_err(openviking_rpc)?;
        let result = client
            .write(uri.as_str(), &params.content, Some(&identity.peer_id))
            .await
            .map_err(openviking_rpc)?;
        Ok(OpenVikingWriteResult {
            uri: uri.as_str().to_owned(),
            result,
        })
    }

    async fn openviking_delete(
        &self,
        _ctx: RequestContext,
        params: OpenVikingDeleteParams,
    ) -> Result<OpenVikingDeleteResult, RpcError> {
        let client = self.openviking_client().await?;
        let identity = self.openviking_identity()?;
        let uri = moc_openviking::OpenVikingUri::parse(&params.uri).map_err(openviking_rpc)?;
        let result = client
            .delete(uri.as_str(), Some(&identity.peer_id), params.recursive)
            .await
            .map_err(openviking_rpc)?;
        Ok(OpenVikingDeleteResult {
            uri: uri.as_str().to_owned(),
            result,
        })
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
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        let skills = self.inner.project_skills(&cwd).map_err(to_rpc)?;
        let context = crate::resources::ResourceContext {
            session_id: &params.session_id,
            cwd: &cwd,
            paths: &record.paths,
            skills: &skills,
            sessions: &self.inner.sessions,
            openviking_configured: !config.openviking.url.trim().is_empty(),
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
        _ctx: RequestContext,
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
        let mut info = self
            .inner
            .sessions
            .create(
                &cwd,
                &model.selector,
                params.mode,
                SessionPermission::Full,
                params.title.as_deref(),
                None,
            )
            .await
            .map_err(to_rpc)?;
        // 以计划模式创建会话：header 记录会话自身模型，随后立即覆盖为计划模型，
        // 退出计划模式时再恢复。
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
        _ctx: RequestContext,
        params: SessionLoadParams,
    ) -> Result<SessionLoadResult, RpcError> {
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
        let _file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let mut loaded = self
            .inner
            .sessions
            .rewind(&params.session_id, &entry_id)
            .await
            .map_err(to_rpc)?;
        loaded.session.lock = None;
        loaded.session.permission = self.inner.effective_permission(loaded.session.permission);
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
        // unload_locked 要求调用者持有提示锁：在飞提示会因运行态被摘除而 panic 或写入失败，
        // 这里还会先杀掉提示正在使用的后台任务。与其他改动类 RPC 一致，先取锁或返回 SESSION_BUSY。
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
        let runtime = self.inner.runtime(&params.session_id).await;
        runtime.jobs.kill_all().await;
        self.commit_openviking_for(&cwd, &params.session_id).await;
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
        let file_lock = self
            .inner
            .sessions
            .acquire_lock(&session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let mut info = self
            .inner
            .sessions
            .rename(&session_id, params)
            .await
            .map_err(to_rpc)?;
        drop(file_lock);
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
        let _file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, "moc-core")
            .await
            .map_err(to_rpc)?;
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
        self.inner
            .sessions
            .unlock(&params.session_id, params.force)
            .await
            .map_err(to_rpc)?;
        Ok(Empty {})
    }

    async fn session_set_permission(
        &self,
        ctx: RequestContext,
        params: SessionSetPermissionParams,
    ) -> Result<SessionInfo, RpcError> {
        if params.permission == SessionPermission::Workspace
            && !self.inner.workspace_sandbox_available()
        {
            return Err(invalid_params("当前环境不可用 bwrap，只有 full 权限可用"));
        }
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let _prompt = record.try_prompt_lock().map_err(to_rpc)?;
        let file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let mut info = self
            .inner
            .sessions
            .set_permission(&params.session_id, params.permission)
            .await
            .map_err(to_rpc)?;
        drop(file_lock);
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
        let file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let (cwd, from) = {
            let state = record.state.lock().await;
            (PathBuf::from(state.session.cwd.clone()), state.mode)
        };
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        // 进入计划模式需要先解析计划模型：解析失败不能留下已切模式、未切模型的半状态。
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
        drop(file_lock);
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
        mode::emit_mode_change(&ctx, &params.session_id, info.mode, switched).await?;
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
        let cwd = record.state.lock().await.session.cwd.clone();
        let config = self
            .inner
            .project_config(std::path::Path::new(&cwd))
            .map_err(to_rpc)?;
        let model = config.resolve_model(&params.selector).map_err(to_rpc)?;
        let file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let outcome = model::switch(
            &self.inner,
            &ctx,
            &params.session_id,
            &model,
            params.overflow,
        )
        .await
        .map_err(to_rpc)?;
        drop(file_lock);
        let mut session = outcome.session;
        session.lock = None;
        session.permission = self.inner.effective_permission(session.permission);
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
        mode::approve_plan(&self.inner, ctx, params).await
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
        let _file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
        let result = compact::manual(
            &self.inner,
            &ctx,
            &params.session_id,
            params.instructions.as_deref(),
            None,
        )
        .await
        .map_err(to_rpc)?;
        self.commit_openviking_for(&cwd, &params.session_id).await;
        Ok(result)
    }

    async fn session_context(
        &self,
        ctx: RequestContext,
        params: SessionContextParams,
    ) -> Result<ContextUsageResult, RpcError> {
        let record = self
            .inner
            .sessions
            .get(&params.session_id)
            .await
            .map_err(to_rpc)?;
        let state = record.state.lock().await.clone();
        let cwd = PathBuf::from(&state.session.cwd);
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        let model = config.resolve_model(&state.model).map_err(to_rpc)?;
        let skills = self.inner.project_skills(&cwd).map_err(to_rpc)?;
        let context_files = self
            .inner
            .project_context_files(&params.session_id, &cwd)
            .await;
        let summary = self.inner.mcp_summary(&config).await.map_err(to_rpc)?;
        let pieces =
            crate::system_prompt::pieces(&config, &model, &cwd, &context_files, &skills, &summary);
        let system = crate::system_prompt::compose(&pieces);
        let synthetic = crate::compact::replay::synthetic_messages(&state, Some(system.clone()));
        let synthetic_tokens = crate::compact::messages_tokens(&synthetic[1..]);
        let visible_start = crate::compact::replay::visible_history_start(&state);
        let history_tokens = synthetic_tokens.saturating_add(
            state.transcript_token_counts[visible_start..]
                .iter()
                .copied()
                .sum::<u64>(),
        );
        let tools = self.inner.tools.definitions(
            state.mode,
            self.inner.client_supports_ask(ctx.connection_id()),
            &config,
            true,
        );
        let system_tokens = crate::compact::messages_tokens(std::slice::from_ref(&system));
        let directory_tokens = pieces
            .iter()
            .filter(|piece| piece.title == "tools")
            .map(|piece| crate::compact::estimate_tokens(&piece.text))
            .sum::<u64>();
        let tool_tokens = crate::compact::tools_tokens(&tools).saturating_add(directory_tokens);
        let total_tokens = system_tokens
            .saturating_add(crate::compact::tools_tokens(&tools))
            .saturating_add(history_tokens);
        let mut project_tokens = 0u64;
        let mut skills_tokens = 0u64;
        for piece in &pieces {
            match piece.title {
                "project" => {
                    project_tokens =
                        project_tokens.saturating_add(crate::compact::estimate_tokens(&piece.text));
                }
                "skills" => {
                    skills_tokens =
                        skills_tokens.saturating_add(crate::compact::estimate_tokens(&piece.text));
                }
                _ => {}
            }
        }
        let system_base_tokens = system_tokens
            .saturating_sub(project_tokens)
            .saturating_sub(skills_tokens)
            .saturating_sub(directory_tokens);

        let mut sections = Vec::with_capacity(5);
        sections.push(ContextUsageSection {
            name: "system".into(),
            tokens: system_base_tokens,
        });
        if project_tokens > 0 {
            sections.push(ContextUsageSection {
                name: "project".into(),
                tokens: project_tokens,
            });
        }
        if skills_tokens > 0 {
            sections.push(ContextUsageSection {
                name: "skills".into(),
                tokens: skills_tokens,
            });
        }
        sections.push(ContextUsageSection {
            name: "tools".into(),
            tokens: tool_tokens,
        });
        sections.push(ContextUsageSection {
            name: "messages".into(),
            tokens: history_tokens,
        });
        Ok(ContextUsageResult {
            context_window: model.context_window,
            total_tokens,
            sections,
        })
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
            models.push(ModelUsageSummary {
                model: model.clone(),
                calls: summary.calls,
                input_tokens: summary.usage.input_tokens,
                output_tokens: summary.usage.output_tokens,
                cached_input_tokens: summary.usage.cached_input_tokens,
                cache_creation_input_tokens: summary.usage.cache_creation_input_tokens,
                reasoning_tokens: summary.usage.reasoning_tokens,
                active_ms: summary.active_ms,
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
            },
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
fn openviking_rpc(error: moc_openviking::OpenVikingError) -> RpcError {
    let code = match &error {
        moc_openviking::OpenVikingError::Disabled => RpcErrorCode::NotSupported,
        moc_openviking::OpenVikingError::InvalidUri(_) => RpcErrorCode::InvalidParams,
        moc_openviking::OpenVikingError::HttpStatus { .. }
        | moc_openviking::OpenVikingError::Server { .. } => RpcErrorCode::InternalError,
        moc_openviking::OpenVikingError::InvalidJson(_)
        | moc_openviking::OpenVikingError::Timeout
        | moc_openviking::OpenVikingError::Transport(_)
        | moc_openviking::OpenVikingError::PendingIo(_) => RpcErrorCode::InternalError,
    };
    RpcError::new(code.code(), error.to_string())
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

fn openviking_session_id(session_id: &str) -> String {
    format!(
        "moc-{}",
        session_id.replace(
            |character: char| !character.is_ascii_alphanumeric()
                && character != '-'
                && character != '_',
            "-"
        )
    )
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

mod bash;
mod management;
pub(crate) mod mode;
mod model;
pub(crate) mod prompt;

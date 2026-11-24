use super::Core;
use super::CoreInner;
use super::CoreServer;
use super::context;
use super::history;
use super::mode;
use crate::CoreConfig;
use crate::config::ResolvedConfig;
use crate::config::resolve_models;
use crate::service::invalid_params;
use crate::service::plugin_tools_of;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use crate::session::SessionTable;
use crate::tools::ToolRegistry;
use moc_i18n::t;
use moc_protocol::RequestContext;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use moc_protocol::methods::SessionUpdate;
use parking_lot::RwLock;
use std::collections::HashMap;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

impl CoreServer {
    /// 建立独立的内嵌连接；容量和协议选项仍由前端决定。
    #[inline(never)]
    pub fn connect(
        &self,
        capacity: usize,
    ) -> (tokio::io::DuplexStream, moc_protocol::ServerConnection) {
        // 具体传输的实例化留在本 crate，不随前端重新生成服务分发和 Core 状态机。
        let (core_side, frontend_side) = moc_protocol::transport::duplex(capacity);
        let connection = self.server.spawn(core_side);
        (frontend_side, connection)
    }

    /// 取消本服务端的全部连接，保持协议层原有的收尾语义。
    pub fn shutdown(&self) {
        self.server.shutdown();
    }
}

impl Core {
    /// 创建前端服务端；同一服务端的多条连接共享连接编号与关闭令牌。
    pub fn server(&self, options: moc_protocol::ServerOptions) -> CoreServer {
        CoreServer {
            server: moc_protocol::Server::with_options(self.clone(), options),
        }
    }

    /// 截断历史后被标孤儿的子代理：还在跑的停掉并广播状态。
    ///
    /// rewind/reask 共用——孤儿代理的收尾投递会污染截断后的历史、也会作废
    /// 前滚尾部；redo 接回引用后标记摘除，但已停的代理不会复活。
    pub(super) async fn stop_orphaned_agents(&self, ctx: &RequestContext, session_id: &str) {
        let orphaned = self.inner.sessions.orphan_marks(session_id).await;
        if orphaned.is_empty() {
            return;
        }
        let stopped = {
            let mut registry = self.inner.agents.lock().await;
            let mut stopped = Vec::new();
            for runtime in registry.items.values_mut() {
                if runtime.info.parent_id == session_id
                    && crate::agents::live(runtime.info.status)
                    && orphaned.contains(&runtime.info.id)
                {
                    runtime.cancel.cancel();
                    runtime.info.status = moc_protocol::methods::AgentStatus::Stopped;
                    stopped.push(runtime.info.clone());
                }
            }
            stopped
        };
        // 与 finish 的正常收尾同样推送状态变化，前端能立刻看到停杀结果。
        for info in stopped {
            let _ = ctx
                .updates()
                .emit(
                    session_id.to_owned(),
                    SessionUpdate::AgentStatusChanged {
                        agent_id: info.id,
                        status: info.status,
                        last_message: info.last_message,
                    },
                )
                .await;
        }
    }

    pub fn config_home(&self) -> PathBuf {
        self.inner.config().home.clone()
    }

    /// 当前全局生效配置的只读快照（供进程内 Web 扩展模块读取模型列表与连接信息）。
    pub fn config_snapshot(&self) -> Arc<ResolvedConfig> {
        self.inner.config()
    }

    /// 进程内复用的 LLM HTTP 客户端。
    pub fn llm_client(&self) -> moc_llm::Client {
        self.inner.client.clone()
    }

    /// 当前平台的沙盒能力；前端据此只展示可用的权限模式。
    pub fn sandbox_support(&self) -> crate::sandbox::SandboxSupport {
        self.inner.sandbox_support.clone()
    }

    /// 会话的上传目录（[`crate::session::store::SessionPaths::uploads`]）；会话不存在时报错。
    pub async fn session_uploads_dir(&self, session_id: &str) -> Result<PathBuf, crate::CoreError> {
        Ok(self.inner.sessions.get(session_id).await?.paths.uploads())
    }

    /// 特性开关有效状态（`[features]` 覆盖合成后）。
    pub fn features(&self) -> moc_protocol::features::FeatureStates {
        self.inner.config().features
    }

    /// resource/* 方法共用的上下文三件套：会话记录、cwd 与会话级 skill 索引。
    pub(super) async fn resource_context(
        &self,
        session_id: &str,
    ) -> Result<
        (
            Arc<crate::session::SessionRecord>,
            PathBuf,
            Arc<crate::skills::SkillIndex>,
        ),
        RpcError,
    > {
        let record = self.inner.sessions.get(session_id).await.map_err(to_rpc)?;
        let cwd = PathBuf::from(&record.state.lock().await.session.cwd);
        let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
        let workflows_active = {
            let state = record.state.lock().await;
            crate::workflow::workflows_active(&state, &config.workflow)
        };
        let skills = self
            .inner
            .session_skills(&cwd, workflows_active)
            .map_err(to_rpc)?;
        Ok((record, cwd, skills))
    }

    /// 只有工具文档请求才准备目录，普通文件读取不触发 MCP/插件发现。
    pub(super) async fn resource_tools(
        &self,
        ctx: &RequestContext,
        record: &crate::session::SessionRecord,
        uri: &str,
    ) -> Result<Option<crate::tools::documentation::ToolDocumentation>, RpcError> {
        if !uri.starts_with("tool://") {
            return Ok(None);
        }
        let metadata = record.state.lock().await.metadata();
        let cwd = Path::new(&metadata.session.cwd);
        let config = self.inner.project_config(cwd).map_err(to_rpc)?;
        let plugins = self.inner.project_plugins(cwd).await.map_err(to_rpc)?;
        let scope = crate::tools::ToolScope::for_metadata(
            &metadata,
            &config,
            crate::agents::roles::descriptions(&self.inner, &config, &plugins),
        );
        let manager = self.inner.project_mcp(cwd).await.map_err(to_rpc)?;
        let snapshot = manager.snapshot().await;
        let catalog = self
            .inner
            .tools
            .catalog(&snapshot, manager, plugin_tools_of(&plugins), scope)
            .map_err(|error| invalid_params(error.to_string()))?;
        Ok(Some(crate::tools::documentation::ToolDocumentation {
            catalog: Arc::new(catalog),
            config,
            engine: metadata.engine,
            mode: mode::Mode::effective(metadata.plan_active, None),
            ask_supported: self
                .inner
                .ask_available(ctx.connection_id(), metadata.session.autonomy),
        }))
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
        // 进程级语言初始化由前端入口（moc-cli）在启动 Core 后调用，避免库构造在并发测试中竞争全局 locale。
        if resolved.sandbox.default_mode != moc_protocol::SessionPermission::Full
            && !sandbox_support.supports(resolved.sandbox.default_mode)
        {
            anyhow::bail!(
                "{}",
                moc_i18n::t!(
                    "core.err.sandbox_mode_unavailable",
                    mode = resolved.sandbox.default_mode.as_str()
                )
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
                client: config.client,
                context_views: parking_lot::Mutex::new(context::Views::default()),
                history_transfers: parking_lot::Mutex::new(history::Transfers::default()),
                sessions,
                tools: ToolRegistry::new(),
                capabilities: RwLock::new(HashMap::new()),
                runtimes: Mutex::new(HashMap::new()),
                project_configs: RwLock::new(HashMap::new()),
                mcp_managers: Mutex::new(HashMap::new()),
                mcp_config_errors: RwLock::new(HashMap::new()),
                mcp_status_sinks: Arc::new(parking_lot::Mutex::new(HashMap::new())),
                mcp_watchers: Mutex::new(tokio::task::JoinSet::new()),
                skill_indexes: RwLock::new(HashMap::new()),
                plugin_hosts: Mutex::new(HashMap::new()),
                ptc_host: Mutex::new(None),
                agents: Mutex::new(crate::agents::AgentRegistry::default()),
                agent_presets: crate::agents::PresetRegistry::with_builtins(),
                agent_tasks: Mutex::new(tokio::task::JoinSet::new()),
                prompts: Mutex::new(tokio::task::JoinSet::new()),
                shutdown: tokio_util::sync::CancellationToken::new(),
                sandbox_support,
                web_state_sinks: parking_lot::Mutex::new(HashMap::new()),
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
        let managers: Vec<_> = std::mem::take(&mut *self.inner.mcp_managers.lock().await)
            .into_values()
            .collect();
        futures_util::future::join_all(managers.into_iter().map(|manager| async move {
            manager.shutdown().await;
        }))
        .await;
        let mut watchers = std::mem::take(&mut *self.inner.mcp_watchers.lock().await);
        while let Some(result) = watchers.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "MCP 状态监听任务异常退出");
            }
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
        self.inner.sessions.clear_history_views();
    }

    /// 返回配置中的语言设置（`auto`/`zh`/`en`），供前端入口初始化进程 locale。
    pub fn language(&self) -> String {
        self.inner.config.read().language.clone()
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
            .ok_or_else(|| anyhow::anyhow!("{}", t!("core.err.no_undo")))?;
        let output = crate::hashline::apply::undo(
            &path,
            &runtime.anchors,
            config.tools.output.max_file_bytes,
        )
        .await?;
        runtime.anchors.flush().await?;
        Ok(output)
    }
    /// 把长操作挂在 Core 上运行，前端断开后也能完成进程回收和会话提交。
    pub(super) async fn run_operation<T: Send + 'static>(
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

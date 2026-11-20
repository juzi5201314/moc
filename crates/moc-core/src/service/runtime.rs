use super::CoreInner;
use super::SessionRuntime;
use super::management;
use super::plugin_persistence;
use super::prompt;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::plugin_tools_of;
use crate::tools::JobManager;
use moc_protocol::RequestContext;
use moc_protocol::methods::SessionAutonomy;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionPermission;
use parking_lot::RwLock;
use std::path::Path;
use std::sync::Arc;
use tokio::sync::Mutex;

impl SessionRuntime {
    pub(super) fn new(support: &crate::sandbox::SandboxSupport) -> Self {
        let scratch = support
            .runner
            .and_then(|_| crate::sandbox::SandboxScratch::new().ok())
            .map(Arc::new);
        Self {
            anchors: Arc::new(crate::hashline::AnchorStore::new()),
            jobs: Arc::new(JobManager::new()),
            context_files: Mutex::new(None),
            reminders: tokio::sync::Mutex::new(
                crate::system_prompt::reminder::ReminderState::default(),
            ),
            loaded_skills: Arc::new(RwLock::new(std::collections::HashSet::new())),
            steering: Mutex::new(management::Steering::default()),
            agent_inbox: Mutex::new(crate::agents::AgentInbox::default()),
            guard: parking_lot::Mutex::new(None),
            goal_armed: crate::goal::GoalArmed::new(),
            goal_active_round: parking_lot::Mutex::new(None),
            goal_op: tokio::sync::Mutex::new(()),
            grok_prefire: crate::compact::grok_build::prefire::PrefireState::default(),
            sandbox_grants: Arc::new(RwLock::new(crate::sandbox::SandboxGrants::default())),
            sandbox_scratch: scratch,
            workflows: tokio::sync::OnceCell::new(),
        }
    }
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
        Arc::clone(
            runtimes
                .entry(session_id.to_owned())
                .or_insert_with(|| Arc::new(SessionRuntime::new(&self.sandbox_support))),
        )
    }
    pub(crate) async fn reset_loaded_skills(&self, session_id: &str) {
        if let Some(runtime) = self.runtimes.lock().await.get(session_id).cloned() {
            runtime.loaded_skills.write().clear();
        }
    }

    /// 在后台跑一次提示：任务挂在 `prompts` JoinSet 上，结果只记日志。
    ///
    /// 用于代理消息、后台任务唤醒顶层会话与目标续跑。和 [`Core::run_operation`]
    /// 的区别：不返回结果（前端通过更新通知观察），并且调用方已经持有该会话的提示锁。
    pub(crate) fn spawn_detached_prompt(
        inner: &Arc<CoreInner>,
        ctx: RequestContext,
        params: moc_protocol::methods::PromptParams,
        record: Arc<crate::session::SessionRecord>,
        prompt_lock: tokio::sync::OwnedMutexGuard<()>,
        run: prompt::PromptRun,
    ) -> futures_util::future::BoxFuture<'_, ()> {
        Box::pin(async move {
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
                    run,
                    true,
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
        })
    }

    pub(crate) fn client_supports_ask(&self, connection_id: u64) -> bool {
        self.capabilities
            .read()
            .get(&connection_id)
            .is_some_and(|caps| caps.ask_requests)
    }

    /// ask 工具可用：前端能转达问题（`ask_requests` 能力），或自主程度让 core
    /// 自己代答（Don't Ask 及以上——提问根本不会送到前端）。
    pub(crate) fn ask_available(&self, connection_id: u64, autonomy: SessionAutonomy) -> bool {
        self.client_supports_ask(connection_id) || autonomy >= SessionAutonomy::DontAsk
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
            .catalog(&snapshot, manager, plugins, crate::tools::ToolScope::top())
            .map_err(|err| CoreError::Tool(err.to_string()))?;
        Ok(catalog.summary())
    }

    /// 渲染角色目录与 PTC 工具声明；压缩、预览和实际请求使用相同的会话权限快照。
    pub(crate) async fn tool_declarations(
        &self,
        engine: SessionEngine,
        config: &ResolvedConfig,
        ask_supported: bool,
        scope: crate::tools::ToolScope,
    ) -> Result<String, CoreError> {
        let manager = self.project_mcp(&config.cwd).await?;
        let snapshot = manager.snapshot().await;
        let plugins = self.plugin_tools(&config.cwd).await;
        let workflows_active = scope.workflows;
        let catalog = self
            .tools
            .catalog(&snapshot, manager, plugins, scope)
            .map_err(|err| CoreError::Tool(err.to_string()))?;
        Ok(crate::tools::declarations::render_signatures(
            &catalog,
            engine,
            ask_supported,
            workflows_active,
            config,
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
        let mut discovered = crate::skills::discovery::discover_index(
            cwd,
            &config.home,
            config.user_home.as_deref(),
            &config.skills,
        );
        crate::skills::cache::Cache::load(&config.home)?.apply(&mut discovered);
        let index = Arc::new(discovered);
        self.skill_indexes.write().insert(key, Arc::clone(&index));
        Ok(index)
    }

    /// 会话级 skill 索引：`workflows_active` 时注入内置 `workflow-authoring`
    ///（只在激活会话的 `<skills>` 目录出现；不激活原样返回——零泄漏）。
    pub(crate) fn session_skills(
        &self,
        cwd: &Path,
        workflows_active: bool,
    ) -> Result<Arc<crate::skills::SkillIndex>, CoreError> {
        let index = self.project_skills(cwd)?;
        if !workflows_active {
            return Ok(index);
        }
        let mut cloned = (*index).clone();
        let config = self.project_config(cwd)?;
        crate::workflow::skill::inject(&mut cloned, &config.home);
        crate::skills::cache::Cache::load(&config.home)?.apply(&mut cloned);
        Ok(Arc::new(cloned))
    }

    async fn cached_context_files(
        &self,
        session_id: &str,
        cwd: &Path,
        take_warning: bool,
    ) -> super::CachedContextFiles {
        let runtime = self.runtime(session_id).await;
        let mut slot = runtime.context_files.lock().await;
        if let Some(cached) = slot.as_mut() {
            let warning = if take_warning {
                cached.warning.take()
            } else {
                cached.warning.clone()
            };
            return super::CachedContextFiles {
                files: Arc::clone(&cached.files),
                warning,
            };
        }
        let config = self.project_config(cwd).expect("project config is loaded");
        let report = if config.context_files.enabled {
            crate::system_prompt::context_files::load_context_files_report(
                cwd,
                &config.home,
                &config.context_files,
            )
        } else {
            crate::system_prompt::context_files::ContextFilesReport::default()
        };
        let files = Arc::new(report.files);
        let stored_warning = if take_warning {
            None
        } else {
            report.warning.clone()
        };
        *slot = Some(super::CachedContextFiles {
            files: Arc::clone(&files),
            warning: stored_warning,
        });
        super::CachedContextFiles {
            files,
            warning: report.warning,
        }
    }

    pub(crate) async fn project_context_files(
        &self,
        session_id: &str,
        cwd: &Path,
    ) -> Arc<Vec<crate::system_prompt::context_files::ContextFile>> {
        self.cached_context_files(session_id, cwd, false)
            .await
            .files
    }

    pub(crate) async fn prompt_context_files(
        &self,
        session_id: &str,
        cwd: &Path,
    ) -> (
        Arc<Vec<crate::system_prompt::context_files::ContextFile>>,
        Option<String>,
    ) {
        let cached = self.cached_context_files(session_id, cwd, true).await;
        (cached.files, cached.warning)
    }
}

//! Core 公开入口、生命周期与 JSON-RPC 服务实现。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moc_llm::Client;
use moc_protocol::methods::{
    AgentInfo, ClientCapabilities, CompactResult, ConfigGetParams, ConfigGetResult, Empty,
    InitializeParams, InitializeResult, ModelInfo, ServerCapabilities, ServerInfo,
    SessionCompactParams, SessionDeleteParams, SessionInfo, SessionListParams, SessionListResult,
    SessionLoadParams, SessionLoadResult, SessionMode, SessionNewParams, SessionPlanApproveParams,
    SessionPlanApproveResult, SessionRenameParams, SessionSetModeParams, SessionUnloadParams,
    SessionUnlockParams, SessionUpdate, TurnResult,
};
use moc_protocol::{AgentService, RequestContext, RpcError, RpcErrorCode};
use parking_lot::RwLock;
use serde_json::json;
use tokio::sync::Mutex;

use crate::CoreConfig;
use crate::compact;
use crate::config::{ResolvedConfig, resolve_models};
use crate::error::CoreError;
use crate::session::SessionTable;
use crate::tools::{JobManager, ToolRegistry};
use crate::workspace::watch::WorkspaceSnapshot;

/// 已加载会话的临时状态，不写入 JSONL。
pub(crate) struct SessionRuntime {
    pub anchors: Arc<crate::hashline::AnchorStore>,
    pub jobs: Arc<JobManager>,
    pub workspace: Mutex<Option<WorkspaceSnapshot>>,
    pub reminders: tokio::sync::Mutex<crate::prompt::reminder::ReminderState>,
    pub permissions: RwLock<HashMap<String, bool>>,
}

impl SessionRuntime {
    fn new() -> Self {
        Self {
            anchors: Arc::new(crate::hashline::AnchorStore::new()),
            jobs: Arc::new(JobManager::new()),
            workspace: Mutex::new(None),
            reminders: tokio::sync::Mutex::new(crate::prompt::reminder::ReminderState::default()),
            permissions: RwLock::new(HashMap::new()),
        }
    }
}

/// core 进程内共享状态。
pub(crate) struct CoreInner {
    pub config: Arc<ResolvedConfig>,
    pub client: Client,
    pub sessions: Arc<SessionTable>,
    pub tools: ToolRegistry,
    pub capabilities: RwLock<HashMap<u64, ClientCapabilities>>,
    pub runtimes: Mutex<HashMap<String, Arc<SessionRuntime>>>,
    pub project_configs: RwLock<HashMap<PathBuf, Arc<ResolvedConfig>>>,
    pub remote_unavailable: RwLock<std::collections::HashSet<String>>,
    pub turns: Mutex<tokio::task::JoinSet<()>>,
    pub shutdown: tokio_util::sync::CancellationToken,
}

impl CoreInner {
    pub(crate) fn project_config(&self, cwd: &Path) -> Result<Arc<ResolvedConfig>, CoreError> {
        let cwd = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_owned());
        if let Some(config) = self.project_configs.read().get(&cwd).cloned() {
            return Ok(config);
        }
        let config = Arc::new(self.config.for_project(&cwd)?);
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
                .or_insert_with(|| Arc::new(SessionRuntime::new())),
        )
    }

    pub(crate) fn client_supports_ask(&self, connection_id: u64) -> bool {
        self.capabilities
            .read()
            .get(&connection_id)
            .is_some_and(|caps| caps.ask_requests)
    }
}

/// 通过协议 [`AgentService`] 暴露的完整代理运行时。
#[derive(Clone)]
pub struct Core {
    pub(crate) inner: Arc<CoreInner>,
}

impl Core {
    /// 加载配置、验证供应商并建立会话存储目录。
    pub fn new(config: CoreConfig) -> anyhow::Result<Arc<Self>> {
        let mut resolved = ResolvedConfig::load(&config)?;
        if let Some(session_dir) = &config.session_dir {
            resolved.session.dir = session_dir.clone();
        }
        let resolved = Arc::new(resolved);
        std::fs::create_dir_all(&resolved.session.dir)?;
        let sessions = Arc::new(SessionTable::new(
            resolved.session.dir.clone(),
            resolved.session.locking,
        ));
        Ok(Arc::new(Self {
            inner: Arc::new(CoreInner {
                config: resolved,
                client: config.client,
                sessions,
                tools: ToolRegistry::new(),
                capabilities: RwLock::new(HashMap::new()),
                runtimes: Mutex::new(HashMap::new()),
                project_configs: RwLock::new(HashMap::new()),
                remote_unavailable: RwLock::new(std::collections::HashSet::new()),
                turns: Mutex::new(tokio::task::JoinSet::new()),
                shutdown: tokio_util::sync::CancellationToken::new(),
            }),
        }))
    }

    /// 等待轮次取消收尾，终止后台进程组并释放临时状态。
    pub async fn shutdown(&self) {
        self.inner.shutdown.cancel();
        let mut turns = std::mem::take(&mut *self.inner.turns.lock().await);
        while let Some(result) = turns.join_next().await {
            if let Err(error) = result {
                tracing::error!(%error, "轮次收尾任务失败");
            }
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
    /// 修改空闲会话的模型，作废与旧模型绑定的原生载荷。
    pub async fn set_model(&self, session_id: &str, selector: &str) -> anyhow::Result<String> {
        let record = self.inner.sessions.get(session_id).await?;
        let _turn = record.try_turn_lock()?;
        let cwd = record.state.lock().await.session.cwd.clone();
        let config = self.inner.project_config(Path::new(&cwd))?;
        let model = config.resolve_model(selector)?;
        let _file_lock = self
            .inner
            .sessions
            .acquire_lock(session_id, "moc-cli")
            .await?;
        if record.state.lock().await.model != model.selector {
            self.inner
                .sessions
                .set_model(session_id, model.selector.clone())
                .await?;
        }
        Ok(model.selector)
    }

    /// 撤销当前会话最近一次文件编辑。
    pub async fn undo(&self, session_id: &str) -> anyhow::Result<String> {
        let record = self.inner.sessions.get(session_id).await?;
        let _turn = record.try_turn_lock()?;
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
        Ok(
            crate::hashline::apply::undo(&path, &runtime.anchors, config.tools.read.max_file_bytes)
                .await?,
        )
    }
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

    async fn config_get(
        &self,
        _ctx: RequestContext,
        _params: ConfigGetParams,
    ) -> Result<ConfigGetResult, RpcError> {
        let models = self
            .inner
            .config
            .available_models()
            .into_iter()
            .map(|model| ModelInfo {
                id: model.id,
                display_name: model.display_name,
                context_window: Some(model.context_window),
                max_output_tokens: Some(model.max_output_tokens),
            })
            .collect();
        Ok(ConfigGetResult {
            models,
            default_model: Some(
                self.inner
                    .config
                    .resolve_role("default")
                    .map_err(to_rpc)?
                    .selector,
            ),
            agents: vec![AgentInfo {
                id: "moc".into(),
                display_name: "moc".into(),
                description: Some("通用编码代理".into()),
            }],
            default_agent: Some("moc".into()),
            cwd: self.inner.config.cwd.to_string_lossy().into_owned(),
        })
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
            .unwrap_or_else(|| self.inner.config.cwd.clone());
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
        let info = self
            .inner
            .sessions
            .create(
                &cwd,
                &model.selector,
                params.mode,
                params.title.as_deref(),
                None,
            )
            .await
            .map_err(to_rpc)?;
        if params.mode != SessionMode::Normal && config.workspace.watch {
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
        Ok(SessionListResult {
            sessions: self.inner.sessions.list().await.map_err(to_rpc)?,
        })
    }

    async fn session_load(
        &self,
        _ctx: RequestContext,
        params: SessionLoadParams,
    ) -> Result<SessionLoadResult, RpcError> {
        self.inner
            .sessions
            .load(&params.session_id, params.max_entries)
            .await
            .map_err(to_rpc)
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
        let _turn = record.try_turn_lock().map_err(to_rpc)?;
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
        let _turn = record.try_turn_lock().map_err(to_rpc)?;
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
        let _turn = record.try_turn_lock().map_err(to_rpc)?;
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
        let _turn = record.try_turn_lock().map_err(to_rpc)?;
        let file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        let mut info = self
            .inner
            .sessions
            .set_mode(&params.session_id, params.mode)
            .await
            .map_err(to_rpc)?;
        drop(file_lock);
        info.lock = None;
        let runtime = self.inner.runtime(&params.session_id).await;
        if params.mode != SessionMode::Normal {
            let cwd = PathBuf::from(&info.cwd);
            let config = self.inner.project_config(&cwd).map_err(to_rpc)?;
            if config.workspace.watch {
                let snapshot = snapshot_workspace(cwd, config.workspace.max_scan_entries)
                    .await
                    .map_err(to_rpc)?;
                *runtime.workspace.lock().await = Some(snapshot);
            }
        }
        ctx.updates()
            .emit(
                params.session_id,
                SessionUpdate::ModeChanged { mode: info.mode },
            )
            .await
            .map_err(protocol_to_rpc)?;
        Ok(info)
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
        let _turn = record.try_turn_lock().map_err(to_rpc)?;
        let _file_lock = self
            .inner
            .sessions
            .acquire_lock(&params.session_id, &ctx.client().frontend)
            .await
            .map_err(to_rpc)?;
        compact::manual(
            &self.inner,
            &ctx,
            &params.session_id,
            params.instructions.as_deref(),
        )
        .await
        .map_err(to_rpc)
    }

    async fn session_prompt(
        &self,
        ctx: RequestContext,
        params: moc_protocol::methods::PromptParams,
    ) -> Result<TurnResult, RpcError> {
        let cancel = ctx.cancel();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let inner = Arc::clone(&self.inner);
        {
            let mut turns = self.inner.turns.lock().await;
            if self.inner.shutdown.is_cancelled() {
                return Err(RpcError::of(RpcErrorCode::RequestCancelled));
            }
            while let Some(result) = turns.try_join_next() {
                if let Err(error) = result {
                    tracing::error!(%error, "轮次任务失败");
                }
            }
            turns.spawn(async move {
                let running = turn::run_turn(&inner, ctx, params);
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

mod mode;
mod turn;

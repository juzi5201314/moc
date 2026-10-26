//! 服务端：路由器、握手门禁、提示注册表与生命周期。
//!
//! 路由用**显式 `match`**（不用宏、不做字符串反射）：方法表就是 crate 文档里那张表，
//! 新增方法时读代码的人能一眼看全所有分支。
//!
//! 每个请求的处理顺序：
//!
//! 1. 并发门禁：[`ServerOptions::max_in_flight`] 个许可用尽 → `-32000`。
//! 2. 握手门禁：除 `initialize` / `ping` 外的请求在握手完成前 → `-32002`；
//!    重复 `initialize` → `-32600`；major 版本不一致 → `-32003`（附 `server_version`）。
//! 3. params 反序列化失败 → `-32602`，**不调用 core**。
//! 4. 分发：`ping` 与 `session/cancel` 由协议层自答；其余进 [`AgentService`]。
//! 5. 结果序列化失败 → `-32603`。
//!
//! `session/prompt` 额外过提示注册表：同一会话已有进行中的提示 → `-32011`，同样**不调用
//! core**。该注册表同时是 `session/cancel` 的落点，也是协议层唯一持有的「会话相关」
//! 状态——它只记「谁在飞」，不记任何会话内容。

use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::connection::{
    BoxFuture, Connection, ConnectionOptions, FinishGuard, Inbound, now_ms, params_from_value,
    value_from,
};
use crate::error::{RpcError, RpcErrorCode};
use crate::frame::{DEFAULT_MAX_FRAME_BYTES, Framing};
use crate::message::{Notification, Request};
use crate::method::Method;
use crate::methods::{
    AgentList, AgentMessage, AgentSpawn, AgentStop, AgentWait, BtwCancelResult, BtwUpdate,
    ClientInfo, ConfigGet, ConfigManage, FsListDir, Initialize, Ping, PingResult, PluginList,
    PluginReload, PluginRunCommand, PromptParams, ResourceComplete, SessionBash, SessionBtw,
    SessionBtwCancel, SessionCancel, SessionCancelResult, SessionCompact, SessionContext,
    SessionContextContent, SessionDelete, SessionList, SessionLoad, SessionLoadPage, SessionNew,
    SessionPlanApprove, SessionPrompt, SessionRename, SessionRewind, SessionSetMode,
    SessionSetModel, SessionSetPermission, SessionSteer, SessionUnload, SessionUnlock,
    SessionUnsteer, SessionUsage,
};

use crate::service::{AgentService, RequestContext};
use crate::transport::Transport;
use crate::{PROTOCOL_VERSION, major_version};

/// 服务端参数。
#[derive(Debug, Clone, Copy)]
pub struct ServerOptions {
    /// 分帧方式；必须与前端一致（不参与握手）。
    pub framing: Framing,
    /// 单条报文体上限。
    pub max_frame_bytes: usize,
    /// 连接内并发请求上限；超出立刻回 `-32000`，不排队。
    pub max_in_flight: usize,
    /// 出站队列容量：更新通知的最大积压量，也是背压的起点。
    pub outbound_capacity: usize,
}

impl Default for ServerOptions {
    fn default() -> Self {
        Self {
            framing: Framing::ContentLength,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            max_in_flight: 64,
            outbound_capacity: 256,
        }
    }
}

impl ServerOptions {
    fn connection_options(self) -> ConnectionOptions {
        ConnectionOptions {
            framing: self.framing,
            max_frame_bytes: self.max_frame_bytes,
            outbound_capacity: self.outbound_capacity,
        }
    }
}

/// 服务端：一个 core 进程一个实例，一个实例服务一个前端。
pub struct Server<S: AgentService> {
    service: S,
    options: ServerOptions,
    shutdown: CancellationToken,
    next_connection: AtomicU64,
}

impl<S: AgentService> Server<S> {
    /// 用默认参数建立服务端；返回 `Arc` 便于跨任务共享。
    pub fn new(service: S) -> Arc<Self> {
        Self::with_options(service, ServerOptions::default())
    }

    /// 用指定参数建立服务端。
    pub fn with_options(service: S, options: ServerOptions) -> Arc<Self> {
        Arc::new(Self {
            service,
            options,
            shutdown: CancellationToken::new(),
            next_connection: AtomicU64::new(0),
        })
    }

    /// 关闭服务端：取消全部连接的令牌，进行中的提示与请求随之结束。
    pub fn shutdown(&self) {
        self.shutdown.cancel();
    }

    /// 服务端是否已关闭。
    pub fn is_shutdown(&self) -> bool {
        self.shutdown.is_cancelled()
    }

    /// 服务一条连接。
    pub fn spawn<T: Transport>(self: &Arc<Self>, stream: T) -> ServerConnection {
        self.launch(stream, self.shutdown.child_token())
    }

    /// 服务标准输入输出：前端以子进程方式拉起的接法。
    pub fn spawn_stdio(self: &Arc<Self>) -> ServerConnection {
        self.spawn(crate::transport::stdio())
    }

    /// 顺序 accept 一个 TCP 前端：断线后继续等下一位（本次提示不支持多连接同时在线）。
    ///
    /// 传输层不做鉴权，`listener` 只应绑定回环地址。
    pub fn spawn_tcp(self: &Arc<Self>, listener: TcpListener) -> ServerConnection {
        let token = self.shutdown.child_token();
        let server = Arc::clone(self);
        let accept_token = token.clone();
        let finished = CancellationToken::new();
        let finished_signal = finished.clone();
        let task_closed = token.clone();
        tokio::spawn(async move {
            let _finish = FinishGuard::new(task_closed, finished_signal);
            loop {
                let stream = tokio::select! {
                    biased;
                    () = accept_token.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, peer)) => {
                            tracing::info!(%peer, "接受前端连接");
                            stream
                        }
                        Err(error) => {
                            tracing::warn!("accept 失败：{error}");
                            continue;
                        }
                    },
                };
                server
                    .launch(stream, accept_token.child_token())
                    .wait()
                    .await;
            }
        });
        ServerConnection {
            closed: token,
            finished,
        }
    }

    /// 顺序 accept 一个 Unix socket 前端。
    #[cfg(unix)]
    pub fn spawn_unix(self: &Arc<Self>, listener: tokio::net::UnixListener) -> ServerConnection {
        let token = self.shutdown.child_token();
        let server = Arc::clone(self);
        let accept_token = token.clone();
        let finished = CancellationToken::new();
        let finished_signal = finished.clone();
        let task_closed = token.clone();
        tokio::spawn(async move {
            let _finish = FinishGuard::new(task_closed, finished_signal);
            loop {
                let stream = tokio::select! {
                    biased;
                    () = accept_token.cancelled() => break,
                    accepted = listener.accept() => match accepted {
                        Ok((stream, _)) => stream,
                        Err(error) => {
                            tracing::warn!("accept 失败：{error}");
                            continue;
                        }
                    },
                };
                server
                    .launch(stream, accept_token.child_token())
                    .wait()
                    .await;
            }
        });
        ServerConnection {
            closed: token,
            finished,
        }
    }

    /// 建立一条连接：拆读写半边、建连接对象、起主循环任务。
    fn launch<T: Transport>(
        self: &Arc<Self>,
        stream: T,
        token: CancellationToken,
    ) -> ServerConnection {
        let (reader, writer) = tokio::io::split(stream);
        let id = self.next_connection.fetch_add(1, Ordering::Relaxed) + 1;
        let (connection, outbound) =
            Connection::new(id, self.options.connection_options(), token.clone());
        let dispatch = Arc::new(ServerDispatch::new(
            Arc::clone(self),
            Arc::clone(&connection),
        ));
        let finished = CancellationToken::new();
        let finished_signal = finished.clone();
        let task_closed = token.clone();
        tokio::spawn(async move {
            // 守卫先建立：`run` 会被处理器 panic 打断，没有它完成信号就永不触发
            let _finish = FinishGuard::new(task_closed, finished_signal);
            connection.run(reader, writer, outbound, dispatch).await;
        });
        ServerConnection {
            closed: token,
            finished,
        }
    }
}

/// 一条服务端连接的句柄。
///
/// 完成信号用令牌而不是 `JoinHandle`：句柄需要能被多次 `wait`（accept 循环里
/// 一个前端断线后等下一位），而 `JoinHandle` 只能被 await 一次。
pub struct ServerConnection {
    closed: CancellationToken,
    finished: CancellationToken,
}

impl ServerConnection {
    /// 关闭这条连接。
    pub fn close(&self) {
        self.closed.cancel();
    }

    /// 连接是否已关闭。
    pub fn is_closed(&self) -> bool {
        self.closed.is_cancelled()
    }

    /// 等待连接彻底收尾（读任务退出、在飞任务 abort、出站排空、写半边关闭）。
    pub async fn wait(&self) {
        self.finished.cancelled().await;
    }
}

/// 连接内的服务端状态：握手、并发门禁、提示与旁路注册表。
struct ServerDispatch<S: AgentService> {
    server: Arc<Server<S>>,
    connection: Arc<Connection>,
    handshake: Mutex<Handshake>,
    in_flight: Arc<Semaphore>,
    prompts: Arc<Mutex<HashMap<String, CancellationToken>>>,
    /// 在飞的旁路请求；与 `prompts` 完全独立，主提示的 busy / 取消互不波及。
    btw_flights: Arc<Mutex<HashMap<String, BtwFlight>>>,
}

/// 一条在飞的旁路请求。
struct BtwFlight {
    request_id: u64,
    cancel: CancellationToken,
}

/// 握手状态机。
///
/// `InProgress` 挡的是并发重复 `initialize`（两个前端请求同时到达时只有一个能
/// 真正握手）；版本不兼容或 core 拒绝时退回 `Fresh`，允许改正后重试。
enum Handshake {
    /// 尚未握手。
    Fresh,
    /// 正在处理 `initialize`。
    InProgress,
    /// 已握手，记录前端信息。
    Ready(ClientInfo),
}

/// 提示注册表的摘除守卫：任务正常结束、被取消、被 abort 都会摘除，绝不留脏记录。
struct PromptGuard {
    prompts: Arc<Mutex<HashMap<String, CancellationToken>>>,
    session_id: String,
}

/// `session/prompt` 的占位结果。
enum PromptClaim {
    /// 不是提示，或参数里读不出 `session_id`（后者交给任务报 `-32602`）。
    NotPrompt,
    /// 占位成功，附带 `session_id` 供守卫摘除。
    Claimed(String),
    /// 同一会话已有进行中的提示。
    Busy,
}

/// `session/btw` 的占位结果。
enum BtwClaim {
    /// 不是旁路请求，或参数里读不出身份（后者交给任务报 `-32602`）。
    NotBtw,
    /// 占位成功；守卫按这两个字段摘除。
    Claimed { session_id: String, request_id: u64 },
    /// 同一会话已有进行中的旁路。
    Busy,
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        self.prompts
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&self.session_id);
    }
}

/// 旁路注册表的摘除守卫：只摘自己那一条，迟到任务不会误删新请求。
struct BtwGuard {
    flights: Arc<Mutex<HashMap<String, BtwFlight>>>,
    session_id: String,
    request_id: u64,
}

impl Drop for BtwGuard {
    fn drop(&mut self) {
        let mut flights = self.flights.lock().unwrap_or_else(PoisonError::into_inner);
        if flights
            .get(&self.session_id)
            .is_some_and(|flight| flight.request_id == self.request_id)
        {
            flights.remove(&self.session_id);
        }
    }
}

impl<S: AgentService> ServerDispatch<S> {
    fn new(server: Arc<Server<S>>, connection: Arc<Connection>) -> Self {
        let in_flight = Arc::new(Semaphore::new(server.options.max_in_flight));
        Self {
            server,
            connection,
            handshake: Mutex::new(Handshake::Fresh),
            in_flight,
            prompts: Arc::new(Mutex::new(HashMap::new())),
            btw_flights: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn handshake(&self) -> std::sync::MutexGuard<'_, Handshake> {
        self.handshake
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    fn prompts(&self) -> std::sync::MutexGuard<'_, HashMap<String, CancellationToken>> {
        self.prompts.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn btw_flights(&self) -> std::sync::MutexGuard<'_, HashMap<String, BtwFlight>> {
        self.btw_flights
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// 在读循环里为 `session/prompt` 占位提示注册表。
    ///
    /// 只从 `params` 里读 `session_id` 一个字段，不解析整个 DTO——参数里可能带着
    /// 数 MiB 的图片，读循环不该为它做一次完整反序列化。
    fn claim_prompt(&self, request: &Request, cancel: &CancellationToken) -> PromptClaim {
        let method = request.method.as_ref();
        let is_prompt = method == <SessionPrompt as Method>::NAME;
        let is_compact = method == <SessionCompact as Method>::NAME;
        let is_bash = method == <SessionBash as Method>::NAME;
        if !is_prompt && !is_compact && !is_bash {
            return PromptClaim::NotPrompt;
        }
        let Some(session_id) = request
            .params
            .as_ref()
            .and_then(|params| params.get("session_id"))
            .and_then(Value::as_str)
        else {
            return PromptClaim::NotPrompt;
        };
        let mut prompts = self.prompts();
        if prompts.contains_key(session_id) {
            return PromptClaim::Busy;
        }
        if is_prompt || is_bash {
            prompts.insert(session_id.to_owned(), cancel.clone());
            PromptClaim::Claimed(session_id.to_owned())
        } else {
            PromptClaim::NotPrompt
        }
    }

    /// 在读循环里为 `session/btw` 占位旁路注册表。
    ///
    /// 同一连接、同一会话同时只允许一条旁路；`request_id` 是前端生成的稳定身份，
    /// 定向取消据此匹配。占位与 `session/prompt` 完全分离：主提示在飞不影响旁路，
    /// 旁路在飞也不占用提示注册表。
    fn claim_btw(&self, request: &Request, cancel: &CancellationToken) -> BtwClaim {
        if request.method.as_ref() != <SessionBtw as Method>::NAME {
            return BtwClaim::NotBtw;
        }
        let Some(session_id) = request
            .params
            .as_ref()
            .and_then(|params| params.get("session_id"))
            .and_then(Value::as_str)
        else {
            return BtwClaim::NotBtw;
        };
        let Some(request_id) = request
            .params
            .as_ref()
            .and_then(|params| params.get("request_id"))
            .and_then(Value::as_u64)
        else {
            return BtwClaim::NotBtw;
        };
        let mut flights = self.btw_flights();
        if flights.contains_key(session_id) {
            return BtwClaim::Busy;
        }
        flights.insert(
            session_id.to_owned(),
            BtwFlight {
                request_id,
                cancel: cancel.clone(),
            },
        );
        BtwClaim::Claimed {
            session_id: session_id.to_owned(),
            request_id,
        }
    }

    /// 定向取消一条旁路；身份不匹配时是成功的空操作，不影响任何主提示。
    fn cancel_btw(&self, session_id: &str, request_id: u64) -> BtwCancelResult {
        let flights = self.btw_flights();
        match flights.get(session_id) {
            Some(flight) if flight.request_id == request_id => {
                flight.cancel.cancel();
                BtwCancelResult { cancelled: true }
            }
            _ => BtwCancelResult { cancelled: false },
        }
    }

    /// 已完成握手时取出前端信息。
    fn ready_client(&self) -> Option<ClientInfo> {
        match &*self.handshake() {
            Handshake::Ready(client) => Some(client.clone()),
            _ => None,
        }
    }

    /// 构造请求上下文。
    fn context(&self, client: ClientInfo, cancel: CancellationToken) -> RequestContext {
        RequestContext::new(
            self.connection.id(),
            client,
            Arc::clone(self.connection.traffic()),
            cancel,
        )
    }

    /// 分发一个已获许可的请求。
    async fn dispatch(
        self: &Arc<Self>,
        request: Request,
        cancel: CancellationToken,
        claim: PromptClaim,
        btw: BtwClaim,
    ) {
        // 守卫在读循环占位之后立刻建立：未握手被拒、方法未知、参数解析失败等任何
        // 提前返回路径都会摘掉占位，只有真正跑进 core 的提示才持占到结束。
        let _guard = match claim {
            PromptClaim::Claimed(session_id) => Some(PromptGuard {
                prompts: Arc::clone(&self.prompts),
                session_id,
            }),
            // 无占位：非提示类请求，或参数里读不出 `session_id`
            PromptClaim::NotPrompt => None,
            // 占位冲突在读循环里就回了 `-32011`，不会走到这里
            PromptClaim::Busy => None,
        };
        let _btw_guard = match btw {
            BtwClaim::Claimed {
                session_id,
                request_id,
            } => Some(BtwGuard {
                flights: Arc::clone(&self.btw_flights),
                session_id,
                request_id,
            }),
            BtwClaim::NotBtw | BtwClaim::Busy => None,
        };
        let Request { id, method, params } = request;
        let payload = match method.as_ref() {
            <Initialize as Method>::NAME => self.initialize(params).await,
            <Ping as Method>::NAME => value_from(PingResult {
                server_time_ms: now_ms(),
            }),
            _ => match self.ready_client() {
                Some(client) => self.route(client, method.as_ref(), params, cancel).await,
                None => Err(RpcError::of(RpcErrorCode::ServerNotInitialized)),
            },
        };
        self.connection.respond(Some(id), payload).await;
    }

    /// 握手：版本协商 + 能力登记。
    async fn initialize(self: &Arc<Self>, params: Option<Value>) -> Result<Value, RpcError> {
        let params = params_from_value::<Initialize>(params)?;
        {
            let mut handshake = self.handshake();
            match *handshake {
                Handshake::Fresh => *handshake = Handshake::InProgress,
                _ => {
                    return Err(RpcError::new(
                        RpcErrorCode::InvalidRequest.code(),
                        "重复 initialize",
                    ));
                }
            }
        }
        if !version_compatible(&params.protocol_version) {
            *self.handshake() = Handshake::Fresh;
            return Err(RpcError::of(RpcErrorCode::UnsupportedProtocolVersion)
                .with_data(serde_json::json!({ "server_version": PROTOCOL_VERSION })));
        }
        let client = params.client.clone();
        let cancel = self.connection.closed().child_token();
        let outcome = self
            .server
            .service
            .initialize(self.context(params.client.clone(), cancel), params)
            .await;
        match outcome {
            Ok(result) => {
                *self.handshake() = Handshake::Ready(client);
                value_from(result)
            }
            Err(error) => {
                *self.handshake() = Handshake::Fresh;
                Err(error)
            }
        }
    }

    /// 已握手连接的方法分发。
    ///
    /// 占位守卫由 [`ServerDispatch::dispatch`] 持有，这里不再接触注册表。
    async fn route(
        self: &Arc<Self>,
        client: ClientInfo,
        method: &str,
        params: Option<Value>,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        match method {
            <ConfigManage as Method>::NAME => {
                let params = params_from_value::<ConfigManage>(params)?;
                value_from(
                    self.server
                        .service
                        .config_manage(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionSteer as Method>::NAME => {
                let params = params_from_value::<SessionSteer>(params)?;
                value_from(
                    self.server
                        .service
                        .session_steer(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionUnsteer as Method>::NAME => {
                let params = params_from_value::<SessionUnsteer>(params)?;
                value_from(
                    self.server
                        .service
                        .session_unsteer(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ConfigGet as Method>::NAME => {
                let params = params_from_value::<ConfigGet>(params)?;
                let result = self
                    .server
                    .service
                    .config_get(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <PluginList as Method>::NAME => {
                let params = params_from_value::<PluginList>(params)?;
                value_from(
                    self.server
                        .service
                        .plugin_list(self.context(client, cancel), params)
                        .await?,
                )
            }
            <PluginReload as Method>::NAME => {
                let params = params_from_value::<PluginReload>(params)?;
                value_from(
                    self.server
                        .service
                        .plugin_reload(self.context(client, cancel), params)
                        .await?,
                )
            }
            <PluginRunCommand as Method>::NAME => {
                let params = params_from_value::<PluginRunCommand>(params)?;
                value_from(
                    self.server
                        .service
                        .plugin_run_command(self.context(client, cancel), params)
                        .await?,
                )
            }
            <ResourceComplete as Method>::NAME => {
                let params = params_from_value::<ResourceComplete>(params)?;
                value_from(
                    self.server
                        .service
                        .resource_complete(self.context(client, cancel), params)
                        .await?,
                )
            }
            <FsListDir as Method>::NAME => {
                let params = params_from_value::<FsListDir>(params)?;
                value_from(
                    self.server
                        .service
                        .fs_list_dir(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionNew as Method>::NAME => {
                let params = params_from_value::<SessionNew>(params)?;
                let result = self
                    .server
                    .service
                    .session_new(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionList as Method>::NAME => {
                let params = params_from_value::<SessionList>(params)?;
                let result = self
                    .server
                    .service
                    .session_list(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionLoad as Method>::NAME => {
                let params = params_from_value::<SessionLoad>(params)?;
                let result = self
                    .server
                    .service
                    .session_load(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionLoadPage as Method>::NAME => {
                let params = params_from_value::<SessionLoadPage>(params)?;
                let result = self
                    .server
                    .service
                    .session_load_page(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionRewind as Method>::NAME => {
                let params = params_from_value::<SessionRewind>(params)?;
                let result = self
                    .server
                    .service
                    .session_rewind(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionUnload as Method>::NAME => {
                let params = params_from_value::<SessionUnload>(params)?;
                let result = self
                    .server
                    .service
                    .session_unload(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionRename as Method>::NAME => {
                let params = params_from_value::<SessionRename>(params)?;
                let result = self
                    .server
                    .service
                    .session_rename(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionDelete as Method>::NAME => {
                let params = params_from_value::<SessionDelete>(params)?;
                let result = self
                    .server
                    .service
                    .session_delete(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionUnlock as Method>::NAME => {
                let params = params_from_value::<SessionUnlock>(params)?;
                let result = self
                    .server
                    .service
                    .session_unlock(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetMode as Method>::NAME => {
                let params = params_from_value::<SessionSetMode>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_mode(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetPermission as Method>::NAME => {
                let params = params_from_value::<SessionSetPermission>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_permission(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionSetModel as Method>::NAME => {
                let params = params_from_value::<SessionSetModel>(params)?;
                let result = self
                    .server
                    .service
                    .session_set_model(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionPlanApprove as Method>::NAME => {
                let params = params_from_value::<SessionPlanApprove>(params)?;
                let result = self
                    .server
                    .service
                    .session_plan_approve(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionCompact as Method>::NAME => {
                let params = params_from_value::<SessionCompact>(params)?;
                let result = self
                    .server
                    .service
                    .session_compact(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }

            <SessionContext as Method>::NAME => {
                let params = params_from_value::<SessionContext>(params)?;
                let result = self
                    .server
                    .service
                    .session_context(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionContextContent as Method>::NAME => {
                let params = params_from_value::<SessionContextContent>(params)?;
                let result = self
                    .server
                    .service
                    .session_context_content(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionUsage as Method>::NAME => {
                let params = params_from_value::<SessionUsage>(params)?;
                let result = self
                    .server
                    .service
                    .session_usage(self.context(client, cancel), params)
                    .await?;
                value_from(result)
            }
            <SessionBash as Method>::NAME => {
                let params = params_from_value::<SessionBash>(params)?;
                value_from(
                    self.server
                        .service
                        .session_bash(self.context(client, cancel), params)
                        .await?,
                )
            }
            <SessionPrompt as Method>::NAME => {
                // 占位已在读循环里完成（保证「先提示后 cancel」的顺序），守卫由
                // `dispatch` 持有：解析失败、core 报错、未握手被拒都会摘掉占位
                let params = params_from_value::<SessionPrompt>(params)?;
                self.prompt(client, params, cancel).await
            }
            <SessionBtw as Method>::NAME => {
                let params = params_from_value::<SessionBtw>(params)?;
                let ctx = self.context(client, cancel);
                // `Started` 必须在 core 动手之前入队：前端据此把「Started 之前收到的
                // 取消意图」换成真正的定向取消，不会因为取消早于登记而落空。
                ctx.updates()
                    .emit_btw(
                        params.session_id.clone(),
                        params.request_id,
                        BtwUpdate::Started,
                    )
                    .await
                    .map_err(|error| {
                        RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
                    })?;
                value_from(self.server.service.session_btw(ctx, params).await?)
            }
            // 旁路取消只 trip 对应请求的令牌；主提示与其它旁路完全不受影响。
            <SessionBtwCancel as Method>::NAME => {
                let params = params_from_value::<SessionBtwCancel>(params)?;
                value_from(self.cancel_btw(&params.session_id, params.request_id))
            }
            // 取消由协议层自答：trip 令牌即可，不经过 core，也不受出站队列拥塞影响
            <SessionCancel as Method>::NAME => {
                let params = params_from_value::<SessionCancel>(params)?;
                value_from(self.cancel_prompt(&params.session_id))
            }
            <AgentSpawn as Method>::NAME => {
                let params = params_from_value::<AgentSpawn>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_spawn(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentList as Method>::NAME => {
                let params = params_from_value::<AgentList>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_list(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentMessage as Method>::NAME => {
                let params = params_from_value::<AgentMessage>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_message(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentStop as Method>::NAME => {
                let params = params_from_value::<AgentStop>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_stop(self.context(client, cancel), params)
                        .await?,
                )
            }
            <AgentWait as Method>::NAME => {
                let params = params_from_value::<AgentWait>(params)?;
                value_from(
                    self.server
                        .service
                        .agent_wait(self.context(client, cancel), params)
                        .await?,
                )
            }
            _ => Err(RpcError::of(RpcErrorCode::MethodNotFound)),
        }
    }

    /// 跑一次提示：进 core；结束后由 `dispatch` 的占位守卫释放注册表。
    async fn prompt(
        self: &Arc<Self>,
        client: ClientInfo,
        params: PromptParams,
        cancel: CancellationToken,
    ) -> Result<Value, RpcError> {
        let result = self
            .server
            .service
            .session_prompt(self.context(client, cancel), params)
            .await;
        value_from(result?)
    }

    /// 取消该会话在飞的提示；没有进行中的提示时是成功的空操作。
    fn cancel_prompt(&self, session_id: &str) -> SessionCancelResult {
        match self.prompts().get(session_id) {
            Some(token) => {
                token.cancel();
                SessionCancelResult { cancelled: true }
            }
            None => SessionCancelResult { cancelled: false },
        }
    }
}

impl<S: AgentService> Inbound for ServerDispatch<S> {
    fn on_request(self: Arc<Self>, request: Request, tasks: &mut JoinSet<()>) {
        let Ok(permit) = Arc::clone(&self.in_flight).try_acquire_owned() else {
            let connection = Arc::clone(&self.connection);
            let id = Some(request.id);
            tasks.spawn(async move {
                connection
                    .respond_error(id, RpcError::of(RpcErrorCode::ServerBusy))
                    .await;
            });
            return;
        };
        // 每请求令牌与提示 / 旁路的占位都在读循环里完成：`session/cancel` 与
        // `session/btw_cancel` 是同一连接上的后续报文，因此「先请求后取消」的顺序
        // 是确定的，取消绝不会因为任务调度顺序而落空。
        let cancel = self.connection.closed().child_token();
        let claim = self.claim_prompt(&request, &cancel);
        let btw = self.claim_btw(&request, &cancel);
        if matches!(&claim, PromptClaim::Busy) || matches!(&btw, BtwClaim::Busy) {
            let connection = Arc::clone(&self.connection);
            let id = Some(request.id);
            tasks.spawn(async move {
                let _permit = permit;
                connection
                    .respond_error(id, RpcError::of(RpcErrorCode::SessionBusy))
                    .await;
            });
            return;
        }
        tasks.spawn(async move {
            let _permit = permit;
            self.dispatch(request, cancel, claim, btw).await;
        });
    }

    fn on_notification(self: Arc<Self>, notification: Notification) -> BoxFuture<()> {
        // 本次提示没有「客户端→服务端」通知；通知永远不产生响应，因此只记日志
        tracing::debug!(
            connection = self.connection.id(),
            method = %notification.method,
            "忽略客户端发来的通知"
        );
        Box::pin(std::future::ready(()))
    }
}

/// 版本是否兼容：比较第一个 `.` 之前的 major 段。
fn version_compatible(client_version: &str) -> bool {
    major_version(client_version) == major_version(PROTOCOL_VERSION)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_negotiation_compares_major_only() {
        assert!(version_compatible("1.0"));
        assert!(version_compatible("1.7"));
        assert!(version_compatible("1"));
        assert!(!version_compatible("2.0"));
        assert!(!version_compatible("0.9"));
        assert!(!version_compatible(""));
        assert_eq!(major_version("1.2.3"), "1");
        assert_eq!(major_version("1"), "1");
    }
}

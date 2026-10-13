//! 客户端：处理器注册、调用、连接生命周期。
//!
//! 处理器**只能通过 [`ClientBuilder`] 注册**，连接建立前完成：这从根上消除了
//! 「通知先于注册到达被丢弃」的竞态，也让 `connect()` 能一次性校验重复注册。
//!
//! 入站分发规则：
//!
//! | 入站 | 有处理器 | 无处理器 |
//! |---|---|---|
//! | 反向请求 | 起任务执行，结果写成响应 | 回 `-32601`（core 按「未授权」处理） |
//! | 通知 | 起任务执行；参数解析失败则整条丢弃 | `debug` 记录后丢弃，不断连接 |
//! | 响应 | 投递给对应调用 | `debug` 记录后丢弃（ID 未知或已过期） |
//!
//! 通知处理器拿到的参数解析失败（例如 core 新增了前端不认识的 `type`）只影响那一条
//! 更新，不影响连接——这正是「忽略未知更新」的落地方式。
//!
//! 生命周期：最后一个 [`ClientHandle`] 被丢弃即关闭连接（写任务随出站队列归零退出，
//! 对端观察到 EOF 后收尾）；[`ClientHandle::close`] 是显式版本，并且会等到收尾完成。

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use serde_json::{Map, Value};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::connection::{
    BoxFuture, Connection, ConnectionOptions, Inbound, Outbound, Traffic, params_from_value,
    value_from,
};
use crate::error::{ConnectError, ProtocolError, RpcCallError, RpcError, RpcErrorCode};
use crate::frame::{DEFAULT_MAX_FRAME_BYTES, Framing};
use crate::message::{Notification, Request, Response};
use crate::method::{ClientRequest, Method, ServerNotification, ServerRequest};
use crate::methods::{
    ClientCapabilities, ClientInfo, Initialize, InitializeParams, InitializeResult,
};
use crate::transport::Transport;
use crate::{PROTOCOL_VERSION, major_version};

/// 客户端出站队列容量（与 `ServerOptions::outbound_capacity` 同量级）。
const OUTBOUND_CAPACITY: usize = 256;

/// 类型擦除后的处理器 future 见连接内核里的 `BoxFuture`（两边共用同一别名）。
///
/// 反向请求处理器：`Value` 进、`Value` 出（进出各在注册处绑定具体 DTO）。
type RequestHandler =
    Arc<dyn Fn(Value, ClientRequestContext) -> BoxFuture<Result<Value, RpcError>> + Send + Sync>;

/// 通知处理器：`Value` 进、无输出。
type NotificationHandler = Arc<dyn Fn(Value, ClientRequestContext) -> BoxFuture<()> + Send + Sync>;

/// 客户端构造器。
pub struct ClientBuilder {
    client: ClientInfo,
    capabilities: ClientCapabilities,
    framing: Framing,
    max_frame_bytes: usize,
    request_handlers: HashMap<&'static str, RequestHandler>,
    notification_handlers: HashMap<&'static str, NotificationHandler>,
    duplicates: Vec<&'static str>,
}

impl ClientBuilder {
    /// 以前端自报家门开始构造。
    pub fn new(client: ClientInfo) -> Self {
        Self {
            client,
            capabilities: ClientCapabilities::default(),
            framing: Framing::ContentLength,
            max_frame_bytes: DEFAULT_MAX_FRAME_BYTES,
            request_handlers: HashMap::new(),
            notification_handlers: HashMap::new(),
            duplicates: Vec::new(),
        }
    }

    /// 声明前端能力。
    pub fn capabilities(mut self, capabilities: ClientCapabilities) -> Self {
        self.capabilities = capabilities;
        self
    }

    /// 分帧方式；必须与服务端一致。
    pub fn framing(mut self, framing: Framing) -> Self {
        self.framing = framing;
        self
    }

    /// 单条报文体上限。
    pub fn max_frame_bytes(mut self, bytes: usize) -> Self {
        self.max_frame_bytes = bytes;
        self
    }

    /// 注册反向请求处理器（如 `session/request_permission`）。
    ///
    /// 同名重复注册会在 [`ClientBuilder::connect`] 时以
    /// [`ProtocolError::DuplicateHandler`] 失败——静默覆盖只会让权限询问
    /// 落到意料之外的处理逻辑上。
    pub fn on_request<M, F, Fut>(mut self, handler: F) -> Self
    where
        M: ServerRequest,
        F: Fn(M::Params, ClientRequestContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<M::Result, RpcError>> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let erased: RequestHandler = Arc::new(move |params, ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let params = params_from_value::<M>(Some(params))?;
                let result = handler(params, ctx).await?;
                value_from(result)
            })
        });
        if self
            .request_handlers
            .insert(<M as Method>::NAME, erased)
            .is_some()
        {
            self.duplicates.push(<M as Method>::NAME);
        }
        self
    }

    /// 注册通知处理器（如 `session/update`）。
    pub fn on_notification<M, F, Fut>(mut self, handler: F) -> Self
    where
        M: ServerNotification,
        F: Fn(M::Params, ClientRequestContext) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let handler = Arc::new(handler);
        let erased: NotificationHandler = Arc::new(move |params, ctx| {
            let handler = Arc::clone(&handler);
            Box::pin(async move {
                let params = match serde_json::from_value::<M::Params>(params) {
                    Ok(params) => params,
                    Err(error) => {
                        // 前端不认识的更新：整条丢弃，连接继续用
                        tracing::debug!("通知参数解析失败，已丢弃：{error}");
                        return;
                    }
                };
                handler(params, ctx).await;
            })
        });
        if self
            .notification_handlers
            .insert(<M as Method>::NAME, erased)
            .is_some()
        {
            self.duplicates.push(<M as Method>::NAME);
        }
        self
    }

    /// 建连并自动完成 `initialize`（含 major 版本校验）。
    ///
    /// 返回句柄与服务端握手结果；任何失败路径都会关闭刚建立的连接。
    pub async fn connect<T: Transport>(
        self,
        stream: T,
    ) -> Result<(ClientHandle, InitializeResult), ConnectError> {
        if let Some(name) = self.duplicates.first() {
            return Err(ProtocolError::DuplicateHandler(name).into());
        }
        let (reader, writer) = tokio::io::split(stream);
        let closed = CancellationToken::new();
        let (connection, outbound) = Connection::new(
            0,
            ConnectionOptions {
                framing: self.framing,
                max_frame_bytes: self.max_frame_bytes,
                outbound_capacity: OUTBOUND_CAPACITY,
            },
            closed.clone(),
        );
        let dispatch = Arc::new(ClientDispatch {
            handlers: Arc::new(Handlers {
                requests: self.request_handlers,
                notifications: self.notification_handlers,
            }),
            traffic: Arc::clone(connection.traffic()),
        });
        let traffic = Arc::clone(connection.traffic());
        let finished = CancellationToken::new();
        let finished_signal = finished.clone();
        tokio::spawn(async move {
            connection.run(reader, writer, outbound, dispatch).await;
            finished_signal.cancel();
        });

        let params = InitializeParams {
            protocol_version: PROTOCOL_VERSION.to_owned(),
            client: self.client,
            capabilities: self.capabilities,
        };
        let handshake = match traffic.call::<Initialize>(params).await {
            Ok(handshake) => handshake,
            Err(error) => {
                closed.cancel();
                return Err(connect_error(error));
            }
        };
        if major_version(&handshake.protocol_version) != major_version(PROTOCOL_VERSION) {
            let server_version = handshake.protocol_version.clone();
            closed.cancel();
            return Err(ConnectError::Rpc(
                RpcError::of(RpcErrorCode::UnsupportedProtocolVersion)
                    .with_data(serde_json::json!({ "server_version": server_version })),
            ));
        }
        let handle = ClientHandle {
            inner: Arc::new(ClientInner {
                traffic,
                closed_token: closed,
                finished,
                handshake: handshake.clone(),
            }),
        };
        Ok((handle, handshake))
    }
}

/// 客户端句柄：`Clone + Send + Sync`，可跨任务共享（TUI 一边跑提示一边接取消键）。
#[derive(Clone)]
pub struct ClientHandle {
    inner: Arc<ClientInner>,
}

struct ClientInner {
    traffic: Arc<Traffic>,
    closed_token: CancellationToken,
    /// 连接彻底收尾后触发（读任务退出、在飞处理器结束、写半边关闭）。
    finished: CancellationToken,
    handshake: InitializeResult,
}

impl Drop for ClientInner {
    fn drop(&mut self) {
        // 最后一个句柄消失即关闭连接
        self.closed_token.cancel();
    }
}

impl ClientHandle {
    /// 发起一次调用。
    ///
    /// 多个任务可以并发调用（TUI 的「跑提示」与「取消」就是两条并发调用）；
    /// 连接死亡时在飞调用统一收到 [`RpcCallError::Closed`]。
    pub async fn call<M: ClientRequest>(
        &self,
        params: M::Params,
    ) -> Result<M::Result, RpcCallError> {
        self.inner.traffic.call::<M>(params).await
    }

    /// 服务端握手结果（协议版本、服务端信息与能力）。
    pub fn server(&self) -> &InitializeResult {
        &self.inner.handshake
    }

    /// 连接是否已经开始收尾。
    pub fn is_closed(&self) -> bool {
        self.inner.closed_token.is_cancelled()
    }

    /// 等待连接彻底收尾。
    pub async fn closed(&self) {
        self.inner.finished.cancelled().await;
    }

    /// 显式关闭：停读、排空已排队出站报文、关闭连接，并等到收尾完成。
    pub async fn close(&self) {
        self.inner.closed_token.cancel();
        self.inner.finished.cancelled().await;
    }
}

/// 处理器表；连接建立后不再变化，因此可以无锁共享。
struct Handlers {
    requests: HashMap<&'static str, RequestHandler>,
    notifications: HashMap<&'static str, NotificationHandler>,
}

/// 客户端入站分发。
struct ClientDispatch {
    handlers: Arc<Handlers>,
    traffic: Arc<Traffic>,
}

/// 反向请求处理器的上下文。
pub struct ClientRequestContext {
    /// 连接级取消：连接收尾（前端关闭、core 关闭、读写任务退出）时触发，处理器
    /// 据此放弃还在等外部输入（如授权模态）的询问。
    ///
    /// 这是**连接级**信号：core 在连接仍存活时放弃某一条反向请求（例如取消提示后
    /// 不再需要授权答案）不会单独通知前端，那种情况下前端的应答会因等待者已摘除
    /// 而被丢弃，处理器不会收到取消。
    pub cancel: CancellationToken,
}

impl Inbound for ClientDispatch {
    fn on_request(self: Arc<Self>, request: Request, tasks: &mut JoinSet<()>) {
        let handler = self.handlers.requests.get(request.method.as_ref()).cloned();
        let Some(handler) = handler else {
            // 没注册处理器：明确拒绝，而不是让 core 一直等
            let traffic = Arc::clone(&self.traffic);
            let id = request.id;
            tasks.spawn(async move {
                let response =
                    Response::error(Some(id), RpcError::of(RpcErrorCode::MethodNotFound));
                let _ = traffic.send(Outbound::Response(response)).await;
            });
            return;
        };
        let cancel = self.traffic.closed().child_token();
        tasks.spawn(async move {
            let params = request.params.unwrap_or_else(empty_object);
            let payload = handler(params, ClientRequestContext { cancel }).await;
            let response = Response {
                id: Some(request.id),
                payload,
            };
            let _ = self.traffic.send(Outbound::Response(response)).await;
        });
    }

    fn on_notification(self: Arc<Self>, notification: Notification) -> BoxFuture<()> {
        let handler = self
            .handlers
            .notifications
            .get(notification.method.as_ref())
            .cloned();
        let Some(handler) = handler else {
            tracing::debug!(method = %notification.method, "未注册的通知，已丢弃");
            return Box::pin(std::future::ready(()));
        };
        let cancel = self.traffic.closed().child_token();
        let params = notification.params.unwrap_or_else(empty_object);
        // 直接返回处理器 future：读循环原地 await，保证同一连接内更新按序落地
        handler(params, ClientRequestContext { cancel })
    }
}

/// 空参数对象。
fn empty_object() -> Value {
    Value::Object(Map::new())
}

/// 握手失败映射：本地失败一律归为「连接已关闭」。
fn connect_error(error: RpcCallError) -> ConnectError {
    match error {
        RpcCallError::Rpc(error) => ConnectError::Rpc(error),
        RpcCallError::Cancelled | RpcCallError::Closed => {
            ConnectError::Protocol(ProtocolError::ConnectionClosed)
        }
    }
}

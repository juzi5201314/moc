//! 连接内核：读写任务、出站队列、pending 表、连接收尾。
//!
//! 客户端与服务端共用这一层，两侧差异全部由 `Inbound` 提供。每条连接的形状固定：
//!
//! ```text
//! 读任务 ──► FrameDecoder ──► RpcMessage::parse ──┬─► Response     → pending 表（唤醒等待者）
//!                                                 ├─► Request      → Inbound::on_request
//!                                                 └─► Notification → Inbound::on_notification
//! 写任务 ◄── mpsc 出站队列 ◄── 响应 / 反向请求 / 更新通知（三方共用一条队列）
//! ```
//!
//! 三条不变量：
//!
//! 1. **背压只有一处**：更新通知与响应共用出站队列，发送方在队列满时 `await`，
//!    压力直接传回 core 的 LLM 流读取；而取消（`session/cancel`）在读任务里
//!    trip 令牌，不经过出站队列，因此队列拥塞时取消依然即时生效。
//! 2. **ID 空间天然不冲突**：客户端发起的调用用从 1 递增的正整数，服务端发起的
//!    反向请求用从 -1 递减的负整数。两个方向无需共享状态、无需随机数，也不会撞车。
//! 3. **收尾单点且幂等**：读任务因 EOF / 致命分帧错 / 令牌取消 / 写任务死亡退出后，
//!    统一 trip 连接令牌 → `JoinSet::shutdown` abort 在飞任务（服务 future 被 drop，
//!    其 Drop guard 自行清理）→ 排空 pending 表（每个等待者立即拿到失败）→ 等写任务
//!    排空已排队报文并关闭写半边。
//!
//! 出站报文**绝不半写**：序列化或长度超限都在写任务内就地降级（响应换 `-32603`、
//! 通知丢弃并 `warn`、反向请求让本地等待者立即失败），不会把断头帧写到线上。

use std::collections::HashMap;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;
use serde_json::{Map, Value};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;

use crate::error::{FrameError, ProtocolError, RpcCallError, RpcError, RpcErrorCode};
use crate::frame::{FrameDecoder, FrameEncoder, Framing};
use crate::message::{Id, Notification, ParseFailure, Request, Response, RpcMessage};
use crate::method::{ClientRequest, Method, ServerRequest};

/// 读缓冲大小：单次 `read` 的上限，更长的报文体由解码器自己拼接。
const READ_CHUNK: usize = 16 * 1024;

/// 连接级参数。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ConnectionOptions {
    /// 分帧方式；两侧必须一致（不参与握手）。
    pub framing: Framing,
    /// 单条报文体上限。
    pub max_frame_bytes: usize,
    /// 出站队列容量；同时是「未写出的更新」的最大积压量。
    pub outbound_capacity: usize,
}

/// 出站报文：写任务只关心它是三种 envelope 里的哪一种。
///
/// 方法名用 `&'static str`（来自 [`Method::NAME`]）而报文类型用 `Cow<'static, str>`，
/// 于是「组帧」这一步是纯 move：不为每条更新通知分配方法名字符串。
pub(crate) enum Outbound {
    /// 响应某个请求。
    Response(Response),
    /// 反向请求。
    Request(Request),
    /// 单向通知。
    Notification(Notification),
}

impl From<Outbound> for RpcMessage {
    fn from(outbound: Outbound) -> Self {
        match outbound {
            Outbound::Response(response) => Self::Response(response),
            Outbound::Request(request) => Self::Request(request),
            Outbound::Notification(notification) => Self::Notification(notification),
        }
    }
}

/// 等待响应的表：ID → 结果投递通道。
///
/// 两个方向各有一个计数器：客户端发起的调用从 1 递增，服务端发起的反向请求从 -1
/// 递减。它们天然不冲突，因此既不需要共享状态，也不需要随机数或碰撞重试。
pub(crate) struct PendingMap {
    entries: Mutex<HashMap<Id, oneshot::Sender<Result<Value, RpcError>>>>,
    calls: AtomicI64,
    reverses: AtomicI64,
}

impl PendingMap {
    fn new() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            calls: AtomicI64::new(0),
            reverses: AtomicI64::new(0),
        }
    }

    /// 客户端发起的调用 ID：`1, 2, 3, …`。
    fn next_call_id(&self) -> Id {
        Id::Number(self.calls.fetch_add(1, Ordering::Relaxed) + 1)
    }

    /// 服务端发起的反向请求 ID：`-1, -2, -3, …`。
    fn next_reverse_id(&self) -> Id {
        Id::Number(self.reverses.fetch_sub(1, Ordering::Relaxed) - 1)
    }

    /// 登记等待者。
    fn register(&self, id: Id) -> oneshot::Receiver<Result<Value, RpcError>> {
        let (sender, receiver) = oneshot::channel();
        self.entries().insert(id, sender);
        receiver
    }

    /// 投递响应；ID 未知或已过期时返回 `false`。
    fn deliver(&self, id: &Id, payload: Result<Value, RpcError>) -> bool {
        let Some(sender) = self.entries().remove(id) else {
            return false;
        };
        // 等待者可能已被取消（提示取消、连接收尾），投递失败无所谓
        sender.send(payload).is_ok()
    }

    /// 摘除等待者（调用方自行取消时用）。
    fn remove(&self, id: &Id) {
        self.entries().remove(id);
    }

    /// 让某个等待者立即失败（出站失败时用）。
    fn fail(&self, id: &Id, error: RpcError) -> bool {
        self.deliver(id, Err(error))
    }

    /// 排空整张表：所有等待者立刻拿到失败，不再悬挂。
    fn drain(&self) {
        self.entries().clear();
    }

    fn entries(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<Id, oneshot::Sender<Result<Value, RpcError>>>> {
        // 临界区只有 HashMap 操作，不可能在持锁期间 panic；中毒时直接取回内层数据
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// 连接的出站通道 + 关闭信号 + pending 表；`Arc` 共享给 core 侧句柄。
pub(crate) struct Traffic {
    outbound: mpsc::Sender<Outbound>,
    closed: CancellationToken,
    pending: Arc<PendingMap>,
}

impl Traffic {
    /// 连接关闭令牌；取消即收尾。
    pub(crate) fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    /// 发送出站报文；连接已关闭或写任务已退出时返回 [`ProtocolError::ConnectionClosed`]。
    pub(crate) async fn send(&self, message: Outbound) -> Result<(), ProtocolError> {
        tokio::select! {
            biased;
            () = self.closed.cancelled() => Err(ProtocolError::ConnectionClosed),
            result = self.outbound.send(message) => result.map_err(|_| ProtocolError::ConnectionClosed),
        }
    }

    /// 客户端发起的调用：正数 ID，等响应。
    ///
    /// 连接死亡时返回 [`RpcCallError::Closed`]（结果已不可达，重试由调用方决定）。
    pub(crate) async fn call<M: ClientRequest>(
        &self,
        params: M::Params,
    ) -> Result<M::Result, RpcCallError> {
        let params = serialize_params::<M>(params)?;
        let id = self.pending.next_call_id();
        let receiver = self.pending.register(id.clone());
        if self
            .send(Outbound::Request(Request::new(
                id.clone(),
                M::NAME,
                Some(params),
            )))
            .await
            .is_err()
        {
            self.pending.remove(&id);
            return Err(RpcCallError::Closed);
        }
        tokio::select! {
            biased;
            result = receiver => match result {
                Ok(Ok(value)) => deserialize_result::<M>(value),
                Ok(Err(error)) => Err(RpcCallError::Rpc(error)),
                // 收尾时发送端被丢弃（连接已关闭）
                Err(_) => Err(RpcCallError::Closed),
            },
            () = self.closed.cancelled() => {
                self.pending.remove(&id);
                Err(RpcCallError::Closed)
            }
        }
    }

    /// 服务端发起的反向请求：负数 ID，等响应。
    ///
    /// 提示被取消或连接关闭时返回 [`RpcCallError::Cancelled`]——对 core 来说两者
    /// 的处理相同（放弃这次询问），不需要再分一层。
    pub(crate) async fn reverse_request<M: ServerRequest>(
        &self,
        params: M::Params,
        cancel: &CancellationToken,
    ) -> Result<M::Result, RpcCallError> {
        let params = serialize_params::<M>(params).map_err(|error| match error {
            RpcCallError::Rpc(error) => RpcCallError::Rpc(error),
            _ => RpcCallError::Cancelled,
        })?;
        let id = self.pending.next_reverse_id();
        let receiver = self.pending.register(id.clone());
        if self
            .send(Outbound::Request(Request::new(
                id.clone(),
                M::NAME,
                Some(params),
            )))
            .await
            .is_err()
        {
            self.pending.remove(&id);
            return Err(RpcCallError::Cancelled);
        }
        tokio::select! {
            biased;
            result = receiver => match result {
                Ok(Ok(value)) => deserialize_result::<M>(value),
                Ok(Err(error)) => Err(RpcCallError::Rpc(error)),
                // 收尾时发送端被丢弃
                Err(_) => Err(RpcCallError::Cancelled),
            },
            () = cancel.cancelled() => {
                self.pending.remove(&id);
                Err(RpcCallError::Cancelled)
            }
        }
    }
}

/// 处理器返回的 future（类型擦除，便于按方法名存放）。
pub(crate) type BoxFuture<T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'static>>;

/// 对端消息的落地方式；实现必须保证「请求必有响应」。
pub(crate) trait Inbound: Send + Sync + 'static {
    /// 处理对端请求。实现可以把工作放进 `tasks`，但必须有人最终写出响应。
    fn on_request(self: Arc<Self>, request: Request, tasks: &mut JoinSet<()>);

    /// 处理对端通知。
    ///
    /// 返回的 future 会被读循环**原地 await**：同一连接上的通知因此严格按线上顺序
    /// 落地（流式文本增量乱序会把渲染撕裂）。实现应当是「快速交接」——把事件转交给
    /// 自己的 UI/业务队列，而不是在处理器里长时间等待，否则会拖慢本连接的读取。
    ///
    /// 没有处理器时返回已就绪的空 future，通知永远不产生响应。
    fn on_notification(self: Arc<Self>, notification: Notification) -> BoxFuture<()>;
}

/// 一条连接。
pub(crate) struct Connection {
    id: u64,
    options: ConnectionOptions,
    traffic: Arc<Traffic>,
    closed: CancellationToken,
}

impl Connection {
    /// 建立连接对象，返回它和出站队列的接收端（交给 [`Connection::run`]）。
    pub(crate) fn new(
        id: u64,
        options: ConnectionOptions,
        closed: CancellationToken,
    ) -> (Arc<Self>, mpsc::Receiver<Outbound>) {
        let (outbound, receiver) = mpsc::channel(options.outbound_capacity);
        let traffic = Arc::new(Traffic {
            outbound,
            closed: closed.clone(),
            pending: Arc::new(PendingMap::new()),
        });
        (
            Arc::new(Self {
                id,
                options,
                traffic,
                closed,
            }),
            receiver,
        )
    }

    /// 连接号；`0` 表示客户端侧（前端只有一条连接，不需要编号）。
    pub(crate) fn id(&self) -> u64 {
        self.id
    }

    /// core 侧句柄的共享部分。
    pub(crate) fn traffic(&self) -> &Arc<Traffic> {
        &self.traffic
    }

    /// 连接令牌：取消即关闭；用作每请求令牌的父。
    pub(crate) fn closed(&self) -> &CancellationToken {
        &self.closed
    }

    /// 写出一个响应。
    pub(crate) async fn respond(&self, id: Option<Id>, payload: Result<Value, RpcError>) {
        if let Err(error) = self
            .traffic
            .send(Outbound::Response(Response { id, payload }))
            .await
        {
            tracing::debug!(connection = self.id, "响应未写出：{error}");
        }
    }

    /// 写出一个错误响应。
    pub(crate) async fn respond_error(&self, id: Option<Id>, error: RpcError) {
        self.respond(id, Err(error)).await;
    }

    /// 读写主循环：起写任务、读帧、分发、收尾。
    pub(crate) async fn run<R, W>(
        &self,
        mut reader: R,
        writer: W,
        outbound: mpsc::Receiver<Outbound>,
        dispatch: Arc<dyn Inbound>,
    ) where
        R: AsyncRead + Unpin + Send + 'static,
        W: AsyncWrite + Unpin + Send + 'static,
    {
        let mut write_task = tokio::spawn(write_loop(
            writer,
            outbound,
            self.options,
            self.closed.clone(),
            Arc::clone(&self.traffic.pending),
        ));
        let mut write_finished = false;
        let mut tasks: JoinSet<()> = JoinSet::new();
        let mut decoder = FrameDecoder::new(self.options.framing, self.options.max_frame_bytes);
        let mut buffer = vec![0u8; READ_CHUNK];

        loop {
            // 先把上一轮读到的完整报文全部发出去（此处的 await 都在 select 之外，
            // 因此可以自由持有 decoder 与 JoinSet）
            if !self
                .dispatch_frames(&mut decoder, &dispatch, &mut tasks)
                .await
            {
                break;
            }
            tokio::select! {
                biased;
                () = self.closed.cancelled() => break,
                result = &mut write_task, if !write_finished => {
                    write_finished = true;
                    if let Err(error) = result {
                        tracing::error!(connection = self.id, "写任务异常退出：{error}");
                    }
                    break;
                }
                Some(result) = tasks.join_next() => {
                    if let Err(error) = result {
                        tracing::error!(connection = self.id, "请求任务未正常结束：{error}");
                    }
                }
                read = reader.read(&mut buffer) => match read {
                    Ok(0) => {
                        tracing::debug!(connection = self.id, "对端关闭连接");
                        break;
                    }
                    Ok(size) => decoder.push(&buffer[..size]),
                    Err(error) => {
                        tracing::debug!(connection = self.id, "读失败：{error}");
                        break;
                    }
                },
            }
        }

        self.closed.cancel();
        tasks.shutdown().await;
        self.traffic.pending.drain();
        if !write_finished {
            // 取消后写任务只会排空已排队报文，不会长等；此处等它关掉写半边
            let _ = write_task.await;
        }
    }

    /// 把解码器里所有完整报文分发出去；返回 `false` 表示连接必须关闭。
    async fn dispatch_frames(
        &self,
        decoder: &mut FrameDecoder,
        dispatch: &Arc<dyn Inbound>,
        tasks: &mut JoinSet<()>,
    ) -> bool {
        loop {
            match read_step(decoder) {
                Ok(ReadStep::Message(message)) => match message {
                    RpcMessage::Response(response) => {
                        let Some(id) = response.id else {
                            tracing::debug!(connection = self.id, "收到无 ID 的错误响应，丢弃");
                            continue;
                        };
                        if !self.traffic.pending.deliver(&id, response.payload) {
                            tracing::debug!(connection = self.id, "收到未知 ID {id} 的响应，丢弃");
                        }
                    }
                    RpcMessage::Request(request) => Arc::clone(dispatch).on_request(request, tasks),
                    RpcMessage::Notification(notification) => {
                        Arc::clone(dispatch).on_notification(notification).await;
                    }
                },
                Ok(ReadStep::InvalidJson(failure)) => {
                    // 报文边界是清楚的：回错误后连接仍可继续用
                    tracing::warn!(connection = self.id, "对端报文非法：{}", failure.message);
                    if self
                        .traffic
                        .send(Outbound::Response(failure.into_response()))
                        .await
                        .is_err()
                    {
                        return false;
                    }
                }
                Ok(ReadStep::NeedMore) => return true,
                Err(error) => {
                    tracing::warn!(connection = self.id, "分帧失败，关闭连接：{error}");
                    return false;
                }
            }
        }
    }
}

/// 单步解码结果。
enum ReadStep {
    /// 解出一条报文。
    Message(RpcMessage),
    /// 报文不是合法 JSON：envelope 层失败，连接仍可用。
    InvalidJson(ParseFailure),
    /// 字节还不够。
    NeedMore,
}

fn read_step(decoder: &mut FrameDecoder) -> Result<ReadStep, FrameError> {
    let Some(frame) = decoder.next_frame()? else {
        return Ok(ReadStep::NeedMore);
    };
    match RpcMessage::parse(frame) {
        Ok(message) => Ok(ReadStep::Message(message)),
        Err(failure) => Ok(ReadStep::InvalidJson(failure)),
    }
}

/// 写任务：独占写半边，组帧后逐条 `write_all` + `flush`。
///
/// 每帧都 flush：stdio 下前端看不到更新会以为卡死。
async fn write_loop<W>(
    mut writer: W,
    mut outbound: mpsc::Receiver<Outbound>,
    options: ConnectionOptions,
    closed: CancellationToken,
    pending: Arc<PendingMap>,
) where
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut buffers = WriteBuffers::new(options.framing, options.max_frame_bytes);
    loop {
        let message = tokio::select! {
            biased;
            message = outbound.recv() => message,
            () = closed.cancelled() => {
                // 收尾：尽力写完已排队的报文，不再等待新报文
                while let Ok(message) = outbound.try_recv() {
                    if !write_one(&mut writer, &mut buffers, message, &pending).await {
                        break;
                    }
                }
                break;
            }
        };
        let Some(message) = message else {
            break;
        };
        if !write_one(&mut writer, &mut buffers, message, &pending).await {
            closed.cancel();
            break;
        }
    }
    let _ = writer.shutdown().await;
}

/// 写任务的复用缓冲：整条连接只在这里分配，避免每帧重新申请。
struct WriteBuffers {
    encoder: FrameEncoder,
    /// 报文体 JSON。
    body: Vec<u8>,
    /// 已组好的帧（可能是多条报文各一帧，但这条连接一次只写一条）。
    frame: Vec<u8>,
    max_frame_bytes: usize,
}

impl WriteBuffers {
    fn new(framing: Framing, max_frame_bytes: usize) -> Self {
        Self {
            encoder: FrameEncoder::new(framing),
            body: Vec::new(),
            frame: Vec::new(),
            max_frame_bytes,
        }
    }

    /// 序列化并组帧；失败时返回降级原因（`frame` 不含断头内容）。
    fn encode(&mut self, message: RpcMessage) -> Result<(), EncodeFailure> {
        self.body.clear();
        self.frame.clear();
        let body = &mut self.body;
        serde_json::to_writer(&mut *body, &message)
            .map_err(|error| EncodeFailure::Serialize(error.to_string()))?;
        if self
            .encoder
            .encode(body, self.max_frame_bytes, &mut self.frame)
        {
            return Ok(());
        }
        Err(EncodeFailure::TooLarge {
            len: self.body.len(),
            max: self.max_frame_bytes,
        })
    }

    /// 用 `-32603` 顶替一个响应。
    fn encode_fallback(&mut self, id: Option<Id>, detail: String) -> bool {
        let fallback = RpcMessage::Response(Response::error(
            id,
            RpcError::new(RpcErrorCode::InternalError.code(), detail),
        ));
        self.body.clear();
        self.frame.clear();
        let body = &mut self.body;
        serde_json::to_writer(&mut *body, &fallback).is_ok()
            && self
                .encoder
                .encode(body, self.max_frame_bytes, &mut self.frame)
    }
}

/// 一条出站报文没能写出去的原因。
enum EncodeFailure {
    /// 本地序列化失败（解释：本端实现 bug，不是对端的问题）。
    Serialize(String),
    /// 超过 `max_frame_bytes`。
    TooLarge {
        /// 实际长度。
        len: usize,
        /// 上限。
        max: usize,
    },
}

impl EncodeFailure {
    /// 中文描述，用于日志与降级错误消息。
    fn detail(&self) -> String {
        match self {
            Self::Serialize(message) => format!("出站报文序列化失败：{message}"),
            Self::TooLarge { len, max } => format!("出站报文 {len} 字节超过上限 {max}"),
        }
    }
}

/// 写出一条出站报文；返回 `false` 表示写半边已不可用。
async fn write_one<W>(
    writer: &mut W,
    buffers: &mut WriteBuffers,
    message: Outbound,
    pending: &PendingMap,
) -> bool
where
    W: AsyncWrite + Unpin,
{
    // 降级需要的信息：响应 ID（换成 -32603）、反向请求 ID（让本地等待者失败）
    let degrade = match &message {
        Outbound::Response(response) => Degrade::Response(response.id.clone()),
        Outbound::Request(request) => Degrade::Request(request.id.clone()),
        Outbound::Notification(notification) => Degrade::Notification(notification.method.clone()),
    };
    let failure = match buffers.encode(message.into()) {
        Ok(()) => return write_frame(writer, &buffers.frame).await,
        Err(failure) => failure,
    };
    let detail = failure.detail();
    match &degrade {
        Degrade::Response(_) => tracing::warn!("{detail}，响应已降级为 -32603"),
        Degrade::Notification(method) => tracing::warn!("通知 {method} 未写出（{detail}），已丢弃"),
        Degrade::Request(id) => {
            tracing::warn!("反向请求 {id} 未写出（{detail}），已让本地等待者失败")
        }
    }
    degrade.apply(writer, buffers, pending, detail).await
}

/// 出站报文的降级处置策略。
enum Degrade {
    /// 响应：本地降级为 `-32603` 并保留原 ID（前端至少知道这一问失败了）。
    Response(Option<Id>),
    /// 通知：丢弃并 `warn`（前端可用 `session/load` 兜底）。
    Notification(std::borrow::Cow<'static, str>),
    /// 反向请求：让本地等待者立刻失败，不写半个请求上去。
    Request(Id),
}

impl Degrade {
    /// 执行降级；只有「响应换成错误报文」这一路还需要继续写。
    async fn apply<W>(
        self,
        writer: &mut W,
        buffers: &mut WriteBuffers,
        pending: &PendingMap,
        detail: String,
    ) -> bool
    where
        W: AsyncWrite + Unpin,
    {
        match self {
            Self::Response(id) => {
                if !buffers.encode_fallback(id, detail) {
                    // 一个几十字节的错误报文也写不出去，说明上限小到不可用：只能放弃
                    tracing::error!("降级错误报文仍无法写出，关闭连接");
                    return false;
                }
                write_frame(writer, &buffers.frame).await
            }
            // 丢弃已在调用处记过日志
            Self::Notification(_) => true,
            Self::Request(id) => {
                pending.fail(
                    &id,
                    RpcError::new(RpcErrorCode::InternalError.code(), detail),
                );
                true
            }
        }
    }
}

/// 把整帧写到线上并 flush。
async fn write_frame<W>(writer: &mut W, frame: &[u8]) -> bool
where
    W: AsyncWrite + Unpin,
{
    if let Err(error) = writer.write_all(frame).await {
        tracing::debug!("写失败：{error}");
        return false;
    }
    if let Err(error) = writer.flush().await {
        tracing::debug!("flush 失败：{error}");
        return false;
    }
    true
}

/// 序列化请求参数；失败按 `-32603` 报告（core / 前端的参数类型必须可序列化）。
fn serialize_params<M: Method>(params: M::Params) -> Result<Value, RpcCallError> {
    serde_json::to_value(params).map_err(|error| {
        RpcCallError::Rpc(RpcError::new(
            RpcErrorCode::InternalError.code(),
            format!("请求参数序列化失败：{error}"),
        ))
    })
}

/// 反序列化响应结果；对端形状不符合契约时按 `-32603` 报告。
fn deserialize_result<M: Method>(value: Value) -> Result<M::Result, RpcCallError> {
    serde_json::from_value(value).map_err(|error| {
        RpcCallError::Rpc(RpcError::new(
            RpcErrorCode::InternalError.code(),
            format!("响应结果解析失败：{error}"),
        ))
    })
}

/// 反序列化 params；报文省略 `params` 时按空对象处理（空参数方法照常调用）。
pub(crate) fn params_from_value<M: Method>(params: Option<Value>) -> Result<M::Params, RpcError> {
    let value = params.unwrap_or_else(|| Value::Object(Map::new()));
    serde_json::from_value(value).map_err(|error| {
        RpcError::new(
            RpcErrorCode::InvalidParams.code(),
            format!("参数非法：{error}"),
        )
    })
}

/// 序列化结果；失败按 `-32603` 报告。
pub(crate) fn value_from<T: Serialize>(value: T) -> Result<Value, RpcError> {
    serde_json::to_value(value).map_err(|error| {
        RpcError::new(
            RpcErrorCode::InternalError.code(),
            format!("结果序列化失败：{error}"),
        )
    })
}

/// 当前 UNIX 毫秒时间戳；时钟回拨到 1970 之前时给 0。
pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pending_ids_do_not_collide_across_directions() {
        let pending = PendingMap::new();
        assert_eq!(pending.next_call_id(), Id::Number(1));
        assert_eq!(pending.next_call_id(), Id::Number(2));
        assert_eq!(pending.next_reverse_id(), Id::Number(-1));
        assert_eq!(pending.next_reverse_id(), Id::Number(-2));
        assert_eq!(pending.next_call_id(), Id::Number(3));
    }

    #[test]
    fn pending_delivery_and_unknown_ids() {
        let pending = PendingMap::new();
        let id = pending.next_call_id();
        let mut receiver = pending.register(id.clone());
        assert!(pending.deliver(&id, Ok(Value::Null)));
        assert_eq!(receiver.try_recv().expect("已投递"), Ok(Value::Null));
        // 已投递的 ID 不能重复投递
        assert!(!pending.deliver(&id, Ok(Value::Null)));
        // 未登记的 ID 投递失败
        assert!(!pending.fail(
            &Id::String("nope".into()),
            RpcError::of(RpcErrorCode::InternalError)
        ));
    }

    #[test]
    fn draining_wakes_every_waiter() {
        let pending = PendingMap::new();
        let first = pending.next_call_id();
        let second = pending.next_reverse_id();
        let mut first_receiver = pending.register(first.clone());
        let mut second_receiver = pending.register(second.clone());
        pending.drain();
        assert!(first_receiver.try_recv().is_err());
        assert!(second_receiver.try_recv().is_err());
        assert!(!pending.deliver(&first, Ok(Value::Null)));
        assert!(!pending.deliver(&second, Ok(Value::Null)));
    }

    #[test]
    fn missing_params_becomes_empty_object() {
        let params = params_from_value::<crate::methods::ConfigGet>(None).expect("空参数可解析");
        assert_eq!(params, crate::methods::ConfigGetParams {});
        let error =
            params_from_value::<crate::methods::SessionDelete>(None).expect_err("缺少必要字段");
        assert_eq!(error.code, RpcErrorCode::InvalidParams.code());
    }
}

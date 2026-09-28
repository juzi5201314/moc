//! moc-protocol：交互层（tui / webui / desktop）与 `moc-core` 之间**唯一**的边界。
//!
//! 传输无关的 JSON-RPC 2.0 协议层：报文与分帧、双向请求 / 通知、类型化客户端、
//! 服务端调度框架，以及每个方法一对 Params/Result DTO 加一个
//! [`AgentService`] 契约（由 core 实现）。
//!
//! # 职责边界
//!
//! | 归协议层 | 归 core |
//! |---|---|
//! | 报文、分帧、序列化 | 会话状态机、上下文与持久化 |
//! | 并发门禁、取消传播、反向请求、更新推送 | 锁的持有、模型调用、工具执行 |
//! | 方法契约（DTO 与错误码） | 每个方法的具体语义 |
//!
//! 协议层**不持有任何会话状态**：唯一的连接内状态是「哪个会话有在飞轮次」的注册表
//! （`session/prompt` 的并发门禁与 `session/cancel` 的落点）。
//!
//! 依赖方向是 `moc-core → moc-protocol → moc-llm`；协议层依赖 `moc-llm` 只为复用
//! wire 内容类型（[`Message`] / [`ContentPart`] / [`ToolCall`] / [`Usage`] 等），
//! 不依赖 core。
//!
//! # 方法表
//!
//! | 方法 | 方向 | 处理方 | 前置 | 失败码 |
//! |---|---|---|---|---|
//! | `initialize` | 前端→core | core | 未初始化（重复 → `-32600`） | `-32003` |
//! | `ping` | 前端→core | 协议层自答 | — | — |
//! | `config/get` | 前端→core | core | 已初始化 | `-32014` |
//! | `session/new` / `list` / `load` / `unload` | 前端→core | core | 已初始化 | `-32602` / `-32010` / `-32014` |
//! | `session/rename` / `delete` / `unlock` | 前端→core | core | 已初始化 | `-32010` / `-32015` |
//! | `session/set_mode` / `session/set_model` | 前端→core | core | 已初始化 | `-32010` / `-32015` |
//! | `session/plan_approve` | 前端→core | core | 已初始化 | `-32010` / `-32602` |
//! | `session/compact` | 前端→core | core | 已初始化 + 该会话无在飞轮次 | `-32010` / `-32011` / `-32014` |
//! | `session/prompt` | 前端→core | core | 已初始化 + 该会话无在飞轮次 | `-32010` / `-32015` / `-32011` |
//! | `session/cancel` | 前端→core | 协议层自答 | 已初始化 | — |
//! | `session/request_permission` / `session/request_ask` | core→前端 | 前端 | 前端已注册处理器 | 前端自定义 |
//! | `session/update` | core→前端 | 前端 | — | — |
//!
//! 方向是强约束：服务端收到 `session/update` / `session/request_permission` /
//! `session/request_ask` 一律按未知方法回 `-32601`。
//!
//! # 线上形状
//!
//! ```json
//! {"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{"session_id":"ses_7f3a","input":[{"type":"text","text":"看一下 README"}]}}
//! {"jsonrpc":"2.0","method":"session/update","params":{"session_id":"ses_7f3a","update":{"type":"assistant_delta","turn_id":"turn_1","delta":{"type":"text","delta":"好的"}}}}
//! {"jsonrpc":"2.0","id":-1,"method":"session/request_permission","params":{"session_id":"ses_7f3a","turn_id":"turn_1","request_id":"perm_1","tool_call_id":"call_9","tool_name":"bash","title":"执行命令：cargo test","body":null,"options":[{"id":"allow_once","label":"允许一次","kind":"allow_once"},{"id":"reject_once","label":"拒绝","kind":"reject_once"}]}}
//! {"jsonrpc":"2.0","id":1,"result":{"session_id":"ses_7f3a","turn_id":"turn_1","message":{"role":"Assistant","content":[{"Text":"好的"}]},"finish_reason":"Stop","usage":{"input_tokens":12,"output_tokens":3,"total_tokens":null,"cached_input_tokens":null,"cache_creation_input_tokens":null,"reasoning_tokens":null}}}
//! ```
//!
//! 两类 ID 天然分离：前端发起的调用用从 `1` 递增的正整数，core 发起的反向请求用从
//! `-1` 递减的负整数。
//!
//! # 兼容规则（前端与 core 都必须遵守）
//!
//! - **版本协商**：`initialize` 只比较 [`PROTOCOL_VERSION`] 的第一个 `.` 之前部分
//!   （major）；不一致回 `-32003`，`data.server_version` 给出服务端版本。
//! - **未知 `type` 必须忽略**：[`methods::update::SessionUpdate`] /
//!   [`methods::update::AssistantDelta`] / [`methods::turn::InputPart`] 是会增长的
//!   枚举，core 可以新增变体而不升 major。前端反序列化失败时必须**整条丢弃**，
//!   不报错、不断连接（[`ClientBuilder`] 的通知分发已经这么做）。
//! - **未知错误码原样透传**：[`RpcError::code`] 是 `i32` 而不是枚举，老前端遇到新
//!   码值应当按通用失败展示，而不是把连接判死。
//! - **分帧必须两侧一致**：分帧不参与握手（第一帧就要求一致），它是连接配置
//!   （[`ServerOptions::framing`] / [`ClientBuilder::framing`]）。
//! - **不支持的还是直接拒绝**：core 未实现的能力回 `-32014`，不要静默降级。
//!
//! # 三种接法
//!
//! ## TUI 内嵌 core：内存管道
//!
//! ```no_run
//! use moc_protocol::{
//!     AgentService, ClientBuilder, ClientHandle, ClientInfo, Server, transport,
//! };
//!
//! /// 内嵌 core：一对内存管道把 `Server` 与客户端接起来，两端跑的是与跨进程
//! /// 完全相同的调用代码（`transport::duplex` 只是把 socket 换成内存队列）。
//! async fn spawn_embedded<S: AgentService>(core: S) -> Result<ClientHandle, moc_protocol::ConnectError> {
//!     let (core_side, frontend_side) = transport::duplex(64 * 1024);
//!     let server = Server::new(core);
//!     let connection = server.spawn(core_side);
//!
//!     let client = ClientInfo {
//!         name: "moc-tui".into(),
//!         version: env!("CARGO_PKG_VERSION").into(),
//!         frontend: "tui".into(),
//!     };
//!     let (handle, _handshake) = ClientBuilder::new(client)
//!         .on_notification::<moc_protocol::methods::SessionUpdateMethod, _, _>(|params, _ctx| async move {
//!             // 渲染更新：这里通常转交给 UI 事件队列
//!             println!("{:?}", params.update);
//!         })
//!         .connect(frontend_side)
//!         .await?;
//!
//!     // 前端退出时关闭连接；`connection.wait()` 可等到服务端收尾完成
//!     drop(connection);
//!     Ok(handle)
//! }
//! ```
//!
//! ## webui / desktop：子进程 stdio，或本机 socket
//!
//! ```no_run
//! use std::net::SocketAddr;
//!
//! use moc_protocol::{
//!     AgentService, ClientBuilder, ClientHandle, ClientInfo, Server, ServerConnection, transport,
//! };
//!
//! /// core 侧：以前端为父进程时用 stdio；想让多个前端各自连一次就用回环 TCP。
//! async fn serve_stdio<S: AgentService>(core: S) -> ServerConnection {
//!     let server = Server::new(core);
//!     server.spawn_stdio()
//! }
//!
//! /// core 侧：回环 TCP（传输层不做鉴权，**不要绑定非回环地址**）。
//! async fn serve_tcp<S: AgentService>(core: S, addr: SocketAddr) -> std::io::Result<ServerConnection> {
//!     let listener = transport::listen_tcp(addr).await?;
//!     let server = Server::new(core);
//!     Ok(server.spawn_tcp(listener))
//! }
//!
//! /// 前端侧：连接并自动完成握手。
//! async fn connect(addr: SocketAddr) -> Result<(ClientHandle, moc_protocol::methods::InitializeResult), moc_protocol::ConnectError> {
//!     let stream = transport::connect_tcp(addr).await.expect("能连上 core");
//!     let client = ClientInfo {
//!         name: "moc-webui".into(),
//!         version: "0.1.0".into(),
//!         frontend: "webui".into(),
//!     };
//!     ClientBuilder::new(client).connect(stream).await
//! }
//! ```
//!
//! ## core 侧：实现 [`AgentService`]
//!
//! ```no_run
//! use moc_protocol::methods::{
//!     CompactResult, ConfigGetParams, ConfigGetResult, ContextUsageResult,
//!     Empty, InitializeParams, InitializeResult,
//!     OpenVikingDeleteParams, OpenVikingDeleteResult, OpenVikingListParams, OpenVikingListResult,
//!     OpenVikingReadParams, OpenVikingReadResult, OpenVikingWriteParams, OpenVikingWriteResult,
//!     PromptParams, ServerCapabilities, ServerInfo, SessionCompactParams, SessionContextParams,
//!     SessionDeleteParams, SessionInfo, SessionListParams, SessionListResult, SessionLoadParams,
//!     SessionLoadResult, SessionNewParams, SessionPlanApproveParams, SessionPlanApproveResult,
//!     SessionRenameParams, SessionSetModeParams, SessionSetModelParams, SessionUnloadParams,
//!     SessionUnlockParams, SessionUpdate, TurnResult,
//! };
//! use moc_protocol::{
//!     AgentService, FinishReason, Message, RequestContext, RpcError, RpcErrorCode, Usage,
//!     PROTOCOL_VERSION,
//! };
//!
//! struct MyCore {
//!     // 会话表、配置、上下文与状态机……
//! }
//!
//! /// 推送失败几乎总是「前端已经断开」：core 应当据此中止本轮。
//! fn emit_error(error: moc_protocol::ProtocolError) -> RpcError {
//!     RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
//! }
//!
//! impl AgentService for MyCore {
//!     async fn initialize(
//!         &self,
//!         _ctx: RequestContext,
//!         _params: InitializeParams,
//!     ) -> Result<InitializeResult, RpcError> {
//!         // 前端能力（`params.capabilities.permission_requests`）在这里登记进 core
//!         Ok(InitializeResult {
//!             protocol_version: PROTOCOL_VERSION.to_owned(),
//!             server: ServerInfo { name: "moc-core".into(), version: "0.1.0".into() },
//!             capabilities: ServerCapabilities { permission_requests: true, ask_requests: true, session_locking: true },
//!         })
//!     }
//!
//!     async fn session_prompt(
//!         &self,
//!         ctx: RequestContext,
//!         params: PromptParams,
//!     ) -> Result<TurnResult, RpcError> {
//!         let session_id = params.session_id.as_str();
//!         let turn_id = "turn_1";
//!         let usage = Usage { input_tokens: 12, output_tokens: 3, ..Usage::default() };
//!
//!         // 1) 过程增量走通知；`ctx.cancel()` 在长任务里 `select!`，覆盖前端取消、
//!         //    连接断开、server 关闭三条路径
//!         ctx.updates()
//!             .emit(session_id, SessionUpdate::turn_started(turn_id, "deepseek-flash", 0))
//!             .await
//!             .map_err(emit_error)?;
//!
//!         // 2) 要动本地环境就先要授权；前端未注册处理器 / 轮次被取消都会拿到 Err
//!         let permission = moc_protocol::methods::PermissionRequestParams {
//!             session_id: session_id.to_owned(),
//!             turn_id: turn_id.to_owned(),
//!             request_id: "perm_1".to_owned(),
//!             tool_call_id: "call_1".to_owned(),
//!             tool_name: "bash".to_owned(),
//!             title: "执行命令：cargo test".to_owned(),
//!             body: None,
//!             diff: None,
//!             options: moc_protocol::methods::default_options(),
//!         };
//!         let allowed = ctx.ask_permission(permission).await.is_ok();
//!
//!         // 3) 轮次结束时给出权威内容：`message` 应当与 `turn_finished` 逐字段相同
//!         let message = Message::assistant_text("已按你的授权执行");
//!         let finish_reason = if allowed { FinishReason::Stop } else { FinishReason::Refusal };
//!         ctx.updates()
//!             .emit(
//!                 session_id,
//!                 SessionUpdate::TurnFinished {
//!                     turn_id: turn_id.to_owned(),
//!                     message: message.clone(),
//!                     finish_reason: finish_reason.clone(),
//!                     usage,
//!                 },
//!             )
//!             .await
//!             .map_err(emit_error)?;
//!         Ok(TurnResult {
//!             session_id: params.session_id.clone(),
//!             turn_id: turn_id.to_owned(),
//!             message,
//!             finish_reason,
//!             usage,
//!         })
//!     }
//!
//!     // 其余方法同理：从 core 自己的状态机取数据后返回。本轮未实现的能力
//!     // 明确回 `-32014`，不要静默降级。
//!     async fn config_get(&self, _ctx: RequestContext, _params: ConfigGetParams) -> Result<ConfigGetResult, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_new(&self, _ctx: RequestContext, _params: SessionNewParams) -> Result<SessionInfo, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_list(&self, _ctx: RequestContext, _params: SessionListParams) -> Result<SessionListResult, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_load(&self, _ctx: RequestContext, _params: SessionLoadParams) -> Result<SessionLoadResult, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_unload(&self, _ctx: RequestContext, _params: SessionUnloadParams) -> Result<Empty, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_rename(&self, _ctx: RequestContext, _params: SessionRenameParams) -> Result<SessionInfo, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_delete(&self, _ctx: RequestContext, _params: SessionDeleteParams) -> Result<Empty, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_unlock(&self, _ctx: RequestContext, _params: SessionUnlockParams) -> Result<Empty, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_set_mode(&self, _ctx: RequestContext, _params: SessionSetModeParams) -> Result<SessionInfo, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_set_model(&self, _ctx: RequestContext, _params: SessionSetModelParams) -> Result<SessionInfo, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_plan_approve(&self, _ctx: RequestContext, _params: SessionPlanApproveParams) -> Result<SessionPlanApproveResult, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_compact(&self, _ctx: RequestContext, _params: SessionCompactParams) -> Result<CompactResult, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn session_context(&self, _ctx: RequestContext, _params: SessionContextParams) -> Result<ContextUsageResult, RpcError> {
//!         Err(RpcError::of(RpcErrorCode::NotSupported))
//!     }
//!     async fn openviking_list(&self, _ctx: RequestContext, params: OpenVikingListParams) -> Result<OpenVikingListResult, RpcError> {
//!         Ok(OpenVikingListResult { uri: params.uri, entries: Vec::new() })
//!     }
//!     async fn openviking_read(&self, _ctx: RequestContext, params: OpenVikingReadParams) -> Result<OpenVikingReadResult, RpcError> {
//!         Ok(OpenVikingReadResult { uri: params.uri, content: String::new() })
//!     }
//!     async fn openviking_write(&self, _ctx: RequestContext, params: OpenVikingWriteParams) -> Result<OpenVikingWriteResult, RpcError> {
//!         Ok(OpenVikingWriteResult { uri: params.uri, result: serde_json::Value::Null })
//!     }
//!     async fn openviking_delete(&self, _ctx: RequestContext, params: OpenVikingDeleteParams) -> Result<OpenVikingDeleteResult, RpcError> {
//!         Ok(OpenVikingDeleteResult { uri: params.uri, result: serde_json::Value::Null })
//!     }
//! }
//! ```
//!
//! # 取消与背压
//!
//! ```text
//! Server::shutdown() ──► 连接令牌 ──► 每请求令牌 ──► RequestContext::cancel()
//! ```
//!
//! - 「前端调 `session/cancel`」「连接断开」「server 关闭」三条路径都汇到同一个令牌；
//!   取消在**读循环**里完成，不经过出站队列，因此队列拥塞时依然即时生效。
//! - 更新通知与响应共用一条出站队列：队列满时 [`UpdateSink::emit`] 会 `await`，
//!   背压直接传回 core 的 LLM 流读取，协议层不会堆出无界缓冲。
//! - 服务方法的 future **必须可被 drop**：连接关闭时协议层会 abort 在飞任务，
//!   需要跨 `await` 的一致状态请放进 core 自己的状态机或用 Drop guard 收拾。
//!
//! # 模块
//!
//! [`message`]（报文与严格解析）/ [`frame`]（分帧）/ [`method`] 与 [`methods`]
//! （方法契约）/ [`service`]（core 侧契约）/ [`server`] / [`client`] /
//! [`transport`]（传输入口）/ [`error`]（错误模型）。

#![forbid(unsafe_code)]

pub mod client;
pub mod connection;
pub mod error;
pub mod frame;
pub mod message;
pub mod method;
pub mod methods;
pub mod server;
pub mod service;
pub mod transport;

pub use client::{ClientBuilder, ClientHandle, ClientRequestContext};
pub use error::{
    ConnectError, FrameError, IoKind, ProtocolError, RpcCallError, RpcError, RpcErrorCode,
};
pub use frame::{DEFAULT_MAX_FRAME_BYTES, FrameDecoder, FrameEncoder, Framing};
pub use message::{Id, Notification, ParseFailure, Request, Response, RpcMessage};
pub use method::{ClientRequest, Method, ServerNotification, ServerRequest};
pub use server::{Server, ServerConnection, ServerOptions};
pub use service::{AgentService, RequestContext, Requester, UpdateSink};

// 方法契约（标记类型与 DTO）整体转发到 crate 根：前端与 core 写
// `moc_protocol::SessionInfo` 即可，不必记住每个 DTO 属于哪个子模块；
// [`methods`] 模块本身照旧可用。与上面的重导出无重名。
pub use methods::*;

pub use moc_llm::{
    ContentPart, FinishReason, ImageDetail, ImageSource, Message, ReasoningBlock, Role, ToolCall,
    ToolResult, Usage,
};

/// 线上协议版本；major 相同即兼容，minor 差异忽略。
///
/// `initialize` 只比较第一个 `.` 之前的 major 段，不一致回
/// [`RpcErrorCode::UnsupportedProtocolVersion`] 并在 `data.server_version` 给出服务端版本。
pub const PROTOCOL_VERSION: &str = "1.1";

/// 取版本号的 major 段（第一个 `.` 之前）；没有 `.` 时整串就是 major。
pub(crate) fn major_version(version: &str) -> &str {
    version.split_once('.').map_or(version, |(major, _)| major)
}

//! 协议桥：内嵌 core 的 JSON-RPC 连接与 UI 事件流。
//!
//! 连接样板与 moc-cli REPL 完全一致（`transport::duplex` + `Server` +
//! `ClientBuilder`），区别只在：通知处理器不做渲染，只把事件转投 UI 队列；
//! 反向请求（授权 / 提问）通过 oneshot 把答复权交给主循环。

use std::sync::Arc;

use anyhow::Context;
use moc_core::Core;
use moc_protocol::{
    ClientBuilder, ClientHandle, ClientInfo, ClientRequestContext, RpcError, RpcErrorCode, Server,
    ServerConnection, ServerOptions,
    methods::{
        AskParams, AskResult, ClientCapabilities, PermissionRequestParams, PermissionResult,
        SessionRequestAsk, SessionRequestPermission, SessionUpdateMethod, SessionUpdateParams,
    },
};
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

/// UI 事件队列承载的消息。
pub enum AppEvent {
    /// `session/update` 通知。
    Update(SessionUpdateParams),
    /// 授权请求；主循环收到后弹出模态，回选经 `reply` 送回协议层。
    Permission {
        params: PermissionRequestParams,
        reply: tokio::sync::oneshot::Sender<Result<PermissionResult, RpcError>>,
    },
    /// `ask` 工具的用户提问。
    Ask {
        params: AskParams,
        reply: tokio::sync::oneshot::Sender<Result<AskResult, RpcError>>,
    },
    /// 剪贴板图片读取结果（ctrl+v；`None` = 无图片或读取失败）。
    ClipboardImage(Option<(String, Vec<u8>)>),
    /// 会话列表就绪（ctrl+r / `/resume` 的选择器数据）。
    Sessions(Vec<moc_protocol::SessionInfo>),
    /// 会话加载完成（选择器选中后切换转写）。
    Loaded(moc_protocol::SessionInfo, Vec<crate::transcript::Entry>),
    /// 模型切换完成（关闭选择器）。
    ModelSwitched,
    /// git 分支就绪（statusline 异步刷新）。
    GitBranch(String),
}

/// 一条内嵌连接。
pub struct EmbeddedConnection {
    pub client: ClientHandle,
    server_connection: ServerConnection,
}

impl EmbeddedConnection {
    /// 关闭连接并等待 core 侧收尾。
    pub async fn shutdown(self) {
        // `close` 停读、排空出站并等到收尾完成
        self.client.close().await;
        drop(self.server_connection);
    }
}

/// 建立内嵌连接并注册全部处理器。
pub async fn connect_embedded(
    core: Arc<Core>,
    events: UnboundedSender<AppEvent>,
) -> anyhow::Result<EmbeddedConnection> {
    let (core_side, frontend_side) = moc_protocol::transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let server_connection = server.spawn(core_side);

    let builder = ClientBuilder::new(ClientInfo {
        name: "moc-tui".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        frontend: "tui".into(),
    })
    .capabilities(ClientCapabilities {
        permission_requests: true,
        ask_requests: true,
    });

    // 通知处理器在协议读循环内被顺序 await（保证事件有序），必须保持轻量：
    // 入队即返回，重活全部留给主循环。
    let update_events = events.clone();
    let builder = builder.on_notification::<SessionUpdateMethod, _, _>(move |params, _ctx| {
        let events = update_events.clone();
        async move {
            tracing::debug!(update = ?params.update, "收到会话更新");
            if let Err(error) = events.send(AppEvent::Update(params)) {
                tracing::debug!(%error, "UI 队列已关闭，丢弃通知");
            }
        }
    });

    let permission_events = events.clone();
    let builder = builder.on_request::<SessionRequestPermission, _, _>(
        move |params, ctx: ClientRequestContext| {
            let events = permission_events.clone();
            let cancel = ctx.cancel;
            async move {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if events
                    .send(AppEvent::Permission {
                        params,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return Err(RpcError::of(RpcErrorCode::RequestCancelled));
                }
                select_reply(reply_rx, cancel).await
            }
        },
    );

    let ask_events = events.clone();
    let builder =
        builder.on_request::<SessionRequestAsk, _, _>(move |params, ctx: ClientRequestContext| {
            let events = ask_events.clone();
            let cancel = ctx.cancel;
            async move {
                let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                if events
                    .send(AppEvent::Ask {
                        params,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    return Err(RpcError::of(RpcErrorCode::RequestCancelled));
                }
                select_reply(reply_rx, cancel).await
            }
        });

    let (client, handshake) = builder
        .connect(frontend_side)
        .await
        .context("内嵌连接握手失败")?;
    anyhow::ensure!(
        handshake.server.name == "moc-core",
        "内嵌服务不是 moc-core：{}",
        handshake.server.name
    );
    Ok(EmbeddedConnection {
        client,
        server_connection,
    })
}

/// 等待 UI 答复或取消：取消（前端 cancel、连接断开、server 关闭）时回协议取消码。
async fn select_reply<T>(
    reply_rx: tokio::sync::oneshot::Receiver<Result<T, RpcError>>,
    cancel: CancellationToken,
) -> Result<T, RpcError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(RpcError::of(RpcErrorCode::RequestCancelled)),
        answer = reply_rx => {
            answer.unwrap_or_else(|_| Err(RpcError::of(RpcErrorCode::RequestCancelled)))
        }
    }
}

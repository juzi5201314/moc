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
        AskParams, AskResult, ClientCapabilities, ContextUsageResult, PermissionRequestParams,
        PermissionResult, SessionRequestAsk, SessionRequestPermission, SessionUpdateMethod,
        SessionUpdateParams,
    },
};
use tokio::sync::mpsc::{Receiver, Sender, UnboundedSender, channel};
use tokio_util::sync::CancellationToken;

/// UI 事件队列承载的消息。
pub enum AppEvent {
    SettingsResponse {
        panel: u64,
        /// 该回执对应的请求类型。
        job: crate::settings::Job,
        result: anyhow::Result<moc_protocol::ConfigReply>,
    },
    ConfigurationLoaded(moc_protocol::ConfigGetResult),
    PromptReturned {
        session: String,
        result: anyhow::Result<()>,
    },
    SteerReturned {
        session: String,
        /// 本地待发送项 ID（不是 core 分配的消息 ID）。
        prompt_id: u64,
        result: anyhow::Result<moc_protocol::SteerResult>,
    },
    UnsteerReturned {
        session: String,
        /// 本地待发送项 ID。
        prompt_id: u64,
        action: crate::queue::WithdrawAction,
        result: anyhow::Result<bool>,
    },
    PlanApproval(anyhow::Result<moc_protocol::methods::SessionPlanApproveResult>),
    /// 从权威会话状态重新载入待审计划。
    PlanReview(anyhow::Result<moc_protocol::methods::PlanInfo>),
    /// 新执行会话加载完成后，前端才能发起执行提示。
    PlanExecutionLoaded {
        result: anyhow::Result<moc_protocol::SessionLoadResult>,
        auto_prompt: Option<String>,
    },
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
    /// 剪贴板粘贴结果（ctrl+v）：图片优先，其次纯文本；`None` = 空或读取失败。
    ClipboardPaste(Option<ClipboardPayload>),
    /// OpenViking 记忆目录列表。
    Memories(Vec<moc_protocol::methods::OpenVikingEntry>),
    /// OpenViking 正文读取完成。
    MemoryRead(moc_protocol::methods::OpenVikingReadResult),
    /// OpenViking 正文保存完成。
    MemorySaved(String),
    /// 当前会话的模型调用用量。
    Usage(moc_protocol::SessionUsageResult),
    /// 上下文估算完成。
    ContextUsage(ContextUsageResult),
    /// 上下文正文读取完成。
    ContextContent(anyhow::Result<moc_protocol::methods::ContextContentResult>),
    Sessions(Vec<moc_protocol::SessionInfo>),
    /// 历史展示页；`prepend=true` 时插入当前转写之前。
    HistoryPageLoaded {
        prepend: bool,
        result: anyhow::Result<moc_protocol::methods::SessionLoadPageResult>,
    },
    /// 会话回退选择器的数据加载结果。
    RewindLoaded(anyhow::Result<moc_protocol::SessionLoadResult>),
    Loaded(
        moc_protocol::SessionInfo,
        Vec<crate::transcript::Entry>,
        bool,
        Option<String>,
    ),
    /// 会话回退完成；保留选中的消息以便用户消息恢复到输入框。
    Rewound {
        selected: moc_protocol::methods::TranscriptEntry,
        result: anyhow::Result<moc_protocol::SessionLoadResult>,
    },
    /// 会占住会话提示锁的前端操作收尾（模型切换、思考等级切换、手动压缩）。
    ///
    /// 先解除 [`crate::app::App::inflight_session_ops`] 忙标记，再继续派发队列。
    SessionControlSettled {
        session_id: String,
        /// 成功时的状态行提示。
        notice: Option<String>,
        /// `Ok(压缩失败原因)`：`None` 表示没压缩或压缩成功；`Err(失败原因)`：操作本身失败。
        result: Result<Option<String>, String>,
    },
    /// 切换模型需要压缩确认（`ask` 动作回报容量缺口）。
    ModelSwitchConfirm {
        session_id: String,
        selector: String,
        overflow: moc_protocol::methods::ModelSwitchOverflow,
    },
    /// resource/complete 返回；输入版本与 prefix 在 app 侧校验。
    ResourceCompletion {
        session_id: String,
        revision: u64,
        prefix: String,
        result: anyhow::Result<Vec<moc_protocol::ResourceCandidate>>,
    },
    /// 非模态异步操作的用户可见提示。
    Notice(String),
    /// 选择器或记忆加载失败。
    PickerError(String),
    /// git 分支就绪（statusline 异步刷新）。
    GitBranch(String),
    SubagentsLoaded(anyhow::Result<Vec<moc_protocol::methods::AgentInfo>>),
    /// 聚焦进入或返回的会话加载结果；`returning` 区分返回主会话。
    FocusLoaded {
        returning: bool,
        result: anyhow::Result<moc_protocol::methods::SessionLoadPageResult>,
    },
    SubagentOperation(anyhow::Result<crate::subagent::AgentOperationNotice>),
}

/// 剪贴板粘贴载荷：图片以 base64 承载，文本内联。
pub enum ClipboardPayload {
    Image { media_type: String, data: String },
    Text(String),
}

/// 一条内嵌连接。
pub struct EmbeddedConnection {
    pub client: ClientHandle,
    /// 流式更新使用有界队列；队列满时阻塞协议读循环，向 core 传播背压。
    pub updates: Receiver<SessionUpdateParams>,
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
    let (update_tx, update_rx) = channel(256);
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
    let update_events: Sender<SessionUpdateParams> = update_tx;
    let builder = builder.on_notification::<SessionUpdateMethod, _, _>(move |params, _ctx| {
        let events = update_events.clone();
        async move {
            if let Err(error) = events.send(params).await {
                tracing::debug!(%error, "更新队列已关闭，丢弃通知");
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
        "内置服务不可用：{}",
        handshake.server.name
    );
    Ok(EmbeddedConnection {
        client,
        updates: update_rx,
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

//! moc-acp：把 moc-core 以 ACP agent 身份暴露给编辑器。
//!
//! `moc acp` 在 stdio 上讲 ACP（NDJSON）：内部照旧起进程内 [`Core`] 与
//! `moc_protocol` 客户端，外层用官方 `agent-client-protocol` SDK 服务 ACP。
//! ACP 会话 ID 就是 moc 的 `ses_*`——ACP 建的会话同样能被 `moc --resume`、
//! TUI、web 打开，反之亦然。
#![forbid(unsafe_code)]

mod agent;
mod backend;
mod bridge;
mod translate;

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use agent_client_protocol::schema::v1::{self as acp, ClientCapabilities, SessionNotification};
use agent_client_protocol::{Client, ConnectionTo};
use moc_core::Core;
use moc_protocol::error::RpcCallError;
use moc_protocol::methods::{
    ConfigGet, ConfigGetParams, ConfigGetResult, SessionInfo, SessionUpdate as MocUpdate,
};
use moc_protocol::{ClientRequest, Server, ServerOptions, transport};

use backend::Backend;
use translate::SessionProjection;

/// `moc acp` 的进程级选项：新会话的默认模型 / 引擎 / plan 入口。
#[derive(Debug, Clone, Default)]
pub struct AcpOptions {
    /// 覆盖新会话模型 selector；`None` 用 core 默认模型。
    pub model: Option<String>,
    /// 新会话引擎。
    pub engine: moc_protocol::methods::SessionEngine,
    /// 新会话直接进入 plan 模式。
    pub plan: bool,
}

/// 两侧连接共享的状态。
///
/// - `moc`：core 客户端（duplex 连接建立后填充）。
/// - `acp`：ACP 连接（`connect_with` 回调先于 `initialize` 填充）。
/// - `sessions`：每个 ACP 会话的模式投影，给 `current_mode` / usage / config 用。
pub(crate) struct Bridge<B> {
    options: AcpOptions,
    moc: Mutex<Option<B>>,
    acp: Mutex<Option<ConnectionTo<Client>>>,
    client_caps: Mutex<ClientCapabilities>,
    /// `config/get` 结果：模型表给 config_option 与 usage 上下文用；失败时为空。
    config: OnceLock<Option<ConfigGetResult>>,
    sessions: Mutex<HashMap<String, SessionProjection>>,
}

impl<B: Backend> Bridge<B> {
    fn new(options: AcpOptions) -> Self {
        Self {
            options,
            moc: Mutex::new(None),
            acp: Mutex::new(None),
            client_caps: Mutex::new(ClientCapabilities::default()),
            config: OnceLock::new(),
            sessions: Mutex::new(HashMap::new()),
        }
    }

    fn options(&self) -> &AcpOptions {
        &self.options
    }

    fn attach_moc(&self, moc: B) {
        *self.moc.lock().expect("moc 槽位") = Some(moc);
    }

    /// core 客户端句柄；`connect` 完成前的早到通知返回 `None`。
    fn moc(&self) -> Option<B> {
        self.moc.lock().expect("moc 槽位").clone()
    }

    /// 调用 core；handlers 里必为 `Some`——ACP 请求到得了这里说明连接已建好。
    async fn moc_call<M: ClientRequest>(&self, params: M::Params) -> Result<M::Result, RpcCallError>
    where
        M::Params: Send,
    {
        self.moc()
            .expect("moc 客户端已连接")
            .call::<M>(params)
            .await
    }

    fn attach_acp(&self, cx: ConnectionTo<Client>) {
        *self.acp.lock().expect("acp 槽位") = Some(cx);
    }

    fn detach_acp(&self) {
        *self.acp.lock().expect("acp 槽位") = None;
    }

    /// ACP 连接句柄；connect 前的更新返回 `None`（此时还没有会话可发）。
    fn acp(&self) -> Option<ConnectionTo<Client>> {
        self.acp.lock().expect("acp 槽位").clone()
    }

    fn set_client_caps(&self, caps: ClientCapabilities) {
        *self.client_caps.lock().expect("客户端能力槽位") = caps;
    }

    fn client_caps(&self) -> ClientCapabilities {
        self.client_caps.lock().expect("客户端能力槽位").clone()
    }

    /// 写入 / 刷新会话投影，返回当前投影副本。
    fn track_session(&self, info: &SessionInfo) -> SessionProjection {
        let proj = SessionProjection::from_info(info);
        self.sessions
            .lock()
            .expect("会话投影")
            .insert(info.id.clone(), proj.clone());
        proj
    }

    fn projection(&self, session_id: &str) -> Option<SessionProjection> {
        self.sessions
            .lock()
            .expect("会话投影")
            .get(session_id)
            .cloned()
    }

    fn drop_session(&self, session_id: &str) {
        self.sessions.lock().expect("会话投影").remove(session_id);
    }

    /// 会话的 config_options 列表（当前只有模型项；拿不到模型表则为空）。
    fn config_options(&self, proj: &SessionProjection) -> Option<Vec<acp::SessionConfigOption>> {
        self.config
            .get()
            .and_then(|c| c.as_ref())
            .and_then(|c| translate::model_option(&c.models, &proj.model))
            .map(|option| vec![option])
    }

    /// moc 会话更新 → ACP 更新序列（顺手维护该会话的投影）。
    fn translate_update(&self, session_id: &str, update: &MocUpdate) -> Vec<acp::SessionUpdate> {
        let mut sessions = self.sessions.lock().expect("会话投影");
        let proj = sessions.entry(session_id.to_owned()).or_default();
        translate::session_update(
            update,
            proj,
            &self.client_caps(),
            self.config.get().and_then(|c| c.as_ref()),
        )
    }

    /// 把 ACP 更新排进 `session/update` 通知；连接未建好（或已关）时丢弃。
    fn emit(&self, session_id: &str, updates: Vec<acp::SessionUpdate>) {
        let Some(cx) = self.acp() else {
            if !updates.is_empty() {
                tracing::debug!(session_id, count = updates.len(), "ACP 未连接，更新已丢弃");
            }
            return;
        };
        Self::emit_on(&cx, session_id, updates);
    }

    /// 用调用方给定的连接发更新——`session/load` 重放必须走在同一个 cx 上。
    fn emit_updates(
        &self,
        cx: &ConnectionTo<Client>,
        session_id: &str,
        updates: Vec<acp::SessionUpdate>,
    ) {
        Self::emit_on(cx, session_id, updates);
    }

    fn emit_on(cx: &ConnectionTo<Client>, session_id: &str, updates: Vec<acp::SessionUpdate>) {
        for update in updates {
            if cx
                .send_notification(SessionNotification::new(session_id.to_owned(), update))
                .is_err()
            {
                // 出站通道关闭：后续更新没有意义，直接停。
                break;
            }
        }
    }
}

/// 运行 ACP agent：起内嵌 core 服务端 + 协议客户端，然后服务 stdio 到 EOF。
///
/// 返回 `Ok` 表示客户端正常断开；core 的关停由调用方（CLI 外层）统一负责。
pub async fn serve(core: Arc<Core>, options: AcpOptions) -> anyhow::Result<()> {
    let (core_side, frontend_side) = transport::duplex(64 * 1024);
    let server = Server::with_options((*core).clone(), ServerOptions::default());
    let connection = server.spawn(core_side);
    let bridge = Arc::new(Bridge::new(options));
    let builder = bridge.moc_client_builder();

    let (client, handshake) = match builder.connect(frontend_side).await {
        Ok(connection) => connection,
        Err(error) => {
            server.shutdown();
            connection.wait().await;
            return Err(error.into());
        }
    };
    if handshake.server.name != "moc-core" {
        anyhow::bail!("连接到非 moc-core 服务：{}", handshake.server.name);
    }
    bridge.attach_moc(client.clone());
    // config/get 只用于模型下拉与 usage 上下文；失败不阻断 ACP 启动。
    match client.call::<ConfigGet>(ConfigGetParams {}).await {
        Ok(config) => {
            let _ = bridge.config.set(Some(config));
        }
        Err(error) => {
            tracing::warn!(?error, "config/get 失败：模型选择与 usage 提示不可用");
            let _ = bridge.config.set(None);
        }
    }

    let result = agent::serve(&bridge).await;
    client.close().await;
    server.shutdown();
    connection.wait().await;
    result
}

//! MCP server 连接管理：并行非阻塞连接、指数退避重连、工具快照与调用。
//!
//! 每个 server 一个监督任务（[`supervise`]），负责连接、监听 `tools/list_changed`
//! 与检测传输关闭；`Peer` 是廉价克隆句柄，调用路径直接从相位快照取用，
//! 不经过任何跨任务通道，保证并行调用的吞吐。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use rmcp::ServiceError;
use rmcp::model::CallToolRequestParams;
use rmcp::service::{Peer, RoleClient};
use serde_json::Value;
use tokio::sync::{mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::client::{Connection, Connector};
use crate::config::{CompiledToolFilter, ServerConfig};
use crate::content::{ToolOutput, map_call_result};
use crate::error::McpError;

/// 调用遇到连接错误后，等待重连成功的最长时长。
const RECONNECT_WAIT: Duration = Duration::from_secs(15);

/// 重连预算与退避参数。
///
/// 默认值即线上语义：连续失败 [`ReconnectPolicy::max_attempts`] 次后进入
/// [`ServerStatus::Failed`]，只有稳定运行满 [`ReconnectPolicy::stable_runtime`]
/// 的连接才清零失败计数——「握手成功但随即退出」的 server 会一路累加，直到放弃。
/// 参数化只为让测试用毫秒级参数验证预算耗尽路径。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ReconnectPolicy {
    /// 连续失败上限；超过即放弃本次启动的监督。
    pub(crate) max_attempts: u32,
    /// 首次退避时长；之后指数增长。
    pub(crate) base_backoff: Duration,
    /// 退避上限。
    pub(crate) max_backoff: Duration,
    /// 连接连续运行满该时长后才清零失败计数。
    pub(crate) stable_runtime: Duration,
    /// 检查传输是否关闭的轮询间隔。
    pub(crate) close_poll: Duration,
}

impl Default for ReconnectPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 10,
            base_backoff: Duration::from_millis(500),
            max_backoff: Duration::from_secs(30),
            stable_runtime: Duration::from_secs(30),
            close_poll: Duration::from_millis(500),
        }
    }
}

impl ReconnectPolicy {
    /// 第 `attempts` 次失败后的退避时长（指数增长、封顶）。
    fn backoff(&self, attempts: u32) -> Duration {
        let factor = 2u32.saturating_pow(attempts.saturating_sub(1).min(16));
        self.base_backoff
            .saturating_mul(factor)
            .min(self.max_backoff)
    }
}

/// 供 core 适配 `Tool` trait 的单个 MCP 工具描述。
#[derive(Debug, Clone)]
pub struct McpToolInfo {
    /// 所属 server 名。
    pub server: String,
    /// server 侧的原始工具名（`tools/call` 线上名称）。
    pub name: String,
    /// 模型可见的限定名：`mcp__<server>__<tool>`。
    pub qualified: String,
    pub title: Option<String>,
    pub description: Option<String>,
    /// MCP `inputSchema`，直接作为 `ToolDef::parameters` 透传。
    pub input_schema: Value,
    /// 服务端声明的输出 schema，供代码模式按需发现。
    pub output_schema: Option<Value>,
    /// `annotations.readOnlyHint`；缺失按可写处理。
    pub read_only: bool,
}

/// server 的对外可见状态。
#[derive(Debug, Clone)]
pub enum ServerStatus {
    /// 正在建立首个连接。
    Connecting,
    Connected {
        tool_count: usize,
    },
    /// 曾连接或仍在尝试；`attempts` 是当前预算内的失败次数。
    Reconnecting {
        attempts: u32,
        error: String,
    },
    /// 重连预算耗尽，放弃直到下次启动。
    Failed {
        error: String,
    },
    Disabled,
}

/// 发往用户的通知；core 会转成 system-reminder 注入会话。
#[derive(Debug, Clone)]
pub struct McpNotice {
    pub server: String,
    pub message: String,
}

/// 每次提示请求取一次的全量快照。
#[derive(Debug, Clone, Default)]
pub struct McpSnapshot {
    pub statuses: BTreeMap<String, ServerStatus>,
    /// 已连接 server 的全部工具，按限定名排序（保证请求内工具顺序稳定）。
    pub tools: Vec<Arc<McpToolInfo>>,
}

enum Phase {
    Connecting,
    Connected {
        peer: Peer<RoleClient>,
        tools: Vec<Arc<McpToolInfo>>,
    },
    Reconnecting {
        attempts: u32,
        error: String,
    },
    Failed {
        error: String,
    },
    Disabled,
}

struct ServerSlot {
    config: ServerConfig,
    /// 相位与代际：任何相位变化都会 bump `generation`，
    /// 调用路径据此等待「断线 → 重连」完成。
    phase: RwLock<Phase>,
    generation: watch::Sender<u64>,
    connector: Connector,
    /// 防止同一 slot 被重复启动监督任务。
    supervising: AtomicBool,
}

impl ServerSlot {
    fn set_phase(&self, phase: Phase) {
        *self.phase.write() = phase;
        self.generation.send_modify(|generation| *generation += 1);
    }
}

/// 进程内共享的 MCP 管理器；按项目构建，随 `Core` 关闭而关闭。
pub struct McpManager {
    shutdown: CancellationToken,
    slots: BTreeMap<String, Arc<ServerSlot>>,
    notices: mpsc::UnboundedSender<McpNotice>,
    notices_rx: tokio::sync::Mutex<Option<mpsc::UnboundedReceiver<McpNotice>>>,
    tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    policy: ReconnectPolicy,
}

impl McpManager {
    /// 构建管理器；调用 [`McpManager::start`] 前不会发起任何连接。
    pub fn new(servers: Vec<ServerConfig>, shutdown: CancellationToken) -> Arc<Self> {
        Self::build(servers, shutdown, |config| {
            crate::client::default_connector(&config.name, &config.transport)
        })
    }

    /// 以自定义连接器构建；测试注入内存 transport 用。
    pub(crate) fn build(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
    ) -> Arc<Self> {
        Self::assemble(servers, shutdown, connector_for, ReconnectPolicy::default())
    }

    /// 以自定义重连参数构建；测试用毫秒级参数验证预算耗尽路径。
    #[cfg(test)]
    pub(crate) fn build_with_policy(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
        policy: ReconnectPolicy,
    ) -> Arc<Self> {
        Self::assemble(servers, shutdown, connector_for, policy)
    }

    fn assemble(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
        policy: ReconnectPolicy,
    ) -> Arc<Self> {
        let (notices, notices_rx) = mpsc::unbounded_channel();
        let mut slots = BTreeMap::new();
        for config in servers {
            let name = config.name.clone();
            let connector = connector_for(&config);
            let (generation, _) = watch::channel(0);
            slots.insert(
                name,
                Arc::new(ServerSlot {
                    config,
                    phase: RwLock::new(Phase::Connecting),
                    generation,
                    connector,
                    supervising: AtomicBool::new(false),
                }),
            );
        }
        Arc::new(Self {
            shutdown,
            slots,
            notices,
            notices_rx: tokio::sync::Mutex::new(Some(notices_rx)),
            tasks: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
            policy,
        })
    }

    /// 为尚未受监督的 server 拉起监督任务（含重连预算耗尽后的重试）。
    pub async fn start(&self) {
        let mut tasks = self.tasks.lock().await;
        for slot in self.slots.values() {
            if slot
                .supervising
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let shutdown = self.shutdown.clone();
                let notices = self.notices.clone();
                let policy = self.policy;
                tasks.spawn(supervise(slot.clone(), shutdown, notices, policy));
            }
        }
    }

    /// 是否没有配置任何 server。
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 取走已累积的通知；通道在构建后长期保留，可反复调用。
    pub async fn drain_notices(&self) -> Vec<McpNotice> {
        let mut guard = self.notices_rx.lock().await;
        let Some(receiver) = guard.as_mut() else {
            return Vec::new();
        };
        let mut notices = Vec::new();
        while let Ok(notice) = receiver.try_recv() {
            notices.push(notice);
        }
        notices
    }

    /// 全量快照：状态表 + 已连接工具（按限定名排序）。
    pub async fn snapshot(&self) -> McpSnapshot {
        let mut statuses = BTreeMap::new();
        let mut tools = Vec::new();
        for (name, slot) in &self.slots {
            match &*slot.phase.read() {
                Phase::Connecting => {
                    statuses.insert(name.clone(), ServerStatus::Connecting);
                }
                Phase::Connected { tools: listed, .. } => {
                    statuses.insert(
                        name.clone(),
                        ServerStatus::Connected {
                            tool_count: listed.len(),
                        },
                    );
                    tools.extend(listed.iter().cloned());
                }
                Phase::Reconnecting { attempts, error } => {
                    statuses.insert(
                        name.clone(),
                        ServerStatus::Reconnecting {
                            attempts: *attempts,
                            error: error.clone(),
                        },
                    );
                }
                Phase::Failed { error } => {
                    statuses.insert(
                        name.clone(),
                        ServerStatus::Failed {
                            error: error.clone(),
                        },
                    );
                }
                Phase::Disabled => {
                    statuses.insert(name.clone(), ServerStatus::Disabled);
                }
            }
        }
        tools.sort_unstable_by(|left, right| left.qualified.cmp(&right.qualified));
        McpSnapshot { statuses, tools }
    }

    /// 按限定名查找工具描述；只覆盖已连接的 server。
    pub fn resolve(&self, qualified: &str) -> Option<Arc<McpToolInfo>> {
        let (server, _) = parse_qualified(qualified)?;
        let slot = self.slots.get(server)?;
        match &*slot.phase.read() {
            Phase::Connected { tools, .. } => tools
                .iter()
                .find(|info| info.qualified == qualified)
                .cloned(),
            _ => None,
        }
    }

    /// 调用一次工具。
    ///
    /// 连接层故障时等待重连（至多 [`RECONNECT_WAIT`]）并重试一次；
    /// 服务端错误与超时不重试，避免副作用型工具被重复执行。
    pub async fn call(
        &self,
        info: &McpToolInfo,
        args: Value,
        cancel: CancellationToken,
    ) -> Result<ToolOutput, McpError> {
        let slot = self
            .slots
            .get(&info.server)
            .ok_or_else(|| McpError::Tool(format!("unknown MCP server `{}`", info.server)))?;
        let mut generation = slot.generation.subscribe();
        let peer = connected_peer(slot).ok_or_else(|| {
            McpError::Transport(format!(
                "server `{}` is not connected (status: {:?})",
                info.server,
                status_of(slot)
            ))
        })?;
        match call_once(&peer, slot, info, args.clone(), cancel.clone()).await {
            Ok(output) => Ok(output),
            Err(error) if error.is_retriable() && !self.shutdown.is_cancelled() => {
                let deadline = Instant::now() + RECONNECT_WAIT;
                loop {
                    if shutdown_or_failed(slot, &self.shutdown) {
                        return Err(McpError::Transport(format!(
                            "server `{}` reconnect failed; call aborted",
                            info.server
                        )));
                    }
                    match tokio::time::timeout_at(deadline.into(), generation.changed()).await {
                        Ok(Ok(())) => match connected_peer(slot) {
                            Some(peer) => {
                                return call_once(&peer, slot, info, args, cancel).await;
                            }
                            // 仍在重连，继续等待代际变化。
                            None => continue,
                        },
                        // 通道关闭说明 slot 已被放弃；立即失败。
                        Ok(Err(_)) => {
                            return Err(McpError::Transport(format!(
                                "server `{}` connection abandoned",
                                info.server
                            )));
                        }
                        Err(_) => {
                            return Err(McpError::Transport(format!(
                                "server `{}` did not reconnect within {}s",
                                info.server,
                                RECONNECT_WAIT.as_secs()
                            )));
                        }
                    }
                }
            }
            Err(error) => Err(error),
        }
    }

    /// 关闭全部连接并等待监督任务退出。
    pub async fn shutdown(&self) {
        self.shutdown.cancel();
        let mut tasks = self.tasks.lock().await;
        while let Some(result) = tasks.join_next().await {
            if let Err(error) = result {
                tracing::debug!(%error, "MCP 监督任务退出异常");
            }
        }
    }
}

fn status_of(slot: &ServerSlot) -> ServerStatus {
    match &*slot.phase.read() {
        Phase::Connecting => ServerStatus::Connecting,
        Phase::Connected { tools, .. } => ServerStatus::Connected {
            tool_count: tools.len(),
        },
        Phase::Reconnecting { attempts, error } => ServerStatus::Reconnecting {
            attempts: *attempts,
            error: error.clone(),
        },
        Phase::Failed { error } => ServerStatus::Failed {
            error: error.clone(),
        },
        Phase::Disabled => ServerStatus::Disabled,
    }
}

fn connected_peer(slot: &ServerSlot) -> Option<Peer<RoleClient>> {
    match &*slot.phase.read() {
        Phase::Connected { peer, .. } => Some(peer.clone()),
        _ => None,
    }
}

fn shutdown_or_failed(slot: &ServerSlot, shutdown: &CancellationToken) -> bool {
    shutdown.is_cancelled()
        || matches!(&*slot.phase.read(), Phase::Failed { .. })
        || slot.generation.is_closed()
}

/// 解析 `mcp__<server>__<tool>`；server 段保证不含 `__`，取首个分隔即可。
pub(crate) fn parse_qualified(qualified: &str) -> Option<(&str, &str)> {
    let rest = qualified.strip_prefix("mcp__")?;
    let (server, tool) = rest.split_once("__")?;
    if server.is_empty() || tool.is_empty() {
        None
    } else {
        Some((server, tool))
    }
}

/// 工具名合法化：限定名只保留 `[A-Za-z0-9_-]`。
pub(crate) fn sanitize_name(raw: &str) -> String {
    let sanitized: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.is_empty() {
        "_".to_owned()
    } else {
        sanitized
    }
}

fn qualified_tool_name(server: &str, tool: &str) -> String {
    format!("mcp__{}__{}", sanitize_name(server), sanitize_name(tool))
}

fn convert_tools(
    server: &str,
    listing: &[rmcp::model::Tool],
    filter: Option<&CompiledToolFilter>,
) -> Vec<Arc<McpToolInfo>> {
    let mut infos: Vec<_> = listing
        .iter()
        .filter(|tool| filter.is_none_or(|filter| filter.admits(tool.name.as_ref())))
        .map(|tool| {
            Arc::new(McpToolInfo {
                server: server.to_owned(),
                name: tool.name.to_string(),
                qualified: qualified_tool_name(server, tool.name.as_ref()),
                title: tool.title.clone(),
                description: tool.description.as_ref().map(|d| d.to_string()),
                input_schema: Value::Object(tool.input_schema.as_ref().clone()),
                output_schema: tool
                    .output_schema
                    .as_ref()
                    .map(|schema| Value::Object(schema.as_ref().clone())),
                read_only: tool
                    .annotations
                    .as_ref()
                    .and_then(|annotations| annotations.read_only_hint)
                    .unwrap_or(false),
            })
        })
        .collect();
    infos.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    infos
}

fn map_service_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(remote) => McpError::Tool(remote.to_string()),
        ServiceError::Timeout { timeout } => McpError::Timeout(timeout.as_secs().max(1)),
        // 其余（transport closed / send 失败 / 响应类型异常）都按连接层处理。
        other => McpError::Transport(other.to_string()),
    }
}

async fn call_once(
    peer: &Peer<RoleClient>,
    slot: &ServerSlot,
    info: &McpToolInfo,
    args: Value,
    cancel: CancellationToken,
) -> Result<ToolOutput, McpError> {
    let Some(arguments) = args.as_object().cloned() else {
        return Err(McpError::Tool(format!(
            "tool `{}` arguments must be a JSON object",
            info.qualified
        )));
    };
    let mut params = CallToolRequestParams::default();
    params.name = info.name.clone().into();
    params.arguments = Some(arguments);

    let timeout = Duration::from_secs(slot.config.call_timeout_secs);
    let result = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(McpError::Cancelled),
        _ = tokio::time::sleep(timeout) => {
            return Err(McpError::Timeout(slot.config.call_timeout_secs));
        }
        result = peer.call_tool(params) => result,
    };
    match result {
        Ok(result) => Ok(map_call_result(&result)),
        Err(error) => Err(map_service_error(error)),
    }
}

/// 单个 server 的终身监督：连接 → 监听变更/关闭 → 重连退避 → 预算耗尽放弃。
async fn supervise(
    slot: Arc<ServerSlot>,
    shutdown: CancellationToken,
    notices: mpsc::UnboundedSender<McpNotice>,
    policy: ReconnectPolicy,
) {
    if !slot.config.enabled {
        slot.set_phase(Phase::Disabled);
        slot.supervising.store(false, Ordering::Release);
        return;
    }
    let server = slot.config.name.clone();
    let connect_timeout = Duration::from_secs(slot.config.connect_timeout_secs);
    let mut attempts: u32 = 0;
    'supervise: loop {
        let (changed_tx, mut changed_rx) = mpsc::unbounded_channel::<()>();
        // 连接 + 列取工具，整体受 connect_timeout 约束。
        let established = async {
            let service = (slot.connector)(changed_tx).await?;
            let peer = service.peer().clone();
            let listing = tokio::time::timeout(connect_timeout, peer.list_all_tools())
                .await
                .map_err(|_| McpError::Timeout(connect_timeout.as_secs().max(1)))?
                .map_err(map_service_error)?;
            Ok::<(Peer<RoleClient>, Vec<Arc<McpToolInfo>>, Connection), McpError>((
                peer,
                convert_tools(&server, &listing, slot.config.tools.as_ref()),
                service,
            ))
        };
        match tokio::time::timeout(connect_timeout, established).await {
            Ok(Ok((peer, tools, mut service))) => {
                let tool_count = tools.len();
                slot.set_phase(Phase::Connected {
                    peer: peer.clone(),
                    tools,
                });
                // 稳定性从握手成功起算：连接尝试耗时长不该算成「稳定运行」
                let connected_at = Instant::now();
                let _ = notices.send(McpNotice {
                    server: server.clone(),
                    message: format!("Connected ({tool_count} tools)"),
                });

                let connection_lost;
                let mut closed_tick = tokio::time::interval(policy.close_poll);
                closed_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => {
                            let _ = service.close_with_timeout(Duration::from_secs(5)).await;
                            slot.supervising.store(false, Ordering::Release);
                            return;
                        }
                        Some(()) = changed_rx.recv() => {
                            match tokio::time::timeout(connect_timeout, peer.list_all_tools()).await {
                                Ok(Ok(listing)) => {
                                    let tools =
                                        convert_tools(&server, &listing, slot.config.tools.as_ref());
                                    let count = tools.len();
                                    let replaced = match &*slot.phase.read() {
                                        Phase::Connected { tools: old, .. } => {
                                            old.len() != count
                                                || old.iter().zip(&tools).any(|(a, b)| {
                                                    a.qualified != b.qualified
                                                })
                                        }
                                        _ => true,
                                    };
                                    if let Phase::Connected { tools: target, .. } =
                                        &mut *slot.phase.write()
                                    {
                                        *target = tools;
                                    }
                                    if replaced {
                                        let _ = notices.send(McpNotice {
                                            server: server.clone(),
                                            message: format!("Tool list updated ({count} tools)"),
                                        });
                                    }
                                }
                                Ok(Err(error)) => {
                                    connection_lost = true;
                                    tracing::warn!(server = %server, %error, "重列工具失败，视为连接中断");
                                    break;
                                }
                                Err(_) => {
                                    connection_lost = true;
                                    tracing::warn!(server = %server, "重列工具超时，视为连接中断");
                                    break;
                                }
                            }
                        }
                        _ = closed_tick.tick() => {
                            if peer.is_transport_closed() {
                                connection_lost = true;
                                break;
                            }
                        }
                    }
                }
                if shutdown.is_cancelled() {
                    slot.supervising.store(false, Ordering::Release);
                    return;
                }
                drop(service);
                if !connection_lost {
                    slot.supervising.store(false, Ordering::Release);
                    return;
                }
                // 只有稳定运行满 `stable_runtime` 的连接才清零预算：握手成功随即退出的
                // server 会一路累加，直到预算耗尽进入 `Failed`。
                if connected_at.elapsed() >= policy.stable_runtime {
                    attempts = 0;
                }
                slot.set_phase(Phase::Reconnecting {
                    attempts,
                    error: "connection interrupted".into(),
                });
                let _ = notices.send(McpNotice {
                    server: server.clone(),
                    message: "Connection interrupted; reconnecting".into(),
                });
            }
            Ok(Err(error)) => {
                slot.set_phase(Phase::Reconnecting {
                    attempts,
                    error: error.to_string(),
                });
                tracing::warn!(server = %server, %error, "MCP 连接失败");
            }
            Err(_) => {
                slot.set_phase(Phase::Reconnecting {
                    attempts,
                    error: format!(
                        "handshake or tool listing exceeded {}s",
                        connect_timeout.as_secs()
                    ),
                });
                tracing::warn!(server = %server, "MCP 连接超时");
            }
        }
        attempts += 1;
        if attempts > policy.max_attempts {
            let error = match &*slot.phase.read() {
                Phase::Reconnecting { error, .. } => error.clone(),
                _ => "unknown error".into(),
            };
            slot.set_phase(Phase::Failed {
                error: error.clone(),
            });
            let _ = notices.send(McpNotice {
                server: server.clone(),
                message: format!("Connection failed; retries abandoned: {error}"),
            });
            break 'supervise;
        }
        let delay = policy.backoff(attempts);
        tokio::select! {
            _ = shutdown.cancelled() => break 'supervise,
            _ = tokio::time::sleep(delay) => {}
        }
    }
    slot.supervising.store(false, Ordering::Release);
}

#[cfg(test)]
mod naming {
    use super::*;

    #[test]
    fn parse_qualified_splits_on_first_separator() {
        assert_eq!(
            parse_qualified("mcp__github__list_issues"),
            Some(("github", "list_issues"))
        );
        assert_eq!(
            parse_qualified("mcp__github__server__nested__tool"),
            Some(("github", "server__nested__tool"))
        );
        assert_eq!(parse_qualified("github__list"), None);
        assert_eq!(parse_qualified("mcp__github__"), None);
        assert_eq!(parse_qualified("mcp____x"), None);
    }

    #[test]
    fn sanitize_keeps_identifier_chars_only() {
        assert_eq!(sanitize_name("get-file.v2"), "get-file_v2");
        assert_eq!(sanitize_name(""), "_");
    }

    #[test]
    fn backoff_is_exponential_and_capped() {
        let policy = ReconnectPolicy::default();
        assert_eq!(policy.backoff(1), Duration::from_millis(500));
        assert_eq!(policy.backoff(2), Duration::from_secs(1));
        assert_eq!(policy.backoff(3), Duration::from_secs(2));
        assert_eq!(policy.backoff(10), policy.max_backoff);
        assert_eq!(policy.max_attempts, 10);
        assert_eq!(policy.stable_runtime, Duration::from_secs(30));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Transport;
    use crate::client::ClientHandlerBridge;
    use rmcp::ServiceExt;
    use rmcp::handler::server::ServerHandler;
    use rmcp::model::{
        CallToolResponse, CallToolResult, ContentBlock, ListToolsResult, ServerCapabilities,
        ServerConfig as RmcpServerInfo, ServerNotification, Tool, ToolsCapability,
    };
    use rmcp::service::{RequestContext, RoleServer};
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;

    const CONNECT: u64 = 5;

    #[derive(Default)]
    struct TestState {
        /// 0：基础工具集；≥1：追加 `extra` 工具（供 list_changed 用例）。
        generation: AtomicUsize,
        /// 触发服务端退出（传输关闭，供重连用例）。
        kill: tokio::sync::Notify,
        connect_count: AtomicUsize,
        /// 一次 `tools/list` 已应答（供「握手成功随即退出」用例同步）。
        listed: tokio::sync::Notify,
    }

    struct TestServer {
        state: Arc<TestState>,
    }

    fn schema(properties: serde_json::Value) -> Arc<rmcp::model::JsonObject> {
        Arc::new(
            json!({"type": "object", "properties": properties, "required": []})
                .as_object()
                .cloned()
                .unwrap(),
        )
    }

    impl ServerHandler for TestServer {
        fn get_info(&self) -> RmcpServerInfo {
            let mut capabilities = ServerCapabilities::default();
            let mut tools = ToolsCapability::default();
            tools.list_changed = Some(true);
            capabilities.tools = Some(tools);
            RmcpServerInfo::new(capabilities)
        }

        async fn list_tools(
            &self,
            _request: Option<rmcp::model::PaginatedRequestParams>,
            _context: RequestContext<RoleServer>,
        ) -> Result<ListToolsResult, rmcp::model::ErrorData> {
            self.state.listed.notify_one();
            let mut tools = vec![
                Tool::new(
                    "echo",
                    "回显文本",
                    schema(json!({"text": {"type": "string"}})),
                ),
                Tool::new("refresh", "触发 list_changed", schema(json!({}))),
                Tool::new("die", "让服务端退出", schema(json!({}))),
            ];
            if self.state.generation.load(Ordering::SeqCst) >= 1 {
                tools.push(Tool::new("extra", "刷新后出现", schema(json!({}))));
            }
            Ok(ListToolsResult::with_all_items(tools))
        }

        async fn call_tool(
            &self,
            request: rmcp::model::CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, rmcp::model::ErrorData> {
            match request.name.as_ref() {
                "echo" => {
                    let text = request
                        .arguments
                        .as_ref()
                        .and_then(|args| args.get("text"))
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("")
                        .to_owned();
                    Ok(
                        CallToolResult::success(vec![ContentBlock::text(format!("echo: {text}"))])
                            .into(),
                    )
                }
                "refresh" => {
                    self.state.generation.store(1, Ordering::SeqCst);
                    let _ = context
                        .peer
                        .send_notification(ServerNotification::ToolListChangedNotification(
                            rmcp::model::ToolListChangedNotification {
                                method: Default::default(),
                                extensions: Default::default(),
                            },
                        ))
                        .await;
                    Ok(CallToolResult::success(vec![ContentBlock::text("ok")]).into())
                }
                "die" => {
                    self.state.kill.notify_waiters();
                    Ok(CallToolResult::success(vec![ContentBlock::text("bye")]).into())
                }
                other => Err(rmcp::model::ErrorData::invalid_params(
                    format!("未知工具 {other}"),
                    None,
                )),
            }
        }
    }

    fn test_connector(state: Arc<TestState>) -> Connector {
        Arc::new(move |tool_list_changed| {
            let state = state.clone();
            Box::pin(async move {
                state.connect_count.fetch_add(1, Ordering::SeqCst);
                let (client_io, server_io) = tokio::io::duplex(1 << 16);
                let serve = TestServer {
                    state: state.clone(),
                }
                .serve(server_io);
                let kill_state = state.clone();
                tokio::spawn(async move {
                    match serve.await {
                        // 服务端握手完成后必须持有句柄，否则 RunningService 被
                        // Drop 会立刻关闭传输——这正是「传输已关闭」的来源。
                        Ok(mut service) => {
                            let _ = kill_state.kill.notified().await;
                            let _ = service.close().await;
                        }
                        Err(error) => eprintln!("[test-server] serve 失败：{error}"),
                    }
                });
                ClientHandlerBridge::new(tool_list_changed)
                    .serve(client_io)
                    .await
                    .map_err(|error| McpError::Transport(error.to_string()))
            })
        })
    }

    /// 「握手成功但随即退出」的连接器：先让客户端完成列出工具，再关掉传输。
    ///
    /// 这正是重连预算必须能耗尽的场景：若握手成功就清零预算，监督任务会以最小退避
    /// 无限重启子进程，`Phase::Failed` 永不出现。
    fn flapping_connector(state: Arc<TestState>) -> Connector {
        Arc::new(move |tool_list_changed| {
            let state = Arc::clone(&state);
            Box::pin(async move {
                state.connect_count.fetch_add(1, Ordering::SeqCst);
                let (client_io, server_io) = tokio::io::duplex(1 << 16);
                let server_state = Arc::clone(&state);
                tokio::spawn(async move {
                    let serving = TestServer {
                        state: Arc::clone(&server_state),
                    }
                    .serve(server_io);
                    if let Ok(service) = serving.await {
                        // 等本次 tools/list 应答完再退出，保证走的是「握手成功」路径
                        server_state.listed.notified().await;
                        drop(service);
                    }
                });
                ClientHandlerBridge::new(tool_list_changed)
                    .serve(client_io)
                    .await
                    .map_err(|error| McpError::Transport(error.to_string()))
            })
        })
    }

    /// 小参数策略：毫秒级退避与轮询，让预算耗尽路径能在测试里跑完。
    fn fast_policy(max_attempts: u32, stable_runtime: Duration) -> ReconnectPolicy {
        ReconnectPolicy {
            max_attempts,
            base_backoff: Duration::from_millis(10),
            max_backoff: Duration::from_millis(10),
            stable_runtime,
            close_poll: Duration::from_millis(10),
        }
    }

    fn test_server(name: &str, enabled: bool) -> ServerConfig {
        ServerConfig {
            name: name.to_owned(),
            transport: Transport::Stdio {
                command: String::new(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            },
            enabled,
            tools: None,
            connect_timeout_secs: CONNECT,
            call_timeout_secs: CONNECT,
        }
    }

    async fn connected_tools(manager: &McpManager, timeout: Duration) -> Vec<Arc<McpToolInfo>> {
        let deadline = Instant::now() + timeout;
        loop {
            let snapshot = manager.snapshot().await;
            if !snapshot.tools.is_empty() {
                return snapshot.tools;
            }
            if Instant::now() >= deadline {
                panic!("server 未在时限内连接并产出工具：{:?}", snapshot.statuses);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test]
    async fn connects_lists_calls_and_reports_images() {
        let state = Arc::new(TestState::default());
        let manager = McpManager::build(
            vec![test_server("test", true)],
            CancellationToken::new(),
            |_| test_connector(state.clone()),
        );
        manager.start().await;
        let tools = connected_tools(&manager, Duration::from_secs(5)).await;
        assert_eq!(tools.len(), 3);
        let echo = tools.iter().find(|t| t.name == "echo").unwrap();
        assert_eq!(echo.qualified, "mcp__test__echo");
        assert!(!echo.read_only);

        let output = manager
            .call(echo, json!({"text": "你好"}), CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(output.text, "echo: 你好");

        let snapshot = manager.snapshot().await;
        assert!(matches!(
            snapshot.statuses.get("test"),
            Some(ServerStatus::Connected { tool_count: 3 })
        ));
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn list_changed_notification_refreshes_tools() {
        let state = Arc::new(TestState::default());
        let manager = McpManager::build(
            vec![test_server("test", true)],
            CancellationToken::new(),
            |_| test_connector(state.clone()),
        );
        manager.start().await;
        let tools = connected_tools(&manager, Duration::from_secs(5)).await;
        assert_eq!(tools.len(), 3);

        let refresh = tools.iter().find(|t| t.name == "refresh").unwrap();
        manager
            .call(refresh, json!({}), CancellationToken::new())
            .await
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let count = manager.snapshot().await.tools.len();
            if count == 4 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "list_changed 后工具数未更新：{count}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let notices = manager.drain_notices().await;
        assert!(
            notices
                .iter()
                .any(|notice| notice.message.contains("Tool list updated")),
            "expected a tool-list update notice: {notices:?}"
        );
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn transport_death_triggers_reconnect() {
        let state = Arc::new(TestState::default());
        let manager = McpManager::build(
            vec![test_server("test", true)],
            CancellationToken::new(),
            |_| test_connector(state.clone()),
        );
        manager.start().await;
        let tools = connected_tools(&manager, Duration::from_secs(5)).await;

        let die = tools.iter().find(|t| t.name == "die").unwrap();
        manager
            .call(die, json!({}), CancellationToken::new())
            .await
            .unwrap();

        // 服务端退出 → 传输关闭 → 重连；期间调用应报连接错误而非 panic。
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let count = state.connect_count.load(Ordering::SeqCst);
            if count >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "未发生重连：{count} 次连接");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let _ = connected_tools(&manager, Duration::from_secs(5)).await;
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn disabled_server_never_connects_and_reports_status() {
        let state = Arc::new(TestState::default());
        let manager = McpManager::build(
            vec![test_server("off", false)],
            CancellationToken::new(),
            |_| test_connector(state.clone()),
        );
        manager.start().await;
        tokio::time::sleep(Duration::from_millis(300)).await;
        let snapshot = manager.snapshot().await;
        assert_eq!(snapshot.tools.len(), 0);
        assert!(matches!(
            snapshot.statuses.get("off"),
            Some(ServerStatus::Disabled)
        ));
        assert_eq!(state.connect_count.load(Ordering::SeqCst), 0);
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn tool_deny_filter_hides_matching_tools() {
        let state = Arc::new(TestState::default());
        let mut config = test_server("test", true);
        config.tools = crate::config::ToolFilter {
            allow: Vec::new(),
            deny: vec!["refresh".into(), "die".into()],
        }
        .compile()
        .unwrap();
        let manager = McpManager::build(vec![config], CancellationToken::new(), |_| {
            test_connector(state.clone())
        });
        manager.start().await;
        let tools = connected_tools(&manager, Duration::from_secs(5)).await;
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "echo");
        manager.shutdown().await;
    }

    #[tokio::test]
    async fn call_before_connect_fails_gracefully() {
        let state = Arc::new(TestState::default());
        let manager = McpManager::build(
            vec![test_server("test", true)],
            CancellationToken::new(),
            |_| test_connector(state.clone()),
        );
        // 未 start：slot 保持 Connecting，调用应得到结构化错误。
        let info = McpToolInfo {
            server: "test".into(),
            name: "echo".into(),
            qualified: "mcp__test__echo".into(),
            title: None,
            description: None,
            input_schema: json!({"type": "object"}),
            output_schema: None,
            read_only: false,
        };
        let result = manager
            .call(&info, json!({}), CancellationToken::new())
            .await;
        assert!(matches!(result, Err(McpError::Transport(_))));
        manager.shutdown().await;
    }

    /// 握手成功但立即退出的 server 必须耗尽重连预算，而不是无限重启。
    #[tokio::test]
    async fn handshake_then_immediate_exit_exhausts_reconnect_budget() {
        let state = Arc::new(TestState::default());
        let manager = McpManager::build_with_policy(
            vec![test_server("flap", true)],
            CancellationToken::new(),
            |_| flapping_connector(Arc::clone(&state)),
            fast_policy(1, Duration::from_secs(30)),
        );
        manager.start().await;

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let snapshot = manager.snapshot().await;
            if matches!(
                snapshot.statuses.get("flap"),
                Some(ServerStatus::Failed { .. })
            ) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "重连预算未耗尽，仍在重启：{:?}（已连接 {} 次）",
                snapshot.statuses,
                state.connect_count.load(Ordering::SeqCst)
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        let connects = state.connect_count.load(Ordering::SeqCst);
        assert!(connects <= 3, "预算耗尽后不得继续重启：{connects} 次连接");
        manager.shutdown().await;
    }

    /// 稳定运行满 `stable_runtime` 的连接才清零预算：达标后掉线继续重连，不进入 Failed。
    #[tokio::test]
    async fn stable_connection_resets_the_reconnect_budget() {
        let state = Arc::new(TestState::default());
        // stable_runtime = 0：每次连接都算「稳定」，预算每轮清零
        let manager = McpManager::build_with_policy(
            vec![test_server("flap", true)],
            CancellationToken::new(),
            |_| flapping_connector(Arc::clone(&state)),
            fast_policy(1, Duration::ZERO),
        );
        manager.start().await;

        let deadline = Instant::now() + Duration::from_secs(5);
        while state.connect_count.load(Ordering::SeqCst) < 3 {
            assert!(Instant::now() < deadline, "稳定期达标后应当继续重连");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert!(
            !matches!(
                manager.snapshot().await.statuses.get("flap"),
                Some(ServerStatus::Failed { .. })
            ),
            "预算已清零，不该进入 Failed"
        );
        manager.shutdown().await;
    }
}

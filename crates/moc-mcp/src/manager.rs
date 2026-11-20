//! MCP server 连接管理：并行非阻塞连接、指数退避重连、工具快照与调用。
//!
//! 每个 server 一个监督任务（[`supervise`]），负责连接、监听 `tools/list_changed`
//! 与检测传输关闭；`Peer` 是廉价克隆句柄，调用路径直接从相位快照取用，
//! 不经过任何跨任务通道，保证并行调用的吞吐。同一 server 的在飞调用数由
//! 每 slot 信号量约束（默认 1，见 [`ServerSlot::calls`]），跨 server 不设限。

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use parking_lot::RwLock;
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, InputResponses, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RequestHandle, RoleClient};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::client::{Connection, Connector};
use crate::config::{CompiledToolFilter, ServerConfig};
use crate::content::{ToolOutput, map_call_result};
use crate::error::McpError;
use crate::mrtr::{InputFulfiller, default_fulfiller, has_requests, translate};

/// 只有 `requestState` 没有 `inputRequests` 的 MRTR 轮次之间的最小间隔。
const STATE_ONLY_BACKOFF: Duration = Duration::from_millis(50);

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
    /// 握手说明仅供按需读取；共享正文，避免每次快照复制长说明。
    pub instructions: BTreeMap<String, Arc<str>>,
}

enum Phase {
    Connecting,
    Connected {
        peer: Peer<RoleClient>,
        tools: Vec<Arc<McpToolInfo>>,
        instructions: Option<Arc<str>>,
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
    /// 同一 server 的在飞调用上限，permit 数取自 `config.max_concurrent_calls`。
    ///
    /// 默认 1（串行）：声称只读、实际会改自身状态的 server 被并发调用会搞乱，
    /// 串行化由本地机制保证而非服务端声明；已知无状态的 server 可在配置里调高。
    /// 调用方（core 调度层）据此把 MCP 工具视为可并行——跨 server 并发不受影响。
    calls: Arc<Semaphore>,
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
    /// 调用时限按实例注入；None 按各 server 的 `call_timeout_secs` 生效。
    /// 测试构造毫秒级时限，不碰全局态，并行测试互不干扰。
    call_timeout: Option<Duration>,
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
        Self::assemble(
            servers,
            shutdown,
            connector_for,
            ReconnectPolicy::default(),
            None,
        )
    }

    /// 以自定义重连参数构建；测试用毫秒级参数验证预算耗尽路径。
    #[cfg(test)]
    pub(crate) fn build_with_policy(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
        policy: ReconnectPolicy,
    ) -> Arc<Self> {
        Self::assemble(servers, shutdown, connector_for, policy, None)
    }

    /// 以自定义调用时限构建；测试用毫秒级时限覆盖真实超时路径。
    #[cfg(test)]
    pub(crate) fn build_with_call_timeout(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
        call_timeout: Duration,
    ) -> Arc<Self> {
        Self::assemble(
            servers,
            shutdown,
            connector_for,
            ReconnectPolicy::default(),
            Some(call_timeout),
        )
    }

    fn assemble(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
        policy: ReconnectPolicy,
        call_timeout: Option<Duration>,
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
                    calls: Arc::new(Semaphore::new(config.max_concurrent_calls)),
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
            call_timeout,
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
                slot.set_phase(if slot.config.enabled {
                    Phase::Connecting
                } else {
                    Phase::Disabled
                });
                let shutdown = self.shutdown.clone();
                let notices = self.notices.clone();
                let policy = self.policy;
                tasks.spawn(supervise(slot.clone(), shutdown, notices, policy));
            }
        }
    }

    /// 并行等待首次连接尝试结束；失败的 server 不阻塞其它能力，取消立即返回。
    /// 各 server 使用自己的握手时限，不把等待时间串行相加。
    pub async fn wait_ready(&self, cancel: &CancellationToken) -> Result<(), McpError> {
        let ready = futures_util::future::join_all(self.slots.values().map(|slot| async move {
            let mut generation = slot.generation.subscribe();
            let wait = async {
                while matches!(&*slot.phase.read(), Phase::Connecting) {
                    if generation.changed().await.is_err() {
                        break;
                    }
                }
            };
            let _ =
                tokio::time::timeout(Duration::from_secs(slot.config.connect_timeout_secs), wait)
                    .await;
        }));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            _ = self.shutdown.cancelled() => Err(McpError::Shutdown),
            _ = ready => Ok(()),
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
        let mut instructions = BTreeMap::new();
        for (name, slot) in &self.slots {
            match &*slot.phase.read() {
                Phase::Connecting => {
                    statuses.insert(name.clone(), ServerStatus::Connecting);
                }
                Phase::Connected {
                    tools: listed,
                    instructions: text,
                    ..
                } => {
                    statuses.insert(
                        name.clone(),
                        ServerStatus::Connected {
                            tool_count: listed.len(),
                        },
                    );
                    tools.extend(listed.iter().cloned());
                    if let Some(text) = text {
                        instructions.insert(name.clone(), Arc::clone(text));
                    }
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
        McpSnapshot {
            statuses,
            tools,
            instructions,
        }
    }

    /// 调用一次工具。
    ///
    /// 同一 server 的在飞调用数受 slot 信号量约束（默认 1）。连接由监督任务重建，
    /// 已发送调用绝不自动重放：响应丢失时无法确定服务端是否已提交副作用。
    ///
    /// `input` 是 MRTR（SEP-2322）履行端：server 返回 `input_required` 时逐键
    /// 履行 `inputRequests` 并原样回显 `requestState` 重试。`None` 时使用
    /// 默认兜底（elicitation decline、空 roots、sampling 报错）。
    pub async fn call(
        &self,
        info: &McpToolInfo,
        args: Value,
        cancel: CancellationToken,
        input: Option<&InputFulfiller>,
    ) -> Result<ToolOutput, McpError> {
        let slot = self
            .slots
            .get(&info.server)
            .ok_or_else(|| McpError::Tool(format!("unknown MCP server `{}`", info.server)))?;
        // 信号量随 manager 终身存活，acquire 只会因取消分支返回。
        let _permit = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
            _ = self.shutdown.cancelled() => return Err(McpError::Cancelled),
            permit = slot.calls.acquire() => permit.expect("server 信号量不会关闭"),
        };
        let timeout = self
            .call_timeout
            .unwrap_or_else(|| Duration::from_secs(slot.config.call_timeout_secs));
        let peer = wait_peer(slot, &cancel, &self.shutdown, timeout).await?;
        call_once(
            &peer,
            slot,
            info,
            args,
            cancel,
            &self.shutdown,
            input,
            self.call_timeout,
        )
        .await
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

async fn wait_peer(
    slot: &ServerSlot,
    cancel: &CancellationToken,
    shutdown: &CancellationToken,
    timeout: Duration,
) -> Result<Peer<RoleClient>, McpError> {
    let mut generation = slot.generation.subscribe();
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        match &*slot.phase.read() {
            Phase::Connected { peer, .. } => return Ok(peer.clone()),
            Phase::Failed { error } => {
                return Err(McpError::Transport(format!(
                    "server `{}` is not connected (status: {:?})",
                    slot.config.name,
                    ServerStatus::Failed {
                        error: error.clone(),
                    }
                )));
            }
            Phase::Disabled => {
                return Err(McpError::Transport(format!(
                    "server `{}` is not connected (status: {:?})",
                    slot.config.name,
                    ServerStatus::Disabled
                )));
            }
            Phase::Connecting | Phase::Reconnecting { .. } => {}
        }

        tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
            _ = shutdown.cancelled() => return Err(McpError::Cancelled),
            _ = tokio::time::sleep_until(deadline) => {
                return Err(McpError::Transport(format!(
                    "timed out waiting for server `{}` to reconnect after {:?}",
                    slot.config.name,
                    timeout
                )));
            }
            changed = generation.changed() => {
                if changed.is_err() {
                    return Err(McpError::Transport(format!(
                        "server `{}` connection status is unavailable",
                        slot.config.name
                    )));
                }
            }
        }
    }
}

/// 两段都按 UTF-8 字节编码 `_`，避免 server 尾部下划线与分隔符产生碰撞。
fn qualified_tool_name(server: &str, tool: &str) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = String::with_capacity(7 + server.len() + tool.len());
    name.push_str("mcp");
    for segment in [server, tool] {
        name.push_str("__");
        if segment.is_empty() {
            name.push('_');
        }
        for byte in segment.bytes() {
            if byte.is_ascii_alphanumeric() || byte == b'-' {
                name.push(char::from(byte));
            } else {
                name.push('_');
                name.push(char::from(HEX[usize::from(byte >> 4)]));
                name.push(char::from(HEX[usize::from(byte & 15)]));
            }
        }
    }
    name
}

fn convert_tools(
    server: &str,
    listing: &[rmcp::model::Tool],
    filter: Option<&CompiledToolFilter>,
) -> Result<Vec<Arc<McpToolInfo>>, McpError> {
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
    if let Some(pair) = infos.windows(2).find(|pair| pair[0].name == pair[1].name) {
        return Err(McpError::Tool(format!(
            "server `{server}` listed duplicate tool {:?}",
            pair[0].name
        )));
    }
    Ok(infos)
}

fn map_service_error(error: ServiceError) -> McpError {
    match error {
        ServiceError::McpError(remote) => McpError::Tool(remote.to_string()),
        ServiceError::Timeout { timeout } => McpError::Timeout(timeout.as_secs().max(1)),
        ServiceError::Cancelled { .. } => McpError::Cancelled,
        ServiceError::UnexpectedResponse => McpError::Tool("unexpected MCP response".into()),
        other => McpError::Transport(other.to_string()),
    }
}

/// 上层 select 可以直接丢弃工具 future；仍须通知服务端取消已发出的请求。
struct PendingCall(Option<RequestHandle<RoleClient>>);

impl PendingCall {
    async fn cancel(&mut self, reason: &str) {
        if let Some(request) = self.0.take() {
            // 即使外层再次丢弃 future，已启动的取消通知也会独立完成。
            let reason = reason.to_owned();
            let _ = tokio::spawn(async move {
                let _ = tokio::time::timeout(Duration::from_secs(1), request.cancel(Some(reason)))
                    .await;
            })
            .await;
        }
    }
}

impl Drop for PendingCall {
    fn drop(&mut self) {
        if let Some(request) = self.0.take() {
            tokio::spawn(async move {
                let _ = tokio::time::timeout(
                    Duration::from_secs(1),
                    request.cancel(Some("caller stopped waiting".into())),
                )
                .await;
            });
        }
    }
}

/// 一轮 `tools/call` + 可能的 MRTR 后续轮。
///
/// 每次 wire 往返都受 `call_timeout_secs` 与 `cancel` 约束；履行 `inputRequests`
/// 的等待只受 `cancel` 约束（与 ask 一致——用户思考不计入线超时）。`requestState`
/// 按规范逐字节回显；超过 [`rmcp::model::DEFAULT_MRTR_MAX_ROUNDS`] 轮仍无最终结果
/// 按服务端行为异常报错。
async fn call_once(
    peer: &Peer<RoleClient>,
    slot: &ServerSlot,
    info: &McpToolInfo,
    args: Value,
    cancel: CancellationToken,
    shutdown: &CancellationToken,
    input: Option<&InputFulfiller>,
    call_timeout: Option<Duration>,
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

    let timeout =
        call_timeout.unwrap_or_else(|| Duration::from_secs(slot.config.call_timeout_secs));
    let fallback;
    let fulfiller = match input {
        Some(fulfiller) => fulfiller,
        None => {
            fallback = default_fulfiller();
            &fallback
        }
    };
    for _round in 0..rmcp::model::DEFAULT_MRTR_MAX_ROUNDS {
        let deadline = tokio::time::Instant::now() + timeout;
        let request = ClientRequest::CallToolRequest(CallToolRequest::new(params.clone()));
        let handle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(McpError::Cancelled),
            _ = shutdown.cancelled() => return Err(McpError::Shutdown),
            _ = tokio::time::sleep_until(deadline) => return Err(McpError::Timeout(slot.config.call_timeout_secs)),
            result = peer.send_request_with_option(request, PeerRequestOptions::no_options()) => result.map_err(map_service_error)?,
        };
        let mut pending = PendingCall(Some(handle));
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                pending.cancel("caller cancelled").await;
                return Err(McpError::Cancelled);
            }
            _ = shutdown.cancelled() => {
                pending.cancel("MCP manager closed").await;
                return Err(McpError::Shutdown);
            }
            _ = tokio::time::sleep_until(deadline) => {
                pending.cancel("request timeout").await;
                return Err(McpError::Timeout(slot.config.call_timeout_secs));
            }
            response = &mut pending.0.as_mut().expect("请求尚未完成").rx => {
                response.unwrap_or_else(|_| Err(ServiceError::TransportClosed))
            }
        };
        pending.0 = None;
        let required = match response.map_err(|error| match map_service_error(error) {
            McpError::Transport(message) => McpError::OutcomeUnknown(message),
            error => error,
        })? {
            ServerResult::CallToolResult(result) => return Ok(map_call_result(&result)),
            ServerResult::InputRequiredResult(required) => required,
            // tasks 扩展（SEP-2663）未声明：spec 不允许 server 在此返回 task。
            ServerResult::CreateTaskResult(_) => {
                return Err(McpError::Tool(format!(
                    "tool `{}` returned resultType `task`, which moc does not support",
                    info.qualified
                )));
            }
            // rmcp 标注 non_exhaustive：未知 resultType 按服务端行为异常报错。
            _ => {
                return Err(McpError::Tool(format!(
                    "tool `{}` returned an unsupported result type",
                    info.qualified
                )));
            }
        };
        let had_requests = has_requests(required.input_requests.as_ref());
        let mut responses = InputResponses::new();
        if let Some(requests) = required.input_requests {
            for (key, request) in requests {
                let pending = translate(&request)?;
                let response = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(McpError::Cancelled),
                    _ = shutdown.cancelled() => return Err(McpError::Shutdown),
                    response = fulfiller(pending) => response,
                }?;
                responses.insert(key, response);
            }
        }
        params.input_responses = (!responses.is_empty()).then_some(responses);
        params.request_state = required.request_state;
        if !had_requests {
            // state-only 轮：server 借 requestState 做无状态续跑，
            // 立即重试即可，加一拍退避防两端忙轮询。
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(McpError::Cancelled),
                _ = shutdown.cancelled() => return Err(McpError::Shutdown),
                _ = tokio::time::sleep(STATE_ONLY_BACKOFF) => {}
            }
        }
    }
    Err(McpError::Tool(format!(
        "tool `{}` kept requesting input after {} rounds",
        info.qualified,
        rmcp::model::DEFAULT_MRTR_MAX_ROUNDS
    )))
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
        let (changed_tx, mut changed_rx) = mpsc::channel::<()>(1);
        // 连接 + 列取工具，整体受 connect_timeout 约束。
        let established = async {
            let service = (slot.connector)(changed_tx).await?;
            let peer = service.peer().clone();
            let listing = peer.list_all_tools().await.map_err(map_service_error)?;
            Ok::<(Peer<RoleClient>, Vec<Arc<McpToolInfo>>, Connection), McpError>((
                peer,
                convert_tools(&server, &listing, slot.config.tools.as_ref())?,
                service,
            ))
        };
        let established = tokio::select! {
            biased;
            _ = shutdown.cancelled() => break 'supervise,
            result = tokio::time::timeout(connect_timeout, established) => result,
        };
        match established {
            Ok(Ok((peer, tools, mut service))) => {
                let tool_count = tools.len();
                let instructions = peer
                    .peer_info()
                    .and_then(|info| info.instructions.as_deref().map(Arc::from));
                slot.set_phase(Phase::Connected {
                    peer: peer.clone(),
                    tools,
                    instructions,
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
                            slot.set_phase(Phase::Disabled);
                            let _ = service.close_with_timeout(Duration::from_secs(5)).await;
                            slot.supervising.store(false, Ordering::Release);
                            return;
                        }
                        Some(()) = changed_rx.recv() => {
                            let listing = tokio::select! {
                                biased;
                                _ = shutdown.cancelled() => break 'supervise,
                                result = tokio::time::timeout(connect_timeout, peer.list_all_tools()) => result,
                            };
                            match listing {
                                Ok(Ok(listing)) => {
                                    let tools = match convert_tools(&server, &listing, slot.config.tools.as_ref()) {
                                        Ok(tools) => tools,
                                        Err(error) => {
                                            let message = error.to_string();
                                            slot.set_phase(Phase::Failed { error: message.clone() });
                                            let _ = notices.send(McpNotice { server: server.clone(), message });
                                            break 'supervise;
                                        }
                                    };
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
            Ok(Err(McpError::Tool(message))) => {
                slot.set_phase(Phase::Failed {
                    error: message.clone(),
                });
                let _ = notices.send(McpNotice {
                    server: server.clone(),
                    message,
                });
                break 'supervise;
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
    if shutdown.is_cancelled() {
        slot.set_phase(Phase::Disabled);
    }
    slot.supervising.store(false, Ordering::Release);
}

#[cfg(test)]
mod naming;

#[cfg(test)]
mod tests;

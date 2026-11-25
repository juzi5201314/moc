//! MCP server 连接管理：并行非阻塞连接、指数退避重连、工具快照与调用。
//!
//! 每个 server 一个监督任务（[`supervise`]），负责连接、监听 `tools/list_changed`
//! 与检测传输关闭；`Peer` 是廉价克隆句柄，调用路径直接从相位快照取用，
//! 不经过任何跨任务通道，保证并行调用的吞吐。同一 server 的在飞调用数由
//! 每 slot 信号量约束（默认 1，见 [`ServerSlot::calls`]），跨 server 不设限。

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::{Mutex, RwLock};
use rmcp::ServiceError;
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientRequest, InputResponses, ServerResult,
};
use rmcp::service::{Peer, PeerRequestOptions, RequestHandle, RoleClient};
use serde_json::Value;
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_util::sync::CancellationToken;

use crate::client::{Connection, Connector, StderrTail};
use crate::config::{CompiledToolFilter, ServerConfig};
use crate::content::{ToolOutput, map_call_result};
use crate::error::McpError;
use crate::mrtr::{InputFulfiller, default_fulfiller, has_requests, translate};

/// 只有 `requestState` 没有 `inputRequests` 的 MRTR 轮次之间的最小间隔。
const STATE_ONLY_BACKOFF: Duration = Duration::from_millis(50);
/// 关闭单条传输（含 HTTP session DELETE）的最长等待；超时即放手，避免拖慢保存或退出。
const GRACEFUL_CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| {
            u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX)
        })
}

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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// 面向前端与诊断命令的单 server 运行态视图。
#[derive(Debug, Clone)]
pub struct McpServerState {
    pub name: String,
    pub transport: &'static str,
    pub target: String,
    pub status: ServerStatus,
    /// 当前握手尝试开始的 UNIX 毫秒时间戳；非连接中为 `None`。
    pub started_at_ms: Option<u64>,
    /// stdio 子进程最近的 stderr 行（有界环形缓冲），用于定位卡死或启动报错。
    pub recent_stderr: Vec<String>,
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
    /// 相位与代际：任何相位变化都会 bump `generation` 与管理器级 `status_version`，
    /// 调用路径据此等待「断线 → 重连」完成，前端据此接收实时状态推送。
    phase: RwLock<Phase>,
    generation: watch::Sender<u64>,
    status_version: watch::Sender<u64>,
    connector: Connector,
    /// 同一 server 的在飞调用上限，permit 数取自 `config.max_concurrent_calls`。
    ///
    /// 默认 1（串行）：声称只读、实际会改自身状态的 server 被并发调用会搞乱，
    /// 串行化由本地机制保证而非服务端声明；已知无状态的 server 可在配置里调高。
    /// 调用方（core 调度层）据此把 MCP 工具视为可并行——跨 server 并发不受影响。
    calls: Arc<Semaphore>,
    /// 是否已经由 `start()` 或 `reconnect()` 发起过监督：
    /// 预算耗尽进入 `Failed` 后，`start()` 不会自动复活，只有显式 `reconnect()` 才重启。
    started: AtomicBool,
    /// 防止同一 slot 被重复启动监督任务。
    supervising: AtomicBool,
    /// 监督代际号：手动重连递增此号，旧监督任务的延迟清理绝不能覆盖新代际的相位。
    epoch: AtomicU64,
    /// 本 slot 当前监督任务的取消令牌（`shutdown` 的子令牌），手动重连时替换。
    slot_cancel: Mutex<CancellationToken>,
    /// 当前握手尝试开始的 UNIX 毫秒时间戳；非连接中为 0。
    attempt_started_at_ms: AtomicU64,
    /// stdio stderr 尾部缓冲。
    stderr_tail: StderrTail,
}

impl ServerSlot {
    fn set_phase(&self, phase: Phase) {
        *self.phase.write() = phase;
        self.generation.send_modify(|generation| *generation += 1);
        self.status_version.send_modify(|version| *version += 1);
    }

    fn set_phase_for_epoch(&self, epoch: u64, phase: Phase) -> bool {
        if self.epoch.load(Ordering::Acquire) != epoch {
            return false;
        }
        self.set_phase(phase);
        true
    }

    fn status_snapshot(&self) -> ServerStatus {
        match &*self.phase.read() {
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
}

/// 进程内共享的 MCP 管理器；按项目构建，随 `Core` 关闭而关闭。
pub struct McpManager {
    shutdown: CancellationToken,
    slots: BTreeMap<String, Arc<ServerSlot>>,
    status_version: watch::Sender<u64>,
    tasks: tokio::sync::Mutex<tokio::task::JoinSet<()>>,
    policy: ReconnectPolicy,
    /// 调用时限按实例注入；None 按各 server 的 `call_timeout_secs` 生效。
    /// 测试构造毫秒级时限，不碰全局态，并行测试互不干扰。
    call_timeout: Option<Duration>,
}

impl McpManager {
    /// 构建管理器；调用 [`McpManager::start`] 前不会发起任何连接。
    pub fn new(servers: Vec<ServerConfig>, shutdown: CancellationToken) -> Arc<Self> {
        Self::assemble(
            servers,
            shutdown,
            |config, stderr_tail| {
                crate::client::default_connector(
                    &config.name,
                    &config.transport,
                    Duration::from_secs(config.connect_timeout_secs),
                    stderr_tail,
                )
            },
            ReconnectPolicy::default(),
            None,
        )
    }

    /// 以自定义连接器构建；测试注入内存 transport 用。
    #[cfg(test)]
    pub(crate) fn build(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig) -> Connector,
    ) -> Arc<Self> {
        Self::assemble(
            servers,
            shutdown,
            |config, _| connector_for(config),
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
        Self::assemble(
            servers,
            shutdown,
            |config, _| connector_for(config),
            policy,
            None,
        )
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
            |config, _| connector_for(config),
            ReconnectPolicy::default(),
            Some(call_timeout),
        )
    }

    fn assemble(
        servers: Vec<ServerConfig>,
        shutdown: CancellationToken,
        connector_for: impl Fn(&ServerConfig, StderrTail) -> Connector,
        policy: ReconnectPolicy,
        call_timeout: Option<Duration>,
    ) -> Arc<Self> {
        let (status_version, _) = watch::channel(0u64);
        let mut slots = BTreeMap::new();
        for config in servers {
            let name = config.name.clone();
            let stderr_tail: StderrTail = Arc::new(Mutex::new(VecDeque::new()));
            let connector = connector_for(&config, Arc::clone(&stderr_tail));
            let (generation, _) = watch::channel(0);
            let initial_phase = if config.enabled {
                Phase::Connecting
            } else {
                Phase::Disabled
            };
            slots.insert(
                name,
                Arc::new(ServerSlot {
                    calls: Arc::new(Semaphore::new(config.max_concurrent_calls)),
                    config,
                    phase: RwLock::new(initial_phase),
                    generation,
                    status_version: status_version.clone(),
                    connector,
                    started: AtomicBool::new(false),
                    supervising: AtomicBool::new(false),
                    epoch: AtomicU64::new(0),
                    slot_cancel: Mutex::new(shutdown.child_token()),
                    attempt_started_at_ms: AtomicU64::new(0),
                    stderr_tail,
                }),
            );
        }
        Arc::new(Self {
            shutdown,
            slots,
            status_version,
            tasks: tokio::sync::Mutex::new(tokio::task::JoinSet::new()),
            policy,
            call_timeout,
        })
    }

    fn reap_finished(tasks: &mut tokio::task::JoinSet<()>) {
        while let Some(result) = tasks.try_join_next() {
            if let Err(error) = result {
                tracing::debug!(%error, "MCP 监督任务退出异常");
            }
        }
    }

    fn spawn_supervisor(&self, tasks: &mut tokio::task::JoinSet<()>, slot: &Arc<ServerSlot>) {
        let epoch = slot.epoch.fetch_add(1, Ordering::AcqRel) + 1;
        let cancel = {
            let mut guard = slot.slot_cancel.lock();
            guard.cancel();
            let next = self.shutdown.child_token();
            *guard = next.clone();
            next
        };
        slot.supervising.store(true, Ordering::Release);
        slot.attempt_started_at_ms
            .store(now_ms(), Ordering::Release);
        slot.set_phase(Phase::Connecting);
        tasks.spawn(supervise(
            Arc::clone(slot),
            epoch,
            cancel,
            self.shutdown.clone(),
            self.policy,
        ));
    }

    /// 判断当前管理器的生效配置是否与 `servers` 完全一致。
    ///
    /// 前置条件：`servers` 按 `name` 严格升序排列且无重名（`moc_mcp::resolve_all` 从
    /// `BTreeMap` 产出，天然满足该不变量），从而无需构造临时集合即可 O(n) 逐项比对。
    pub fn matches_configs(&self, servers: &[ServerConfig]) -> bool {
        debug_assert!(
            servers.windows(2).all(|pair| pair[0].name < pair[1].name),
            "servers must be strictly sorted by unique name"
        );
        self.slots.len() == servers.len()
            && self
                .slots
                .values()
                .zip(servers)
                .all(|(slot, config)| slot.config == *config)
    }

    /// 为尚未启动过的 enabled server 拉起监督任务。
    ///
    /// 已耗尽重连预算进入 `Failed` 的 server 不会由 `start()` 自动复活，
    /// 避免坏 server 在后续每次提示时再次触发连接风暴或阻塞；恢复请调用 [`Self::reconnect`]。
    pub async fn start(&self) {
        if self.shutdown.is_cancelled() {
            return;
        }
        let mut tasks = self.tasks.lock().await;
        Self::reap_finished(&mut tasks);
        for slot in self.slots.values() {
            if slot
                .started
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            if !slot.config.enabled {
                slot.set_phase(Phase::Disabled);
                continue;
            }
            self.spawn_supervisor(&mut tasks, slot);
        }
    }

    /// 手动重连指定 server（`None` 时重连全部已启用的 server），重置重试预算与 stderr 尾部。
    pub async fn reconnect(&self, server: Option<&str>) -> Result<(), McpError> {
        if self.shutdown.is_cancelled() {
            return Err(McpError::Shutdown);
        }
        let targets: Vec<Arc<ServerSlot>> = match server {
            Some(name) => {
                let slot = self
                    .slots
                    .get(name)
                    .ok_or_else(|| McpError::Tool(format!("unknown MCP server `{name}`")))?;
                if !slot.config.enabled {
                    return Err(McpError::Tool(format!("MCP server `{name}` is disabled")));
                }
                vec![Arc::clone(slot)]
            }
            None => self
                .slots
                .values()
                .filter(|slot| slot.config.enabled)
                .cloned()
                .collect(),
        };
        let mut tasks = self.tasks.lock().await;
        Self::reap_finished(&mut tasks);
        for slot in targets {
            slot.started.store(true, Ordering::Release);
            slot.stderr_tail.lock().clear();
            self.spawn_supervisor(&mut tasks, &slot);
        }
        Ok(())
    }

    /// 并行等待首次连接尝试结束；失败的 server 不阻塞其它能力，取消立即返回。
    /// 各 server 使用自己的握手时限，不把等待时间串行相加。
    pub async fn wait_ready(&self, cancel: &CancellationToken) -> Result<(), McpError> {
        self.wait_ready_within(cancel, Duration::MAX).await
    }

    /// 在给定宽限期 `max_wait` 内并行等待仍在首次握手中的 server；
    /// 超出宽限期立即返回 `Ok(())`，慢 server 继续在后台完成连接，不阻塞正常会话。
    pub async fn wait_ready_within(
        &self,
        cancel: &CancellationToken,
        max_wait: Duration,
    ) -> Result<(), McpError> {
        if cancel.is_cancelled() {
            return Err(McpError::Cancelled);
        }
        if self.shutdown.is_cancelled() {
            return Err(McpError::Shutdown);
        }
        if max_wait.is_zero() {
            return Ok(());
        }
        let ready = futures_util::future::join_all(self.slots.values().map(|slot| async move {
            let mut generation = slot.generation.subscribe();
            let wait = async {
                while matches!(&*slot.phase.read(), Phase::Connecting) {
                    if generation.changed().await.is_err() {
                        break;
                    }
                }
            };
            let limit = Duration::from_secs(slot.config.connect_timeout_secs).min(max_wait);
            let _ = tokio::time::timeout(limit, wait).await;
        }));
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(McpError::Cancelled),
            _ = self.shutdown.cancelled() => Err(McpError::Shutdown),
            _ = ready => Ok(()),
        }
    }

    /// 订阅状态版本号变化：任意 server 相位或工具数变化时递增，天然合并高频变更。
    pub fn subscribe_status(&self) -> watch::Receiver<u64> {
        self.status_version.subscribe()
    }

    /// 当前状态版本号；供上层做零分配的脏检查（例如 `ToolCatalog` 延迟吸收慢连接 server）。
    pub fn status_version(&self) -> u64 {
        *self.status_version.borrow()
    }

    /// 管理器关闭令牌；状态监听任务可据此在管理器关闭时立即退出。
    pub fn shutdown_token(&self) -> CancellationToken {
        self.shutdown.clone()
    }

    /// 面向 UI 与诊断命令的全部 server 状态视图（按名称排序）。
    pub fn server_states(&self) -> Vec<McpServerState> {
        self.slots
            .values()
            .map(|slot| {
                let status = slot.status_snapshot();
                let raw_started = slot.attempt_started_at_ms.load(Ordering::Acquire);
                let started_at_ms = match status {
                    ServerStatus::Connecting | ServerStatus::Reconnecting { .. }
                        if raw_started > 0 =>
                    {
                        Some(raw_started)
                    }
                    _ => None,
                };
                let recent_stderr = slot.stderr_tail.lock().iter().cloned().collect();
                McpServerState {
                    name: slot.config.name.clone(),
                    transport: slot.config.transport.kind_name(),
                    target: slot.config.transport.target_summary(),
                    status,
                    started_at_ms,
                    recent_stderr,
                }
            })
            .collect()
    }

    /// 仍在首次握手中的 server 名（按名称排序）。
    pub fn connecting(&self) -> Vec<String> {
        self.slots
            .iter()
            .filter(|(_, slot)| matches!(&*slot.phase.read(), Phase::Connecting))
            .map(|(name, _)| name.clone())
            .collect()
    }

    /// 是否没有配置任何 server。
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// 全量快照（同步版）：状态表 + 已连接工具（按限定名排序）。
    pub fn snapshot_now(&self) -> McpSnapshot {
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

    /// 全量快照：状态表 + 已连接工具（按限定名排序）。
    pub async fn snapshot(&self) -> McpSnapshot {
        self.snapshot_now()
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
        // 重连等待计入同一份调用预算。
        let started = Instant::now();
        let peer = wait_peer(slot, &cancel, &self.shutdown, timeout).await?;
        let remaining = timeout.saturating_sub(started.elapsed());
        call_once(
            &peer,
            slot,
            info,
            args,
            cancel,
            &self.shutdown,
            input,
            Some(remaining),
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
    epoch: u64,
    slot_cancel: CancellationToken,
    shutdown: CancellationToken,
    policy: ReconnectPolicy,
) {
    if !slot.config.enabled {
        slot.attempt_started_at_ms.store(0, Ordering::Release);
        slot.set_phase_for_epoch(epoch, Phase::Disabled);
        if slot.epoch.load(Ordering::Acquire) == epoch {
            slot.supervising.store(false, Ordering::Release);
        }
        return;
    }
    let server = slot.config.name.clone();
    let connect_timeout = Duration::from_secs(slot.config.connect_timeout_secs);
    let mut attempts: u32 = 0;
    'supervise: loop {
        if slot_cancel.is_cancelled() || slot.epoch.load(Ordering::Acquire) != epoch {
            break 'supervise;
        }
        slot.attempt_started_at_ms
            .store(now_ms(), Ordering::Release);
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
            _ = slot_cancel.cancelled() => break 'supervise,
            result = tokio::time::timeout(connect_timeout, established) => result,
        };
        let failure_error: String = match established {
            Ok(Ok((peer, tools, mut service))) => {
                let instructions = peer
                    .peer_info()
                    .and_then(|info| info.instructions.as_deref().map(Arc::from));
                slot.attempt_started_at_ms.store(0, Ordering::Release);
                if !slot.set_phase_for_epoch(
                    epoch,
                    Phase::Connected {
                        peer: peer.clone(),
                        tools,
                        instructions,
                    },
                ) {
                    tokio::spawn(async move {
                        let _ = service.close_with_timeout(GRACEFUL_CLOSE_TIMEOUT).await;
                    });
                    return;
                }
                // 稳定性从握手成功起算：连接尝试耗时长不该算成「稳定运行」
                let connected_at = Instant::now();

                let connection_lost;
                let mut closed_tick = tokio::time::interval(policy.close_poll);
                closed_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    tokio::select! {
                        biased;
                        _ = slot_cancel.cancelled() => {
                            if shutdown.is_cancelled() {
                                slot.set_phase_for_epoch(epoch, Phase::Disabled);
                            }
                            let _ = service.close_with_timeout(GRACEFUL_CLOSE_TIMEOUT).await;
                            if slot.epoch.load(Ordering::Acquire) == epoch {
                                slot.supervising.store(false, Ordering::Release);
                            }
                            return;
                        }
                        Some(()) = changed_rx.recv() => {
                            let listing = tokio::select! {
                                biased;
                                _ = slot_cancel.cancelled() => {
                                    tokio::spawn(async move {
                                        let _ = service.close_with_timeout(GRACEFUL_CLOSE_TIMEOUT).await;
                                    });
                                    break 'supervise;
                                }
                                result = tokio::time::timeout(connect_timeout, peer.list_all_tools()) => result,
                            };
                            match listing {
                                Ok(Ok(listing)) => {
                                    let tools = match convert_tools(&server, &listing, slot.config.tools.as_ref()) {
                                        Ok(tools) => tools,
                                        Err(error) => {
                                            let message = error.to_string();
                                            slot.set_phase_for_epoch(epoch, Phase::Failed { error: message });
                                            tokio::spawn(async move {
                                                let _ = service.close_with_timeout(GRACEFUL_CLOSE_TIMEOUT).await;
                                            });
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
                                    if slot.epoch.load(Ordering::Acquire) == epoch
                                        && let Phase::Connected { tools: target, .. } =
                                            &mut *slot.phase.write()
                                    {
                                        *target = tools;
                                    }
                                    if replaced {
                                        slot.status_version.send_modify(|version| *version += 1);
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
                // 异步关闭断掉的旧传输（如慢 HTTP session DELETE），不拖慢重连循环。
                tokio::spawn(async move {
                    let _ = service.close_with_timeout(GRACEFUL_CLOSE_TIMEOUT).await;
                });
                if slot_cancel.is_cancelled() || !connection_lost {
                    break 'supervise;
                }
                // 只有稳定运行满 `stable_runtime` 的连接才清零预算：握手成功随即退出的
                // server 会一路累加，直到预算耗尽进入 `Failed`。
                if connected_at.elapsed() >= policy.stable_runtime {
                    attempts = 0;
                }
                "connection interrupted".into()
            }
            Ok(Err(McpError::Tool(message))) => {
                slot.attempt_started_at_ms.store(0, Ordering::Release);
                slot.set_phase_for_epoch(epoch, Phase::Failed { error: message });
                break 'supervise;
            }
            Ok(Err(error)) => {
                let message = error.to_string();
                tracing::warn!(server = %server, %error, "MCP 连接失败");
                message
            }
            Err(_) => {
                let message = format!(
                    "handshake or tool listing exceeded {}s",
                    connect_timeout.as_secs()
                );
                tracing::warn!(server = %server, "MCP 连接超时");
                message
            }
        };
        attempts += 1;
        if attempts > policy.max_attempts {
            slot.attempt_started_at_ms.store(0, Ordering::Release);
            slot.set_phase_for_epoch(
                epoch,
                Phase::Failed {
                    error: failure_error,
                },
            );
            break 'supervise;
        }
        slot.attempt_started_at_ms.store(0, Ordering::Release);
        if !slot.set_phase_for_epoch(
            epoch,
            Phase::Reconnecting {
                attempts,
                error: failure_error,
            },
        ) {
            break 'supervise;
        }
        let delay = policy.backoff(attempts);
        tokio::select! {
            biased;
            _ = slot_cancel.cancelled() => break 'supervise,
            _ = tokio::time::sleep(delay) => {}
        }
    }
    if shutdown.is_cancelled() {
        slot.attempt_started_at_ms.store(0, Ordering::Release);
        slot.set_phase_for_epoch(epoch, Phase::Disabled);
    }
    if slot.epoch.load(Ordering::Acquire) == epoch {
        slot.supervising.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod naming;

#[cfg(test)]
mod tests;

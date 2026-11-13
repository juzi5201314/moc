//! Workflow 执行引擎（纯核心）：无 IO、无时钟、无随机。
//!
//! 输入是 worker guest 上报的 events/requests/script-state，输出是
//! [`EngineAction`]：重放命中立即回执、miss 交给 [`Driver`]（运行时由
//! `runtime::driver` 实现，测试用 fake）、会话事件、终态。journal 经 [`JournalStore`] 注入。
//!
//! 语义（spec §4）：
//! - site×ordinal 身份：同一 site_id 的请求按到达次序计数 ordinal；
//! - 输入哈希：xxh3(规范化 JSON)；命中即重放，不一致记 `InputHashMismatch`；
//! - settle 序号：落地单调增，回放严格按序；
//! - 终态先到者胜：首个终态写 run 记录，其后忽略；
//! - 脚本返回时取消在飞 ask（CancelAsk dispatch）；
//! - amend：具名 actor 的 prior ask 按 actor_seq 顺序导入，actor 名 +
//!   persona + 指令逐字一致且前序已全部导入；分叉后不再导入；
//! - 在飞 ask 并发上限 `max_concurrency`（0 = 不限）；
//! - 用量记账：`usage` 汇总。

pub mod journal;

pub use journal::{
    FileJournal, JournalRecord, JournalStore, MemoryJournal, RecordKind, SettleQueue,
    canonical_json, input_hash,
};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

// —— 输入类型（host 适配层把 wire 映射到这里）——

/// 一次 guest step。
pub struct Step {
    pub events: Vec<EngineEvent>,
    pub requests: Vec<EngineRequest>,
    pub state: ScriptState,
    pub stall: Option<StallInfo>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ScriptState {
    Running,
    /// 脚本返回，携带返回值。
    Done {
        value: Value,
    },
    Failed {
        code: String,
        message: String,
    },
}

#[derive(Debug, Clone)]
pub struct StallInfo {
    /// 等待中的 channel 名。
    pub waiting: Vec<String>,
}

/// guest 上报的即时事件（无回执）。
#[derive(Debug, Clone)]
pub enum EngineEvent {
    CreateActor {
        site_id: String,
        name: Option<String>,
        persona: Value,
    },
    Phase {
        site_id: String,
        name: String,
    },
    Log {
        site_id: String,
        level: String,
        message: String,
    },
    Report {
        site_id: String,
        item: Value,
        artifact_id: Option<String>,
    },
    DeclareArtifact {
        site_id: String,
        member: String,
        spec: Value,
    },
    HoleOpened {
        site_id: String,
        name: String,
        prompt: String,
        schema: Option<Value>,
    },
    HoleCancelled {
        site_id: String,
        name: String,
    },
}

/// 需要回执的 guest 请求。`request_id` 由 guest 分配、原样回传。
#[derive(Debug, Clone)]
pub enum EngineRequest {
    Ask {
        request_id: u64,
        site_id: String,
        actor_site_id: String,
        actor_ordinal: u32,
        actor_seq: u32,
        instructions: String,
        schema: Option<Value>,
    },
    WorldRead {
        request_id: u64,
        site_id: String,
        op: String,
        args: Value,
    },
    WorldRun {
        request_id: u64,
        site_id: String,
        command: String,
        args: Vec<String>,
        timeout_ms: u64,
        output: Value,
    },
    PublishArtifact {
        request_id: u64,
        site_id: String,
        member: String,
        spec: Value,
    },
    /// hole 无内联 body，等宿主填充。
    HoleResolve { request_id: u64, site_id: String },
    CancelAsk {
        request_id: u64,
        site_id: String,
        actor_site_id: String,
        actor_ordinal: u32,
        actor_seq: u32,
    },
}

impl EngineRequest {
    pub fn request_id(&self) -> u64 {
        match self {
            Self::Ask { request_id, .. }
            | Self::WorldRead { request_id, .. }
            | Self::WorldRun { request_id, .. }
            | Self::PublishArtifact { request_id, .. }
            | Self::HoleResolve { request_id, .. }
            | Self::CancelAsk { request_id, .. } => *request_id,
        }
    }
    pub fn site_id(&self) -> &str {
        match self {
            Self::Ask { site_id, .. }
            | Self::WorldRead { site_id, .. }
            | Self::WorldRun { site_id, .. }
            | Self::PublishArtifact { site_id, .. }
            | Self::HoleResolve { site_id, .. }
            | Self::CancelAsk { site_id, .. } => site_id,
        }
    }
    fn kind(&self) -> RecordKind {
        match self {
            Self::Ask { .. } | Self::CancelAsk { .. } => RecordKind::Ask,
            Self::WorldRead { .. } | Self::WorldRun { .. } => RecordKind::World,
            Self::PublishArtifact { .. } => RecordKind::Artifact,
            Self::HoleResolve { .. } => RecordKind::Hole,
        }
    }
}

// —— 输出类型 ——

/// 引擎交给 host 的动作。
#[derive(Debug)]
pub enum EngineAction {
    /// 立即回执 worker（重放命中 / 本地判定）。
    Complete {
        request_id: u64,
        outcome: DriverOutcome,
    },
    /// 派发外部动作（由 driver 执行副作用）。
    Dispatch(DriverRequest),
    /// 会话侧事件（运行时转为 `SessionUpdate::Workflow` 推给前端）。
    Session(SessionEvent),
    /// 终态。
    Finalized(Box<Terminal>),
}

/// actor 上下文接续策略（由引擎按 journal 中的 actor 会话记录决定）。
#[derive(Debug, Clone, PartialEq)]
pub enum ActorContext {
    /// 新子会话。
    Fresh,
    /// 沿用该 actor 在本 run 已有的子会话（resume 后从 journal 恢复，或本 run 首个 ask 已建）。
    Existing { child_session_id: String },
    /// amend：用前驱同名 actor 子会话的 transcript 截到第 `user_turns` 个 user 消息之前 fork
    ///（取最后一条已导入 ask 记录的 `result.user_turns`，含 schema 修复重试轮）。
    SeedFrom {
        run_id: String,
        child_session_id: String,
        user_turns: u32,
    },
}

/// 派发请求（与 EngineRequest 同形，已带 ordinal）。
#[derive(Debug, Clone)]
pub enum DriverRequest {
    Ask {
        /// 对应 guest 请求 id（host 用 `settle(request_id, outcome)` 回执）。
        request_id: u64,
        site_id: String,
        ordinal: u32,
        actor_site_id: String,
        actor_ordinal: u32,
        actor_seq: u32,
        instructions: String,
        schema: Option<Value>,
        /// actor 上下文续接策略；driver 建会话后须回报 `actor_session_created`。
        context: ActorContext,
        /// guest 声明的完整 persona（`{name?,system?,model?}`）；driver 以
        /// `persona.system` 作子会话 context、`persona.model` 作模型兜底。
        persona: Value,
    },
    WorldRead {
        request_id: u64,
        site_id: String,
        ordinal: u32,
        op: String,
        args: Value,
    },
    WorldRun {
        request_id: u64,
        site_id: String,
        ordinal: u32,
        command: String,
        args: Vec<String>,
        timeout_ms: u64,
        output: Value,
    },
    PublishArtifact {
        request_id: u64,
        site_id: String,
        ordinal: u32,
        member: String,
        spec: Value,
    },
    CancelAsk {
        actor_site_id: String,
        actor_ordinal: u32,
        actor_seq: u32,
    },
}

impl DriverRequest {
    pub fn key(&self) -> Option<(String, u32)> {
        match self {
            Self::Ask {
                site_id, ordinal, ..
            }
            | Self::WorldRead {
                site_id, ordinal, ..
            }
            | Self::WorldRun {
                site_id, ordinal, ..
            }
            | Self::PublishArtifact {
                site_id, ordinal, ..
            } => Some((site_id.clone(), *ordinal)),
            Self::CancelAsk { .. } => None,
        }
    }
}

/// 驱动结果；engine 不定义错误码全集，透传 code/message。
#[derive(Debug, Clone, PartialEq)]
pub enum DriverOutcome {
    Completed(Value),
    Failed { code: String, message: String },
}

impl DriverOutcome {
    pub fn into_outcome_json(&self) -> Value {
        match self {
            Self::Completed(v) => json!({"status": "completed", "result": v}),
            Self::Failed { code, message } => {
                json!({"status": "failed", "code": code, "message": message})
            }
        }
    }
}

/// 会话侧事件（供 UI 与审批使用）。
#[derive(Debug, Clone)]
pub enum SessionEvent {
    ActorCreated {
        site_id: String,
        ordinal: u32,
        name: Option<String>,
    },
    Phase {
        name: String,
    },
    Log {
        level: String,
        message: String,
    },
    Report {
        item: Value,
        artifact_id: Option<String>,
    },
    DeclareArtifact {
        member: String,
        spec: Value,
        artifact_id: Option<String>,
    },
    HoleOpened {
        site_id: String,
        ordinal: u32,
        name: String,
        prompt: String,
        schema: Option<Value>,
    },
    HoleCancelled {
        name: String,
    },
    ActorDiverged {
        name: String,
    },
}

/// run 终态（先到者胜）。
#[derive(Debug, Clone, PartialEq)]
pub struct Terminal {
    pub status: TerminalStatus,
    pub result: Option<Value>,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// 终态：completed / errored / stopped(user|interrupted|provider)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum TerminalStatus {
    Completed,
    /// 脚本抛错、契约违反或不可恢复（含 InputHashMismatch/ChannelDeadlock/...）。
    Errored,
    Stopped {
        reason: StopReason,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    User,
    Interrupted,
    Provider,
}

/// 用量记账：按 run 汇总 token 与调用计数。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Usage {
    pub spent_tokens: u64,
    pub asks_completed: u32,
    pub asks_failed: u32,
    pub world_calls: u32,
    pub artifacts_published: u32,
}

/// hash 不一致记录。
#[derive(Debug, Clone)]
pub struct HashMismatch {
    pub site_id: String,
    pub ordinal: u32,
    pub expected: String,
    pub actual: String,
}

/// run 模式：决定是否重放与重放规则。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// 新 run：无历史。
    Fresh,
    /// 同一 run_id 重启：journal 按 (site,ordinal) 重放（含 ask 零 token），
    /// 哈希不一致 → errored(InputHashMismatch)。
    Resume,
    /// 新 run_id 继承前驱：ask 走 actor 导入（名+persona+指令逐字+前序），
    /// world/artifact 重新执行；分叉只发 ActorDiverged。
    Amend,
}

/// 引擎配置。
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// 在飞 ask 上限；0 = 不限。
    pub max_concurrency: usize,
    /// 会话标签。
    pub label: String,
    /// run 模式（默认 Fresh）。
    pub mode: RunMode,
    /// amend/resume 的前驱 run id（若有）。
    pub resumed_from: Option<String>,
}

impl Default for EngineConfig {
    fn default() -> Self {
        Self {
            max_concurrency: crate::workflow::facade::MAX_CONCURRENCY_CEILING,
            label: String::new(),
            mode: RunMode::Fresh,
            resumed_from: None,
        }
    }
}

struct Pending {
    site_id: String,
    ordinal: u32,
    kind: RecordKind,
    input_hash: String,
    request: PendingReq,
    queued: bool,
}

#[derive(Debug)]
enum PendingReq {
    Ask {
        actor_site_id: String,
        actor_ordinal: u32,
        actor_seq: u32,
        instructions: String,
        schema: Option<Value>,
    },
    WorldRead {
        op: String,
        args: Value,
    },
    WorldRun {
        command: String,
        args: Vec<String>,
        timeout_ms: u64,
        output: Value,
    },
    PublishArtifact {
        member: String,
        spec: Value,
    },
    HoleResolve,
}

/// actor 信息（ordinal 在 create_actor 事件到达时分配）。
struct ActorInfo {
    name: Option<String>,
    /// persona 规范化哈希（amend 的 persona 一致判定）。
    persona_key: String,
    /// guest 声明的完整 persona（system/model），Ask 派发时携带给 driver。
    persona: Value,
    /// 具名 actor 的 prior journal 导入状态。
    import: Option<ImportedActor>,
}

/// 具名 actor 的 amend 导入状态。
struct ImportedActor {
    /// prior journal 中该 actor 的 ask 记录，按 actor_seq 排序。
    entries: Vec<JournalRecord>,
    /// 已消费条数（前序全部导入才可导入下一条）。
    consumed: usize,
    /// 已分叉：不再导入。
    diverged: bool,
}

/// 引擎核心。`new` 传入 journal 存储与配置。
pub struct Engine {
    store: Box<dyn JournalStore>,
    config: EngineConfig,
    ordinals: HashMap<String, u32>,
    settle: u32,
    replay: SettleQueue,
    /// 本 run 已消费的 prior 记录键。
    consumed: HashSet<(String, u32)>,
    pending: HashMap<u64, Pending>,
    /// (site_id, ordinal) → request_id（dispatched 未 settle）。
    in_flight: HashMap<(String, u32), u64>,
    queued_asks: VecDeque<u64>,
    in_flight_asks: usize,
    /// 同 actor FIFO（facade：busy 的 actor 上后来的 ask 排队等位）：
    /// "busy" = 该 actor 有一个 ask 在飞或在全局 queued_asks 中。
    busy_actors: HashSet<(String, u32)>,
    /// (actor_site_id, actor_ordinal) → 等位的 ask request_id（FIFO）。
    actor_queues: HashMap<(String, u32), VecDeque<u64>>,
    /// artifact id → member（DeclareArtifact 事件与 PublishArtifact 请求共同登记；
    /// 同一 id 换 member 违规）。
    artifact_members: HashMap<String, String>,
    /// preset 声明 spec（相同重声明 no-op、不同 errored）。
    artifact_specs: HashMap<String, Value>,
    /// report 计数与累计序列化字节（facade 上限校验）。
    report_count: usize,
    report_bytes: u64,
    terminal: Option<Terminal>,
    pub usage: Usage,
    pub mismatches: Vec<HashMismatch>,
    actors: HashMap<(String, u32), ActorInfo>,
    /// (site_id, ordinal) → hole 名；fill 时取名发 HoleCancelled。
    open_holes: HashMap<(String, u32), String>,
    /// 已用过名字 → (site_id, ordinal)；具名 actor 去重。
    actor_names: HashMap<String, (String, u32)>,
    /// prior journal 中按 actor 名分桶的 ask 记录（amend 源）。
    prior_actor_asks: HashMap<String, Vec<JournalRecord>>,
    /// 本 run（含 resume 恢复的）actor (site,ordinal) → 子会话 id。
    actor_sessions: HashMap<(String, u32), String>,
    /// amend 源：prior 具名 actor → 子会话 id（SeedFrom 用）。
    prior_actor_sessions_by_name: HashMap<String, String>,
}

mod events;
mod requests;
mod scheduling;

impl Engine {
    pub fn new(store: Box<dyn JournalStore>, config: EngineConfig) -> Self {
        let mut prior_actor_asks: HashMap<String, Vec<JournalRecord>> = HashMap::new();
        let mut actor_sessions: HashMap<(String, u32), String> = HashMap::new();
        let mut prior_actor_sessions_by_name: HashMap<String, String> = HashMap::new();
        for r in store.prior() {
            match r.kind {
                RecordKind::Ask => {
                    if let Some(name) = r.extra.get("actor_name").and_then(|v| v.as_str()) {
                        prior_actor_asks
                            .entry(name.to_string())
                            .or_default()
                            .push(r.clone());
                    }
                }
                RecordKind::Actor => {
                    if let Some(session) = r.extra.get("child_session_id").and_then(|v| v.as_str())
                    {
                        // Resume：前驱会话即本会话，直接沿用（Existing）。
                        // Amend：前驱会话仅作为 SeedFrom 的播种源，不能直接续。
                        if config.mode == RunMode::Resume {
                            actor_sessions
                                .insert((r.site_id.clone(), r.ordinal), session.to_string());
                        }
                        if let Some(name) = r.extra.get("actor_name").and_then(|v| v.as_str()) {
                            prior_actor_sessions_by_name
                                .insert(name.to_string(), session.to_string());
                        }
                    }
                }
                _ => {}
            }
        }
        for entries in prior_actor_asks.values_mut() {
            entries.sort_by_key(|r| {
                r.extra
                    .get("actor_seq")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            });
        }
        Self {
            replay: SettleQueue::new(store.prior()),
            store,
            config,
            ordinals: HashMap::new(),
            settle: 0,
            consumed: HashSet::new(),
            pending: HashMap::new(),
            in_flight: HashMap::new(),
            queued_asks: VecDeque::new(),
            in_flight_asks: 0,
            busy_actors: HashSet::new(),
            actor_queues: HashMap::new(),
            artifact_members: HashMap::new(),
            artifact_specs: HashMap::new(),
            report_count: 0,
            report_bytes: 0,
            terminal: None,
            usage: Usage::default(),
            mismatches: Vec::new(),
            actors: HashMap::new(),
            open_holes: HashMap::new(),
            actor_names: HashMap::new(),
            prior_actor_asks,
            actor_sessions,
            prior_actor_sessions_by_name,
        }
    }

    fn ordinal_of(&mut self, site_id: &str) -> u32 {
        let n = self.ordinals.entry(site_id.to_string()).or_default();
        *n += 1;
        *n - 1
    }

    /// 下一序号的预读：HoleOpened 与 HoleResolve 是同一次 hole 调用的事件与
    /// 请求两面，必须共享序号——事件先处理，只能 peek，消耗留给请求。
    fn peek_ordinal(&self, site_id: &str) -> u32 {
        *self.ordinals.get(site_id).unwrap_or(&0)
    }
}

/// guest 交付形态：journal 原样记录全量（usage/user_turns 供记账与 amend 播种），
/// 回给 worker 的只是 facade 承诺的值（ask<T> 解析为 T）。
fn guest_outcome(kind: RecordKind, outcome: DriverOutcome) -> DriverOutcome {
    match (kind, &outcome) {
        (RecordKind::Ask, DriverOutcome::Completed(v)) => {
            DriverOutcome::Completed(v.get("value").cloned().unwrap_or(Value::Null))
        }
        _ => outcome,
    }
}

/// preset 声明 member（report 的 artifact_id 只能指向这些）。
fn is_preset_member(member: &str) -> bool {
    matches!(member, "chart" | "table" | "metrics" | "board")
}

fn outcome_of(value: &Value) -> DriverOutcome {
    match value.get("status").and_then(|s| s.as_str()) {
        Some("failed") => DriverOutcome::Failed {
            code: value
                .get("code")
                .and_then(|c| c.as_str())
                .unwrap_or("FAILED")
                .into(),
            message: value
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("")
                .into(),
        },
        _ => DriverOutcome::Completed(value.get("result").cloned().unwrap_or(Value::Null)),
    }
}

#[cfg(test)]
mod tests;

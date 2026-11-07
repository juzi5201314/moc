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
use serde_json::{Map, Value, json};
use std::collections::{HashMap, HashSet, VecDeque};

use crate::workflow::facade;

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

    /// driver 建立 actor 子会话后回报：写 `actor` 记录并登记上下文。
    /// resume/amend 依赖它恢复 Existing/SeedFrom 决策。
    pub fn actor_session_created(
        &mut self,
        actor_site_id: &str,
        actor_ordinal: u32,
        child_session_id: &str,
    ) {
        let key = (actor_site_id.to_string(), actor_ordinal);
        if self.actor_sessions.contains_key(&key) {
            return;
        }
        self.actor_sessions
            .insert(key, child_session_id.to_string());
        let actor = self.actors.get(&(actor_site_id.to_string(), actor_ordinal));
        let mut extra = Map::new();
        extra.insert("child_session_id".into(), json!(child_session_id));
        if let Some(actor) = actor {
            if let Some(name) = &actor.name {
                extra.insert("actor_name".into(), json!(name));
            }
            extra.insert("persona_key".into(), json!(actor.persona_key));
            // 完整 persona 入 journal：resume/amend 恢复 actor 时用真实值，
            // 不回退 plan.roles 的截断摘要。
            extra.insert("persona".into(), actor.persona.clone());
        }
        let record = JournalRecord {
            kind: RecordKind::Actor,
            site_id: actor_site_id.to_string(),
            ordinal: actor_ordinal,
            input_hash: String::new(),
            settle: self.settle,
            outcome: json!({"status": "completed", "result": null}),
            extra,
        };
        self.settle += 1;
        let _ = self.store.append(&record);
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

    /// 处理一次 guest step：先事件（含 actor 注册），再请求（重放→回执 / miss→dispatch），
    /// 最后脚本状态（终态/停滞）。
    pub fn handle_step(&mut self, step: Step) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        if self.terminal.is_some() {
            return actions;
        }
        for event in step.events {
            self.handle_event(event, &mut actions);
        }
        for request in step.requests {
            self.handle_request(request, &mut actions);
        }
        match step.state {
            ScriptState::Done { value } => {
                self.finish(
                    TerminalStatus::Completed,
                    Some(value),
                    None,
                    None,
                    &mut actions,
                );
            }
            ScriptState::Failed { code, message } => {
                self.finish(
                    TerminalStatus::Errored,
                    None,
                    Some(code),
                    Some(message),
                    &mut actions,
                );
            }
            ScriptState::Running => {
                // 停滞 = 不可恢复：整个 run errored，附带 channel 名与等待数。
                if let Some(stall) = step.stall {
                    if stall.waiting.is_empty() {
                        self.errored(
                            "ScriptStalled",
                            "the script is suspended with no request in flight and no channel receiver waiting, so nothing can ever resolve what it awaits",
                            &mut actions,
                        );
                    } else {
                        self.errored(
                            "ChannelDeadlock",
                            format!(
                                "deadlock: {} receiver(s) waiting on channel(s) [{}] with no request in flight",
                                stall.waiting.len(),
                                stall.waiting.join(", ")
                            ),
                            &mut actions,
                        );
                    }
                }
            }
        }
        actions
    }

    /// 不可恢复错误 → 终态 errored（先到者胜由 finish 保证）。
    fn errored(&mut self, code: &str, message: impl ToString, actions: &mut Vec<EngineAction>) {
        self.finish(
            TerminalStatus::Errored,
            None,
            Some(code.into()),
            Some(message.to_string()),
            actions,
        );
    }

    fn handle_event(&mut self, event: EngineEvent, actions: &mut Vec<EngineAction>) {
        match event {
            EngineEvent::CreateActor {
                site_id,
                name,
                persona,
            } => {
                let ordinal = self.ordinal_of(&site_id);
                // 运行期重名（模板/变量名）：整个 run errored。字面量重名编译期已拦。
                if let Some(n) = &name
                    && self.actor_names.contains_key(n)
                {
                    self.errored(
                        "DuplicateActorName",
                        format!("actor name \"{n}\" is already taken by another actor in this run"),
                        actions,
                    );
                    return;
                }
                let persona_key = input_hash(&persona);
                let import = if self.config.mode == RunMode::Amend {
                    name.as_ref().and_then(|n| {
                        self.prior_actor_asks.get(n).map(|entries| ImportedActor {
                            entries: entries.clone(),
                            consumed: 0,
                            diverged: false,
                        })
                    })
                } else {
                    None
                };
                if let Some(n) = &name {
                    self.actor_names
                        .insert(n.clone(), (site_id.clone(), ordinal));
                }
                self.actors.insert(
                    (site_id.clone(), ordinal),
                    ActorInfo {
                        name: name.clone(),
                        persona_key,
                        persona: persona.clone(),
                        import,
                    },
                );
                actions.push(EngineAction::Session(SessionEvent::ActorCreated {
                    site_id,
                    ordinal,
                    name,
                }));
            }
            EngineEvent::Phase { site_id: _, name } => {
                actions.push(EngineAction::Session(SessionEvent::Phase { name }));
            }
            EngineEvent::Log { level, message, .. } => {
                actions.push(EngineAction::Session(SessionEvent::Log { level, message }));
            }
            EngineEvent::Report {
                item, artifact_id, ..
            } => {
                // artifact_id 必须指向已执行的 preset 声明；未声明或指向
                // file/markdown → 整 run 失败（facade：tag naming nothing fails）。
                if let Some(id) = &artifact_id
                    && !self
                        .artifact_members
                        .get(id)
                        .is_some_and(|m| is_preset_member(m))
                {
                    self.errored(
                        "ReportTarget",
                        format!("report artifact_id \"{id}\" is not a declared preset artifact"),
                        actions,
                    );
                    return;
                }
                // caps fail the whole run（facade）：条目数 / 单条 / 累计。
                self.report_count += 1;
                if self.report_count > facade::REPORT_MAX_ITEMS {
                    self.errored(
                        "ReportLimit",
                        format!("run exceeds {} report items", facade::REPORT_MAX_ITEMS),
                        actions,
                    );
                    return;
                }
                let bytes = serde_json::to_vec(&item)
                    .map(|v| v.len() as u64)
                    .unwrap_or(u64::MAX);
                if bytes > facade::REPORT_MAX_ITEM_BYTES as u64 {
                    self.errored(
                        "ReportLimit",
                        format!(
                            "report item exceeds {} bytes serialized",
                            facade::REPORT_MAX_ITEM_BYTES
                        ),
                        actions,
                    );
                    return;
                }
                self.report_bytes += bytes;
                if self.report_bytes > facade::REPORT_MAX_TOTAL_BYTES {
                    self.errored(
                        "ReportLimit",
                        format!(
                            "run exceeds {} bytes of reports",
                            facade::REPORT_MAX_TOTAL_BYTES
                        ),
                        actions,
                    );
                    return;
                }
                actions.push(EngineAction::Session(SessionEvent::Report {
                    item,
                    artifact_id,
                }));
            }
            EngineEvent::DeclareArtifact { member, spec, .. } => {
                // wire 形：[id, specObj]；preset 声明同步落引擎校验。
                let artifact_id = spec.get(0).and_then(|v| v.as_str()).map(String::from);
                let spec_obj = spec.get(1).cloned();
                match (artifact_id, spec_obj) {
                    (Some(id), Some(spec_obj)) if spec_obj.is_object() => {
                        match self.artifact_members.get(&id) {
                            Some(prev) if *prev == member => {
                                // 相同重声明 no-op；不同 spec 或不同 member → errored。
                                if self.artifact_specs.get(&id) != Some(&spec_obj) {
                                    self.errored(
                                        "ArtifactRedeclared",
                                        format!(
                                            "artifact \"{id}\" redeclared with a different spec"
                                        ),
                                        actions,
                                    );
                                }
                            }
                            Some(prev) => {
                                self.errored(
                                    "ArtifactMemberConflict",
                                    format!("artifact \"{id}\" is used with both `{prev}` and `{member}`"),
                                    actions,
                                );
                            }
                            None => {
                                if self.artifact_members.len() >= facade::ARTIFACT_MAX_IDS {
                                    self.errored(
                                        "ArtifactLimit",
                                        format!(
                                            "run exceeds {} artifact ids",
                                            facade::ARTIFACT_MAX_IDS
                                        ),
                                        actions,
                                    );
                                } else {
                                    self.artifact_members.insert(id.clone(), member.clone());
                                    self.artifact_specs.insert(id.clone(), spec_obj.clone());
                                    actions.push(EngineAction::Session(
                                        SessionEvent::DeclareArtifact {
                                            member,
                                            spec: spec_obj,
                                            artifact_id: Some(id),
                                        },
                                    ));
                                }
                            }
                        }
                    }
                    _ => {
                        self.errored(
                            "ArtifactSpec",
                            "artifact declaration must carry an id and a spec object",
                            actions,
                        );
                    }
                }
            }
            EngineEvent::HoleOpened {
                site_id,
                name,
                prompt,
                schema,
            } => {
                // 与同次 HoleResolve 请求共享序号：事件侧 peek，请求侧消耗。
                let ordinal = self.peek_ordinal(&site_id);
                self.open_holes
                    .insert((site_id.clone(), ordinal), name.clone());
                actions.push(EngineAction::Session(SessionEvent::HoleOpened {
                    site_id,
                    ordinal,
                    name,
                    prompt,
                    schema,
                }));
            }
            EngineEvent::HoleCancelled { name, .. } => {
                actions.push(EngineAction::Session(SessionEvent::HoleCancelled { name }));
            }
        }
    }

    fn handle_request(&mut self, request: EngineRequest, actions: &mut Vec<EngineAction>) {
        let request_id = request.request_id();
        let site_id = request.site_id().to_string();
        let ordinal = self.ordinal_of(&site_id);
        let kind = request.kind();
        let hash = self.request_hash(&request);

        // —— artifact 发布侧运行期门禁（facade：content 类 catchable reject）——
        // 位于重放/导入查找之前：重放命中的 publish 同样登记 id/member，
        // 否则 resume 后 32-id 上限与 member 冲突判定失真。
        if let EngineRequest::PublishArtifact { member, spec, .. } = &request {
            match spec.get(0).and_then(|v| v.as_str()).map(String::from) {
                Some(id) => {
                    match self.artifact_members.get(&id) {
                        Some(prev) if *prev == *member => {}
                        Some(prev) => {
                            if is_preset_member(prev) {
                                // 与已声明 preset 冲突 → 拒绝本次发布（可捕获）。
                                actions.push(EngineAction::Complete {
                                    request_id,
                                    outcome: DriverOutcome::Failed {
                                        code: "E_ARTIFACT".into(),
                                        message: format!(
                                            "artifact id \"{id}\" is already declared as `{prev}`"
                                        ),
                                    },
                                });
                            } else {
                                // 同一 id 换 member（file↔markdown）→ 整 run 失败。
                                self.errored(
                                "ArtifactMemberConflict",
                                format!("artifact \"{id}\" is used with both `{prev}` and `{member}`"),
                                actions,
                            );
                            }
                            return;
                        }
                        None => {
                            if self.artifact_members.len() >= facade::ARTIFACT_MAX_IDS {
                                actions.push(EngineAction::Complete {
                                    request_id,
                                    outcome: DriverOutcome::Failed {
                                        code: "E_ARTIFACT".into(),
                                        message: format!(
                                            "run exceeds {} artifact ids",
                                            facade::ARTIFACT_MAX_IDS
                                        ),
                                    },
                                });
                                return;
                            }
                            self.artifact_members.insert(id, member.clone());
                        }
                    }
                }
                None => {
                    actions.push(EngineAction::Complete {
                        request_id,
                        outcome: DriverOutcome::Failed {
                            code: "E_ARTIFACT".into(),
                            message: format!("artifact.{member} 缺 id"),
                        },
                    });
                    return;
                }
            }
        }

        // —— ask 的 amend 导入路径（仅 Amend 模式）——
        if self.config.mode == RunMode::Amend
            && let EngineRequest::Ask {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                ..
            } = &request
        {
            let actor_key = (actor_site_id.clone(), *actor_ordinal);
            // 先在借用域内算出结果，再推 actions（避免 self 二次可变借用）。
            enum Import {
                Hit(Value),
                Diverged(Option<String>),
            }
            let result = (|| {
                let actor = self.actors.get_mut(&actor_key)?;
                let import = actor.import.as_mut()?;
                if import.diverged {
                    return None;
                }
                Some(match import.entries.get(import.consumed) {
                    Some(entry)
                        if entry.extra.get("actor_seq").and_then(|v| v.as_u64())
                            == Some(*actor_seq as u64)
                            && entry.extra.get("persona_key").and_then(|v| v.as_str())
                                == Some(actor.persona_key.as_str())
                            && entry.input_hash == hash =>
                    {
                        import.consumed += 1;
                        Import::Hit(entry.outcome.clone())
                    }
                    _ => {
                        import.diverged = true;
                        Import::Diverged(actor.name.clone())
                    }
                })
            })();
            match result {
                Some(Import::Hit(outcome_json)) => {
                    self.account(&outcome_json);
                    actions.push(EngineAction::Complete {
                        request_id,
                        outcome: guest_outcome(kind, outcome_of(&outcome_json)),
                    });
                    return;
                }
                Some(Import::Diverged(Some(name))) => {
                    actions.push(EngineAction::Session(SessionEvent::ActorDiverged { name }));
                }
                _ => {}
            }
        }

        // —— (site_id, ordinal) 重放路径：仅 Resume（同一 run 重启，含 ask 零 token）——
        let record = if self.config.mode == RunMode::Resume
            && !self.consumed.contains(&(site_id.clone(), ordinal))
        {
            self.replay
                .get(&site_id, ordinal)
                .filter(|r| r.kind == kind && !self.consumed.contains(&r.key()))
                .cloned()
        } else {
            None
        };
        if let Some(record) = record {
            if record.input_hash == hash {
                self.consumed.insert(record.key());
                let outcome = guest_outcome(kind, outcome_of(&record.outcome));
                self.account(&record.outcome);
                actions.push(EngineAction::Complete {
                    request_id,
                    outcome,
                });
                return;
            }
            // resume 的哈希不一致说明脚本或输入不可重放 → 整个 run errored，不得重执行。
            self.mismatches.push(HashMismatch {
                site_id: site_id.clone(),
                ordinal,
                expected: record.input_hash.clone(),
                actual: hash.clone(),
            });
            self.consumed.insert(record.key());
            self.errored(
                "InputHashMismatch",
                format!(
                    "site {site_id}#{ordinal} produced a different input than recorded in the journal; the script is non-deterministic and the run cannot be replayed"
                ),
                actions,
            );
            return;
        }

        // —— miss → dispatch（ask 受并发上限钳制）——
        let pending_req = match &request {
            EngineRequest::Ask {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                instructions,
                schema,
                ..
            } => PendingReq::Ask {
                actor_site_id: actor_site_id.clone(),
                actor_ordinal: *actor_ordinal,
                actor_seq: *actor_seq,
                instructions: instructions.clone(),
                schema: schema.clone(),
            },
            EngineRequest::WorldRead { op, args, .. } => PendingReq::WorldRead {
                op: op.clone(),
                args: args.clone(),
            },
            EngineRequest::WorldRun {
                command,
                args,
                timeout_ms,
                output,
                ..
            } => PendingReq::WorldRun {
                command: command.clone(),
                args: args.clone(),
                timeout_ms: *timeout_ms,
                output: output.clone(),
            },
            EngineRequest::PublishArtifact { member, spec, .. } => PendingReq::PublishArtifact {
                member: member.clone(),
                spec: spec.clone(),
            },
            EngineRequest::HoleResolve { .. } => PendingReq::HoleResolve,
            EngineRequest::CancelAsk {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                ..
            } => {
                // CancelAsk 不做 journal 重放，直接派发并回执。
                actions.push(EngineAction::Dispatch(DriverRequest::CancelAsk {
                    actor_site_id: actor_site_id.clone(),
                    actor_ordinal: *actor_ordinal,
                    actor_seq: *actor_seq,
                }));
                actions.push(EngineAction::Complete {
                    request_id,
                    outcome: DriverOutcome::Completed(json!({"cancelled": true})),
                });
                return;
            }
        };
        let actor_key = match &pending_req {
            PendingReq::Ask {
                actor_site_id,
                actor_ordinal,
                ..
            } => Some((actor_site_id.clone(), *actor_ordinal)),
            _ => None,
        };
        // 同 actor FIFO：actor 忙（有 ask 在飞或在全局队列）→ 进 actor 队列等位。
        if let Some(key) = &actor_key {
            if self.busy_actors.contains(key) {
                self.pending.insert(
                    request_id,
                    Pending {
                        site_id: site_id.clone(),
                        ordinal,
                        kind,
                        input_hash: hash,
                        request: pending_req,
                        queued: true,
                    },
                );
                self.actor_queues
                    .entry(key.clone())
                    .or_default()
                    .push_back(request_id);
                return;
            }
            self.busy_actors.insert(key.clone());
        }
        let queued = actor_key.is_some()
            && self.config.max_concurrency > 0
            && self.in_flight_asks >= self.config.max_concurrency;
        self.pending.insert(
            request_id,
            Pending {
                site_id: site_id.clone(),
                ordinal,
                kind,
                input_hash: hash,
                request: pending_req,
                queued,
            },
        );
        if queued {
            self.queued_asks.push_back(request_id);
        } else {
            self.dispatch(request_id, actions);
        }
    }

    /// 一个 ask 走全局派发逻辑：并发满则进全局队列（actor 保持 busy），否则立即派发。
    fn offer_ask(&mut self, request_id: u64, actions: &mut Vec<EngineAction>) {
        let full =
            self.config.max_concurrency > 0 && self.in_flight_asks >= self.config.max_concurrency;
        if let Some(p) = self.pending.get_mut(&request_id) {
            p.queued = full;
        }
        if full {
            self.queued_asks.push_back(request_id);
        } else {
            self.dispatch(request_id, actions);
        }
    }

    /// 派发一个 pending 请求（ask 计数在飞额度）。
    fn dispatch(&mut self, request_id: u64, actions: &mut Vec<EngineAction>) {
        let Some(p) = self.pending.get(&request_id) else {
            return;
        };
        let (site_id, ordinal) = (p.site_id.clone(), p.ordinal);
        let drv = match &p.request {
            PendingReq::Ask {
                actor_site_id,
                actor_ordinal,
                actor_seq,
                instructions,
                schema,
            } => {
                let actor_key = (actor_site_id.clone(), *actor_ordinal);
                let context = if let Some(session) = self.actor_sessions.get(&actor_key) {
                    ActorContext::Existing {
                        child_session_id: session.clone(),
                    }
                } else {
                    // amend：具名 actor 已导入过 ask → 用前驱子会话 transcript 播种。
                    self.actors
                        .get(&actor_key)
                        .and_then(|a| a.name.as_ref().zip(a.import.as_ref()))
                        .and_then(|(name, import)| {
                            let session = self.prior_actor_sessions_by_name.get(name)?.clone();
                            // 截断点 = 最后一条已导入 ask 的 user_turns（driver 交付时
                            // 记入 outcome.result；缺省 0 表示不导入任何 user 回合）。
                            let user_turns = import
                                .consumed
                                .checked_sub(1)
                                .and_then(|last| import.entries.get(last))
                                .and_then(|e| e.outcome.pointer("/result/user_turns"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0) as u32;
                            Some(ActorContext::SeedFrom {
                                run_id: self.config.resumed_from.clone().unwrap_or_default(),
                                child_session_id: session,
                                user_turns,
                            })
                        })
                        .unwrap_or(ActorContext::Fresh)
                };
                DriverRequest::Ask {
                    request_id,
                    site_id,
                    ordinal,
                    actor_site_id: actor_site_id.clone(),
                    actor_ordinal: *actor_ordinal,
                    actor_seq: *actor_seq,
                    instructions: instructions.clone(),
                    schema: schema.clone(),
                    context,
                    persona: self
                        .actors
                        .get(&(actor_site_id.clone(), *actor_ordinal))
                        .map(|a| a.persona.clone())
                        .unwrap_or(Value::Null),
                }
            }
            PendingReq::WorldRead { op, args } => DriverRequest::WorldRead {
                request_id,
                site_id,
                ordinal,
                op: op.clone(),
                args: args.clone(),
            },
            PendingReq::WorldRun {
                command,
                args,
                timeout_ms,
                output,
            } => DriverRequest::WorldRun {
                request_id,
                site_id,
                ordinal,
                command: command.clone(),
                args: args.clone(),
                timeout_ms: *timeout_ms,
                output: output.clone(),
            },
            PendingReq::PublishArtifact { member, spec } => DriverRequest::PublishArtifact {
                request_id,
                site_id,
                ordinal,
                member: member.clone(),
                spec: spec.clone(),
            },
            PendingReq::HoleResolve => return, // hole 由 fill_hole 回执，无需 dispatch
        };
        if let Some(key) = drv.key() {
            self.in_flight.insert(key, request_id);
        }
        if matches!(drv, DriverRequest::Ask { .. }) {
            self.in_flight_asks += 1;
        }
        actions.push(EngineAction::Dispatch(drv));
    }

    /// 请求输入哈希：种类 + 决定输出的参数字段（不含 request_id/site_id）。
    fn request_hash(&self, request: &EngineRequest) -> String {
        match request {
            EngineRequest::Ask {
                instructions,
                schema,
                actor_seq,
                actor_site_id,
                actor_ordinal,
                ..
            } => {
                // spec：ask 哈希 = 指令 + persona（schema/seq 亦决定交付形态）。
                let persona_key = self
                    .actors
                    .get(&(actor_site_id.clone(), *actor_ordinal))
                    .map_or(Value::Null, |a| json!(a.persona_key));
                input_hash(&json!({
                    "instructions": instructions,
                    "schema": schema,
                    "actor_seq": actor_seq,
                    "persona_key": persona_key,
                }))
            }
            EngineRequest::WorldRead { op, args, .. } => {
                input_hash(&json!({"op": op, "args": args}))
            }
            EngineRequest::WorldRun {
                command,
                args,
                timeout_ms,
                output,
                ..
            } => input_hash(&json!({
                "cmd": command, "args": args, "timeout_ms": timeout_ms, "output": output,
            })),
            EngineRequest::PublishArtifact { member, spec, .. } => {
                input_hash(&json!({"member": member, "spec": spec}))
            }
            EngineRequest::HoleResolve { .. } => input_hash(&json!("hole")),
            EngineRequest::CancelAsk { .. } => String::new(),
        }
    }

    /// driver 完成一个派发：写 journal（settle 序）、回执 worker、释放额度。
    /// `request_id` 对应 pending；重复 settle 以首个落地为准。
    pub fn settle(&mut self, request_id: u64, outcome: DriverOutcome) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        let Some(pending) = self.pending.remove(&request_id) else {
            return actions;
        };
        if matches!(pending.request, PendingReq::Ask { .. }) && self.in_flight_asks > 0 {
            self.in_flight_asks -= 1;
        }
        self.in_flight
            .remove(&(pending.site_id.clone(), pending.ordinal));
        // 写 journal（ask 记录带 actor 身份供 amend 匹配）。
        let mut extra = Map::new();
        if let PendingReq::Ask {
            actor_site_id,
            actor_ordinal,
            actor_seq,
            instructions,
            schema,
        } = &pending.request
        {
            extra.insert("actor_site_id".into(), json!(actor_site_id));
            extra.insert("actor_ordinal".into(), json!(actor_ordinal));
            extra.insert("actor_seq".into(), json!(actor_seq));
            extra.insert("instructions".into(), json!(instructions));
            if let Some(schema) = schema {
                extra.insert("schema".into(), schema.clone());
            }
            if let Some(actor) = self.actors.get(&(actor_site_id.clone(), *actor_ordinal)) {
                if let Some(name) = &actor.name {
                    extra.insert("actor_name".into(), json!(name));
                }
                extra.insert("persona_key".into(), json!(actor.persona_key));
            }
        }
        let record = JournalRecord {
            kind: pending.kind,
            site_id: pending.site_id.clone(),
            ordinal: pending.ordinal,
            input_hash: pending.input_hash.clone(),
            settle: self.settle,
            outcome: outcome.into_outcome_json(),
            extra,
        };
        self.settle += 1;
        if let Err(e) = self.store.append(&record) {
            actions.push(EngineAction::Session(SessionEvent::Log {
                level: "error".into(),
                message: format!("journal append failed: {e}"),
            }));
        }
        // 记账
        if let DriverOutcome::Completed(v) = &outcome
            && let Some(u) = v.get("usage")
        {
            self.usage.spent_tokens += u.get("spent_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        }
        match (&pending.kind, &outcome) {
            (RecordKind::Ask, DriverOutcome::Completed(_)) => self.usage.asks_completed += 1,
            (RecordKind::Ask, DriverOutcome::Failed { .. }) => self.usage.asks_failed += 1,
            (RecordKind::World, _) => self.usage.world_calls += 1,
            (RecordKind::Artifact, DriverOutcome::Completed(_)) => {
                self.usage.artifacts_published += 1;
            }
            _ => {}
        }
        actions.push(EngineAction::Complete {
            request_id,
            outcome: guest_outcome(pending.kind, outcome),
        });
        // 同 actor FIFO：释放 actor；其队列有下一个 → 走同样的全局逻辑（保持 busy）。
        if let PendingReq::Ask {
            actor_site_id,
            actor_ordinal,
            ..
        } = &pending.request
        {
            let actor_key = (actor_site_id.clone(), *actor_ordinal);
            match self
                .actor_queues
                .get_mut(&actor_key)
                .and_then(|q| q.pop_front())
            {
                Some(next) => {
                    if self
                        .actor_queues
                        .get(&actor_key)
                        .is_some_and(|q| q.is_empty())
                    {
                        self.actor_queues.remove(&actor_key);
                    }
                    self.offer_ask(next, &mut actions);
                }
                None => {
                    self.actor_queues.remove(&actor_key);
                    self.busy_actors.remove(&actor_key);
                }
            }
        }
        // 释放额度后派发排队的 ask（FIFO）。
        while let Some(next) = self.queued_asks.pop_front() {
            let slot_free = self.config.max_concurrency == 0
                || self.in_flight_asks < self.config.max_concurrency;
            if !slot_free {
                self.queued_asks.push_front(next);
                break;
            }
            if let Some(p) = self.pending.get_mut(&next) {
                p.queued = false;
                self.dispatch(next, &mut actions);
            }
        }
        actions
    }

    /// 宿主填充 hole（fill_hole 工具 → worker 回执）。
    pub fn fill_hole(&mut self, site_id: &str, ordinal: u32, code: String) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        let Some(request_id) = self.pending.iter().find_map(|(id, p)| {
            (p.site_id == site_id
                && p.ordinal == ordinal
                && matches!(p.request, PendingReq::HoleResolve))
            .then_some(*id)
        }) else {
            return actions;
        };
        if let Some(pending) = self.pending.remove(&request_id) {
            // 交付形状 = `{"body": code}`：guest 的 hole() 读 `answer.body`
            // 再 evalFill。journal outcome 的 `result` 键必须同形——重放经
            // `outcome_of` 取 `result` 交付。
            let record = JournalRecord {
                kind: RecordKind::Hole,
                site_id: pending.site_id.clone(),
                ordinal: pending.ordinal,
                input_hash: input_hash(&json!(code)),
                settle: self.settle,
                outcome: json!({"status": "filled", "code": code, "result": {"body": code}}),
                extra: Map::new(),
            };
            self.settle += 1;
            let _ = self.store.append(&record);
            if let Some(name) = self.open_holes.remove(&(pending.site_id, pending.ordinal)) {
                actions.push(EngineAction::Session(SessionEvent::HoleCancelled { name }));
            }
            actions.push(EngineAction::Complete {
                request_id,
                outcome: DriverOutcome::Completed(json!({"body": code})),
            });
        }
        actions
    }

    /// run 终态（先到者胜）。
    pub fn finish(
        &mut self,
        status: TerminalStatus,
        result: Option<Value>,
        code: Option<String>,
        message: Option<String>,
        actions: &mut Vec<EngineAction>,
    ) {
        if self.terminal.is_some() {
            return;
        }
        let terminal = Terminal {
            status,
            result: result.clone(),
            error_code: code.clone(),
            error_message: message.clone(),
        };
        self.terminal = Some(terminal.clone());
        // 脚本返回/终态 → 取消在飞 ask。
        let in_flight: Vec<(u64, String, u32)> = self
            .pending
            .iter()
            .filter(|(_, p)| matches!(p.request, PendingReq::Ask { .. }))
            .map(|(id, p)| (*id, p.site_id.clone(), p.ordinal))
            .collect();
        for (request_id, site_id, ordinal) in in_flight {
            if let Some(p) = self.pending.get(&request_id)
                && let PendingReq::Ask {
                    actor_site_id,
                    actor_ordinal,
                    actor_seq,
                    ..
                } = &p.request
            {
                actions.push(EngineAction::Dispatch(DriverRequest::CancelAsk {
                    actor_site_id: actor_site_id.clone(),
                    actor_ordinal: *actor_ordinal,
                    actor_seq: *actor_seq,
                }));
                actions.push(EngineAction::Complete {
                    request_id,
                    outcome: DriverOutcome::Failed {
                        code: "CANCELLED".into(),
                        message: "run finished while the ask was in flight".into(),
                    },
                });
            }
            self.in_flight.remove(&(site_id, ordinal));
            // 从 pending 摘除：迟到的 settle 不得二次回执或追加 journal。
            self.pending.remove(&request_id);
        }
        self.queued_asks.clear();
        self.actor_queues.clear();
        self.busy_actors.clear();
        // run 记录
        let record = JournalRecord {
            kind: RecordKind::Run,
            site_id: "run".into(),
            ordinal: 0,
            input_hash: String::new(),
            settle: self.settle,
            outcome: json!({
                "status": match status {
                    TerminalStatus::Completed => "completed",
                    TerminalStatus::Errored => "errored",
                    TerminalStatus::Stopped { .. } => "stopped",
                },
                "stop_reason": match status {
                    TerminalStatus::Stopped { reason } => match reason {
                        StopReason::User => "user",
                        StopReason::Interrupted => "interrupted",
                        StopReason::Provider => "provider",
                    },
                    _ => "",
                },
                "result": result,
                "error_code": code,
                "error_message": message,
                "usage": serde_json::to_value(&self.usage).unwrap_or_default(),
                "resumed_from": self.config.resumed_from,
            }),
            extra: Map::new(),
        };
        self.settle += 1;
        let _ = self.store.append(&record);
        actions.push(EngineAction::Finalized(Box::new(terminal)));
    }

    /// 取消整个 run。
    pub fn cancel(&mut self, code: impl Into<String>) -> Vec<EngineAction> {
        let mut actions = Vec::new();
        self.finish(
            TerminalStatus::Stopped {
                reason: StopReason::User,
            },
            None,
            Some(code.into()),
            Some("run cancelled".into()),
            &mut actions,
        );
        actions
    }

    fn account(&mut self, outcome: &Value) {
        if let Some(u) = outcome.get("usage") {
            self.usage.spent_tokens += u.get("spent_tokens").and_then(|t| t.as_u64()).unwrap_or(0);
        }
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

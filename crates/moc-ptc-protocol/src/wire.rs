//! 宿主和 worker 共用的 JSON 通信协议。

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 包含 JSON 转义与 envelope 的传输帧硬上限；读端分配前即检查。
pub const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartRequest {
    pub code: String,
    pub memory_limit_bytes: usize,
    pub max_output_bytes: usize,
    pub max_message_bytes: usize,
    pub max_pending_calls: usize,
    pub max_total_calls: usize,
    pub timeout_ms: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum HostMessage {
    Start {
        execution_id: u64,
        request: StartRequest,
    },
    Complete {
        execution_id: u64,
        completions: Vec<HostCompletion>,
    },
    Cancel {
        execution_id: u64,
        code: String,
    },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestMessage {
    Step { execution_id: u64, step: GuestStep },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostRequest {
    pub id: u32,
    pub operation: Operation,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Tool {
        name: String,
        arguments: Value,
    },
    Servers,
    Search {
        query: Option<String>,
        server: Option<String>,
        offset: Option<usize>,
        limit: Option<usize>,
    },
    Describe {
        name: String,
    },
    Store {
        key: String,
        value: Value,
    },
    Load {
        key: String,
    },
    Remove {
        key: String,
    },
    Keys,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostCompletion {
    pub id: u32,
    pub result: Result<Value, ScriptError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScriptToolResult {
    pub content: String,
    pub summary: Option<String>,
    pub structured_content: Option<Value>,
    pub is_error: bool,
    pub status: String,
    pub artifact: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScriptError {
    pub code: String,
    pub message: String,
    // 装箱收窄 Err 变体：ScriptToolResult 本身上百字节，内联会让所有
    // `Result<_, ScriptError>` 的搬运成本跟着翻倍；线上序列化不受影响。
    pub result: Option<Box<ScriptToolResult>>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestStep {
    pub requests: Vec<HostRequest>,
    pub state: GuestState,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum GuestState {
    Pending,
    Done { value: Value },
    Failed { error: ScriptError },
}

// —— workflow 模式：一个 run 独占一个 worker 进程，无墙钟超时（取消走 Cancel）——

/// workflow run 的启动请求；沿用 StartRequest 的资源约束字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStartRequest {
    /// 编译器降级后的脚本体（async 函数体形态，非完整脚本）。
    pub code: String,
    /// 脚本全局 `args` 的值。
    pub args: Value,
    /// 脚本全局 `run` 的值。
    pub run: WorkflowRunInfo,
    pub memory_limit_bytes: usize,
    pub max_output_bytes: usize,
    pub max_message_bytes: usize,
    pub max_pending_calls: usize,
    pub max_total_calls: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowRunInfo {
    pub id: String,
    /// amend 时指向前驱 run。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowHostMessage {
    Start { request: WorkflowStartRequest },
    /// 回应请求的幂等粒度是单个 request id；一批里可以只补一部分。
    Complete { completions: Vec<HostCompletion> },
    Cancel { code: String },
    Shutdown,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowGuestMessage {
    Step { step: WorkflowGuestStep },
}

/// guest 一步的产出：事件（无需回应）+ 请求（等待回应）+ 当前状态。
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGuestStep {
    pub events: Vec<WorkflowEvent>,
    pub requests: Vec<WorkflowHostRequest>,
    pub state: GuestState,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowHostRequest {
    pub id: u32,
    /// 编译器分配的调用点标识（`<kind>#<n>`）；宿主计到达次序得 ordinal。
    pub site_id: String,
    pub operation: WorkflowOperation,
}

/// 需要宿主回应的调用。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowOperation {
    /// `Agent.ask<T>(instructions)`；actor 是 create_actor 事件里的 guest 本地 id，
    /// actor_seq 是该 actor 上第 n 次 ask（guest 侧单调计数，保证 FIFO 对齐）。
    Ask { actor: u32, actor_seq: u32, instructions: String },
    /// `files.*` / `git.*` 只读调用。
    WorldRead { kind: WorldReadKind, arguments: Vec<Value> },
    /// `world.run(cmd, args, opts)`；cmd 编译期已强制为字面量。
    WorldRun { command: String, args: Vec<String>, options: WorkflowWorldRunOptions },
    /// `artifact.file` / `artifact.markdown`：有返回值（ArtifactRef）的发布。
    PublishArtifact { member: PublishArtifactMember, arguments: Vec<Value> },
    /// `hole(name, prompt?)` 无预置体：挂起等 fill_workflow_hole 交付编译好的体。
    Hole { name: String, prompt: Option<String> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorldReadKind {
    FilesGlob,
    FilesRead,
    FilesGrep,
    GitChangedFiles,
    GitDiff,
    GitStatus,
    GitLog,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishArtifactMember {
    File,
    Markdown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowWorldRunOptions {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
}

/// 即发即忘消息；到达顺序即执行顺序，宿主按 site_id 计 ordinal。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkflowEvent {
    /// `agent(...)`：guest 同步分配 local_id，宿主建 local_id → actor 映射。
    CreateActor { local_id: u32, site_id: String, name: Option<String>, persona: Value },
    /// `phase(name)` 标记到达；同名 phase 合并为一个节点。
    PhaseEntered { site_id: String, name: String },
    /// `log(msg)`。
    Log { site_id: String, message: String },
    /// `report(item, artifactId?)`。
    Report { site_id: String, item: Value, artifact_id: Option<String> },
    /// `artifact.{chart,table,metrics,board}`：预置类声明（同步 void）。
    DeclareArtifact { site_id: String, member: DeclareArtifactMember, arguments: Vec<Value> },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeclareArtifactMember {
    Chart,
    Table,
    Metrics,
    Board,
}

/// 停滞内省：`__wf.stalled()` 的返回值；channel 名与挂在它上面的接收数。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStallInfo {
    /// 非空即 ChannelDeadlock；空则 ScriptStalled。
    pub waiting: Vec<WorkflowChannelWait>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowChannelWait {
    pub channel: String,
    pub receivers: u32,
}

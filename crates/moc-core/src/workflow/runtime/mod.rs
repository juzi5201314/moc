//! workflow 运行时：run 生命周期、journal/worker/driver 粘合。
//!
//! 一个 run = 一个 `moc --workflow-worker` 子进程（NDJSON 帧）+ 内存中的确定性
//! [`Engine`] + 一组 driver 副作用。run 目录 `<sidecar>/workflows/<run_id>/`：
//! `run.json` 元数据、`journal.jsonl`（每条 fsync）、`script.ts` 草稿、`artifacts/`。
//! 加载会话时状态仍 `running` 且本进程不持有执行者的 run 标为 `stopped(interrupted)`。

mod driver;
pub mod tools;

pub(crate) use driver::WorkflowDriver;
pub use tools::WORKFLOW_TOOLS;

use rust_i18n::t;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moc_protocol::{
    WorkflowEvent, WorkflowPhaseState, WorkflowRoleState, WorkflowRunStatus, WorkflowRunSummary,
    WorkflowStopReason,
};
use moc_ptc_protocol::wire::WorkflowHostMessage;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::store::SessionPaths;
use crate::workflow::engine::{DriverRequest, Engine, Terminal, TerminalStatus};

/// workflow worker 启动命令：`--ptc-worker` 换成 `--workflow-worker`，程序与
/// 其余参数沿用 PTC worker 配置（测试经 MOC_TEST_BIN 指向 moc 可执行文件）。
pub(crate) fn worker_command(
    inner: &CoreInner,
) -> Result<(std::path::PathBuf, Vec<std::ffi::OsString>), CoreError> {
    let worker = inner
        .boot
        .ptc_worker
        .as_ref()
        .ok_or_else(|| CoreError::Session(t!("core.err.worker_not_configured").into_owned()))?;
    let mut args = worker.args.clone();
    let mut replaced = false;
    for arg in &mut args {
        if arg == "--ptc-worker" {
            *arg = "--workflow-worker".into();
            replaced = true;
        }
    }
    if !replaced {
        args.push("--workflow-worker".into());
    }
    Ok((worker.program.clone(), args))
}

/// 会话级 workflow 管理器：run 注册表 + 待决审批。
pub(crate) struct WorkflowManager {
    inner: Arc<CoreInner>,
    session_id: String,
    paths: SessionPaths,
    /// 本进程持有的运行中 run。
    runs: Mutex<HashMap<String, Arc<RunHandle>>>,
    /// request_id → 审批决策通道。
    approvals: Mutex<HashMap<String, oneshot::Sender<moc_protocol::WorkflowDecision>>>,
    /// run_id → role_id → 模型覆盖：approve→launch 窗口内 set_role_model
    /// 落到此表（run.json 与注册表都还不可见），launch 落盘前合并进 meta。
    pending_role_models: Mutex<HashMap<String, HashMap<String, String>>>,
    seq: Mutex<u64>,
    /// 本实例启动时刻：interrupted 降级只认更早创建的 run——晚于它的
    /// running run.json 属于本进程未注册完成的活 run，不能误杀。
    birth_ms: u64,
}

/// run 元数据（run.json）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct RunMeta {
    pub run_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub plan: Value,
    #[serde(default)]
    pub args: Value,
    #[serde(default)]
    pub commands: Vec<String>,
    /// role_id → 运行中模型覆盖（`workflow_set_role_model`）。
    #[serde(default)]
    pub role_models: HashMap<String, String>,
    /// `model()` ref → 模型（审批窗重映射表）。
    #[serde(default)]
    pub model_ref_map: HashMap<String, String>,
    /// run 级默认子代理模型（`create_workflow.subagent_model`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subagent_model: Option<String>,
    /// 审批时的会话模型快照（解析链兜底）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_from: Option<String>,
    #[serde(default)]
    pub mode: String,
    #[serde(default)]
    pub created_at_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished_at_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default)]
    pub reports: Vec<Value>,
    #[serde(default)]
    pub artifacts: Vec<Value>,
    #[serde(default)]
    pub holes: Vec<Value>,
    /// 终态落盘的运行态投影：完成后 view 丢弃，快照从 meta 还原。
    #[serde(default)]
    pub roles: Vec<WorkflowRoleState>,
    #[serde(default)]
    pub phases: Vec<WorkflowPhaseState>,
    #[serde(default)]
    pub usage: Value,
}

/// 运行中 run 的运行态投影（供 snapshot/status）。
pub(crate) struct RunView {
    pub phases: Vec<WorkflowPhaseState>,
    pub roles: Vec<WorkflowRoleState>,
    pub usage: Value,
    pub terminal: Option<Terminal>,
    /// actor (site,ordinal) → 子会话 id（driver 建立）。
    pub actor_sessions: HashMap<(String, u32), String>,
    /// 首 ask 已随 spawn 发出的 actor（防重复投递）。
    pub actor_first_sent: std::collections::HashSet<(String, u32)>,
}

pub(crate) struct RunHandle {
    pub id: String,
    pub dir: PathBuf,
    pub meta: Mutex<RunMeta>,
    pub view: Mutex<RunView>,
    pub engine: Mutex<Engine>,
    /// artifact 发布串行化：版本号分配 → 写文件 → push 条目必须原子，
    /// 否则同 id 并发 publish 算出同一版本互相覆盖。
    pub publish_lock: Mutex<()>,
    pub cancel: CancellationToken,
    /// 工作区 cwd（world/files/git 基准）。
    pub cwd: PathBuf,
    /// run 发起时的反向请求上下文（子代理 spawn/message 用）。
    pub request: moc_protocol::RequestContext,
    /// world.run 沙箱策略快照（run 启动时冻结）。
    pub policy: Mutex<Option<crate::sandbox::SandboxPolicy>>,
    /// 发给 worker stdin 的帧。
    to_worker: tokio::sync::mpsc::UnboundedSender<WorkflowHostMessage>,
}

mod approval;
mod execution;
mod journal;
mod lifecycle;

impl WorkflowManager {
    pub fn new(inner: Arc<CoreInner>, session_id: &str, paths: SessionPaths) -> Arc<Self> {
        let manager = Arc::new(Self {
            inner,
            session_id: session_id.to_string(),
            paths,
            runs: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            pending_role_models: Mutex::new(HashMap::new()),
            seq: Mutex::new(0),
            birth_ms: crate::session::store::now_ms(),
        });
        // 加载即做 interrupted 标记：磁盘上 running 但本进程不持有的 run → stopped(interrupted)。
        for meta in list_run_metas(&manager.paths.workflows()) {
            if meta.status == "running" {
                let mut meta = meta;
                meta.status = "stopped".into();
                meta.stop_reason = Some("interrupted".into());
                let dir = manager.paths.workflows().join(&meta.run_id);
                let _ = std::fs::write(
                    dir.join("run.json"),
                    serde_json::to_vec_pretty(&meta).unwrap_or_default(),
                );
            }
        }
        manager
    }

    /// run 目录；run_id 来自外部输入（工具/RPC 参数），校验字符集防路径穿越。
    pub fn run_dir(&self, run_id: &str) -> Result<PathBuf, CoreError> {
        if !valid_run_id(run_id) {
            return Err(CoreError::Session(format!(
                "invalid workflow run_id \"{run_id}\" (must match [A-Za-z0-9_-]{{1,64}})"
            )));
        }
        Ok(self.paths.workflows().join(run_id))
    }

    /// 分配 run_id（`run_<ms>_<seq>`，按目录唯一）。
    pub fn alloc_run_id(&self) -> String {
        let mut seq = self.seq.lock();
        *seq += 1;
        format!("run_{}_{:03}", crate::session::store::now_ms(), *seq)
    }

    /// 审批 request_id：seq 计数器分配（毫秒戳会碰撞）。
    pub fn alloc_request_id(&self, prefix: &str, run_id: &str) -> String {
        let mut seq = self.seq.lock();
        *seq += 1;
        format!("{prefix}-{run_id}-{:03}", *seq)
    }
}

/// run_id 字符集：`[A-Za-z0-9_-]{1,64}`（run_id 直接拼目录名）。
fn valid_run_id(run_id: &str) -> bool {
    !run_id.is_empty()
        && run_id.len() <= 64
        && run_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// 终态 → meta 字段投影（status/stop_reason/result/error/finished_at_ms）；
/// persist_terminal 落盘与 stop() 的即时 summary 共用。
fn apply_terminal(meta: &mut RunMeta, terminal: &Terminal) {
    meta.finished_at_ms = Some(crate::session::store::now_ms());
    match terminal.status {
        TerminalStatus::Completed => {
            meta.status = "completed".into();
            meta.result = terminal.result.clone();
        }
        TerminalStatus::Errored => {
            meta.status = "errored".into();
            meta.error = terminal
                .error_message
                .clone()
                .or(terminal.error_code.clone());
        }
        TerminalStatus::Stopped { reason } => {
            meta.status = "stopped".into();
            meta.stop_reason = Some(
                match reason {
                    crate::workflow::engine::StopReason::User => "user",
                    crate::workflow::engine::StopReason::Interrupted => "interrupted",
                    crate::workflow::engine::StopReason::Provider => "provider",
                }
                .into(),
            );
            // 崩溃类 stopped 也带诊断：worker 秒退的原因（stderr）在
            // error_message 里，不写进 meta.error 就无从排查。
            if terminal.error_message.is_some() {
                meta.error = terminal.error_message.clone();
            }
        }
    }
}

fn node_update(view: &RunView) -> Option<WorkflowEvent> {
    Some(WorkflowEvent::NodeUpdated {
        phases: view.phases.clone(),
        roles: view.roles.clone(),
    })
}

fn request_id_of(request: &DriverRequest) -> u64 {
    match request {
        DriverRequest::Ask { request_id, .. }
        | DriverRequest::WorldRead { request_id, .. }
        | DriverRequest::WorldRun { request_id, .. }
        | DriverRequest::PublishArtifact { request_id, .. } => *request_id,
        DriverRequest::CancelAsk { .. } => 0,
    }
}

/// `RunMeta` → 协议摘要（service 方法与 `list()` 共用）。
pub(crate) fn workflow_summary(meta: &RunMeta) -> WorkflowRunSummary {
    let status = match meta.status.as_str() {
        "completed" => WorkflowRunStatus::Completed,
        "errored" => WorkflowRunStatus::Errored,
        "stopped" => WorkflowRunStatus::Stopped {
            reason: match meta.stop_reason.as_deref() {
                Some("interrupted") => WorkflowStopReason::Interrupted,
                Some("provider") => WorkflowStopReason::Provider,
                _ => WorkflowStopReason::User,
            },
        },
        _ => WorkflowRunStatus::Running,
    };
    WorkflowRunSummary {
        run_id: meta.run_id.clone(),
        status,
        title: (!meta.title.is_empty()).then(|| meta.title.clone()),
        created_at_ms: (meta.created_at_ms > 0).then_some(meta.created_at_ms),
        finished_at_ms: meta.finished_at_ms,
        resumed_from: meta.resumed_from.clone(),
        result: meta.result.clone(),
        error: meta.error.clone(),
    }
}

/// 磁盘上的全部 run.json（含历史 run）。
pub(crate) fn list_run_metas(dir: &Path) -> Vec<RunMeta> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut metas: Vec<RunMeta> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().is_dir())
        .filter_map(|e| read_run_meta(&e.path()).ok())
        .collect();
    metas.sort_by_key(|m| m.created_at_ms);
    metas
}

pub(crate) fn read_run_meta(dir: &Path) -> Result<RunMeta, CoreError> {
    let data = std::fs::read(dir.join("run.json"))
        .map_err(|e| CoreError::Session(t!("core.err.run_json_read", error = e).into_owned()))?;
    serde_json::from_slice(&data)
        .map_err(|e| CoreError::Session(t!("core.err.run_json_parse", error = e).into_owned()))
}

/// running 且无执行者的 run 就地降级为 stopped(interrupted) 并回写 run.json。
/// `birth_ms` 是调用方 manager 的启动时刻：`created_at_ms` 不早于它的 run
/// 属于本进程（写盘/注册窗口中的活 run），不得按"进程崩溃残留"降级。
fn mark_interrupted(dir: &Path, meta: &mut RunMeta, birth_ms: u64) {
    if meta.status != "running" || meta.created_at_ms >= birth_ms {
        return;
    }
    meta.status = "stopped".into();
    meta.stop_reason = Some("interrupted".into());
    meta.finished_at_ms = Some(crate::session::store::now_ms());
    let _ = std::fs::write(
        dir.join("run.json"),
        serde_json::to_vec_pretty(meta).unwrap_or_default(),
    );
}

/// `session/workflow_run` 的完整快照。
pub(crate) fn snapshot_of(
    meta: &RunMeta,
    view: Option<&RunView>,
    dir: &Path,
) -> moc_protocol::WorkflowRunSnapshot {
    moc_protocol::WorkflowRunSnapshot {
        summary: workflow_summary(meta),
        plan: (meta.plan != Value::Null).then(|| meta.plan.clone()),
        phases: view
            .map(|v| v.phases.clone())
            .unwrap_or_else(|| meta.phases.clone()),
        roles: view
            .map(|v| v.roles.clone())
            .unwrap_or_else(|| meta.roles.clone()),
        commands: meta.commands.clone(),
        reports: meta.reports.clone(),
        artifacts: meta.artifacts.clone(),
        holes: meta.holes.clone(),
        usage: view
            .map(|v| v.usage.clone())
            .unwrap_or_else(|| meta.usage.clone()),
        script_path: dir
            .join("script.ts")
            .exists()
            .then(|| dir.join("script.ts").display().to_string()),
    }
}

#[cfg(test)]
mod tests;

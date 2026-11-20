//! workflow 运行时：run 生命周期、journal/worker/driver 粘合。
//!
//! 一个 run = 一个 `moc --workflow-worker` 子进程（NDJSON 帧）+ 内存中的确定性
//! [`Engine`] + 一组 driver 副作用。run 目录 `<sidecar>/workflows/<run_id>/`：
//! `run.json` 元数据、`journal.jsonl`（每条 fsync）、`script.ts` 草稿、`artifacts/`。
//! 取得会话写租约后首次访问时，将残留的 `running` run 标为 `stopped(interrupted)`。

mod driver;
pub mod tools;

pub(crate) use driver::WorkflowDriver;
pub use tools::WORKFLOW_TOOLS;

use rust_i18n::t;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};

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
    /// 全部孤儿写回成功后才跳过扫描；失败留待下次访问重试。
    orphan_marked: AtomicBool,
    /// 串行化扫描；并发访问须等本次标记完成再启动本进程的 run。
    orphan_marking: StdMutex<()>,
    /// 首次持写租约扫描的时刻；重试不能误伤此后新建但尚未注册的 run。
    orphan_epoch: AtomicU64,
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
    /// 包含终态落盘与失败收尾的主循环任务；卸载须等其结束再释放租约。
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
        Arc::new(Self {
            inner,
            session_id: session_id.to_string(),
            paths,
            runs: Mutex::new(HashMap::new()),
            approvals: Mutex::new(HashMap::new()),
            pending_role_models: Mutex::new(HashMap::new()),
            seq: Mutex::new(0),
            orphan_marked: AtomicBool::new(false),
            orphan_marking: StdMutex::new(()),
            orphan_epoch: AtomicU64::new(0),
        })
    }

    /// 只有本进程持有写租约，才能确认磁盘上的 running run 已无其它进程执行。
    /// 只读创建管理器不消耗标记机会；共享租约保证整个扫描写回期间内核锁仍被持有。
    pub fn mark_orphans(&self, record: &crate::session::SessionRecord) {
        if self.orphan_marked.load(Ordering::Acquire) {
            return;
        }
        let Some(_lease) = record.write_lease() else {
            return;
        };
        let _marking = self
            .orphan_marking
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if self.orphan_marked.load(Ordering::Acquire) {
            return;
        }
        let epoch = self.orphan_epoch.load(Ordering::Relaxed);
        if epoch == 0 {
            self.orphan_epoch
                .store(crate::session::store::now_ms(), Ordering::Relaxed);
        }
        // 恢复的旧 run 保留原创建时间，须按注册表排除；快照后释放锁再读写磁盘。
        let running: HashSet<_> = self.runs.lock().keys().cloned().collect();
        let (metas, read_errors) = list_run_metas_with_errors(&self.paths.workflows());
        let mut all_marked = read_errors == 0;
        for mut meta in metas {
            // 首次扫描涵盖原写者的全部 run；重试只处理首次扫描前的残留。
            if meta.status != "running"
                || running.contains(&meta.run_id)
                || (epoch != 0 && meta.created_at_ms >= epoch)
            {
                continue;
            }
            if let Ok(dir) = self.run_dir(&meta.run_id) {
                all_marked &= mark_interrupted(&dir, &mut meta);
            }
        }
        if all_marked {
            self.orphan_marked.store(true, Ordering::Release);
        }
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
    list_run_metas_with_errors(dir).0
}

/// 同时统计已有 run.json 的读取或解析失败，供孤儿扫描重试；杂散空目录不计入。
fn list_run_metas_with_errors(dir: &Path) -> (Vec<RunMeta>, usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (Vec::new(), 0);
    };
    let mut metas = Vec::new();
    let mut read_errors = 0;
    for entry in entries.filter_map(Result::ok) {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        match read_run_meta(&path) {
            Ok(meta) => metas.push(meta),
            Err(_) if path.join("run.json").exists() => read_errors += 1,
            Err(_) => {}
        }
    }
    metas.sort_by_key(|m| m.created_at_ms);
    (metas, read_errors)
}

pub(crate) fn read_run_meta(dir: &Path) -> Result<RunMeta, CoreError> {
    let data = std::fs::read(dir.join("run.json"))
        .map_err(|e| CoreError::Session(t!("core.err.run_json_read", error = e).into_owned()))?;
    serde_json::from_slice(&data)
        .map_err(|e| CoreError::Session(t!("core.err.run_json_parse", error = e).into_owned()))
}

/// 持写租约扫描时，将孤儿 run 降级为 stopped(interrupted)，返回写回是否成功。
fn mark_interrupted(dir: &Path, meta: &mut RunMeta) -> bool {
    meta.status = "stopped".into();
    meta.stop_reason = Some("interrupted".into());
    meta.finished_at_ms = Some(crate::session::store::now_ms());
    std::fs::write(
        dir.join("run.json"),
        serde_json::to_vec_pretty(meta).unwrap_or_default(),
    )
    .is_ok()
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

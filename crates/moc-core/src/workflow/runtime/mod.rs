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

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use moc_protocol::{
    UpdateSink, WorkflowEvent, WorkflowPhaseState, WorkflowRoleState, WorkflowRunStatus,
    WorkflowRunSummary, WorkflowStopReason,
};
use moc_ptc_protocol::wire::{
    HostCompletion, WorkflowGuestMessage, WorkflowHostMessage, WorkflowRunInfo as WireRunInfo,
    WorkflowStartRequest,
};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::store::SessionPaths;
use crate::workflow::compile::{CompiledWorkflow, Diagnostic, compile};
use crate::workflow::engine::{
    DriverOutcome, DriverRequest, Engine, EngineAction, EngineConfig, EngineEvent, EngineRequest,
    FileJournal, RunMode, ScriptState, SessionEvent, Step, Terminal, TerminalStatus,
};

/// workflow worker 启动命令：`--ptc-worker` 换成 `--workflow-worker`，程序与
/// 其余参数沿用 PTC worker 配置（测试经 MOC_TEST_BIN 指向 moc 可执行文件）。
pub(crate) fn worker_command(
    inner: &CoreInner,
) -> Result<(std::path::PathBuf, Vec<std::ffi::OsString>), CoreError> {
    let worker = inner
        .boot
        .ptc_worker
        .as_ref()
        .ok_or_else(|| CoreError::Session("workflow worker 未配置（ptc_worker）".into()))?;
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

    /// 待决审批：注册并发出 `approval_requested` 事件。
    pub fn request_approval(
        &self,
        approval: moc_protocol::WorkflowApproval,
        sink: &UpdateSink,
    ) -> oneshot::Receiver<moc_protocol::WorkflowDecision> {
        let (tx, rx) = oneshot::channel();
        let request_id = approval.request_id.clone();
        let run_id = approval.run_id.clone();
        self.approvals.lock().insert(request_id, tx);
        let session_id = self.session_id.clone();
        let sink = sink.clone();
        tokio::spawn(async move {
            let _ = sink
                .emit(
                    session_id,
                    moc_protocol::SessionUpdate::Workflow {
                        run_id,
                        event: WorkflowEvent::ApprovalRequested {
                            request: Box::new(approval),
                        },
                    },
                )
                .await;
        });
        rx
    }

    /// `session/workflow_approve` 决策落地。
    pub fn resolve_approval(
        &self,
        request_id: &str,
        decision: moc_protocol::WorkflowDecision,
        sink: &UpdateSink,
    ) -> Result<String, CoreError> {
        let Some(tx) = self.approvals.lock().remove(request_id) else {
            return Err(CoreError::Session(format!(
                "workflow approval {request_id} 不存在或已解决"
            )));
        };
        let approved = matches!(decision, moc_protocol::WorkflowDecision::Approve { .. });
        let feedback = match &decision {
            moc_protocol::WorkflowDecision::Reject { feedback } => feedback.clone(),
            _ => None,
        };
        let _ = tx.send(decision);
        let session_id = self.session_id.clone();
        let sink = sink.clone();
        let request_id = request_id.to_string();
        tokio::spawn(async move {
            let _ = sink
                .emit(
                    session_id,
                    moc_protocol::SessionUpdate::Workflow {
                        run_id: String::new(),
                        event: WorkflowEvent::ApprovalResolved {
                            request_id,
                            approved,
                            feedback,
                        },
                    },
                )
                .await;
        });
        Ok(String::new())
    }

    /// 编译 + 落草稿；诊断非空时返回它们。
    pub fn compile_script(
        &self,
        run_id: &str,
        source: &str,
    ) -> Result<CompiledWorkflow, Vec<Diagnostic>> {
        let compiled = compile(source, &crate::workflow::compile::CompileOptions::default())?;
        let dir = self.run_dir(run_id).map_err(|e| {
            vec![Diagnostic {
                code: "E_IO",
                message: e.to_string(),
                line: 0,
                col: 0,
            }]
        })?;
        std::fs::create_dir_all(&dir).map_err(|e| {
            vec![Diagnostic {
                code: "E_IO",
                message: format!("run 目录创建失败：{e}"),
                line: 0,
                col: 0,
            }]
        })?;
        std::fs::write(dir.join("script.ts"), source).map_err(|e| {
            vec![Diagnostic {
                code: "E_IO",
                message: format!("草稿脚本写入失败：{e}"),
                line: 0,
                col: 0,
            }]
        })?;
        Ok(compiled)
    }

    /// 启动 run：建元数据、spawn worker、起引擎循环。
    #[allow(clippy::too_many_arguments)]
    pub async fn launch(
        self: &Arc<Self>,
        meta: RunMeta,
        compiled: CompiledWorkflow,
        request: moc_protocol::RequestContext,
        cwd: PathBuf,
        policy: crate::sandbox::SandboxPolicy,
        sink: UpdateSink,
        cancel_parent: CancellationToken,
    ) -> Result<Arc<RunHandle>, CoreError> {
        let run_id = meta.run_id.clone();
        let dir = self.run_dir(&run_id)?;
        std::fs::create_dir_all(dir.join("artifacts"))
            .map_err(|e| CoreError::Session(format!("run 目录创建失败：{e}")))?;
        // approve→launch 窗口内经 set_role_model 记下的覆盖合并进 meta
        // （"inherit" 表示移除该 role 的覆盖）。
        let mut meta = meta;
        if let Some(overrides) = self.pending_role_models.lock().remove(&run_id) {
            for (role, model) in overrides {
                if model == "inherit" {
                    meta.role_models.remove(&role);
                } else {
                    meta.role_models.insert(role, model);
                }
            }
        }
        // run.json 先行落盘（approve→set_role_model 窗口内磁盘必须可读）；
        // 本进程 run 的 created_at_ms ≥ birth_ms，interrupted 降级不会误杀。
        std::fs::write(
            dir.join("run.json"),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        )
        .map_err(|e| CoreError::Session(format!("run.json 写入失败：{e}")))?;
        let mode = match meta.mode.as_str() {
            "resume" => RunMode::Resume,
            "amend" => RunMode::Amend,
            _ => RunMode::Fresh,
        };
        let journal = FileJournal::open(&dir)
            .map_err(|e| CoreError::Session(format!("journal 打开失败：{e}")))?;
        let config = EngineConfig {
            max_concurrency: self.inner.config.read().workflow.max_concurrency_ceiling,
            label: meta.title.clone(),
            mode,
            resumed_from: meta.resumed_from.clone(),
        };
        let engine = Engine::new(Box::new(journal), config);
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let handle = Arc::new(RunHandle {
            id: run_id.clone(),
            dir: dir.clone(),
            meta: Mutex::new(meta),
            view: Mutex::new(RunView {
                phases: Vec::new(),
                roles: Vec::new(),
                usage: json!({}),
                terminal: None,
                actor_sessions: HashMap::new(),
                actor_first_sent: std::collections::HashSet::new(),
            }),
            engine: Mutex::new(engine),
            publish_lock: Mutex::new(()),
            cancel: cancel_parent.child_token(),
            cwd,
            request,
            policy: Mutex::new(Some(policy)),
            to_worker: tx,
        });
        self.runs.lock().insert(run_id.clone(), Arc::clone(&handle));
        // 落盘到注册的窗口内经 set_role_model 记的覆盖：应用进内存 meta
        // （盘上的 run.json 已由 set_role_model 写好同值）。
        if let Some(overrides) = self.pending_role_models.lock().remove(&run_id) {
            let mut meta = handle.meta.lock();
            for (role, model) in overrides {
                if model == "inherit" {
                    meta.role_models.remove(&role);
                } else {
                    meta.role_models.insert(role, model);
                }
            }
        }
        let manager = Arc::clone(self);
        let handle2 = Arc::clone(&handle);
        tokio::spawn(async move {
            if let Err(error) = manager.run_loop(&handle2, compiled, rx, sink).await {
                manager.fail_run(&handle2, &format!("{error}"));
            }
        });
        Ok(handle)
    }

    /// 运行中的 handle（本进程持有才算 running）。
    pub fn running(&self, run_id: &str) -> Option<Arc<RunHandle>> {
        self.runs.lock().get(run_id).cloned()
    }

    /// 读取 run 元数据（优先本进程，否则读磁盘）。磁盘上 running 但本进程不
    /// 持有执行者的 run 就地降级为 stopped(interrupted)——租约语义：只有持
    /// 写租约的进程才执行 run。
    pub fn meta(&self, run_id: &str) -> Result<RunMeta, CoreError> {
        if let Some(handle) = self.runs.lock().get(run_id) {
            return Ok(handle.meta.lock().clone());
        }
        let dir = self.run_dir(run_id)?;
        let mut meta = read_run_meta(&dir)?;
        mark_interrupted(&dir, &mut meta, self.birth_ms);
        Ok(meta)
    }

    pub fn list(&self) -> Vec<WorkflowRunSummary> {
        let held = self.runs.lock();
        list_run_metas(&self.paths.workflows())
            .into_iter()
            .map(|mut meta| {
                if let Some(handle) = held.get(&meta.run_id) {
                    workflow_summary(&handle.meta.lock())
                } else {
                    if let Ok(dir) = self.run_dir(&meta.run_id) {
                        mark_interrupted(&dir, &mut meta, self.birth_ms);
                    }
                    workflow_summary(&meta)
                }
            })
            .collect()
    }

    /// 停止 run：引擎终态 + worker cancel + 进程回收。
    pub async fn stop(self: &Arc<Self>, run_id: &str) -> Result<WorkflowRunSummary, CoreError> {
        let Some(handle) = self.runs.lock().get(run_id).cloned() else {
            return Err(CoreError::Session(format!(
                "workflow run {run_id} 不在运行中"
            )));
        };
        let actions = handle.engine.lock().cancel("E_STOPPED");
        handle.cancel.cancel();
        let _ = handle.to_worker.send(WorkflowHostMessage::Cancel {
            code: "E_STOPPED".into(),
        });
        self.apply_actions(&handle, actions).await;
        // 终态落盘在 run_loop 异步完成；summary 先按已记录的终态投影。
        let mut meta = handle.meta.lock().clone();
        if let Some(terminal) = handle.view.lock().terminal.clone() {
            apply_terminal(&mut meta, &terminal);
        }
        Ok(workflow_summary(&meta))
    }

    /// `workflow_set_role_model`：run 中生效（写 meta + journal 之外的覆盖表）。
    pub fn set_role_model(
        &self,
        run_id: &str,
        role_id: &str,
        model: &str,
    ) -> Result<(), CoreError> {
        if let Some(handle) = self.runs.lock().get(run_id) {
            let mut meta = handle.meta.lock();
            if model == "inherit" {
                meta.role_models.remove(role_id);
            } else {
                meta.role_models
                    .insert(role_id.to_string(), model.to_string());
            }
            let dir = handle.dir.clone();
            drop(meta);
            let meta = handle.meta.lock().clone();
            let _ = std::fs::write(
                dir.join("run.json"),
                serde_json::to_vec_pretty(&meta).unwrap_or_default(),
            );
            return Ok(());
        }
        let dir = self.run_dir(run_id)?;
        if let Ok(mut meta) = read_run_meta(&dir) {
            if model == "inherit" {
                meta.role_models.remove(role_id);
            } else {
                meta.role_models
                    .insert(role_id.to_string(), model.to_string());
            }
            let running = meta.status == "running";
            std::fs::write(
                dir.join("run.json"),
                serde_json::to_vec_pretty(&meta).unwrap_or_default(),
            )
            .map_err(|e| CoreError::Session(format!("run.json 写入失败：{e}")))?;
            // run.json 已写盘但 status 仍 running：launch 的写盘→注册窗口，
            // 覆盖要进 pending，注册后二次应用到 handle.meta。
            if running {
                self.pending_role_models
                    .lock()
                    .entry(run_id.to_string())
                    .or_default()
                    .insert(role_id.to_string(), model.to_string());
            }
            return Ok(());
        }
        // approve→launch 窗口：run.json 还没写但草稿已在 → 记 pending，
        // launch 落盘前合并进 meta。
        if dir.is_dir() && dir.join("script.ts").exists() {
            self.pending_role_models
                .lock()
                .entry(run_id.to_string())
                .or_default()
                .insert(role_id.to_string(), model.to_string());
            return Ok(());
        }
        Err(CoreError::Session(format!("workflow run {run_id} 不存在")))
    }

    /// 丢弃 run 的待合并 role 覆盖（launch 审批被 Reject 后不留孤儿条目）。
    pub fn drop_pending_role_models(&self, run_id: &str) {
        self.pending_role_models.lock().remove(run_id);
    }

    /// resume 准备：只允许 stopped run；读草稿编译；改过的草稿不得引入
    /// 审批命令集之外的 world.run（否则绕过命令集审批）。
    pub fn prepare_resume(&self, run_id: &str) -> Result<(RunMeta, CompiledWorkflow), CoreError> {
        let dir = self.run_dir(run_id)?;
        let mut meta = read_run_meta(&dir)?;
        if meta.status != "stopped" {
            return Err(CoreError::Session(format!(
                "run {run_id} is {} and cannot be resumed (only stopped runs resume)",
                meta.status
            )));
        }
        let source = std::fs::read_to_string(dir.join("script.ts"))
            .map_err(|e| CoreError::Session(format!("draft unreadable: {e}")))?;
        let compiled = crate::workflow::compile::compile(
            &source,
            &crate::workflow::compile::CompileOptions::default(),
        )
        .map_err(|diags| {
            CoreError::Session(format!(
                "script no longer compiles: {}",
                diags
                    .iter()
                    .map(|d| d.message.clone())
                    .collect::<Vec<_>>()
                    .join("; ")
            ))
        })?;
        let approved: std::collections::BTreeSet<&String> = meta.commands.iter().collect();
        let new_commands: Vec<String> = compiled
            .plan
            .commands
            .iter()
            .map(|c| &c.command)
            .filter(|c| !approved.contains(*c))
            .cloned()
            .collect();
        if !new_commands.is_empty() {
            return Err(CoreError::Session(format!(
                "draft introduces unapproved world.run commands [{}]; use amend_workflow",
                new_commands.join(", ")
            )));
        }
        meta.status = "running".into();
        meta.mode = "resume".into();
        Ok((meta, compiled))
    }

    /// 填充 hole：`fill_workflow_hole` → 引擎 fill_hole → worker 回执。
    pub async fn fill_hole(
        self: &Arc<Self>,
        run_id: &str,
        name: &str,
        code: String,
    ) -> Result<(), CoreError> {
        let Some(handle) = self.runs.lock().get(run_id).cloned() else {
            return Err(CoreError::Session(format!(
                "workflow run {run_id} 不在运行中"
            )));
        };
        // 引擎按 hole 名找待决 HoleResolve。
        let actions = {
            let mut engine = handle.engine.lock();
            let (site_id, ordinal) = {
                let meta = handle.meta.lock();
                meta.holes
                    .iter()
                    .find(|h| h["name"].as_str() == Some(name))
                    .and_then(|h| {
                        Some((
                            h["site_id"].as_str()?.to_string(),
                            h["ordinal"].as_u64()? as u32,
                        ))
                    })
                    .ok_or_else(|| CoreError::Session(format!("hole {name} 未打开")))?
            };
            engine.fill_hole(&site_id, ordinal, code)
        };
        self.apply_actions(&handle, actions).await;
        Ok(())
    }

    /// worker/host 主循环：Step → 引擎 → actions（Complete 回 worker、Dispatch 走 driver、
    /// Session 事件 → SessionUpdate、Finalized → 落盘 + 通知）。
    async fn run_loop(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        compiled: CompiledWorkflow,
        mut rx: tokio::sync::mpsc::UnboundedReceiver<WorkflowHostMessage>,
        sink: UpdateSink,
    ) -> Result<(), CoreError> {
        // —— spawn worker ——
        let (program, args) = worker_command(&self.inner)?;
        let mut child = Command::new(program)
            .args(&args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| CoreError::Session(format!("workflow worker 启动失败：{e}")))?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| CoreError::Session("worker stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| CoreError::Session("worker stdout".into()))?;
        // stderr 收集进 fail_run 诊断（worker 秒退时无事件可看）。
        let stderr_buf = Arc::new(Mutex::new(Vec::<String>::new()));
        if let Some(stderr) = child.stderr.take() {
            let buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = buf.lock();
                    if buf.len() < 20 {
                        buf.push(line);
                    }
                }
            });
        }
        // 写线程：channel → stdin 帧。
        let write_cancel = handle.cancel.clone();
        let writer = tokio::spawn(async move {
            while let Some(message) = rx.recv().await {
                let mut frame = serde_json::to_vec(&message).unwrap_or_default();
                frame.push(b'\n');
                if stdin.write_all(&frame).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
                if matches!(message, WorkflowHostMessage::Shutdown) {
                    break;
                }
            }
            write_cancel.cancel();
        });
        // Start 帧。
        let cfg = self.inner.config.read().tools.ptc.clone();
        let meta = handle.meta.lock().clone();
        handle
            .to_worker
            .send(WorkflowHostMessage::Start {
                request: WorkflowStartRequest {
                    code: compiled.lowered.clone(),
                    args: meta.args.clone(),
                    run: WireRunInfo {
                        id: handle.id.clone(),
                        resumed_from: meta.resumed_from.clone(),
                    },
                    memory_limit_bytes: cfg.memory_limit_bytes,
                    max_output_bytes: cfg.max_output_bytes,
                    max_message_bytes: cfg.max_result_bytes,
                    max_pending_calls: cfg.max_pending_calls,
                    max_total_calls: cfg.max_total_calls,
                },
            })
            .map_err(|_| CoreError::Session("worker 通道已关闭".into()))?;
        drop(compiled);
        let meta_plan = meta.plan.clone();
        drop(meta);

        // —— 读步循环 ——
        let mut reader = BufReader::new(stdout).lines();
        // guest local actor id → (site_id, ordinal)。
        let mut local_actors: HashMap<u32, (String, u32)> = HashMap::new();
        let mut actor_ordinals: HashMap<String, u32> = HashMap::new();
        loop {
            let line = tokio::select! {
                _ = handle.cancel.cancelled() => break,
                line = reader.next_line() => line,
            };
            let line = match line {
                Ok(Some(line)) => line,
                Ok(None) => break,
                Err(e) => return Err(CoreError::Session(format!("worker 输出读取失败：{e}"))),
            };
            let message: WorkflowGuestMessage = match serde_json::from_str(&line) {
                Ok(m) => m,
                Err(e) => {
                    tracing::warn!("workflow worker 帧解析失败：{e}");
                    continue;
                }
            };
            let WorkflowGuestMessage::Step { step } = message;
            let guest = self.map_step(
                &handle.id,
                step,
                &meta_plan,
                &mut local_actors,
                &mut actor_ordinals,
            );
            let actions = handle.engine.lock().handle_step(guest);
            self.apply_actions(handle, actions).await;
            if handle.view.lock().terminal.is_some() {
                break;
            }
        }
        // worker stdout EOF / cancel 而未到终态：视作崩溃，不能无声消失。
        if handle.view.lock().terminal.is_none() {
            let stderr = stderr_buf.lock().join("\n");
            let detail = if stderr.is_empty() {
                String::new()
            } else {
                format!("：stderr: {stderr}")
            };
            self.fail_run(handle, &format!("workflow worker 未达终态即退出{detail}"));
        }
        let _ = handle.to_worker.send(WorkflowHostMessage::Shutdown);
        let _ = child.kill().await;
        let _ = writer.await;
        // 终态落盘 + 主会话投递。
        self.persist_terminal(handle);
        self.runs.lock().remove(&handle.id);
        let _ = sink;
        Ok(())
    }

    /// wire Step → engine Step。`plan` 是 run.json 里的 plan JSON（schemas 按 site_id 查）。
    fn map_step(
        &self,
        _run_id: &str,
        step: moc_ptc_protocol::wire::WorkflowGuestStep,
        plan: &Value,
        local_actors: &mut HashMap<u32, (String, u32)>,
        actor_ordinals: &mut HashMap<String, u32>,
    ) -> Step {
        let schema_of = |site_id: &str| {
            plan.get("schemas")
                .and_then(|s| s.as_array())
                .and_then(|pairs| {
                    pairs.iter().find_map(|pair| {
                        (pair.get(0).and_then(|s| s.as_str()) == Some(site_id))
                            .then(|| pair.get(1).cloned().unwrap_or(Value::Null))
                    })
                })
        };
        use moc_ptc_protocol::wire::{WorkflowEvent as WEvent, WorkflowOperation};
        let mut events = Vec::new();
        let mut requests = Vec::new();
        for event in step.events {
            match event {
                WEvent::CreateActor {
                    local_id,
                    site_id,
                    name,
                    persona,
                } => {
                    let ordinal = *actor_ordinals.get(&site_id).unwrap_or(&0);
                    local_actors.insert(local_id, (site_id.clone(), ordinal));
                    events.push(EngineEvent::CreateActor {
                        site_id,
                        name,
                        persona,
                    });
                }
                WEvent::PhaseEntered { site_id, name } => {
                    events.push(EngineEvent::Phase { site_id, name });
                }
                WEvent::Log { site_id, message } => {
                    // 单条 log 上限按字符边界截断（[workflow].log_bytes）。
                    let cap = self.inner.config.read().workflow.log_bytes;
                    let message = if message.len() > cap {
                        let mut end = cap;
                        while !message.is_char_boundary(end) {
                            end -= 1;
                        }
                        message[..end].to_string()
                    } else {
                        message
                    };
                    events.push(EngineEvent::Log {
                        site_id,
                        level: "info".into(),
                        message,
                    });
                }
                WEvent::Report {
                    site_id,
                    item,
                    artifact_id,
                } => {
                    events.push(EngineEvent::Report {
                        site_id,
                        item,
                        artifact_id,
                    });
                }
                WEvent::DeclareArtifact {
                    site_id,
                    member,
                    arguments,
                } => {
                    events.push(EngineEvent::DeclareArtifact {
                        site_id,
                        member: driver::serde_op_name(&member),
                        spec: Value::Array(arguments),
                    });
                }
            }
        }
        for request in step.requests {
            let site_id = request.site_id;
            let id = request.id as u64;
            match request.operation {
                WorkflowOperation::Ask {
                    actor,
                    actor_seq,
                    instructions,
                } => {
                    let (actor_site_id, actor_ordinal) =
                        local_actors.get(&actor).cloned().unwrap_or_default();
                    let schema = schema_of(&site_id);
                    requests.push(EngineRequest::Ask {
                        request_id: id,
                        site_id,
                        actor_site_id,
                        actor_ordinal,
                        actor_seq,
                        instructions,
                        schema,
                    });
                }
                WorkflowOperation::WorldRead { kind, arguments } => {
                    requests.push(EngineRequest::WorldRead {
                        request_id: id,
                        site_id,
                        op: driver::serde_op_name(&kind),
                        args: Value::Array(arguments),
                    });
                }
                WorkflowOperation::WorldRun {
                    command,
                    args,
                    options,
                } => {
                    requests.push(EngineRequest::WorldRun {
                        request_id: id,
                        site_id,
                        command,
                        args,
                        timeout_ms: options.timeout_ms.unwrap_or_else(|| {
                            self.inner.config.read().workflow.world_run_timeout_ms
                        }),
                        output: options.output.unwrap_or(Value::Null),
                    });
                }
                WorkflowOperation::PublishArtifact { member, arguments } => {
                    requests.push(EngineRequest::PublishArtifact {
                        request_id: id,
                        site_id,
                        member: driver::serde_op_name(&member),
                        spec: Value::Array(arguments),
                    });
                }
                WorkflowOperation::Hole { name, prompt } => {
                    let schema = schema_of(&site_id);
                    events.push(EngineEvent::HoleOpened {
                        site_id: site_id.clone(),
                        name: name.clone(),
                        prompt: prompt.unwrap_or_default(),
                        schema,
                    });
                    requests.push(EngineRequest::HoleResolve {
                        request_id: id,
                        site_id,
                    });
                }
            }
        }
        let state = match step.state {
            moc_ptc_protocol::wire::GuestState::Pending => ScriptState::Running,
            moc_ptc_protocol::wire::GuestState::Done { value } => ScriptState::Done { value },
            moc_ptc_protocol::wire::GuestState::Failed { error } => ScriptState::Failed {
                code: error.code,
                message: error.message,
            },
        };
        Step {
            events,
            requests,
            state,
            stall: None,
        }
    }

    /// 执行引擎动作；Dispatch 走 driver（异步 settle 后回流 Complete）。
    async fn apply_actions(self: &Arc<Self>, handle: &Arc<RunHandle>, actions: Vec<EngineAction>) {
        for action in actions {
            match action {
                EngineAction::Complete {
                    request_id,
                    outcome,
                } => {
                    let result = match &outcome {
                        DriverOutcome::Completed(v) => Ok(v.clone()),
                        DriverOutcome::Failed { code, message } => {
                            Err(moc_ptc_protocol::wire::ScriptError {
                                code: code.clone(),
                                message: message.clone(),
                                result: None,
                            })
                        }
                    };
                    let _ = handle.to_worker.send(WorkflowHostMessage::Complete {
                        completions: vec![HostCompletion {
                            id: request_id as u32,
                            result,
                        }],
                    });
                }
                EngineAction::Dispatch(request) => {
                    self.record_dispatch(handle, &request);
                    let driver = WorkflowDriver {
                        inner: Arc::clone(&self.inner),
                        session_id: self.session_id.clone(),
                        run: Arc::clone(handle),
                    };
                    let manager = Arc::clone(self);
                    let handle2 = Arc::clone(handle);
                    tokio::spawn(async move {
                        let outcome = driver.execute(&request).await;
                        manager.record_settle(&handle2, &request, &outcome);
                        let request_id = request_id_of(&request);
                        let actions = handle2.engine.lock().settle(request_id, outcome);
                        manager.apply_actions(&handle2, actions).await;
                    });
                }
                EngineAction::Session(event) => {
                    self.on_session_event(handle, event);
                }
                EngineAction::Finalized(terminal) => {
                    handle.view.lock().terminal = Some(*terminal);
                }
            }
        }
    }

    /// 向前端发一条 workflow 事件（异步投递，不阻塞引擎循环）。
    fn emit(&self, handle: &Arc<RunHandle>, event: WorkflowEvent) {
        let sink = handle.request.updates();
        let session_id = self.session_id.clone();
        let run_id = handle.id.clone();
        tokio::spawn(async move {
            let _ = sink
                .emit(
                    session_id,
                    moc_protocol::SessionUpdate::Workflow { run_id, event },
                )
                .await;
        });
    }

    /// ask 派发：对应 role 的在飞计数 + 解析模型写入 view（供面板与快照）。
    fn record_dispatch(&self, handle: &Arc<RunHandle>, request: &DriverRequest) {
        let DriverRequest::Ask {
            actor_site_id,
            actor_ordinal,
            persona,
            ..
        } = request
        else {
            return;
        };
        let model = {
            let meta = handle.meta.lock();
            driver::resolve_actor_model(&meta, actor_site_id, *actor_ordinal, persona)
        };
        let mut view = handle.view.lock();
        if let Some(role) = view.roles.iter_mut().find(|r| {
            r.role_id == *actor_site_id || r.role_id == format!("{actor_site_id}#{actor_ordinal}")
        }) {
            role.asks_in_flight += 1;
            if role.model.is_none() {
                role.model = Some(model);
            }
        }
    }

    /// ask/请求结算：role 完成计数与 token 记账，发 Usage + NodeUpdated 更新；
    /// artifact 发布成功补发 Artifact 事件（与 preset 声明同形）。
    fn record_settle(
        &self,
        handle: &Arc<RunHandle>,
        request: &DriverRequest,
        outcome: &DriverOutcome,
    ) {
        if let DriverRequest::PublishArtifact { .. } = request
            && let DriverOutcome::Completed(v) = outcome
        {
            let (id, version) = (
                v.get("id").and_then(|x| x.as_str()),
                v.get("version").and_then(|x| x.as_u64()),
            );
            let entry = handle
                .meta
                .lock()
                .artifacts
                .iter()
                .rev()
                .find(|a| {
                    a.get("artifact_id").and_then(|x| x.as_str()) == id
                        && a.get("version").and_then(|x| x.as_u64()) == version
                })
                .cloned();
            if let Some(artifact) = entry {
                self.emit(handle, WorkflowEvent::Artifact { artifact });
            }
            return;
        }
        let (is_ask, tokens) = match (request, outcome) {
            (DriverRequest::Ask { .. }, DriverOutcome::Completed(v)) => (
                true,
                v.get("usage")
                    .and_then(|u| u.get("spent_tokens"))
                    .and_then(|t| t.as_u64())
                    .unwrap_or(0),
            ),
            (DriverRequest::Ask { .. }, _) => (true, 0),
            _ => (false, 0),
        };
        if !is_ask {
            return;
        }
        let DriverRequest::Ask {
            actor_site_id,
            actor_ordinal,
            ..
        } = request
        else {
            unreachable!()
        };
        let usage = {
            let mut view = handle.view.lock();
            if let Some(role) = view.roles.iter_mut().find(|r| {
                r.role_id == *actor_site_id
                    || r.role_id == format!("{actor_site_id}#{actor_ordinal}")
            }) {
                role.asks_in_flight = role.asks_in_flight.saturating_sub(1);
                role.asks_done += 1;
                role.spent_tokens += tokens;
            }
            let spent = view
                .usage
                .get("spent_tokens")
                .and_then(|t| t.as_u64())
                .unwrap_or(0)
                + tokens;
            view.usage["spent_tokens"] = json!(spent);
            view.usage.clone()
        };
        let updates = {
            let view = handle.view.lock();
            (view.phases.clone(), view.roles.clone())
        };
        self.emit(
            handle,
            WorkflowEvent::NodeUpdated {
                phases: updates.0,
                roles: updates.1,
            },
        );
        self.emit(handle, WorkflowEvent::Usage { usage });
    }

    /// Session 事件 → SessionUpdate::Workflow + run view 更新。
    fn on_session_event(&self, handle: &Arc<RunHandle>, event: SessionEvent) {
        let run_id = handle.id.clone();
        let update = match event {
            SessionEvent::ActorCreated {
                site_id,
                ordinal,
                name,
            } => {
                let mut view = handle.view.lock();
                // fan-out 时同名 actor 的 ordinal>0 实例以 `site#ord` 区分记账。
                let role_id = if ordinal == 0 {
                    site_id
                } else {
                    format!("{site_id}#{ordinal}")
                };
                view.roles.push(WorkflowRoleState {
                    role_id,
                    name,
                    status: "running".into(),
                    model: None,
                    asks_done: 0,
                    asks_in_flight: 0,
                    spent_tokens: 0,
                    fan_out: false,
                });
                node_update(&view)
            }
            SessionEvent::Phase { name } => {
                let next = {
                    let mut view = handle.view.lock();
                    let next = format!("phase#{}", view.phases.len() + 1);
                    view.phases.push(WorkflowPhaseState {
                        id: next.clone(),
                        name: name.clone(),
                        status: "running".into(),
                        entered_at_ms: Some(crate::session::store::now_ms()),
                        asks_done: 0,
                        world_done: 0,
                    });
                    next
                };
                let _ = next;
                Some(WorkflowEvent::PhaseEntered { phase: name })
            }
            SessionEvent::Log { level, message } => Some(WorkflowEvent::Log { level, message }),
            SessionEvent::Report { item, artifact_id } => {
                // 存 {item, artifact_id}：table/metrics/board/chart 的数据源靠
                // artifact_id 把 report 项路由到声明的 artifact。
                handle
                    .meta
                    .lock()
                    .reports
                    .push(if let Some(artifact_id) = &artifact_id {
                        json!({"item": item, "artifact_id": artifact_id})
                    } else {
                        json!({"item": item})
                    });
                Some(WorkflowEvent::Report { item, artifact_id })
            }
            SessionEvent::DeclareArtifact {
                member,
                spec,
                artifact_id,
            } => {
                let mut meta = handle.meta.lock();
                meta.artifacts.push(json!({
                    "member": member,
                    "artifact_id": artifact_id,
                    "spec": spec,
                }));
                Some(WorkflowEvent::Artifact {
                    artifact: json!({"member": member, "artifact_id": artifact_id, "spec": spec}),
                })
            }
            SessionEvent::HoleOpened {
                site_id,
                ordinal,
                name,
                prompt,
                schema,
            } => {
                handle.meta.lock().holes.push(json!({
                    "site_id": site_id,
                    "ordinal": ordinal,
                    "name": name,
                    "prompt": prompt,
                    "schema": schema,
                }));
                Some(WorkflowEvent::HoleOpened {
                    site_id,
                    ordinal,
                    name,
                    prompt,
                    schema,
                })
            }
            SessionEvent::HoleCancelled { name } => {
                handle
                    .meta
                    .lock()
                    .holes
                    .retain(|h| h["name"].as_str() != Some(name.as_str()));
                Some(WorkflowEvent::HoleFilled { name })
            }
            SessionEvent::ActorDiverged { name } => Some(WorkflowEvent::Log {
                level: "warn".into(),
                message: format!("actor \"{name}\" diverged from its predecessor's transcript"),
            }),
        };
        if let Some(event) = update {
            let sink = handle.request.updates();
            let session_id = self.session_id.clone();
            tokio::spawn(async move {
                let _ = sink
                    .emit(
                        session_id,
                        moc_protocol::SessionUpdate::Workflow { run_id, event },
                    )
                    .await;
            });
        }
    }

    /// 终态：更新 meta、写 run.json、向主会话投递完成通知。
    fn persist_terminal(&self, handle: &Arc<RunHandle>) {
        let Some(terminal) = handle.view.lock().terminal.clone() else {
            return;
        };
        let mut meta = handle.meta.lock();
        apply_terminal(&mut meta, &terminal);
        let (status, notice_body) = match &meta.status {
            s if s == "completed" => (
                WorkflowRunStatus::Completed,
                format!(
                    "Workflow run \"{}\" ({}) completed. Result:\n{}",
                    meta.title,
                    handle.id,
                    serde_json::to_string_pretty(&meta.result.clone().unwrap_or(Value::Null))
                        .unwrap_or_default()
                ),
            ),
            s if s == "errored" => (
                WorkflowRunStatus::Errored,
                format!(
                    "Workflow run \"{}\" ({}) errored: {}",
                    meta.title,
                    handle.id,
                    meta.error.clone().unwrap_or_default()
                ),
            ),
            _ => (
                WorkflowRunStatus::Stopped {
                    reason: match meta.stop_reason.as_deref() {
                        Some("interrupted") => WorkflowStopReason::Interrupted,
                        Some("provider") => WorkflowStopReason::Provider,
                        _ => WorkflowStopReason::User,
                    },
                },
                format!(
                    "Workflow run \"{}\" ({}) stopped ({}).",
                    meta.title,
                    handle.id,
                    meta.stop_reason.clone().unwrap_or_else(|| "user".into())
                ),
            ),
        };
        // 终态投影写进 meta：view 随后释放，快照/列表从 meta 还原。
        {
            let view = handle.view.lock();
            meta.roles = view.roles.clone();
            meta.phases = view.phases.clone();
            meta.usage = view.usage.clone();
        }
        // 终态通知进主会话 notice 队列，父模型下一轮能看到（与子代理收尾同款）。
        let mut notice = notice_body;
        if !meta.reports.is_empty() {
            notice.push_str(&format!(
                "\nReports: {}",
                serde_json::to_string(&meta.reports).unwrap_or_default()
            ));
        }
        let dir = handle.dir.clone();
        let meta_json = serde_json::to_vec_pretty(&*meta).unwrap_or_default();
        drop(meta);
        let _ = std::fs::write(dir.join("run.json"), meta_json);
        let inner = Arc::clone(&self.inner);
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            if let Ok(parent) = inner.sessions.get(&session_id).await {
                parent.state.lock().await.notices.push(notice);
            }
        });
        self.emit(handle, WorkflowEvent::RunStatus { status });
    }

    fn fail_run(&self, handle: &Arc<RunHandle>, message: &str) {
        let mut view = handle.view.lock();
        if view.terminal.is_none() {
            view.terminal = Some(Terminal {
                status: TerminalStatus::Stopped {
                    reason: crate::workflow::engine::StopReason::Interrupted,
                },
                result: None,
                error_code: Some("E_HOST".into()),
                error_message: Some(message.to_string()),
            });
        }
        drop(view);
        self.persist_terminal(handle);
        self.runs.lock().remove(&handle.id);
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
        .map_err(|e| CoreError::Session(format!("run.json 读取失败：{e}")))?;
    serde_json::from_slice(&data).map_err(|e| CoreError::Session(format!("run.json 解析失败：{e}")))
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
mod tests {
    use super::*;

    #[test]
    fn run_id_charset_rejects_traversal() {
        assert!(valid_run_id("run_1735_001"));
        assert!(valid_run_id("a-b_C1"));
        assert!(!valid_run_id("../x"));
        assert!(!valid_run_id("a/b"));
        assert!(!valid_run_id("a.b"));
        assert!(!valid_run_id(""));
        assert!(!valid_run_id(&"x".repeat(65)));
    }
}

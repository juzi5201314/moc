use super::RunHandle;
use super::RunMeta;
use super::RunView;
use super::WorkflowManager;
use super::apply_terminal;
use super::list_run_metas;
use super::read_run_meta;
use super::workflow_summary;
use crate::error::CoreError;
use crate::workflow::compile::CompiledWorkflow;
use crate::workflow::engine::Engine;
use crate::workflow::engine::EngineConfig;
use crate::workflow::engine::FileJournal;
use crate::workflow::engine::RunMode;
use moc_protocol::UpdateSink;
use moc_protocol::WorkflowRunSummary;
use moc_ptc_protocol::wire::WorkflowHostMessage;
use parking_lot::Mutex;
use rust_i18n::t;
use serde_json::json;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

impl WorkflowManager {
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
        std::fs::create_dir_all(dir.join("artifacts")).map_err(|e| {
            CoreError::Session(t!("core.err.run_dir_create", error = e).into_owned())
        })?;
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
        // run.json 先行落盘（approve→set_role_model 窗口内磁盘必须可读）。
        std::fs::write(
            dir.join("run.json"),
            serde_json::to_vec_pretty(&meta).unwrap_or_default(),
        )
        .map_err(|e| CoreError::Session(t!("core.err.run_json_write", error = e).into_owned()))?;
        let mode = match meta.mode.as_str() {
            "resume" => RunMode::Resume,
            "amend" => RunMode::Amend,
            _ => RunMode::Fresh,
        };
        let journal = FileJournal::open(&dir)
            .map_err(|e| CoreError::Session(t!("core.err.journal_open", error = e).into_owned()))?;
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

    /// 只读查询元数据（优先本进程，否则读磁盘），避免改写其它持租约进程的 run。
    pub fn meta(&self, run_id: &str) -> Result<RunMeta, CoreError> {
        if let Some(handle) = self.runs.lock().get(run_id) {
            return Ok(handle.meta.lock().clone());
        }
        let dir = self.run_dir(run_id)?;
        read_run_meta(&dir)
    }

    pub fn list(&self) -> Vec<WorkflowRunSummary> {
        let held = self.runs.lock();
        list_run_metas(&self.paths.workflows())
            .into_iter()
            .map(|meta| {
                if let Some(handle) = held.get(&meta.run_id) {
                    workflow_summary(&handle.meta.lock())
                } else {
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
        self.cancel_run(&handle, "E_STOPPED");
        // 终态落盘在 run_loop 异步完成；summary 先按已记录的终态投影。
        let mut meta = handle.meta.lock().clone();
        if let Some(terminal) = handle.view.lock().terminal.clone() {
            apply_terminal(&mut meta, &terminal);
        }
        Ok(workflow_summary(&meta))
    }

    /// 会话卸载或删除前取消全部 run；run_loop 随后回收 worker 并落盘终态。
    pub fn shutdown(self: &Arc<Self>) {
        let handles: Vec<_> = self.runs.lock().values().cloned().collect();
        for handle in handles {
            self.cancel_run(&handle, "E_SHUTDOWN");
        }
    }

    fn cancel_run(self: &Arc<Self>, handle: &Arc<RunHandle>, code: &str) {
        let actions = handle.engine.lock().cancel(code);
        // 先发布终态再唤醒取消令牌，避免 run_loop 将主动关闭误判为 worker 崩溃。
        self.apply_actions(handle, actions);
        handle.cancel.cancel();
        let _ = handle
            .to_worker
            .send(WorkflowHostMessage::Cancel { code: code.into() });
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
            .map_err(|e| {
                CoreError::Session(t!("core.err.run_json_write", error = e).into_owned())
            })?;
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
        Err(CoreError::Session(
            t!("core.err.run_missing", run_id = run_id).into_owned(),
        ))
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
                    .ok_or_else(|| {
                        CoreError::Session(t!("core.err.hole_not_open", name = name).into_owned())
                    })?
            };
            engine.fill_hole(&site_id, ordinal, code)
        };
        self.apply_actions(&handle, actions);
        Ok(())
    }
}

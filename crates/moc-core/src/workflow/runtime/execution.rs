use super::RunHandle;
use super::WorkflowManager;
use super::driver;
use super::request_id_of;
use super::worker_command;
use crate::error::CoreError;
use crate::workflow::compile::CompiledWorkflow;
use crate::workflow::engine::DriverOutcome;
use crate::workflow::engine::EngineAction;
use crate::workflow::engine::EngineEvent;
use crate::workflow::engine::EngineRequest;
use crate::workflow::engine::ScriptState;
use crate::workflow::engine::Step;
use crate::workflow::runtime::WorkflowDriver;
use moc_protocol::UpdateSink;
use moc_ptc_protocol::wire::HostCompletion;
use moc_ptc_protocol::wire::WorkflowGuestMessage;
use moc_ptc_protocol::wire::WorkflowHostMessage;
use moc_ptc_protocol::wire::WorkflowRunInfo as WireRunInfo;
use moc_ptc_protocol::wire::WorkflowStartRequest;
use parking_lot::Mutex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::AsyncBufReadExt;
use tokio::io::AsyncWriteExt;
use tokio::io::BufReader;
use tokio::process::Command;

impl WorkflowManager {
    /// worker/host 主循环：Step → 引擎 → actions（Complete 回 worker、Dispatch 走 driver、
    /// Session 事件 → SessionUpdate、Finalized → 落盘 + 通知）。
    pub(super) async fn run_loop(
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
            self.apply_actions(handle, actions);
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
    pub(super) fn map_step(
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
    pub(super) fn apply_actions(
        self: &Arc<Self>,
        handle: &Arc<RunHandle>,
        actions: Vec<EngineAction>,
    ) {
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
                        manager.apply_actions(&handle2, actions);
                    });
                }
                EngineAction::Session(event) => {
                    self.on_session_event(handle, event);
                }
                EngineAction::Finalized(terminal) => {
                    handle.view.lock().terminal = Some(*terminal);
                    handle.cancel.cancel();
                }
            }
        }
    }
}

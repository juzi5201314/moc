use super::RunHandle;
use super::WorkflowManager;
use super::apply_terminal;
use super::driver;
use super::node_update;
use crate::workflow::engine::DriverOutcome;
use crate::workflow::engine::DriverRequest;
use crate::workflow::engine::SessionEvent;
use crate::workflow::engine::Terminal;
use crate::workflow::engine::TerminalStatus;
use moc_protocol::WorkflowEvent;
use moc_protocol::WorkflowPhaseState;
use moc_protocol::WorkflowRoleState;
use moc_protocol::WorkflowRunStatus;
use moc_protocol::WorkflowStopReason;
use serde_json::Value;
use serde_json::json;
use std::io::Write;
use std::sync::Arc;

impl WorkflowManager {
    /// 向前端发一条 workflow 事件（异步投递，不阻塞引擎循环）。
    pub(super) fn emit(&self, handle: &Arc<RunHandle>, event: WorkflowEvent) {
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
    pub(super) fn record_dispatch(&self, handle: &Arc<RunHandle>, request: &DriverRequest) {
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
    pub(super) fn record_settle(
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
    pub(super) fn on_session_event(&self, handle: &Arc<RunHandle>, event: SessionEvent) {
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
    pub(super) fn persist_terminal(&self, handle: &Arc<RunHandle>) {
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
        // shutdown 等待此任务结束；返回前须同步终态，避免释放租约后仍有旧写入。
        if let Err(error) = std::fs::File::create(dir.join("run.json")).and_then(|mut file| {
            file.write_all(&meta_json)?;
            file.sync_all()
        }) {
            tracing::warn!(run_id = %handle.id, %error, "workflow 终态落盘失败");
        }
        let inner = Arc::clone(&self.inner);
        let session_id = self.session_id.clone();
        tokio::spawn(async move {
            if let Ok(parent) = inner.sessions.get(&session_id).await {
                parent
                    .state
                    .lock()
                    .await
                    .notices
                    .push(crate::session::state::SessionNotice::new(notice));
            }
        });
        self.emit(handle, WorkflowEvent::RunStatus { status });
    }

    pub(super) fn fail_run(&self, handle: &Arc<RunHandle>, message: &str) {
        handle.cancel.cancel();
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
    }
}

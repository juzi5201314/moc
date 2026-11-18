use parking_lot::Mutex;
use std::collections::HashMap;
use std::io::Write;
use std::sync::Arc;

use moc_protocol::methods::{AssistantDelta, SessionUpdate, SessionUpdateParams, UpstreamField};
use rust_i18n::t;
use serde_json::Value;

#[derive(Clone)]
pub struct Renderer(Arc<Mutex<State>>);

struct State {
    print_mode: bool,
    hide_thinking: bool,
    /// `--print` 下接管全部输出：收集事件、写 stream-json 行、累计轮数与拒绝数。
    collector: crate::print::PrintCollector,
    calls: HashMap<String, ToolCallView>,
    /// workflow 审批回传用的连接句柄与会话（repl 启动后注入）。
    client: Option<moc_protocol::ClientHandle>,
    session_id: Option<String>,
    /// 与主输入循环共用的 stdin 行通道（审批读行走它，不直接抢 stdin）。
    input: Option<crate::repl::Input>,
}

struct ToolCallView {
    name: String,
    arguments: String,
    announced: bool,
}

impl Renderer {
    pub fn new(
        print_mode: bool,
        hide_thinking: bool,
        output_format: crate::args::OutputFormat,
        max_turns: Option<u32>,
    ) -> Self {
        Self(Arc::new(Mutex::new(State {
            print_mode,
            hide_thinking,
            collector: crate::print::PrintCollector::new(output_format, max_turns),
            calls: HashMap::new(),
            client: None,
            session_id: None,
            input: None,
        })))
    }

    /// repl 建立会话后注入 workflow 审批所需的回传通道与输入行通道。
    pub fn bind_workflow(
        &self,
        client: moc_protocol::ClientHandle,
        session_id: String,
        cwd: std::path::PathBuf,
        input: crate::repl::Input,
    ) {
        let mut state = self.0.lock();
        state
            .collector
            .bind(client.clone(), session_id.clone(), cwd);
        state.client = Some(client);
        state.session_id = Some(session_id);
        state.input = Some(input);
    }

    /// 提示结束后取 `--print` 的收集快照（非 print 模式内容为空）。
    pub fn report(&self) -> crate::print::PrintReport {
        self.0.lock().collector.report()
    }

    pub fn handle(&self, params: SessionUpdateParams) {
        let mut state = self.0.lock();
        if state.print_mode {
            state.collector.handle(&params);
            return;
        }
        let stdout = std::io::stdout();
        let mut out = stdout.lock();
        match params.update {
            SessionUpdate::AgentStarted { agent } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.agent_started",
                        title = agent.title,
                        id = agent.id
                    )
                );
            }
            SessionUpdate::AgentStatusChanged {
                agent_id, status, ..
            } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.agent_status",
                        id = agent_id,
                        status = format!("{status:?}")
                    )
                );
            }
            SessionUpdate::Workflow { run_id, event } => {
                use moc_protocol::methods::WorkflowEvent as E;
                match event {
                    E::ApprovalRequested { request } => {
                        // 文本版编排图：phase 列表 + role 默认模型 + 命令集。
                        let _ = writeln!(
                            out,
                            "{}",
                            t!("cli.render.workflow_approval", id = request.request_id)
                        );
                        if let Some(phases) = request.plan.get("phases").and_then(|p| p.as_array())
                        {
                            for phase in phases {
                                let name =
                                    phase.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                                let _ = writeln!(out, "  ↓ {name}");
                            }
                        }
                        for (role, model) in &request.defaults.role_models {
                            let _ = writeln!(out, "  role {role} → {model}");
                        }
                        for (id, model) in &request.defaults.model_ref_map {
                            let _ = writeln!(out, "  model({id}) → {model}");
                        }
                        if let Some(commands) =
                            request.plan.get("commands").and_then(|c| c.as_array())
                        {
                            for command in commands {
                                let text = command
                                    .get("command")
                                    .and_then(|v| v.as_str())
                                    .unwrap_or("?");
                                let _ = writeln!(out, "  cmd {text}");
                            }
                        }
                        if let Some(new) = &request.new_commands {
                            for command in new {
                                let _ = writeln!(out, "  +cmd {command}");
                            }
                        }
                        let decision = workflow_decision_prompt(&mut out, state.input.clone());
                        let client = self.0.lock().client.clone();
                        if let Some((client, session_id)) =
                            client.zip(self.0.lock().session_id.clone())
                        {
                            let request_id = request.request_id.clone();
                            tokio::spawn(async move {
                                let _ = client
                                    .call::<moc_protocol::methods::SessionWorkflowApprove>(
                                        moc_protocol::methods::SessionWorkflowApproveParams {
                                            session_id,
                                            request_id,
                                            decision,
                                        },
                                    )
                                    .await;
                            });
                        }
                    }
                    E::ApprovalResolved { approved, .. } => {
                        let _ = writeln!(
                            out,
                            "{}",
                            t!(
                                "cli.render.workflow_resolved",
                                decision = if approved {
                                    t!("cli.render.approved")
                                } else {
                                    t!("cli.render.rejected")
                                }
                                .as_ref()
                            )
                        );
                    }
                    E::RunStatus { status } => {
                        let label = match status {
                            moc_protocol::methods::WorkflowRunStatus::Completed => "completed",
                            moc_protocol::methods::WorkflowRunStatus::Errored => "errored",
                            moc_protocol::methods::WorkflowRunStatus::Running => "running",
                            moc_protocol::methods::WorkflowRunStatus::Pending => "pending",
                            moc_protocol::methods::WorkflowRunStatus::Stopped { .. } => "stopped",
                        };
                        let _ = writeln!(
                            out,
                            "{}",
                            t!("cli.render.workflow_status", run = run_id, label = label)
                        );
                    }
                    E::PhaseEntered { phase } => {
                        let _ = writeln!(
                            out,
                            "{}",
                            t!("cli.render.workflow_phase", run = run_id, phase = phase)
                        );
                    }
                    E::Log { message, .. } => {
                        let _ = writeln!(out, "[workflow {run_id}] {message}");
                    }
                    E::HoleOpened { name, .. } => {
                        let _ = writeln!(
                            out,
                            "{}",
                            t!("cli.render.hole_opened", run = run_id, name = name)
                        );
                    }
                    E::HoleFilled { name } => {
                        let _ = writeln!(
                            out,
                            "{}",
                            t!("cli.render.hole_filled", run = run_id, name = name)
                        );
                    }
                    E::Report { .. }
                    | E::Artifact { .. }
                    | E::NodeUpdated { .. }
                    | E::Usage { .. } => {}
                }
            }
            SessionUpdate::AgentMessage { meta, body } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.agent_message",
                        from = meta.from_title,
                        body = body
                    )
                );
            }
            SessionUpdate::ToolProgress { .. } => {}
            SessionUpdate::ToolBatchStarted { .. } => {}
            SessionUpdate::GenerationStarted { .. } => state.calls.clear(),
            SessionUpdate::GenerationRetry { message, .. } => {
                state.calls.clear();
                let _ = writeln!(out, "{}", t!("cli.render.retry", message = message));
            }
            SessionUpdate::GuardNotice {
                message, terminal, ..
            } => {
                if terminal {
                    state.calls.clear();
                }
                let _ = writeln!(out, "{}", t!("cli.render.guard", message = message));
            }
            SessionUpdate::PluginNotice {
                plugin,
                code,
                message,
            } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.plugin_disabled",
                        plugin = plugin,
                        code = code,
                        message = message
                    )
                );
            }
            SessionUpdate::AssistantDelta { delta, .. } => match delta {
                AssistantDelta::Text { delta } => {
                    let _ = write!(out, "{delta}");
                    let _ = out.flush();
                }
                AssistantDelta::Reasoning { delta } if !state.hide_thinking => {
                    let _ = write!(out, "\x1b[90m{delta}\x1b[0m");
                    let _ = out.flush();
                }
                AssistantDelta::Reasoning { .. } | AssistantDelta::Usage { .. } => {}
            },
            SessionUpdate::BashStarted {
                tool_call_id,
                command,
            } => {
                let marker = if command.exclude_from_context {
                    "!!bash"
                } else {
                    "!bash"
                };
                let _ = writeln!(out, "\n[{marker}] $ {}", command.command);
                state.calls.insert(
                    tool_call_id,
                    ToolCallView {
                        name: marker.into(),
                        arguments: String::new(),
                        announced: true,
                    },
                );
            }
            SessionUpdate::ToolStarted {
                tool_call_id,
                name,
                arguments,
                ..
            } => {
                let mut call = ToolCallView {
                    name,
                    arguments,
                    announced: false,
                };
                announce_call(&mut out, &mut call);
                state.calls.insert(tool_call_id, call);
            }
            SessionUpdate::ToolFinished {
                tool_call_id,
                name,
                arguments,
                status,
                result,
                duration_ms,
                ..
            } => {
                let mut call = state.calls.remove(&tool_call_id).unwrap_or(ToolCallView {
                    name,
                    arguments,
                    announced: false,
                });
                if !call.announced {
                    announce_call(&mut out, &mut call);
                }
                let summary = if call.name.starts_with('!') {
                    result.content
                } else {
                    result
                        .content
                        .lines()
                        .take(3)
                        .collect::<Vec<_>>()
                        .join("\n")
                };
                let _ = writeln!(
                    out,
                    "  {}: {} ({status:?}, {} ms)",
                    call.name,
                    summary,
                    duration_ms.unwrap_or_default()
                );
            }
            SessionUpdate::PromptFinished { usage, .. } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.usage",
                        input = usage.input_tokens,
                        output = usage.output_tokens
                    )
                );
            }
            SessionUpdate::TodoChanged { phases } => {
                let total: usize = phases.iter().map(|phase| phase.items.len()).sum();
                let done: usize = phases
                    .iter()
                    .flat_map(|phase| &phase.items)
                    .filter(|item| item.status == moc_protocol::methods::TodoStatus::Completed)
                    .count();
                let _ = writeln!(out, "{}", t!("cli.render.todo", done = done, total = total));
            }
            SessionUpdate::TodoReminder { open_items, .. } => {
                let _ = writeln!(out, "{}", t!("cli.render.todo_reminder", open = open_items));
            }
            SessionUpdate::PlanSubmitted { plan } => {
                let _ = writeln!(
                    out,
                    "{}\n{}",
                    t!(
                        "cli.render.plan_submitted",
                        version = plan.version,
                        title = plan.title,
                        id = plan.plan_id,
                        path = plan.path
                    ),
                    plan.markdown
                );
            }
            SessionUpdate::PlanUpdated { plan } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.plan_status",
                        title = plan.title,
                        id = plan.plan_id,
                        status = plan_status_label(plan.status).as_ref()
                    )
                );
            }
            SessionUpdate::ContextCompactionStarted => {
                let _ = writeln!(out, "{}", t!("cli.render.compacting"));
            }
            SessionUpdate::ContextCompacted {
                method,
                tokens_before,
                tokens_after,
                full,
            } => {
                if full {
                    let _ = writeln!(
                        out,
                        "{}",
                        t!(
                            "cli.render.compacted",
                            method = method,
                            before = tokens_before,
                            after = tokens_after
                        )
                    );
                } else {
                    let _ = writeln!(
                        out,
                        "{}",
                        t!(
                            "cli.render.trimmed",
                            before = tokens_before,
                            after = tokens_after
                        )
                    );
                }
            }
            SessionUpdate::ContextCompactionFailed { message } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.compact_failed", message = message)
                );
            }
            SessionUpdate::JobFinished { job } => {
                let code = job
                    .exit_code
                    .map_or_else(String::new, |code| format!(" ({code})"));
                let status_str = serde_json::to_value(job.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", job.status));
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.job_finished",
                        id = job.job_id,
                        command = first_line(&job.command),
                        status = status_str,
                        code = code,
                        log = job.log_uri
                    )
                );
            }
            SessionUpdate::UpstreamMismatch { fields, .. } => {
                let detail = fields
                    .iter()
                    .map(|mismatch| {
                        let label = match mismatch.field {
                            UpstreamField::Model => t!("cli.render.field_model"),
                            UpstreamField::Effort => t!("cli.render.field_effort"),
                        };
                        t!(
                            "cli.render.mismatch_field",
                            label = label.as_ref(),
                            requested = mismatch.requested,
                            reported = mismatch.reported
                        )
                        .into_owned()
                    })
                    .collect::<Vec<_>>()
                    .join(t!("cli.repl.arg_sep").as_ref());
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.upstream_mismatch", detail = detail)
                );
            }
            SessionUpdate::Error { message, .. } => {
                let _ = writeln!(out, "{}", t!("cli.render.error", message = message));
            }
            SessionUpdate::EngineChanged { engine } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.engine", engine = format!("{engine:?}"))
                );
            }
            SessionUpdate::AutonomyChanged { autonomy } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.autonomy", autonomy = format!("{autonomy:?}"))
                );
            }
            SessionUpdate::FastModeChanged { enabled } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.fast_mode", state = on_off(enabled).as_ref())
                );
            }
            SessionUpdate::PlanModeChanged { active } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.plan_mode", state = on_off(active).as_ref())
                );
            }
            SessionUpdate::ModelChanged { model } => {
                let _ = writeln!(out, "{}", t!("cli.render.model", model = model));
            }
            SessionUpdate::PermissionChanged { permission } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.permission", permission = permission.as_str())
                );
            }
            SessionUpdate::TitleChanged { title } => {
                let _ = writeln!(out, "{}", t!("cli.render.title", title = title));
            }
            SessionUpdate::PermissionRequested { tool_call_id, .. } => {
                let _ = writeln!(
                    out,
                    "{}",
                    t!("cli.render.permission_wait", id = tool_call_id)
                );
            }
            SessionUpdate::GoalChanged { goal } => {
                let Some(goal) = goal else {
                    let _ = writeln!(out, "{}", t!("cli.render.goal_cleared"));
                    return;
                };
                let _ = writeln!(
                    out,
                    "{}",
                    t!(
                        "cli.render.goal",
                        objective = goal.objective,
                        rounds = goal.rounds_started
                    )
                );
            }
            SessionUpdate::GoalRoundMessage { meta, .. } => {
                let _ = writeln!(out, "{}", t!("cli.render.goal_round", round = meta.round));
            }
            SessionUpdate::PermissionResolved { .. }
            | SessionUpdate::PromptStarted { .. }
            | SessionUpdate::SteeringApplied { .. }
            | SessionUpdate::PromptRewound { .. }
            | SessionUpdate::LockChanged { .. }
            | SessionUpdate::AgentResult { .. }
            | SessionUpdate::JobNotified { .. }
            | SessionUpdate::JobStarted { .. }
            | SessionUpdate::JobUpdated { .. }
            | SessionUpdate::JobOutput { .. }
            | SessionUpdate::ToolPreview { .. }
            | SessionUpdate::ToolPreviewsResolved { .. } => {}
        }
    }
}

fn announce_call(out: &mut impl Write, call: &mut ToolCallView) {
    if call.announced {
        return;
    }
    let detail = serde_json::from_str::<Value>(&call.arguments)
        .ok()
        .and_then(|value| {
            value
                .get("command")
                .and_then(Value::as_str)
                .map(first_line)
                .or_else(|| {
                    (!call.arguments.is_empty()).then(|| call.arguments.chars().take(120).collect())
                })
        })
        .unwrap_or_default();
    if detail.is_empty() {
        let _ = writeln!(out, "\n→ {}", call.name);
    } else {
        let _ = writeln!(out, "\n→ {}: {detail}", call.name);
    }
    call.announced = true;
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_owned()
}

/// 计划状态的本地化标签（非交互输出里用于标记状态迁移）。
fn plan_status_label(status: moc_protocol::methods::PlanStatus) -> std::borrow::Cow<'static, str> {
    match status {
        moc_protocol::methods::PlanStatus::Draft => t!("cli.render.plan.draft"),
        moc_protocol::methods::PlanStatus::Submitted => t!("cli.render.plan.submitted"),
        moc_protocol::methods::PlanStatus::Revised => t!("cli.render.plan.revised"),
        moc_protocol::methods::PlanStatus::Superseded => t!("cli.render.plan.superseded"),
        moc_protocol::methods::PlanStatus::Approved => t!("cli.render.plan.approved"),
        moc_protocol::methods::PlanStatus::Rejected => t!("cli.render.plan.rejected"),
    }
}

/// 开/关的本地化短词（Fast Mode、计划模式等开关行共用）。
fn on_off(enabled: bool) -> std::borrow::Cow<'static, str> {
    if enabled {
        t!("common.on")
    } else {
        t!("common.off")
    }
}

/// 行式 REPL 的审批输入：`y` 批准 / `n [反馈]` 拒绝 / `role=模型` 覆盖后再 y。
/// 读行必须走与主循环同一个 stdin 行通道：直接 `stdin().read_line` 会和输入
/// 读取线程抢行（行被 channel 先吃掉，审批永远等不到输入）。
fn workflow_decision_prompt(
    out: &mut impl std::io::Write,
    input: Option<crate::repl::Input>,
) -> moc_protocol::methods::WorkflowDecision {
    use moc_protocol::methods::WorkflowDecision;
    let mut role_models = std::collections::BTreeMap::new();
    let Some(input) = input else {
        return WorkflowDecision::Reject { feedback: None };
    };
    loop {
        let _ = write!(out, "{}", t!("cli.render.approve_prompt"));
        let _ = out.flush();
        // handle() 在 runtime worker 上跑：直接 blocking_recv 会 panic——用
        // block_in_place 让出 worker 再阻塞取行（repl 是 multi_thread）。
        let line = match tokio::task::block_in_place(|| input.blocking_lock().blocking_recv()) {
            Some(Ok(line)) => line,
            _ => return WorkflowDecision::Reject { feedback: None },
        };
        match parse_workflow_decision(line.trim()) {
            DecisionParse::Approve => {
                return WorkflowDecision::Approve {
                    role_models: (!role_models.is_empty()).then_some(role_models),
                    model_ref_map: None,
                };
            }
            DecisionParse::Reject(feedback) => {
                return WorkflowDecision::Reject { feedback };
            }
            DecisionParse::Override(role, model) => {
                role_models.insert(role, model);
                let _ = writeln!(out, "{}", t!("cli.render.override_recorded"));
            }
        }
    }
}

/// 审批行解析：`y|yes` / `n|no [反馈]` / `role=模型`。
#[derive(Debug, PartialEq)]
enum DecisionParse {
    Approve,
    Reject(Option<String>),
    Override(String, String),
}

fn parse_workflow_decision(line: &str) -> DecisionParse {
    if line == "y" || line == "yes" {
        return DecisionParse::Approve;
    }
    if let Some(rest) = line.strip_prefix('n').map(str::trim)
        && (line == "n" || line == "no" || line.starts_with("n ") || line.starts_with("no "))
    {
        return DecisionParse::Reject((!rest.is_empty()).then(|| rest.to_string()));
    }
    if let Some((role, model)) = line.split_once('=') {
        let role = role.trim();
        let model = model.trim();
        if !role.is_empty() && !model.is_empty() {
            return DecisionParse::Override(role.to_string(), model.to_string());
        }
    }
    DecisionParse::Reject(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decision_line_parses() {
        assert_eq!(parse_workflow_decision("y"), DecisionParse::Approve);
        assert_eq!(
            parse_workflow_decision("n too big"),
            DecisionParse::Reject(Some("too big".into()))
        );
        assert_eq!(parse_workflow_decision("n"), DecisionParse::Reject(None));
        assert_eq!(
            parse_workflow_decision("planner=test/gpt-4"),
            DecisionParse::Override("planner".into(), "test/gpt-4".into())
        );
    }

    /// 审批读行走与主循环同一个 Input 行通道：喂进 channel 的行被审批消费，
    /// 不需要触碰真实 stdin。
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn decision_reads_from_shared_input_channel() {
        let (tx, rx) = tokio::sync::mpsc::channel(4);
        let input: crate::repl::Input = Arc::new(tokio::sync::Mutex::new(rx));
        tx.send(Ok("planner=test/gpt-4".to_string())).await.unwrap();
        tx.send(Ok("y".to_string())).await.unwrap();
        let mut out = Vec::new();
        let decision = workflow_decision_prompt(&mut out, Some(input));
        match decision {
            moc_protocol::methods::WorkflowDecision::Approve { role_models, .. } => {
                assert_eq!(
                    role_models
                        .and_then(|m| m.get("planner").cloned())
                        .as_deref(),
                    Some("test/gpt-4")
                );
            }
            other => panic!("应批准：{other:?}"),
        }
    }
}

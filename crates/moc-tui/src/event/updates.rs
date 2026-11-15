//! 会话协议通知到界面状态的投影。

use rust_i18n::t;

use super::agents::load_subagents;
use super::context::refresh_context;
use super::plan::open_plan_review;
use super::plan::request_plan_diff;
use super::plugins::plugin_notice_note;
use super::usage::refresh_usage;
use super::workflow::apply_workflow_update;
use crate::app::App;
use crate::event::cancel_prompt;
use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crate::transcript::NoteTone;
use crate::transcript::ToolCallFinish;
use crate::transcript::ToolCallStart;
use moc_protocol::InputPart;
use moc_protocol::SessionEngine;
use moc_protocol::SessionPlanApprove;
use moc_protocol::SessionPlanApproveParams;
use moc_protocol::SessionUpdate;
use moc_protocol::UpstreamField;

/// 把一条会话更新折进界面；只接受当前会话与已知子代理的更新。
///
/// - 当前会话：正常折叠进转写与状态；
/// - 已知子代理（其会话 ID 就是代理 ID）：折进侧栏的子代理视图；
/// - 其余来源（运行中切走后的旧会话、别的会话）：内容一律丢弃，只按
///   `PromptFinished` 回收本前端的在飞记账——旧会话的 delta/工具块写进新会话就是串台。
pub(super) fn apply_ui_update(
    app: &mut App,
    connection: &EmbeddedConnection,
    update: crate::protocol::UiUpdate,
) {
    match update {
        crate::protocol::UiUpdate::Session(params) => {
            apply_update_and_refresh(app, connection, params.session_id, params.update);
        }
        crate::protocol::UiUpdate::Btw(params) => {
            app.btw.apply_update(params);
            crate::btw::flush_cancel(app, connection);
        }
    }
}

pub(super) fn apply_update_and_refresh(
    app: &mut App,
    connection: &EmbeddedConnection,
    session_id: String,
    update: SessionUpdate,
) {
    if let Some(Modal::ContextContent { state }) = app.modal.as_mut()
        && state.session_id == session_id
    {
        state.stale = true;
    }
    let is_current = app
        .session
        .as_ref()
        .is_some_and(|session| session.id == session_id);
    // 补充消息的落位事件对所有会话都要处理，否则切走期间回来的回执会留下幽灵项。
    if let SessionUpdate::SteeringApplied { id, text } = &update {
        crate::queue::steering_applied(app, &session_id, *id, text, is_current);
        if is_current {
            // pending 清空后才能安全启动下一条提示，保证用户块先于新提示落地。
            crate::queue::dispatch(app, connection);
        }
        return;
    }
    if !is_current {
        // 聚焦子代理时主会话不再是当前会话，但代理面仍挂靠在它上面：
        // 主会话发出的状态事件与新建代理照旧更新名册，避免侧栏在聚焦期间僵住。
        if app.agents_root_id().as_deref() == Some(session_id.as_str()) {
            if let SessionUpdate::AgentStatusChanged {
                agent_id,
                status,
                last_message,
            } = &update
                && let Some(view) = app.subagents.get_mut(agent_id)
            {
                view.set_status(*status, last_message.clone());
                app.subagent_generation = app.subagent_generation.wrapping_add(1);
                app.sync_subagent_sidebar();
            }
            if matches!(&update, SessionUpdate::AgentStarted { .. }) {
                load_subagents(app, connection);
            }
            if matches!(&update, SessionUpdate::PromptFinished { .. }) {
                app.inflight_prompts.remove(&session_id);
                app.interrupt_pending.remove(&session_id);
            }
            return;
        }
        // 子代理自身的流：TUI 不再另存一份转写（聚焦时它才是当前会话），
        // 只按在飞记账收尾。
        if matches!(&update, SessionUpdate::PromptFinished { .. }) {
            app.inflight_prompts.remove(&session_id);
            app.interrupt_pending.remove(&session_id);
        }
        return;
    }
    if let SessionUpdate::AgentStarted { agent } = &update {
        app.subagents
            .entry(agent.id.clone())
            .or_insert_with(|| crate::subagent::SubagentView::new(agent.clone()));
        app.subagent_generation = app.subagent_generation.wrapping_add(1);
        app.sync_subagent_sidebar();
    }
    if let SessionUpdate::AgentStatusChanged {
        agent_id,
        status,
        last_message,
    } = &update
        && let Some(view) = app.subagents.get_mut(agent_id)
    {
        view.set_status(*status, last_message.clone());
        app.subagent_generation = app.subagent_generation.wrapping_add(1);
        app.sync_subagent_sidebar();
    }
    let prompt_started = matches!(
        &update,
        SessionUpdate::PromptStarted { .. } | SessionUpdate::BashStarted { .. }
    );
    let refresh = matches!(
        &update,
        SessionUpdate::PromptFinished { .. }
            | SessionUpdate::EngineChanged { .. }
            | SessionUpdate::AutonomyChanged { .. }
            | SessionUpdate::PlanModeChanged { .. }
            | SessionUpdate::ModelChanged { .. }
            | SessionUpdate::PermissionChanged { .. }
            | SessionUpdate::ContextCompacted { .. }
    ) || matches!(&update, SessionUpdate::ToolFinished { prompt_id, .. } if prompt_id.starts_with("bash_"));
    // 用量只在这些事件后变化：一次提示（含工具轮次）结束，或压缩本身花掉一次调用；
    // 非阻塞的 sidekick/子代理到达终态（完成/停止/失败/空闲）时同样刷新——
    // 它们的用量记在后代会话上，不等下一次提示。
    let usage_changed = matches!(
        &update,
        SessionUpdate::PromptFinished { .. } | SessionUpdate::ContextCompacted { .. }
    ) || (is_current
        && matches!(
            &update,
            SessionUpdate::AgentStatusChanged { status, .. }
                if matches!(
                    status,
                    moc_protocol::methods::AgentStatus::Completed
                        | moc_protocol::methods::AgentStatus::Stopped
                        | moc_protocol::methods::AgentStatus::Failed
                        | moc_protocol::methods::AgentStatus::Idle
                )
        ));
    // unattended 自主程度下计划提交不弹审阅：直接自动批准「压缩并执行」。
    let unattended_plan = match &update {
        SessionUpdate::PlanSubmitted { plan }
            if app.session.as_ref().is_some_and(|session| {
                session.autonomy == moc_protocol::SessionAutonomy::Unattended
            }) =>
        {
            Some(plan.plan_id.clone())
        }
        _ => None,
    };
    // 有上一版时，提交后立刻把「本版改动」拉下来；审阅窗口默认展示它。
    let revised_plan = match &update {
        SessionUpdate::PlanSubmitted { plan }
            if unattended_plan.is_none() && plan.previous_plan_id.is_some() =>
        {
            Some(plan.plan_id.clone())
        }
        _ => None,
    };
    apply_update(app, update);
    if let Some(plan_id) = unattended_plan
        && let Some(session) = app.session.as_ref()
    {
        let client = connection.client.clone();
        let events = app.events.clone();
        let (session_id, engine) = (session.id.clone(), session.engine);
        app.set_notice(t!("tui.event.unattended_approved").into_owned());
        tokio::spawn(async move {
            let result = client
                .call::<SessionPlanApprove>(SessionPlanApproveParams {
                    session_id,
                    plan_id,
                    action: moc_protocol::methods::PlanAction::ExecuteCompacted,
                    execution_engine: Some(engine),
                })
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::PlanApproval(result));
        });
    }
    if let Some(plan_id) = revised_plan {
        request_plan_diff(app, connection, plan_id);
    }
    if prompt_started && app.cancel_requested {
        cancel_prompt(&connection.client, app);
    }
    if refresh {
        refresh_context(app, connection, false);
    }
    if usage_changed {
        refresh_usage(app, connection, false);
    }
}

/// 压缩完成提示：全量摘要与仅裁剪用不同措辞，避免把裁剪说成完整压缩。
pub(super) fn compaction_done_note(full: bool, tokens_before: u64, tokens_after: u64) -> String {
    let before = crate::render::format_tokens(tokens_before);
    let after = crate::render::format_tokens(tokens_after);
    if full {
        t!("tui.event.compacted_fmt", before = before, after = after).into_owned()
    } else {
        t!("tui.event.trimmed_fmt", before = before, after = after).into_owned()
    }
}

/// 把协议更新折叠进块序列：流式增量续写活动块，工具调用按 ID 归组。
pub(super) fn apply_update(app: &mut App, update: SessionUpdate) {
    use crate::transcript::Block;
    match update {
        SessionUpdate::AgentStarted { .. } | SessionUpdate::AgentStatusChanged { .. } => {}
        // 在主循环的 apply_update_and_refresh 里单独处理（需要会话上下文）。
        SessionUpdate::SteeringApplied { .. } => {}
        // workflow 事件走 `apply_workflow_update`：审批弹窗、run 面板实时着色、
        // 状态栏胶囊与终态通知都在那里更新，不进转写块序列。
        SessionUpdate::Workflow { run_id, event } => {
            apply_workflow_update(app, run_id, event);
        }
        SessionUpdate::AgentMessage { meta, body } => {
            app.push_block(Block::AgentMessage { meta, body })
        }
        SessionUpdate::AgentResult {
            meta, body, at_ms, ..
        } => {
            app.blocks.push(crate::transcript::Entry {
                at_ms: Some(at_ms),
                ..crate::transcript::Entry::new(Block::AgentResult { meta, body })
            });
        }
        SessionUpdate::GoalRoundMessage { meta, body } => {
            app.push_block(Block::GoalRound { meta, body })
        }
        SessionUpdate::GoalChanged { goal } => {
            app.goal = goal;
        }

        SessionUpdate::ToolPreview {
            preview_id,
            name,
            preview,
            ..
        } => {
            app.preview_tool(preview_id, name, preview);
        }
        SessionUpdate::ToolPreviewsResolved { calls, .. } => app.resolve_tool_previews(calls),
        SessionUpdate::ToolProgress {
            tool_call_id, rows, ..
        } => app.set_tool_progress(&tool_call_id, rows),
        SessionUpdate::ToolBatchStarted {
            batch_id, calls, ..
        } => {
            app.register_tool_batch(batch_id, calls.into_iter().map(|call| call.tool_call_id));
        }
        SessionUpdate::GenerationStarted { .. } => {
            // 新一轮尝试开始：重试倒数便签定稿回 core 原文。
            app.finish_retry_wait();
            app.start_generation();
        }
        SessionUpdate::GenerationRetry {
            message,
            retry_in_ms,
            ..
        } => {
            app.discard_generation();
            app.start_retry_wait(message.clone(), retry_in_ms);
            app.set_warning(message);
        }
        SessionUpdate::GuardNotice {
            message, terminal, ..
        } => {
            if terminal {
                app.discard_generation();
            }
            // 非终态是「已自动纠正/仍在等待」一类的干预提醒：不是失败，但值得留意。
            let tone = if terminal {
                NoteTone::Error
            } else {
                NoteTone::Warn
            };
            app.push_note(message.clone(), tone);
            app.set_warning(message);
        }
        // 插件失败即在本进程内停用：转写留下原因与恢复方式，状态栏给一行摘要。
        SessionUpdate::PluginNotice {
            plugin,
            code,
            message,
        } => {
            app.push_note(
                plugin_notice_note(&plugin, &code, &message),
                NoteTone::Error,
            );
            app.set_warning(
                t!("tui.event.plugin_disabled_fmt", plugin = plugin.as_str(), code = code.as_str())
                    .into_owned(),
            );
        }
        SessionUpdate::AssistantDelta { delta, .. } => match delta {
            moc_protocol::methods::AssistantDelta::Text { delta } => {
                app.finish_thinking();
                if !delta.is_empty()
                    && !app.has_streaming(|block| matches!(block, Block::Assistant { .. }))
                {
                    app.push_block(Block::Assistant {
                        text: String::new(),
                    });
                }
                app.append_streaming(
                    |block| match block {
                        Block::Assistant { text } => Some(text),
                        _ => None,
                    },
                    &delta,
                );
            }
            moc_protocol::methods::AssistantDelta::Reasoning { delta } => {
                // 只有仍在流式中的思考块才续写：已定稿的块属于上一响应，
                // 新响应要另起一块并重新计时。
                if !delta.is_empty()
                    && !app.has_streaming(|block| {
                        matches!(
                            block,
                            Block::Thinking {
                                duration_ms: None,
                                ..
                            }
                        )
                    })
                {
                    app.start_thinking();
                }
                app.append_streaming_limited(
                    |block| match block {
                        Block::Thinking { text, .. } => Some(text),
                        _ => None,
                    },
                    &delta,
                    128 * 1024,
                    &t!("tui.event.thinking_truncated").into_owned(),
                );
            }
            moc_protocol::methods::AssistantDelta::Usage { usage } => app.apply_usage(usage),
        },
        SessionUpdate::BashStarted {
            tool_call_id,
            command,
        } => app.start_tool_block(ToolCallStart {
            id: tool_call_id,
            name: "bash".into(),
            parent_id: None,
            arguments: serde_json::to_string(&command).expect("Bash 参数可序列化"),
        }),
        SessionUpdate::ToolStarted {
            tool_call_id,
            name,
            parent_tool_call_id,
            arguments,
            prompt_id: _,
        } => {
            app.commit_generation();
            app.start_tool_block(ToolCallStart {
                id: tool_call_id,
                name,
                parent_id: parent_tool_call_id,
                arguments,
            });
        }
        SessionUpdate::ToolFinished {
            tool_call_id,
            name,
            parent_tool_call_id,
            arguments,
            status,
            result,
            duration_ms,
            diff,
            prompt_id: _,
        } => app.finish_tool_block(ToolCallFinish {
            id: tool_call_id,
            name,
            parent_id: parent_tool_call_id,
            arguments,
            status,
            result,
            duration_ms,
            diff,
        }),
        SessionUpdate::PromptStarted { .. } => {
            app.prompt_running = true;
            app.ctrl_c_pending = None;
            app.prompt_started_at = Some(std::time::Instant::now());
            if !app.cancel_requested {
                app.notice = None;
                app.warning = None;
            }
        }
        SessionUpdate::PromptFinished { .. } => app.finish_prompt(),
        SessionUpdate::TitleChanged { title } => {
            if let Some(session) = app.session.as_mut() {
                session.title = title;
            }
        }
        SessionUpdate::EngineChanged { engine } => {
            if let Some(session) = app.session.as_mut() {
                session.engine = engine;
            }
            if engine == SessionEngine::ClmExperimental {
                app.set_notice(t!("tui.event.clm_enabled").into_owned());
            }
        }
        SessionUpdate::AutonomyChanged { autonomy } => {
            if let Some(session) = app.session.as_mut() {
                session.autonomy = autonomy;
            }
        }
        SessionUpdate::FastModeChanged { enabled } => {
            if let Some(session) = app.session.as_mut() {
                session.fast_mode = enabled;
            }
        }
        SessionUpdate::PlanModeChanged { active } => {
            if let Some(session) = app.session.as_mut() {
                session.plan_active = active;
            }
        }
        SessionUpdate::ModelChanged { model } => {
            app.remember_model(&model);
            if let Some(session) = app.session.as_mut() {
                session.model = model;
            }
        }
        SessionUpdate::PermissionChanged { permission } => {
            if let Some(session) = app.session.as_mut() {
                session.permission = permission;
            }
        }
        SessionUpdate::TodoChanged { phases } => app.update_todos(phases),
        SessionUpdate::TodoReminder { .. } => app.push_todo_reminder(),
        // unattended 时不弹审阅；自动批准由 apply_update_and_refresh 发起。
        SessionUpdate::PlanSubmitted { plan } => {
            if !app.session.as_ref().is_some_and(|session| {
                session.autonomy == moc_protocol::SessionAutonomy::Unattended
            }) {
                open_plan_review(app, plan);
            }
        }
        SessionUpdate::PlanUpdated { plan } => {
            // 同一次审阅里计划被改写（收到批注、被取代、批准）：只更新投影，保留草稿。
            if let Some(Modal::Plan { state }) = app.modal.as_mut()
                && state.plan_id == plan.plan_id
            {
                state.status = plan.status;
                state.version = plan.version;
                state.previous_plan_id = plan.previous_plan_id;
                state.submitted = plan.annotations;
            }
        }
        SessionUpdate::ContextCompactionStarted => app.start_compaction(),
        SessionUpdate::ContextCompacted {
            method: _,
            tokens_before,
            tokens_after,
            full,
        } => app.finish_compaction(
            compaction_done_note(full, tokens_before, tokens_after),
            NoteTone::Info,
        ),
        SessionUpdate::ContextCompactionFailed { message } => {
            app.finish_compaction(
                t!("tui.steer.compact_failed_fmt", err = message.as_str()).into_owned(),
                NoteTone::Error,
            );
        }
        SessionUpdate::JobStarted { job } | SessionUpdate::JobUpdated { job } => {
            // 前台 bash 任务会同时发送 JobStarted 和 ToolStarted；
            // 后台任务（explicit、自动或手动转后台）只发任务更新。
            // job_id 只在任务确为后台来源时挂上：前台命令不挂「后台」徽标，
            // 转后台时 origin 已在 core 侧改写，这里直接按当前值判断。
            if job.origin.is_background()
                && let Some(entry) = app.tool_entry_mut(&job.tool_call_id)
                && let Block::Tool(tool) = &mut entry.block
            {
                tool.job_id = Some(job.job_id.clone());
                entry.touch();
            }
        }
        SessionUpdate::JobOutput {
            job_id: _,
            tool_call_id,
            tail,
        } => {
            // 更新前台任务的实时输出
            if let Some(entry) = app.tool_entry_mut(&tool_call_id)
                && let Block::Tool(tool) = &mut entry.block
            {
                tool.live_output = Some(tail);
                entry.touch();
            }
        }
        SessionUpdate::JobFinished { job } => {
            // 任务脱离「后台运行中」：摘除徽标。只有后台任务结束才推便签——
            // 前台命令的结果已经随工具卡片落地，再提示就是把前台误报成后台。
            if let Some(entry) = app.tool_entry_mut(&job.tool_call_id)
                && let Block::Tool(tool) = &mut entry.block
            {
                tool.job_id = None;
                entry.touch();
            }
            if job.origin.is_background() {
                let code = job.exit_code.map_or_else(String::new, |code| {
                    t!("tui.event.exit_code_fmt", code = code).into_owned()
                });
                // 与 WebUI 同口径：只有干净退出（exit 0）算普通信息；
                // 非零退出、被终止、超时都值得留意。
                let tone = if job.exit_code == Some(0) {
                    NoteTone::Info
                } else {
                    NoteTone::Warn
                };
                app.push_note(
                    t!(
                        "tui.event.job_done_fmt",
                        cmd = job.command.as_str(),
                        code = code.as_str()
                    )
                    .into_owned(),
                    tone,
                );
            }
        }
        SessionUpdate::PermissionRequested { .. } => {}
        SessionUpdate::PermissionResolved { .. } => {}
        SessionUpdate::LockChanged { .. } => {}
        SessionUpdate::UpstreamMismatch { fields, .. } => {
            let detail = fields
                .iter()
                .map(|mismatch| match mismatch.field {
                    UpstreamField::Model => {
                        format!("model {} → {}", mismatch.requested, mismatch.reported)
                    }
                    UpstreamField::Effort => {
                        format!("effort {} → {}", mismatch.requested, mismatch.reported)
                    }
                })
                .collect::<Vec<_>>()
                .join(" ｜ ");
            app.set_warning(detail);
        }
        SessionUpdate::Error {
            message, retryable, ..
        } => {
            // 请求失败了：这段没落库的流式预览不属于任何一次对话，撤掉；
            // 在途的重试倒数同样定稿——错误便签才是这次失败的终态。
            app.finish_retry_wait();
            app.discard_generation();
            app.push_note(
                t!(
                    "tui.event.error_fmt",
                    message = message.as_str(),
                    retryable = if retryable {
                        t!("tui.event.retryable")
                    } else {
                        "".into()
                    }
                )
                .into_owned(),
                NoteTone::Error,
            );
        }
        SessionUpdate::PromptRewound { replayed_tools } => {
            app.rewind_failed_turn(replayed_tools);
        }
    }
}

pub(super) fn start_auto_prompt(
    app: &mut App,
    connection: &EmbeddedConnection,
    auto_prompt: Option<String>,
) {
    let Some(prompt) = auto_prompt else {
        return;
    };
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    app.push_user(prompt.clone());
    app.inflight_prompts.insert(session_id.clone());
    app.prompt_running = true;
    crate::queue::send_prompt(
        &connection.client,
        session_id,
        vec![InputPart::Text { text: prompt }],
        None,
        app.events.clone(),
    );
}

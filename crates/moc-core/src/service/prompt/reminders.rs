//! 每轮提示的动态提醒候选与持久化；保留一次性通知的消费顺序。

use super::flush_reminders;
use crate::service::CoreInner;
use crate::service::protocol_to_rpc;
use crate::service::to_rpc;
use crate::session::entries::EntryPayload;
use crate::system_prompt::engine_reminder;
use crate::system_prompt::mode_reminder;
use crate::system_prompt::reminder;
use crate::system_prompt::reminder::Reminder;
use crate::system_prompt::sandbox_contract;
use moc_protocol::RpcError;
use moc_protocol::methods::SessionUpdate;
use std::sync::Arc;

pub(super) struct Scope<'a> {
    pub(super) inner: &'a Arc<CoreInner>,
    pub(super) record: &'a Arc<crate::session::SessionRecord>,
    pub(super) runtime: &'a Arc<crate::service::SessionRuntime>,
    pub(super) update: &'a moc_protocol::UpdateSink,
    pub(super) session_id: &'a str,
    pub(super) prompt_id: &'a String,
    pub(super) project_config: &'a crate::ResolvedConfig,
    pub(super) tools_available: &'a [moc_llm::ToolDef],
    pub(super) engine: moc_protocol::SessionEngine,
    pub(super) mode: crate::service::mode::Mode,
    pub(super) permission: moc_protocol::SessionPermission,
    pub(super) roster: &'a Option<String>,
    pub(super) mcp_index: &'a Option<String>,
    pub(super) mcp_state: &'a Option<crate::tools::declarations::McpStateReminder>,
    pub(super) cycle_redirect: Option<String>,
    pub(super) thinking_redirect: Option<String>,
    pub(super) first_turn: bool,
}

pub(super) async fn prepare(scope: Scope<'_>) -> Result<Vec<Reminder>, RpcError> {
    let Scope {
        inner,
        record,
        runtime,
        update,
        session_id,
        prompt_id,
        project_config,
        tools_available,
        engine,
        mode,
        permission,
        roster,
        mcp_index,
        mcp_state,
        mut cycle_redirect,
        mut thinking_redirect,
        first_turn,
    } = scope;
    let mut candidates = Vec::new();
    // todo 停滞提醒：仍有未完成项，且距上次 todo 变更已累计足够多次工具调用时注入一次。
    // 每个提示最多 `reminders_max` 次；前端把它显示成一行工具样式的占位，没有参数和结果。
    if project_config.tools.todo.enabled
        && project_config.tools.todo.reminders
        && mode != crate::service::mode::Mode::Plan
        && tools_available.iter().any(|def| def.name == "todo")
    {
        let mut state = record.state.lock().await;
        let open_items = state
            .todos
            .iter()
            .flat_map(|phase| &phase.items)
            .filter(|item| {
                matches!(
                    item.status,
                    moc_protocol::TodoStatus::Pending | moc_protocol::TodoStatus::InProgress
                )
            })
            .count();
        let due = open_items > 0
            && state.tool_calls_since_todo >= project_config.tools.todo.reminders_after
            && state.todo_reminders_sent < project_config.tools.todo.reminders_max;
        if due {
            state.tool_calls_since_todo = 0;
            state.todo_reminders_sent += 1;
            drop(state);
            candidates.push(Reminder::once(
                reminder::key::TODO_STALL,
                format!(
                    "{open_items} todo items open. Mark finished tasks done now, or keep working."
                ),
            ));
            update
                .emit(
                    session_id.to_owned(),
                    SessionUpdate::TodoReminder {
                        prompt_id: prompt_id.clone(),
                        open_items,
                    },
                )
                .await
                .map_err(protocol_to_rpc)?;
        }
    }
    candidates.push(Reminder::durable(
        reminder::key::ENGINE,
        engine_reminder(engine),
    ));
    // 模式提醒独立成条目：plan 进出与 ask 到期都会改写或撤掉它。
    match mode_reminder(mode) {
        Some(text) => {
            candidates.push(Reminder::durable(reminder::key::MODE, text));
        }
        None => candidates.push(Reminder::retract(
            reminder::key::MODE,
            "The earlier execution mode no longer applies: the workspace is writable under the current engine.",
        )),
    }
    candidates.push(Reminder::durable(
        reminder::key::SANDBOX,
        sandbox_contract(permission, &project_config.sandbox),
    ));
    match &roster {
        Some(roster) => {
            candidates.push(Reminder::durable(reminder::key::ROSTER, roster.clone()));
        }
        // 清单消失时撤回旧条目：否则会话里会一直留着一份失效的可通信代理清单。
        None => candidates.push(Reminder::retract(
            reminder::key::ROSTER,
            "The earlier agent roster no longer applies: this session currently has no parent agent or siblings to coordinate with.",
        )),
    }
    // goal 上下文：有目标时注入，无目标（或已清除）时撤回旧条目。
    let goal_view = {
        let state = record.state.lock().await;
        crate::goal::view(
            state.goal.as_ref(),
            state.goal_rounds_started,
            runtime.goal_armed.is_armed(),
        )
    };
    match &goal_view {
        Some(goal) => candidates.push(Reminder::durable(
            reminder::key::GOAL,
            crate::goal::goal_context(goal),
        )),
        None => candidates.push(Reminder::retract(
            reminder::key::GOAL,
            "The earlier goal context no longer applies: this session has no goal.",
        )),
    }
    // 动态目录不进入系统前缀，正文不变时由提醒摘要去重。
    if let Some(index) = &mcp_index {
        candidates.push(Reminder::durable(reminder::key::MCP_INDEX, index.clone()));
    } else {
        candidates.push(Reminder::retract(
            reminder::key::MCP_INDEX,
            "The earlier MCP index no longer applies. Discover currently available tools through catalog.search().",
        ));
    }
    // MCP server 状态与工具集合快照：全员正常时静默记基线，仅异常或相对上次快照变更时注入一次。
    if let Some(state) = mcp_state {
        let item = if state.has_issue {
            Reminder::durable_with_fingerprint(
                reminder::key::MCP_STATE,
                state.text.clone(),
                state.fingerprint.clone(),
            )
        } else {
            Reminder::baseline_with_fingerprint(
                reminder::key::MCP_STATE,
                state.text.clone(),
                state.fingerprint.clone(),
            )
        };
        candidates.push(item);
    }
    if let Some(detail) = cycle_redirect.take() {
        candidates.push(Reminder::once(
            reminder::key::CYCLE,
            format!("<system-interrupt reason=\"tool_loop\">{detail}. Stop repeating: gather new evidence or summarize and finish. Another no-progress loop ends this prompt.</system-interrupt>"),
        ));
    }
    if let Some(detail) = thinking_redirect.take() {
        candidates.push(Reminder::once(
            reminder::key::THINKING,
            format!("<system-interrupt reason=\"thinking_loop\">{detail}. Stop repeating the same reasoning; act on what you know or state the blocker and finish.</system-interrupt>"),
        ));
    }
    // notice 是一次性事实，drain 之后就不可重复获取。
    let notices = {
        let mut state = record.state.lock().await;
        runtime.jobs.mark_all_reported();
        std::mem::take(&mut state.notices)
    };
    for notice in notices {
        let Some(job) = notice.job else {
            if engine == moc_protocol::SessionEngine::ClmExperimental {
                crate::tools::clm::append_message(
                    &inner.sessions,
                    &record.state,
                    session_id,
                    &moc_llm::Message::custom(reminder::wrap(&notice.text)),
                )
                .await
                .map_err(to_rpc)?;
            }
            candidates.push(Reminder::once(reminder::key::NOTICE, notice.text));
            continue;
        };
        // 任务通知单独成条：它带实质交付内容，前端靠 job_notice 元数据渲染卡片，
        // 混进提醒大杂烩就没有可识别的结构了。
        let body = reminder::wrap(&notice.text);
        // CLM 请求只读取实时上下文，一次性通知还需同步进去，不能只留在 transcript。
        if engine == moc_protocol::SessionEngine::ClmExperimental {
            crate::tools::clm::append_message(
                &inner.sessions,
                &record.state,
                session_id,
                &moc_llm::Message::custom(body.clone()),
            )
            .await
            .map_err(to_rpc)?;
        }
        let entry = inner
            .sessions
            .append(
                session_id,
                EntryPayload::JobNotice {
                    job: job.clone(),
                    body: body.clone(),
                },
            )
            .await
            .map_err(to_rpc)?;
        update
            .emit(
                session_id.to_owned(),
                SessionUpdate::JobNotified {
                    entry_id: entry.id().to_owned(),
                    at_ms: entry.at_ms(),
                    job,
                    body,
                },
            )
            .await
            .map_err(protocol_to_rpc)?;
    }
    {
        let mut reminder_state = runtime.reminders.lock().await;
        if first_turn {
            reminder_state.render(&crate::system_prompt::reminder::local_time());
        }
        // 已注入过的本地时间每轮都作为候选：压缩清空去重表后要补回历史。
        if let Some(text) = reminder_state.last_text() {
            candidates.push(Reminder::durable(
                reminder::key::LOCAL_TIME,
                text.to_owned(),
            ));
        }
    }
    flush_reminders(inner, session_id, record, engine, &candidates, true).await?;
    Ok(candidates)
}

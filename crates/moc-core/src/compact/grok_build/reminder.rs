//! Grok Build 压缩后注入的运行时状态提醒。
//!
//! 整体替换历史后只剩摘要，而 todos、后台任务、子代理和活动计划都是运行状态，既不在
//! 摘要里也无法从 transcript 推导出来，必须在压缩时快照成一条 system reminder。

use moc_protocol::methods::{AgentStatus, PlanStatus};

use crate::service::CoreInner;
use crate::session::state::SessionState;
use crate::session::store::now_ms;
use crate::tools::job::JobStatus;

use super::super::digest::TodoLines;

/// 渲染整条提醒；所有分节都为空时返回 `None`，不注入空标签。
pub(crate) async fn render(inner: &CoreInner, state: &SessionState) -> Option<String> {
    let mut sections = Vec::with_capacity(5);
    if let Some(section) = edited_files(state) {
        sections.push(section);
    }
    if let Some(section) = running_jobs(inner, state).await {
        sections.push(section);
    }
    if let Some(section) = todo_list(state) {
        sections.push(section);
    }
    if let Some(section) = running_agents(inner, state).await {
        sections.push(section);
    }
    if let Some(section) = active_plan(state) {
        sections.push(section);
    }
    if sections.is_empty() {
        return None;
    }
    Some(format!(
        "<system-reminder>\n{}\n</system-reminder>",
        sections.join("\n\n")
    ))
}

/// 本次会话成功写入过的文件；与 `remote::checkpoint_summary` 用同一个「成功」判断。
fn edited_files(state: &SessionState) -> Option<String> {
    let files = super::super::digest::edited_files(state);
    if files.is_empty() {
        return None;
    }
    let mut section = String::from(
        "## Files Edited This Session\nThese files were modified by you during this session:",
    );
    for path in files {
        section.push_str("\n- ");
        section.push_str(&path);
    }
    Some(section)
}

/// 仍在运行或正在终止的后台任务。
async fn running_jobs(inner: &CoreInner, state: &SessionState) -> Option<String> {
    let jobs = inner.runtime(&state.session.id).await.jobs.list().await;
    let mut lines = Vec::new();
    for job in jobs {
        let status = match job.status {
            JobStatus::Running => "running",
            JobStatus::Killing => "killing",
            _ => continue,
        };
        lines.push(format!("- \"{}\": `{}` ({status})", job.id, job.command));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Running Background Tasks\nThese tasks are still running:\n{}",
        lines.join("\n")
    ))
}

/// 未完成的 todo 项；已完成和已丢弃的只计数。
fn todo_list(state: &SessionState) -> Option<String> {
    let TodoLines {
        lines,
        completed,
        cancelled,
    } = super::super::digest::todo_lines(state);
    if lines.is_empty() && completed == 0 && cancelled == 0 {
        return None;
    }
    let mut section = String::from(
        "## TODO List\nThis is your task list from before the conversation was compacted — it is still active. Keep working through the items below and update their status as you make progress:",
    );
    if !lines.is_empty() {
        section.push('\n');
        section.push_str(&lines.join("\n"));
    }
    let trailer = match (completed, cancelled) {
        (0, 0) => None,
        (count, 0) => Some(format!("({count} completed)")),
        (0, count) => Some(format!("({count} cancelled)")),
        (completed, cancelled) => Some(format!("({completed} completed, {cancelled} cancelled)")),
    };
    if let Some(trailer) = trailer {
        section.push('\n');
        section.push_str(&trailer);
    }
    Some(section)
}

/// 压缩前已启动、仍在排队或运行的子代理。
async fn running_agents(inner: &CoreInner, state: &SessionState) -> Option<String> {
    let now = now_ms();
    let mut lines = Vec::new();
    for runtime in inner.agents.lock().await.items.values() {
        let info = &runtime.info;
        if info.parent_id != state.session.id {
            continue;
        }
        let status = match info.status {
            AgentStatus::Queued => "queued",
            AgentStatus::Running => "running",
            _ => continue,
        };
        let secs = now.saturating_sub(info.created_at_ms) / 1000;
        lines.push(format!(
            "- \"{}\": `{}` (running for {secs}s, {status})",
            info.id, info.title
        ));
    }
    if lines.is_empty() {
        return None;
    }
    Some(format!(
        "## Running Subagents\nThese subagents were launched before this compaction and are still running. Use `wait_agent` with the agent_id to check their status or retrieve results. Use `stop_agent` with the agent_id to cancel a subagent.\n{}",
        lines.join("\n")
    ))
}

/// 已批准、已提交或已收到修订意见的最新计划；计划本身不进摘要，必须整段重新注入。
fn active_plan(state: &SessionState) -> Option<String> {
    let plan = state.plans.iter().rev().find(|plan| {
        matches!(
            plan.status,
            PlanStatus::Approved | PlanStatus::Submitted | PlanStatus::Revised
        )
    })?;
    if plan.markdown.trim().is_empty() {
        return None;
    }
    Some(format!("## Active Plan\n{}", plan.markdown))
}

#[cfg(test)]
mod tests {
    use moc_llm::{ContentPart, Message, ToolCall};
    use moc_protocol::methods::{
        PlanInfo, SessionEngine, SessionPermission, TodoItem, TodoPhase, TodoStatus,
    };

    use super::*;
    use crate::session::entries::{Entry, EntryPayload, SessionOrigin};

    fn state() -> SessionState {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            1,
        );
        SessionState::from_entries(vec![header]).expect("header 可回放")
    }

    fn push_message(state: &mut SessionState, message: Message) {
        let entry = Entry::from_payload(
            EntryPayload::Message {
                message,
                tool: None,
                finish_reason: None,
                agent: None,
            },
            state.last_entry_id.clone(),
            2,
        );
        state.apply_entry(entry);
    }

    #[test]
    fn edited_files_list_only_successful_operations() {
        let mut state = state();
        push_message(
            &mut state,
            Message {
                role: moc_llm::Role::Assistant,
                content: vec![
                    ContentPart::ToolCall(ToolCall::new("c1", "write", r#"{"path":"b.rs"}"#)),
                    ContentPart::ToolCall(ToolCall::new("c2", "edit", r#"{"path":"a.rs"}"#)),
                    ContentPart::ToolCall(ToolCall::new("c3", "edit", r#"{"path":"c.rs"}"#)),
                    ContentPart::ToolCall(ToolCall::new(
                        "c4",
                        "move",
                        r#"{"move":{"source_path":"x","target_path":"d.rs"}}"#,
                    )),
                ],
            },
        );
        push_message(
            &mut state,
            Message::tool_result_with_images("c1", "ok", false, Vec::new()),
        );
        push_message(
            &mut state,
            Message::tool_result_with_images("c2", "ok", false, Vec::new()),
        );
        push_message(
            &mut state,
            Message::tool_result_with_images("c3", "失败", true, Vec::new()),
        );
        push_message(
            &mut state,
            Message::tool_result_with_images("c4", "ok", false, Vec::new()),
        );
        assert_eq!(
            edited_files(&state).as_deref(),
            Some(
                "## Files Edited This Session\nThese files were modified by you during this session:\n- a.rs\n- b.rs\n- d.rs"
            ),
            "失败的工具调用不进清单，move 只取目标路径"
        );
    }

    #[test]
    fn todos_render_ids_statuses_reasons_and_counts() {
        let mut state = state();
        state.todos = vec![TodoPhase {
            phase: "阶段".into(),
            items: vec![
                TodoItem {
                    status: TodoStatus::Pending,
                    task: "实现".into(),
                    reason: None,
                },
                TodoItem {
                    status: TodoStatus::InProgress,
                    task: "接线".into(),
                    reason: Some("等待上游".into()),
                },
                TodoItem {
                    status: TodoStatus::Completed,
                    task: "调研".into(),
                    reason: None,
                },
                TodoItem {
                    status: TodoStatus::Abandoned,
                    task: "旧方案".into(),
                    reason: None,
                },
            ],
        }];
        assert_eq!(
            todo_list(&state).as_deref(),
            Some(
                "## TODO List\nThis is your task list from before the conversation was compacted — it is still active. Keep working through the items below and update their status as you make progress:\n- [pending] todo-1-1: 实现\n- [in_progress] todo-1-2: 接线 (等待上游)\n(1 completed, 1 cancelled)"
            )
        );
        state.todos[0].items.truncate(1);
        assert_eq!(
            todo_list(&state).as_deref(),
            Some(
                "## TODO List\nThis is your task list from before the conversation was compacted — it is still active. Keep working through the items below and update their status as you make progress:\n- [pending] todo-1-1: 实现"
            ),
            "没有已完成项时不出现统计尾注"
        );
    }

    #[test]
    fn active_plan_prefers_latest_approved_or_submitted() {
        let mut state = state();
        state.plans = vec![
            PlanInfo {
                plan_id: "plan_a".into(),
                title: "旧".into(),
                path: "/tmp/a.md".into(),
                markdown: "# 旧计划".into(),
                status: PlanStatus::Rejected,
                version: 1,
                previous_plan_id: None,
                annotations: Vec::new(),
            },
            PlanInfo {
                plan_id: "plan_b".into(),
                title: "新".into(),
                path: "/tmp/b.md".into(),
                markdown: "# 新计划".into(),
                status: PlanStatus::Submitted,
                version: 2,
                previous_plan_id: None,
                annotations: Vec::new(),
            },
        ];
        assert_eq!(
            active_plan(&state).as_deref(),
            Some("## Active Plan\n# 新计划")
        );
        state.plans[1].status = PlanStatus::Rejected;
        assert_eq!(active_plan(&state), None);
    }
}

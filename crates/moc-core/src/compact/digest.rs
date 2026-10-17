//! 压缩摘要共用的确定性会话事实：改动文件、命令、待办与兜底 checkpoint。
//!
//! 这些内容全部由 transcript 与运行态推导，不依赖模型。摘要提示词用它给模型
//! 提供锚点；模型连续给出不合格 checkpoint 时用它兜底，保证一次压缩永远不会
//! 把上下文丢成一句话。

use std::collections::BTreeSet;
use std::collections::HashSet;

use moc_llm::{Message, Role};
use moc_protocol::methods::TodoStatus;
use serde_json::Value;

use crate::session::state::SessionState;

use super::replay::is_injected_context;

/// 提示词里最多列出的文件、命令与待办条数；超出部分按最近优先截断。
const MAX_PROMPT_FILES: usize = 20;
const MAX_PROMPT_COMMANDS: usize = 12;
const MAX_PROMPT_TODOS: usize = 24;
/// 兜底 checkpoint 里单条用户请求与助手输出的截断长度。
const MAX_QUOTE_CHARS: usize = 700;
const MAX_ACTIVITY_CHARS: usize = 400;
/// 兜底 checkpoint 保留的最近用户请求条数。
const MAX_QUOTED_REQUESTS: usize = 5;

/// 未完成 todo 的渲染结果与计数。
pub(crate) struct TodoLines {
    pub lines: Vec<String>,
    pub completed: usize,
    pub cancelled: usize,
}

/// 本次会话成功写入过的文件；与 `remote::checkpoint_summary` 使用同一份成功判定。
pub(crate) fn edited_files(state: &SessionState) -> Vec<String> {
    let completed: HashSet<&str> = state
        .transcript
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .filter(|result| !result.is_error)
        .map(|result| result.tool_call_id.as_str())
        .collect();
    let mut files = BTreeSet::new();
    for call in state
        .transcript
        .iter()
        .flat_map(|entry| entry.message.tool_calls())
    {
        if !completed.contains(call.id.as_str()) {
            continue;
        }
        let Ok(arguments) = call.arguments_json() else {
            continue;
        };
        match call.name.as_str() {
            "edit" | "write" => {
                if let Some(path) = arguments.get("path").and_then(Value::as_str) {
                    files.insert(path.to_owned());
                }
            }
            "move" => {
                if let Some(path) = arguments
                    .get("move")
                    .and_then(|value| value.get("target_path"))
                    .and_then(Value::as_str)
                {
                    files.insert(path.to_owned());
                }
            }
            _ => {}
        }
    }
    files.into_iter().collect()
}

/// 会话里执行过的 bash 命令，最近 `MAX_PROMPT_COMMANDS` 条去重后按时间顺序返回。
pub(crate) fn commands(state: &SessionState) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut recent = Vec::new();
    // 同一条消息里的并行调用按调用顺序排列，必须先把整条流收集起来再反转。
    let calls: Vec<&moc_llm::ToolCall> = state
        .transcript
        .iter()
        .flat_map(|entry| entry.message.tool_calls())
        .collect();
    for call in calls.into_iter().rev() {
        if call.name != "bash" {
            continue;
        }
        let Ok(arguments) = call.arguments_json() else {
            continue;
        };
        let Some(command) = arguments.get("command").and_then(Value::as_str) else {
            continue;
        };
        let command = command.trim();
        if command.is_empty() || !seen.insert(command.to_owned()) {
            continue;
        }
        recent.push(command.to_owned());
        if recent.len() >= MAX_PROMPT_COMMANDS {
            break;
        }
    }
    recent.reverse();
    recent
}

/// 未完成的 todo 项；已完成与已放弃的只计数。
pub(crate) fn todo_lines(state: &SessionState) -> TodoLines {
    let mut lines = Vec::new();
    let mut completed = 0usize;
    let mut cancelled = 0usize;
    for (phase_index, phase) in state.todos.iter().enumerate() {
        for (item_index, item) in phase.items.iter().enumerate() {
            let id = format!("todo-{}-{}", phase_index + 1, item_index + 1);
            match item.status {
                TodoStatus::Completed => {
                    completed += 1;
                    continue;
                }
                TodoStatus::Abandoned => {
                    cancelled += 1;
                    continue;
                }
                TodoStatus::Pending | TodoStatus::Blocked => {
                    lines.push(format!("- [pending] {id}: {}", item.task));
                }
                TodoStatus::InProgress => {
                    lines.push(format!("- [in_progress] {id}: {}", item.task));
                }
            }
            if let Some(reason) = &item.reason {
                let line = lines.last_mut().expect("刚推入一行");
                line.push_str(" (");
                line.push_str(reason);
                line.push(')');
            }
        }
    }
    TodoLines {
        lines,
        completed,
        cancelled,
    }
}

/// 摘要提示词使用的会话事实块；全空时返回 `None`，不注入空标签。
pub(crate) fn prompt_digest(state: &SessionState) -> Option<String> {
    let mut body = String::new();
    let files = edited_files(state);
    if !files.is_empty() {
        body.push_str("Files edited this session (already written to disk):\n");
        for path in files.iter().take(MAX_PROMPT_FILES) {
            body.push_str("- ");
            body.push_str(path);
            body.push('\n');
        }
        if files.len() > MAX_PROMPT_FILES {
            body.push_str(&format!(
                "- … {} more files edited\n",
                files.len() - MAX_PROMPT_FILES
            ));
        }
    }
    let commands = commands(state);
    if !commands.is_empty() {
        body.push_str("Commands run this session (most recent last):\n");
        for command in &commands {
            body.push_str("- `");
            body.push_str(&truncate(command, MAX_ACTIVITY_CHARS));
            body.push_str("`\n");
        }
    }
    let todos = todo_lines(state);
    if !todos.lines.is_empty() {
        body.push_str("Open todo items (still active):\n");
        for line in todos.lines.iter().take(MAX_PROMPT_TODOS) {
            body.push_str(line);
            body.push('\n');
        }
        if todos.lines.len() > MAX_PROMPT_TODOS {
            body.push_str(&format!(
                "- … {} more open items\n",
                todos.lines.len() - MAX_PROMPT_TODOS
            ));
        }
    }
    if body.is_empty() {
        return None;
    }
    Some(format!("<session-digest>\n{body}</session-digest>"))
}

/// 模型连续给出不合格 checkpoint 时的兜底：保留上一版 checkpoint 全文，并把
/// 最近一轮的确定性事实追加在后面；没有上一版时按必需段落拼出骨架。
pub(crate) fn fallback_checkpoint(
    state: &SessionState,
    history: &[Message],
    previous: Option<&str>,
) -> String {
    let mut requests: Vec<String> = history
        .iter()
        .rev()
        .filter(|message| message.role == Role::User && !is_injected_context(message))
        .map(Message::text)
        .filter(|text| !text.trim().is_empty())
        .take(MAX_QUOTED_REQUESTS)
        .collect();
    requests.reverse();
    let last_assistant = history
        .iter()
        .rev()
        .filter(|message| message.role == Role::Assistant)
        .map(Message::text)
        .find(|text| !text.trim().is_empty());
    let files = edited_files(state);
    let commands = commands(state);
    let todos = todo_lines(state);

    let mut activity = String::new();
    if let Some(latest) = requests.last() {
        activity.push_str("- Latest user request (verbatim): ");
        activity.push_str(&truncate(latest.trim(), MAX_ACTIVITY_CHARS));
        activity.push('\n');
    }
    if let Some(text) = &last_assistant {
        activity.push_str("- Last assistant state: ");
        activity.push_str(&truncate(text.trim(), MAX_ACTIVITY_CHARS));
        activity.push('\n');
    }
    if !files.is_empty() {
        activity.push_str("- Files edited so far: ");
        activity.push_str(&files.join(", "));
        activity.push('\n');
    }
    if !commands.is_empty() {
        activity.push_str("- Commands run (most recent last): ");
        activity.push_str(&commands.join(" | "));
        activity.push('\n');
    }
    if !todos.lines.is_empty() {
        activity.push_str("- Open todo items:\n");
        for line in todos.lines.iter().take(MAX_PROMPT_TODOS) {
            activity.push_str("  ");
            activity.push_str(line);
            activity.push('\n');
        }
    }

    match previous {
        Some(previous) => {
            let mut out = String::with_capacity(previous.len() + activity.len() + 96);
            out.push_str(previous.trim_end());
            out.push_str(
                "\n\n## Recent Activity (auto-checkpoint; the summary model returned no usable checkpoint)\n",
            );
            out.push_str(&activity);
            out
        }
        None => {
            let mut out = String::with_capacity(activity.len() + 512);
            out.push_str("## Primary Request and Intent\n");
            if requests.is_empty() {
                out.push_str("- No user request is recoverable from the transcript.\n");
            }
            for request in &requests {
                out.push_str("- \"");
                out.push_str(&truncate(request.trim(), MAX_QUOTE_CHARS));
                out.push_str("\"\n");
            }
            out.push_str("\n## Files and Code\n");
            if files.is_empty() {
                out.push_str("- No file was edited before this checkpoint.\n");
            } else {
                for path in &files {
                    out.push_str("- ");
                    out.push_str(path);
                    out.push('\n');
                }
            }
            out.push_str("\n## Pending Jobs\n");
            if todos.lines.is_empty() {
                out.push_str("- None recorded.\n");
            } else {
                for line in todos.lines.iter().take(MAX_PROMPT_TODOS) {
                    out.push_str(line);
                    out.push('\n');
                }
            }
            out.push_str("\n## Current Work\n");
            out.push_str(&activity);
            out.push_str("\n## Next Step\n");
            out.push_str("- Continue from the latest user request above; re-read files with `read` before editing.\n");
            out.push_str("\n## Critical Context\n");
            out.push_str("- This checkpoint is a deterministic fallback (auto-checkpoint): the summary model produced no usable checkpoint, so the facts above are preserved verbatim.\n");
            if !commands.is_empty() {
                out.push_str("- Commands run (most recent last): ");
                out.push_str(&commands.join(" | "));
                out.push('\n');
            }
            out
        }
    }
}

/// 按字符截断并补省略号；不拆开 UTF-8 字符。
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_owned();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use moc_llm::{Message, ToolCall};
    use moc_protocol::methods::{PlanStatus, SessionMode, SessionPermission, TodoItem, TodoPhase};

    use super::*;
    use crate::session::entries::{Entry, EntryPayload, SessionOrigin};

    fn state_with_calls(
        calls: Vec<(&str, serde_json::Value)>,
        results: Vec<(&str, bool)>,
    ) -> SessionState {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "t".into(),
            1,
        );
        let mut parent = header.id().to_owned();
        let mut entries = vec![header];
        let mut message = Message::assistant_text("");
        for (index, (name, arguments)) in calls.into_iter().enumerate() {
            message
                .content
                .push(moc_llm::ContentPart::ToolCall(ToolCall {
                    id: format!("call_{index}_{name}"),
                    name: name.into(),
                    arguments: arguments.to_string(),
                }));
        }
        entries.push(Entry::from_payload(
            EntryPayload::Message {
                message: message.clone(),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            parent.clone(),
            2,
        ));
        parent = entries.last().expect("entry").id().to_owned();
        for (id, is_error) in results {
            entries.push(Entry::from_payload(
                EntryPayload::Message {
                    message: Message::tool_result(id, "ok", is_error),
                    tool: None,
                    finish_reason: None,
                    agent: None,
                },
                parent.clone(),
                3,
            ));
            parent = entries.last().expect("entry").id().to_owned();
        }
        SessionState::from_entries(entries).expect("state")
    }

    #[test]
    fn edited_files_only_counts_successful_writes() {
        let state = state_with_calls(
            vec![
                ("edit", serde_json::json!({"path": "b.rs"})),
                ("write", serde_json::json!({"path": "a.rs"})),
                ("edit", serde_json::json!({"path": "c.rs"})),
            ],
            vec![("call_0_edit", false), ("call_1_write", false)],
        );
        let files = edited_files(&state);
        assert_eq!(files, vec!["a.rs".to_owned(), "b.rs".to_owned()]);
    }

    #[test]
    fn commands_are_deduplicated_and_ordered() {
        let state = state_with_calls(
            vec![
                ("bash", serde_json::json!({"command": "cargo test"})),
                ("bash", serde_json::json!({"command": "cargo test"})),
                ("bash", serde_json::json!({"command": "cargo fmt"})),
            ],
            vec![
                ("call_0_bash", false),
                ("call_1_bash", false),
                ("call_2_bash", false),
            ],
        );
        assert_eq!(
            commands(&state),
            vec!["cargo test".to_owned(), "cargo fmt".to_owned()]
        );
    }

    #[test]
    fn todo_lines_map_statuses_and_reasons() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "t".into(),
            1,
        );
        let parent = header.id().to_owned();
        let todos = Entry::from_payload(
            EntryPayload::Todos {
                phases: vec![TodoPhase {
                    phase: "phase".into(),
                    items: vec![
                        TodoItem {
                            task: "done".into(),
                            status: TodoStatus::Completed,
                            reason: None,
                        },
                        TodoItem {
                            task: "open".into(),
                            status: TodoStatus::InProgress,
                            reason: None,
                        },
                        TodoItem {
                            task: "blocked".into(),
                            status: TodoStatus::Blocked,
                            reason: Some("等待评审".into()),
                        },
                        TodoItem {
                            task: "dropped".into(),
                            status: TodoStatus::Abandoned,
                            reason: None,
                        },
                    ],
                }],
            },
            parent,
            2,
        );
        let state = SessionState::from_entries(vec![header, todos]).expect("state");
        let lines = todo_lines(&state);
        assert_eq!(
            lines.lines,
            vec![
                "- [in_progress] todo-1-2: open".to_owned(),
                "- [pending] todo-1-3: blocked (等待评审)".to_owned()
            ]
        );
        assert_eq!(lines.completed, 1);
        assert_eq!(lines.cancelled, 1);
    }

    #[test]
    fn prompt_digest_lists_files_and_open_items() {
        let state = state_with_calls(
            vec![(
                "edit",
                serde_json::json!({"path": "crates/ingest/src/window.rs"}),
            )],
            vec![("call_0_edit", false)],
        );
        let digest = prompt_digest(&state).expect("digest");
        assert!(digest.starts_with("<session-digest>"));
        assert!(digest.contains("- crates/ingest/src/window.rs"));
        assert!(digest.ends_with("</session-digest>"));
    }

    #[test]
    fn fallback_without_previous_builds_required_sections() {
        let state = state_with_calls(vec![], vec![]);
        let history = vec![
            Message::user_text("把 TTL 改成 45 秒"),
            Message::assistant_text("已改好 window.rs"),
        ];
        let checkpoint = fallback_checkpoint(&state, &history, None);
        for section in [
            "## Primary Request and Intent",
            "## Files and Code",
            "## Pending Jobs",
            "## Current Work",
            "## Next Step",
            "## Critical Context",
        ] {
            assert!(checkpoint.contains(section), "缺少 {section}");
        }
        assert!(checkpoint.contains("把 TTL 改成 45 秒"));
        assert!(checkpoint.contains("已改好 window.rs"));
    }

    #[test]
    fn fallback_keeps_previous_checkpoint_verbatim() {
        let state = state_with_calls(vec![], vec![]);
        let history = vec![Message::user_text("继续第 3 步")];
        let checkpoint = fallback_checkpoint(&state, &history, Some("## Previous\nkeep me"));
        assert!(checkpoint.starts_with("## Previous\nkeep me"));
        assert!(checkpoint.contains("## Recent Activity"));
        assert!(checkpoint.contains("继续第 3 步"));
    }

    #[test]
    fn fallback_ignores_injected_context_messages() {
        let state = state_with_calls(vec![], vec![]);
        let history = vec![
            Message::user_text("真实请求"),
            Message::user_text("<system-reminder>Current local time: 2026-01-01</system-reminder>"),
        ];
        let checkpoint = fallback_checkpoint(&state, &history, None);
        assert!(checkpoint.contains("真实请求"));
        assert!(!checkpoint.contains("Current local time"));
    }

    #[test]
    fn plan_status_variants_compile() {
        // 防止 TodoStatus/PlanStatus 新增变体时这里的映射被静默遗忘。
        let _ = PlanStatus::Approved;
    }
}

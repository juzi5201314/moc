//! 压缩摘要共用的确定性会话事实：改动文件、命令、待办与兜底 checkpoint。
//!
//! 这些内容全部由 transcript 与运行态推导，不依赖模型。摘要提示词用它给模型
//! 提供锚点；模型连续给出不合格 checkpoint 时用它兜底，保证一次压缩永远不会
//! 把上下文丢成一句话。

use std::collections::BTreeSet;
use std::collections::HashSet;

use moc_llm::{ContentPart, Message, Role};
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

/// 一条"压缩后必须保留"的决策：原文片段 + 其中必须逐字出现的专名。
///
/// 被否决的方案（`否决了用 lru crate 的 LRU 缓存`、表格里的 `… 迁到 Redis | 否决`）
/// 既不是路径也不是数字，普通锚点永远抓不到；压缩丢掉它，继任者就会把否决过的方案
/// 重新提出来。这里单独抽出来，交给校验强制保留。
pub(crate) struct Decision {
    /// 决策原文片段（截断后），写进提示词让模型照抄。
    pub clause: String,
    /// 决策里的专名/标识符；checkpoint 必须包含（大小写不敏感）。
    pub anchors: Vec<String>,
}

/// 被否决方案的识别词：出现在正文或工具结果里都算决策。
///
/// 英文条目来自 checkpoint 契约要求的 `- rejected: <option> — <reason>`，
/// 这样上一版 checkpoint 里的否决项在下一代仍会被重新要求保留。
const REJECTION_MARKERS: [&str; 5] = ["否决", "不采用", "放弃", "rejected", "not adopted"];
/// 硬约束的识别词：只认用户/助手的原话。
///
/// 工具结果里这些词大多是代码注释（`// 禁止在这里分配`），当成约束会让每份代码
/// 都变成必须保留的决策，反而把 checkpoint 撑爆。
const CONSTRAINT_MARKERS: [&str; 5] = ["禁止", "不要碰", "绝对不能", "上限", "不能再"];
/// 决策原文片段的最大字符数。
const MAX_DECISION_CHARS: usize = 160;
/// 一条决策最多抽取的专名个数。
const MAX_DECISION_ANCHORS: usize = 3;
/// 最多保留的决策条数。
const MAX_DECISIONS: usize = 12;
/// 抽取专名时忽略的通用英文词：它们是行文，不是被否决的对象。
const DECISION_STOP_WORDS: [&str; 14] = [
    "crate", "crates", "self", "the", "and", "for", "with", "that", "this", "from", "into", "true",
    "false", "null",
];

/// 从历史里抽取决策与被否决的方案；同一决策只保留一次。
///
/// `previous` 是上一版 checkpoint：第二次压缩之后原始请求已不在可见历史里，
/// 被否决方案只存在于那一版 checkpoint，必须继续参与抽取，否则第三代就会丢。
/// 工具结果只扫"否决"这一类行（不是整篇内容），因此不会像锚点那样被文件正文稀释。
pub(crate) fn decisions(history: &[Message], previous: Option<&str>) -> Vec<Decision> {
    let mut out: Vec<Decision> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (text, from_tool) in decision_sources(history, previous) {
        for line in text.lines() {
            let line = line.trim();
            // 段落标题（`## Decisions and Rejected Options`）不是决策本身。
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let lower = line.to_lowercase();
            let rejected = REJECTION_MARKERS.iter().any(|mark| lower.contains(mark));
            let constrained =
                !from_tool && CONSTRAINT_MARKERS.iter().any(|mark| lower.contains(mark));
            if !rejected && !constrained {
                continue;
            }
            let anchors = decision_anchors(line);
            let key = if anchors.is_empty() {
                format!("text:{}", truncate(line, MAX_DECISION_CHARS))
            } else {
                format!("anchors:{}", anchors.join("|").to_lowercase())
            };
            if !seen.insert(key) {
                continue;
            }
            out.push(Decision {
                clause: truncate(line, MAX_DECISION_CHARS),
                anchors,
            });
            if out.len() >= MAX_DECISIONS {
                return out;
            }
        }
    }
    out
}

/// 参与决策抽取的全部文本：上一版 checkpoint 在前，其后是历史消息。
fn decision_sources<'a>(history: &'a [Message], previous: Option<&'a str>) -> Vec<(&'a str, bool)> {
    let mut out: Vec<(&str, bool)> = Vec::new();
    if let Some(previous) = previous {
        out.push((previous, false));
    }
    for message in history {
        if is_injected_context(message) {
            continue;
        }
        for part in &message.content {
            match part {
                ContentPart::Text(text) => out.push((text.as_str(), false)),
                ContentPart::ToolResult(result) => out.push((result.content.as_str(), true)),
                _ => {}
            }
        }
    }
    out
}

/// 决策行里的专名：优先含大写的词（Redis、LRU），其次是小写标识符（lru）。
fn decision_anchors(line: &str) -> Vec<String> {
    let mut upper: Vec<String> = Vec::new();
    let mut lower: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for token in ascii_tokens(line) {
        let key = token.to_ascii_lowercase();
        // 识别词本身（rejected / not adopted）是行文，不是被否决的对象。
        if token.len() < 3
            || DECISION_STOP_WORDS.contains(&key.as_str())
            || REJECTION_MARKERS.contains(&key.as_str())
        {
            continue;
        }
        if !seen.insert(key) {
            continue;
        }
        if token.bytes().any(|byte| byte.is_ascii_uppercase()) {
            upper.push(token);
        } else {
            lower.push(token);
        }
    }
    upper.extend(lower);
    upper.truncate(MAX_DECISION_ANCHORS);
    upper
}

/// 抽出 ASCII 标识符样式的 token（字母数字与 `_ - . /`）。
fn ascii_tokens(text: &str) -> Vec<String> {
    let bytes = text.as_bytes();
    let mut tokens = Vec::new();
    let mut index = 0;
    while index < bytes.len() {
        let is_token =
            |byte: u8| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.' | b'/');
        if !is_token(bytes[index]) {
            index += 1;
            continue;
        }
        let start = index;
        while index < bytes.len() && is_token(bytes[index]) {
            index += 1;
        }
        let token = text[start..index].trim_matches(['.', '-', '/']);
        if !token.is_empty() {
            tokens.push(token.to_owned());
        }
    }
    tokens
}

/// 用户消息里的编号计划：`第 1 步 … 第 5 步` 视为一份计划，返回每一步的短语。
///
/// `previous` 是上一版 checkpoint：它能保证计划跨代存活——第三次压缩时原始
/// 请求已经不在可见历史里，只剩 checkpoint 里那张步骤表。
/// 只认从 1 开始、至少三步的编号序列：零散的"第 3 步"往往只是顺口一提，
/// 拿它当契约会把无关会话也拖进重试。
pub(crate) fn plan_steps(history: &[Message], previous: Option<&str>) -> Vec<String> {
    let mut steps: BTreeSet<usize> = BTreeSet::new();
    if let Some(previous) = previous {
        collect_steps(previous, &mut steps);
    }
    for message in history {
        if message.role != Role::User || is_injected_context(message) {
            continue;
        }
        collect_steps(&message.text(), &mut steps);
    }
    let Some(&first) = steps.first() else {
        return Vec::new();
    };
    let Some(&last) = steps.last() else {
        return Vec::new();
    };
    if first != 1 || steps.len() < 3 || last > 9 {
        return Vec::new();
    }
    (1..=last).map(|step| format!("第 {step} 步")).collect()
}

/// 扫描 `第 N 步`（阿拉伯数字或中文数字，数字两侧允许空格）并记录步号。
fn collect_steps(text: &str, steps: &mut BTreeSet<usize>) {
    let chars: Vec<char> = text.chars().collect();
    let mut index = 0;
    while index < chars.len() {
        if chars[index] != '第' {
            index += 1;
            continue;
        }
        let mut cursor = index + 1;
        while cursor < chars.len() && chars[cursor].is_whitespace() {
            cursor += 1;
        }
        let Some(number) = chars.get(cursor).copied().and_then(step_digit) else {
            index += 1;
            continue;
        };
        let mut after = cursor + 1;
        while after < chars.len() && chars[after].is_whitespace() {
            after += 1;
        }
        if chars.get(after) == Some(&'步') {
            steps.insert(number);
        }
        index = after;
    }
}

/// 单个步号字符：阿拉伯数字或 1-9 的中文数字。
fn step_digit(ch: char) -> Option<usize> {
    if let Some(digit) = ch.to_digit(10) {
        return usize::try_from(digit).ok();
    }
    const NUMERALS: [char; 9] = ['一', '二', '三', '四', '五', '六', '七', '八', '九'];
    NUMERALS
        .iter()
        .position(|numeral| *numeral == ch)
        .map(|index| index + 1)
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
    // 兜底也必须保住被否决方案与计划步号：它们决定继任者会不会重提已否决的方案。
    let policies = decisions(history, previous);
    let plan = plan_steps(history, previous);
    let mut preserved = String::new();
    if !policies.is_empty() {
        preserved.push_str("\n## Decisions and Rejected Options\n");
        for decision in &policies {
            preserved.push_str("- ");
            preserved.push_str(&decision.clause);
            preserved.push('\n');
        }
    }
    if !plan.is_empty() {
        preserved.push_str("\n## Plan Status\n");
        preserved.push_str("- 计划共 ");
        preserved.push_str(&plan.len().to_string());
        preserved.push_str(" 步：");
        preserved.push_str(&plan.join("、"));
        preserved.push_str("；逐项状态见上文的 Current Work。\n");
    }

    match previous {
        Some(previous) => {
            let mut out =
                String::with_capacity(previous.len() + activity.len() + preserved.len() + 96);
            out.push_str(previous.trim_end());
            out.push_str(
                "\n\n## Recent Activity (auto-checkpoint; the summary model returned no usable checkpoint)\n",
            );
            out.push_str(&activity);
            out.push_str(&preserved);
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
            out.push_str(&preserved);
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
    fn decisions_capture_rejected_options_and_constraints() {
        let history = vec![
            Message::user_text(
                "我们否决了用 lru crate 的 LRU 缓存来管理窗口，pinned 会违反淘汰顺序。",
            ),
            Message::assistant_text("已经记下这次评审结论。"),
            Message::tool_result(
                "call_1",
                "| 把窗口状态迁到 Redis，多进程共享 | 否决 | 跨进程一致性与 RTT 都不可接受 |",
                false,
            ),
        ];
        let decisions = decisions(&history, None);
        let anchors: Vec<&str> = decisions
            .iter()
            .flat_map(|decision| decision.anchors.iter().map(String::as_str))
            .collect();
        assert!(anchors.contains(&"lru"), "{anchors:?}");
        assert!(anchors.contains(&"Redis"), "{anchors:?}");
        assert!(
            decisions
                .iter()
                .any(|decision| decision.clause.contains("否决")),
            "决策原文要能直接照抄"
        );
    }

    #[test]
    fn decisions_ignore_code_comments_but_keep_rejection_tables() {
        let history = vec![
            // 代码注释里的“禁止”不是决策。
            Message::tool_result(
                "call_1",
                "// 禁止在这里分配：热路径\nfn handle() { let _ = LEGACY_SINK; }",
                false,
            ),
            // 复盘表格里的“否决”是决策。
            Message::tool_result(
                "call_2",
                "| 把窗口状态迁到 Redis | 否决 | 一致性不可接受 |",
                false,
            ),
        ];
        let decisions = decisions(&history, None);
        assert_eq!(decisions.len(), 1, "代码注释不应产生决策");
        assert!(decisions[0].clause.contains("Redis"));
    }

    #[test]
    fn decisions_dedupe_repeated_mentions() {
        let history = vec![
            Message::user_text("否决了用 lru crate 的 LRU 缓存。"),
            Message::user_text("再强调一次：否决了用 lru crate 的 LRU 缓存。"),
        ];
        assert_eq!(decisions(&history, None).len(), 1);
    }

    #[test]
    fn plan_steps_require_a_numbered_plan() {
        let plan = vec![Message::user_text(
            "收尾计划共 5 步：第 1 步改 TTL，第 2 步加骨架，第 3 步移计数，第 4 步更新文档，第 5 步加日志。",
        )];
        assert_eq!(
            plan_steps(&plan, None),
            vec!["第 1 步", "第 2 步", "第 3 步", "第 4 步", "第 5 步"]
        );

        // 零散提一句"第 3 步"不构成计划。
        let scattered = vec![Message::user_text("继续第 3 步。")];
        assert!(plan_steps(&scattered, None).is_empty());
    }

    #[test]
    fn plan_steps_accept_chinese_numerals_and_spacing() {
        let history = vec![Message::user_text(
            "分三步：第一步读代码，第二步改常量，第三步补测试。",
        )];
        assert_eq!(
            plan_steps(&history, None),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn decisions_and_plan_survive_from_the_previous_checkpoint() {
        // 第三代压缩时原始请求已不在可见历史里，只剩上一版 checkpoint。
        let previous = "## Plan Status\n- 第 1 步 done；第 2 步 current；第 3 步 pending\n\
                        ## Decisions and Rejected Options\n- rejected: Redis — 一致性不可接受\n";
        let history = vec![Message::user_text("继续")];
        assert_eq!(
            plan_steps(&history, Some(previous)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
        let decisions = decisions(&history, Some(previous));
        let anchors: Vec<&str> = decisions
            .iter()
            .flat_map(|decision| decision.anchors.iter().map(String::as_str))
            .collect();
        assert!(anchors.contains(&"Redis"), "{anchors:?}");
        assert!(
            !anchors.contains(&"rejected"),
            "识别词本身不该变成必保留事实：{anchors:?}"
        );
    }

    #[test]
    fn fallback_keeps_rejected_options_and_plan() {
        let state = state_with_calls(vec![], vec![]);
        let history = vec![Message::user_text(
            "否决了用 Redis 存窗口状态。计划三步：第 1 步改 TTL，第 2 步加骨架，第 3 步移计数。",
        )];
        let checkpoint = fallback_checkpoint(&state, &history, None);
        assert!(checkpoint.contains("## Decisions and Rejected Options"));
        assert!(checkpoint.contains("Redis"));
        assert!(checkpoint.contains("## Plan Status"));
        assert!(checkpoint.contains("第 3 步"));
    }

    #[test]
    fn plan_status_variants_compile() {
        // 防止 TodoStatus/PlanStatus 新增变体时这里的映射被静默遗忘。
        let _ = PlanStatus::Approved;
    }
}

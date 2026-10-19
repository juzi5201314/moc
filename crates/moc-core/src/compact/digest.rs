//! 压缩摘要共用的会话事实：改过的文件、执行过的命令、待办，以及兜底 checkpoint。
//!
//! 这些内容全部由 transcript 和运行状态推导，不依赖模型。摘要提示词用它给模型提供
//! 参考；模型连续给出不合格 checkpoint 时用它兜底，保证一次压缩不会把上下文丢成一句话。

use std::collections::BTreeSet;
use std::collections::HashSet;

use moc_llm::{ContentPart, Message, Role};
use moc_protocol::methods::{PlanStatus, TodoStatus};
use serde_json::Value;

use crate::session::state::SessionState;

use super::replay::is_injected_context;

/// 提示词里最多列出的文件、命令、待办条数；多出的部分优先保留最近的。
const MAX_PROMPT_FILES: usize = 20;
const MAX_PROMPT_COMMANDS: usize = 12;
const MAX_PROMPT_TODOS: usize = 24;
/// 兜底 checkpoint 里单条用户请求和助手输出的截断长度。
const MAX_QUOTE_CHARS: usize = 700;
const MAX_ACTIVITY_CHARS: usize = 400;
/// 兜底 checkpoint 保留的最近用户请求条数。
const MAX_QUOTED_REQUESTS: usize = 5;
/// 参与校验的计划步数上限；为被否决方案等决策留出 `required_facts` 的名额。
const MAX_PLAN_STEPS: usize = 16;

/// 未完成 todo 的渲染结果和计数。
pub(crate) struct TodoLines {
    pub lines: Vec<String>,
    pub completed: usize,
    pub cancelled: usize,
}

/// 本次会话成功写入过的文件；与 `remote::checkpoint_summary` 用同一个「成功」判断。
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

/// 会话里执行过的 bash 命令：最近 `MAX_PROMPT_COMMANDS` 条，去重后按时间顺序返回。
pub(crate) fn commands(state: &SessionState) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut recent = Vec::new();
    // 同一条消息里的并行调用按调用顺序排列，所以要先把整条流收集起来再反转。
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

/// 未完成的 todo 项；已完成和已丢弃的只计数。
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

/// 一条「压缩后必须保留」的决策：原文片段 + 其中必须逐字出现的名字。
///
/// 被否决的方案（`否决了用 lru crate 的 LRU 缓存`、表格里的 `… 迁到 Redis | 否决`）
/// 既不是路径也不是数字，普通锚点永远抓不到。压缩丢掉它，之后的会话就会把它重新
/// 提出来。这里单独抽出来，交给校验强制保留。
pub(crate) struct Decision {
    /// 决策原文片段（截断后），写进提示词让模型照抄。
    pub clause: String,
    /// 决策里的名字或标识符；checkpoint 必须包含（忽略大小写）。
    pub anchors: Vec<String>,
}

/// 识别被否决方案的词：出现在正文或工具结果里都算。
///
/// 英文词来自 checkpoint 契约要求的 `- rejected: <option> — <reason>` 格式，
/// 这样上一版 checkpoint 里的否决项在下一代仍会被要求保留。
const REJECTION_MARKERS: [&str; 5] = ["否决", "不采用", "放弃", "rejected", "not adopted"];
/// 识别硬约束的词：只认用户和助手的原话。
///
/// 工具结果里出现这些词大多来自代码注释（`// 禁止在这里分配`）；当成约束会把每份代码
/// 都变成必须保留的决策，反而把 checkpoint 撑爆。
const CONSTRAINT_MARKERS: [&str; 5] = ["禁止", "不要碰", "绝对不能", "上限", "不能再"];
/// 决策原文片段的最大字符数。
const MAX_DECISION_CHARS: usize = 160;
/// 一条决策最多抽取的名字个数。
const MAX_DECISION_ANCHORS: usize = 3;
/// 最多保留的决策条数。
const MAX_DECISIONS: usize = 12;
/// 抽取名字时忽略的常见英文词：它们是行文，不是被否决的对象。
const DECISION_STOP_WORDS: [&str; 14] = [
    "crate", "crates", "self", "the", "and", "for", "with", "that", "this", "from", "into", "true",
    "false", "null",
];

/// 从历史里抽取决策和被否决的方案；同一条只保留一次。
///
/// `previous` 是上一版 checkpoint：第二次压缩之后，原始请求已不在可见历史里，
/// 被否决的方案只存在于那一版 checkpoint，必须继续参与抽取，否则第三代就会丢。
/// 工具结果只扫「否决」这一类行，不扫整篇内容，所以不会像锚点那样被文件正文稀释。
pub(crate) fn decisions(history: &[Message], previous: Option<&str>) -> Vec<Decision> {
    let mut out: Vec<Decision> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for (text, from_tool) in decision_sources(history, previous) {
        for line in text.lines() {
            let line = line.trim();
            // 段落标题（`## Decisions and Rejected Options`）本身不是决策。
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

/// 参与决策抽取的全部文本：上一版 checkpoint 在前，然后是历史消息。
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

/// 决策行里的名字：优先含大写的词（Redis、LRU），其次是小写标识符（lru）。
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

/// 抽出 ASCII 标识符样式的词（字母、数字与 `_ - . /`）。
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

/// 已批准计划的 markdown 原文；计划以 `<active-plan>` 注入每轮请求，但历史扫描
/// 会跳过注入消息，步骤需要单独从计划文档推导。
pub(crate) fn approved_plan(state: &SessionState) -> Option<&str> {
    state
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == PlanStatus::Approved)
        .map(|plan| plan.markdown.as_str())
}

/// 用户原文里的编号计划：`第 1 步 … 第 5 步` 视为一份计划。
///
/// 来源按「用户原文 → 计划文档 → 上一版 checkpoint」取第一个合法计划。计划文档
/// 是当前计划的事实来源（审批执行时由调用方显式传入待执行计划）；上一版 checkpoint
/// 只是兜底：批准新计划后它可能已经过期，不能压过计划文档。第三次压缩时原始请求
/// 已经不在可见历史里，`previous` 仍能保证计划跨代存活。
/// 只认从 1 开始、至少三步的连续编号序列：零散提一句「第 3 步」往往只是顺口一提，
/// 把它当契约会把无关会话也拖进重试。
pub(crate) fn plan_steps(
    history: &[Message],
    previous: Option<&str>,
    plan: Option<&str>,
) -> Vec<String> {
    let mut from_user = BTreeSet::new();
    for message in history {
        if message.role != Role::User || is_injected_context(message) {
            continue;
        }
        collect_steps(&message.text(), &mut from_user);
    }
    let user = numbered_plan(&from_user);
    if !user.is_empty() {
        return user;
    }
    if let Some(plan) = plan {
        let steps = plan_steps_from_markdown(plan);
        if !steps.is_empty() {
            return steps;
        }
    }
    let mut from_previous = BTreeSet::new();
    if let Some(previous) = previous {
        collect_steps(previous, &mut from_previous);
    }
    numbered_plan(&from_previous)
}

/// 连续编号 `1..=n`（3 ≤ n ≤ [`MAX_PLAN_STEPS`]）的步号才算计划。
fn numbered_plan(steps: &BTreeSet<usize>) -> Vec<String> {
    let Some(&last) = steps.last() else {
        return Vec::new();
    };
    if steps.first() != Some(&1) || steps.len() < 3 || steps.len() != last || last > MAX_PLAN_STEPS
    {
        return Vec::new();
    }
    (1..=last).map(|step| format!("第 {step} 步")).collect()
}

/// 标题对应的步骤小节等级；`Steps` 比 `Plan` 更具体。
#[derive(Clone, Copy, PartialEq, Eq)]
enum StepTier {
    /// 「计划 / 任务 / 工作项」这类泛化标题，作为没有步骤小节时的兜底。
    Plan,
    /// 「步骤 / steps / checklist」这类明确的步骤小节。
    Steps,
}

/// 一个候选小节及其顶格编号列表。
struct StepSection {
    tier: StepTier,
    steps: BTreeSet<usize>,
}

/// 计划文档里的编号步骤。
///
/// 收集所有候选小节，优先取 `Steps` 级中第一份合法计划，没有时取 `Plan` 级中
/// 第一份合法计划。小节内只取顶格的 `1.` `1)` `1、`（含全角 `）`/`．`）列表项；
/// 必须从 1 起连续编号、3 ≤ n ≤ [`MAX_PLAN_STEPS`]。代码围栏和缩进的子列表都不算。
fn plan_steps_from_markdown(markdown: &str) -> Vec<String> {
    let mut sections: Vec<StepSection> = Vec::new();
    let mut current: Option<StepSection> = None;
    let mut fence: Option<(char, usize)> = None;
    for line in markdown.lines() {
        if let Some((marker, open_len)) = fence {
            if fence_closes(line, marker, open_len) {
                fence = None;
            }
            continue;
        }
        if let Some(open) = fence_open(line) {
            fence = Some(open);
            continue;
        }
        if let Some((level, title)) = heading(line) {
            if let Some(section) = current.take() {
                sections.push(section);
            }
            current = step_heading_tier(level, title).map(|tier| StepSection {
                tier,
                steps: BTreeSet::new(),
            });
            continue;
        }
        let Some(section) = current.as_mut() else {
            continue;
        };
        if line.starts_with([' ', '\t']) {
            continue;
        }
        if let Some(number) = list_item_number(line) {
            section.steps.insert(number);
        }
    }
    if let Some(section) = current {
        sections.push(section);
    }
    first_valid(&sections, StepTier::Steps)
        .or_else(|| first_valid(&sections, StepTier::Plan))
        .unwrap_or_default()
}

/// 指定等级中第一份合法计划。
fn first_valid(sections: &[StepSection], tier: StepTier) -> Option<Vec<String>> {
    sections
        .iter()
        .filter(|section| section.tier == tier)
        .find_map(|section| {
            let steps = numbered_plan(&section.steps);
            (!steps.is_empty()).then_some(steps)
        })
}

/// markdown 标题：行首 1–6 个 `#`，其后必须是空格或行尾；返回级别与标题文本。
fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if !(1..=6).contains(&hashes) {
        return None;
    }
    let rest = &line[hashes..];
    if rest.is_empty() {
        return Some((hashes, rest));
    }
    rest.strip_prefix(' ').map(|title| (hashes, title.trim()))
}

/// 标题是否指向计划步骤小节。
///
/// `Steps`（`步骤` / `step` / `steps` / `checklist`）比 `Plan`（`实施` / `实现` /
/// `执行` / `计划` / `任务` / `工作项` / `plan` / `todo`）更具体；H1 是文档标题，
/// 最多算 `Plan`。英文按词匹配，`Explanation` 不会命中 `plan`；`Steps to reproduce`
/// 不是实施计划。
fn step_heading_tier(level: usize, title: &str) -> Option<StepTier> {
    let lower = title.to_ascii_lowercase();
    let words: Vec<&str> = lower
        .split(|ch: char| !ch.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .collect();
    if words
        .iter()
        .any(|word| matches!(*word, "reproduce" | "reproduction" | "repro"))
    {
        return None;
    }
    let specific = title.contains("步骤")
        || words
            .iter()
            .any(|word| matches!(*word, "step" | "steps" | "checklist"));
    if specific {
        return Some(if level > 1 {
            StepTier::Steps
        } else {
            StepTier::Plan
        });
    }
    let general = title.contains("实施")
        || title.contains("实现")
        || title.contains("执行")
        || title.contains("计划")
        || title.contains("任务")
        || title.contains("工作项")
        || words
            .iter()
            .any(|word| matches!(*word, "plan" | "todo" | "todos"));
    general.then_some(StepTier::Plan)
}

/// 顶格列表项的编号：行首数字后紧跟 `.` `)` `、` `）` `．` 之一。
///
/// `.`/`)` 后必须有一个空格（避免把 `1.5 亿` 当列表项）；中文标记 `、` `）` `．`
/// 不要求空格（`1、读取配置` 是常见写法）。
fn list_item_number(line: &str) -> Option<usize> {
    const MARKERS: [char; 5] = ['.', ')', '、', '）', '．'];
    let digits_end = line.find(|ch: char| !ch.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let number = line[..digits_end].parse::<usize>().ok()?;
    let rest = &line[digits_end..];
    let marker = MARKERS.iter().copied().find(|marker| rest.starts_with(*marker))?;
    let after = &rest[marker.len_utf8()..];
    if matches!(marker, '、' | '）' | '．') || after.starts_with(' ') {
        Some(number)
    } else {
        None
    }
}

/// 行首（最多 3 个空格缩进）的围栏标记：字符与连续长度。
///
/// 只认 ``` 或 ~~~ 的 3 个及以上连续字符；4 空格是缩进代码，不是围栏。
fn fence_open(line: &str) -> Option<(char, usize)> {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return None;
    }
    let rest = &line[indent..];
    let marker = rest.chars().next()?;
    if marker != '`' && marker != '~' {
        return None;
    }
    let len = rest.chars().take_while(|ch| *ch == marker).count();
    (len >= 3).then_some((marker, len))
}

/// 闭合围栏：同字符、长度不短于开启、其后只有空白。
///
/// 例如 4 反引号围栏里的 3 反引号行不会提前闭合；`marker` 是 ASCII，`len` 同时也是
/// 字节下标，`rest[len..]` 安全。
fn fence_closes(line: &str, marker: char, open_len: usize) -> bool {
    let indent = line.len() - line.trim_start_matches(' ').len();
    if indent > 3 {
        return false;
    }
    let rest = &line[indent..];
    let len = rest.chars().take_while(|ch| *ch == marker).count();
    len >= open_len && rest[len..].trim().is_empty()
}

/// 扫描 `第 N 步`（阿拉伯数字或中文数字，两侧允许空格）并记录步号。
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
        let Some((number, number_end)) = step_number(&chars, cursor) else {
            index += 1;
            continue;
        };
        let mut after = number_end;
        while after < chars.len() && chars[after].is_whitespace() {
            after += 1;
        }
        if chars.get(after) == Some(&'步') {
            steps.insert(number);
        }
        index = after;
    }
}

/// 步号与它之后的位置：连续的 ASCII 数字（`第 10 步`）或单个中文数字（`第三步`）。
fn step_number(chars: &[char], start: usize) -> Option<(usize, usize)> {
    let mut cursor = start;
    let mut number = 0usize;
    while let Some(digit) = chars.get(cursor).and_then(|ch| ch.to_digit(10)) {
        // 位数超出上限时饱和：`numbered_plan` 会因步号不连续而拒收，不会溢出。
        number = number
            .saturating_mul(10)
            .saturating_add(usize::try_from(digit).ok()?);
        cursor += 1;
    }
    if cursor > start {
        return Some((number, cursor));
    }
    Some((step_digit(*chars.get(start)?)?, start + 1))
}

/// 把单个步号字符转成数字：阿拉伯数字或一到九的中文数字。
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

/// 摘要提示词使用的会话事实块；内容为空时返回 `None`，不注入空标签。
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

/// 模型连续给出不合格 checkpoint 时的兜底：保留上一版 checkpoint 全文，再把最近一轮
/// 的确定性事实追加在后面；没有上一版时按必需段落拼出骨架。
pub(crate) fn fallback_checkpoint(
    state: &SessionState,
    history: &[Message],
    previous: Option<&str>,
    plan: Option<&str>,
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
    // 兜底也要保住被否决的方案和计划步号：它们决定后续会话会不会重提已否决的方案。
    let policies = decisions(history, previous);
    let steps = plan_steps(history, previous, plan);
    let mut preserved = String::new();
    if !policies.is_empty() {
        preserved.push_str("\n## Decisions and Rejected Options\n");
        for decision in &policies {
            preserved.push_str("- ");
            preserved.push_str(&decision.clause);
            preserved.push('\n');
        }
    }
    if !steps.is_empty() {
        preserved.push_str("\n## Plan Status\n");
        preserved.push_str("- 计划共 ");
        preserved.push_str(&steps.len().to_string());
        preserved.push_str(" 步：");
        preserved.push_str(&steps.join("、"));
        preserved.push_str("；每步的状态见上文的 Current Work。\n");
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

/// 按字符截断并补上省略号，不拆开 UTF-8 字符。
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
    use moc_protocol::methods::{SessionMode, SessionPermission, TodoItem, TodoPhase};

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
        let checkpoint = fallback_checkpoint(&state, &history, None, None);
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
        let checkpoint = fallback_checkpoint(&state, &history, Some("## Previous\nkeep me"), None);
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
        let checkpoint = fallback_checkpoint(&state, &history, None, None);
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
            plan_steps(&plan, None, None),
            vec!["第 1 步", "第 2 步", "第 3 步", "第 4 步", "第 5 步"]
        );

        // 零散提一句"第 3 步"不构成计划。
        let scattered = vec![Message::user_text("继续第 3 步。")];
        assert!(plan_steps(&scattered, None, None).is_empty());
    }

    #[test]
    fn plan_steps_accept_chinese_numerals_and_spacing() {
        let history = vec![Message::user_text(
            "分三步：第一步读代码，第二步改常量，第三步补测试。",
        )];
        assert_eq!(
            plan_steps(&history, None, None),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn plan_steps_come_from_the_approved_plan_markdown() {
        let markdown = "# 重构计划\n\n## 实施步骤\n1. 改 A\n2. 改 B\n3. 改 C\n";
        assert_eq!(
            plan_steps(&[], None, Some(markdown)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
        // 没有计划文档时不会有计划步骤。
        assert!(plan_steps(&[], None, None).is_empty());
    }

    #[test]
    fn plan_steps_ignore_numbered_lists_outside_step_sections() {
        let markdown = "## 证据\n1. 第一条日志\n2. 第二条日志\n3. 第三条日志\n";
        assert!(plan_steps(&[], None, Some(markdown)).is_empty());
    }

    #[test]
    fn plan_steps_skip_fenced_code_numbering() {
        let markdown = "## 实施步骤\n```text\n1. a\n2. b\n3. c\n```\n";
        assert!(plan_steps(&[], None, Some(markdown)).is_empty());

        // 四反引号围栏里的三反引号行不闭合围栏，围栏内的编号都不算步骤。
        let nested = "## 实施步骤\n````text\n```txt\n1. e1\n2. e2\n3. e3\n4. e4\n5. e5\n```\n````\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(&[], None, Some(nested)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );

        // 2 空格缩进的围栏仍算围栏。
        let indented =
            "## 实施步骤\n  ```text\n1. e1\n2. e2\n3. e3\n4. e4\n5. e5\n  ```\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(&[], None, Some(indented)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );

        // 字符不匹配的围栏不闭合：`~~~` 关不掉 ``` 围栏。
        let mismatched =
            "## 实施步骤\n```text\n~~~\n1. e1\n2. e2\n3. e3\n4. e4\n5. e5\n```\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(&[], None, Some(mismatched)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn plan_steps_prefer_user_text_over_the_plan_document() {
        let history = vec![Message::user_text(
            "计划三步：第 1 步改 TTL，第 2 步加骨架，第 3 步移计数。",
        )];
        let markdown = "## 实施步骤\n1. a\n2. b\n3. c\n4. d\n";
        assert_eq!(
            plan_steps(&history, None, Some(markdown)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn plan_steps_prefer_the_plan_over_the_previous_checkpoint() {
        // 上一版 checkpoint 的步号可能比刚批准的计划陈旧，不能压过计划文档。
        let previous: String = (1..=5).map(|step| format!("第 {step} 步 done；")).collect();
        let markdown = "## 实施步骤\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(
                &[Message::user_text("继续")],
                Some(&previous),
                Some(markdown)
            ),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn plan_steps_fall_back_when_user_steps_have_gaps() {
        let history = vec![Message::user_text(
            "第 1 步改 A，第 2 步改 B，第 4 步补测试。",
        )];
        let markdown = "## 实施步骤\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(&history, None, Some(markdown)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn plan_steps_prefer_a_step_section_over_an_h1_overview() {
        let markdown = "# 迁移计划\n1. 概览一\n2. 概览二\n3. 概览三\n4. 概览四\n5. 概览五\n\
                        6. 概览六\n7. 概览七\n## 实施步骤\n1. a\n2. b\n3. c\n4. d\n";
        assert_eq!(
            plan_steps(&[], None, Some(markdown)),
            vec!["第 1 步", "第 2 步", "第 3 步", "第 4 步"]
        );
    }

    #[test]
    fn plan_steps_ignore_explanation_and_reproduce_sections() {
        let markdown = "## Explanation\n1. e1\n2. e2\n3. e3\n\
                        ## Steps to reproduce\n1. r1\n2. r2\n3. r3\n\
                        ## 实施步骤\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(&[], None, Some(markdown)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
        assert!(plan_steps(&[], None, Some("## Explanation\n1. e1\n2. e2\n3. e3\n")).is_empty());
    }

    #[test]
    fn plan_steps_fall_back_to_a_plan_heading() {
        let markdown = "## 计划\n1. a\n2. b\n3. c\n";
        assert_eq!(
            plan_steps(&[], None, Some(markdown)),
            vec!["第 1 步", "第 2 步", "第 3 步"]
        );
    }

    #[test]
    fn list_item_number_accepts_cjk_markers_without_spaces() {
        assert_eq!(list_item_number("1、读取配置"), Some(1));
        assert_eq!(list_item_number("12）收尾"), Some(12));
        assert_eq!(list_item_number("3．验证"), Some(3));
        assert_eq!(list_item_number("1.5 亿"), None);
        assert_eq!(list_item_number("1.没有空格"), None);
    }

    #[test]
    fn plan_steps_parse_multi_digit_numbers() {
        let previous: String = (1..=12)
            .map(|step| format!("第 {step} 步 done；"))
            .collect();
        let steps = plan_steps(&[Message::user_text("继续")], Some(&previous), None);
        assert_eq!(steps.len(), 12);
        assert_eq!(steps[11], "第 12 步");
    }

    #[test]
    fn decisions_and_plan_survive_from_the_previous_checkpoint() {
        // 第三代压缩时原始请求已不在可见历史里，只剩上一版 checkpoint。
        let previous = "## Plan Status\n- 第 1 步 done；第 2 步 current；第 3 步 pending\n\
                        ## Decisions and Rejected Options\n- rejected: Redis — 一致性不可接受\n";
        let history = vec![Message::user_text("继续")];
        assert_eq!(
            plan_steps(&history, Some(previous), None),
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
        let checkpoint = fallback_checkpoint(&state, &history, None, None);
        assert!(checkpoint.contains("## Decisions and Rejected Options"));
        assert!(checkpoint.contains("Redis"));
        assert!(checkpoint.contains("## Plan Status"));
        assert!(checkpoint.contains("第 3 步"));
    }

    #[test]
    fn fallback_preserves_the_plan_override_steps() {
        let state = state_with_calls(vec![], vec![]);
        let history = vec![Message::user_text("继续")];
        let plan = "## 实施步骤\n1. a\n2. b\n3. c\n";
        let checkpoint = fallback_checkpoint(&state, &history, None, Some(plan));
        assert!(checkpoint.contains("计划共 3 步"), "{checkpoint}");
        assert!(checkpoint.contains("第 3 步"), "{checkpoint}");
    }
}

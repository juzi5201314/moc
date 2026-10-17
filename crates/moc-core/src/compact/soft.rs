//! 用显式解析的模型生成可更新的历史 checkpoint。
//!
//! 摘要请求是压缩链路里唯一的模型生成点，也是最脆的一环：模型偶尔会把
//! "把对话压成 checkpoint" 理解成"回一句确认"，旧实现照单全收，上下文从此归零。
//! 现在的流程是「生成 → 校验 → 定向重试 → 确定性兜底」：
//!
//! 1. 提示词要求模型按固定段落记录**当前状态**，并把上一版 checkpoint 合并进来；
//! 2. [`guard`] 校验段落完整性与关键锚点覆盖（路径、标识符、数字）；
//! 3. 不合格时把缺口清单回灌，最多重试 [`SOFT_MAX_ATTEMPTS`] 次；
//! 4. 仍不合格时由 [`digest::fallback_checkpoint`] 保住上一版 checkpoint 与最近
//!    一轮的确定性事实，绝不接受"一句话 checkpoint"。

use std::time::Instant;

use moc_llm::{ContentPart, Message, ToolDef};
use moc_protocol::RequestContext;

use super::replay::visible_history_start;
use super::{digest, guard};
use crate::ResolvedModel;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::state::SessionState;

use super::{CompactionBudget, estimate_tokens, messages_tokens, record_compaction};
// 摘要请求也是模型可见提示词，遵循 `system_prompt` 模块的提示词编写规范。
const SUMMARY_SYSTEM: &str = "You are compressing an agent conversation into a checkpoint. Output only the checkpoint; no tool calls, no mention of this request. Use concise engineering English.";
const SUMMARY_CONTRACT: &str = "\
The successor agent sees ONLY this checkpoint plus the most recent turn: anything you leave out is lost.
- Record the current state, not a chronological request log: what is done, what is in flight, what comes next.
- Preserve every file path, command, error string, identifier, number and signature verbatim; quote user constraints and corrections verbatim.
- Keep each decision together with the reason it was made, and mark rejected alternatives as rejected.
- If <previous-summary> is present, merge it: keep every still-relevant fact, drop only what later turns completed or contradicted, and never restart from scratch.
- If <session-digest> is present it is runtime-recorded ground truth; carry its files, commands and open items into the checkpoint.
";
/// checkpoint 的固定段落；同时也是 [`guard::evaluate`] 的完整性依据。
const SUMMARY_SECTIONS: &[&str] = &[
    "Primary Request and Intent",
    "Key Technical Concepts",
    "Files and Code",
    "Errors and Fixes",
    "Pending Jobs",
    "Current Work",
    "Next Step",
    "Critical Context",
];
/// 最大尝试次数：一次生成 + 两次定向修复。
const SOFT_MAX_ATTEMPTS: u32 = 3;
/// 重试提示词比首轮更长，预算里先留出余量，避免修复轮超窗。
const REPAIR_RESERVE_TOKENS: u64 = 512;
/// 校验锚点只看被丢弃历史的最后这么多个字符（约 6k token）：再往前的细节
/// 允许被压缩掉，模型可以用 `history://transcript` 回查。
const ANCHOR_WINDOW_CHARS: usize = 24_000;

pub(crate) struct SoftContext<'a> {
    pub inner: &'a CoreInner,
    pub ctx: &'a RequestContext,
    pub state: &'a SessionState,
    pub config: &'a ResolvedConfig,
    pub model: &'a ResolvedModel,
    pub system: &'a [Message],
    /// 指令消息的静态前缀块数；摘要请求沿用主循环的缓存断点锚点。
    pub static_prefix: &'a [usize],
    pub tools: &'a [ToolDef],
    pub instructions: Option<&'a str>,
    pub budget: CompactionBudget,
}

/// 一次被拒收的 checkpoint 及重试时回灌的缺口。
struct Repair {
    reason: String,
    missing: Vec<String>,
}

/// 摘要采样所需的请求上下文；多次重试共用。
struct SummaryRequest<'a> {
    inner: &'a CoreInner,
    ctx: &'a RequestContext,
    state: &'a SessionState,
    config: &'a ResolvedConfig,
    model: &'a ResolvedModel,
    static_prefix: &'a [usize],
    tools: &'a [ToolDef],
}

impl SummaryRequest<'_> {
    /// 发一次摘要请求，返回模型原始输出。
    async fn sample(&self, messages: Vec<Message>) -> Result<String, CoreError> {
        let request = crate::service::prompt::request::build_request(
            self.model,
            self.config,
            messages,
            self.tools.to_vec(),
            None,
            &self.state.session.id,
            crate::service::prompt::request::CacheAnchors {
                instruction_static: self.static_prefix,
                // 摘要是本次请求特有的最后一条消息，不能落缓存断点。
                trailing_reminders: 1,
            },
        );
        let cancel = self.ctx.cancel();
        let request_started = Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            result = self.inner.client.complete(&self.model.llm, &request) => match result {
                Ok(response) => response,
                Err(error) => {
                    if let moc_llm::LlmError::Guard(violation) = &error
                        && let Some(guard) = self.inner.runtime(&self.state.session.id).await.guard.lock().clone()
                    { guard.stop(violation.clone()); }
                    return Err(error.into());
                }
            },
        };
        crate::service::record_model_usage(
            self.inner,
            &self.state.session.id,
            &self.model.selector,
            response.usage,
            request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
        )
        .await?;
        Ok(response.message.text())
    }
}

pub(crate) async fn compact(
    input: SoftContext<'_>,
) -> Result<moc_protocol::methods::CompactResult, CoreError> {
    let SoftContext {
        inner,
        ctx,
        state,
        config,
        model,
        system,
        static_prefix,
        tools,
        instructions,
        budget,
    } = input;
    let CompactionBudget {
        tokens_before,
        fixed_tokens,
    } = budget;
    let visible_start = visible_history_start(state);
    // `bash_flags` 与 `history` 一一对应：手动命令的输出是原样文本，不参与锚点校验
    // （否则模型会被要求逐字复述一段命令输出，而不是工程事实）。
    let mut history: Vec<Message> = Vec::new();
    let mut bash_flags: Vec<bool> = Vec::new();
    for entry in state.transcript.iter().skip(visible_start) {
        let Some(message) = entry.context_message() else {
            continue;
        };
        history.push(super::replay::repair_tool_history(message, Some(model)));
        bash_flags.push(entry.bash.is_some());
    }
    if history.is_empty() && state.summary.is_none() {
        return Err(CoreError::ContextOverflow(
            "no history is available to summarize".into(),
        ));
    }
    let keep_tokens = config.compaction.keep_recent_tokens as u64;
    let mut start = history.len();
    let mut retained = 0u64;
    while start > 0 {
        let cost = messages_tokens(std::slice::from_ref(&history[start - 1]));
        if start < history.len() && retained.saturating_add(cost) > keep_tokens {
            break;
        }
        retained = retained.saturating_add(cost);
        start -= 1;
    }
    while start > 0 && start < history.len() && history[start].role != moc_llm::Role::User {
        start -= 1;
    }
    if start == 0 {
        start = history
            .iter()
            .rposition(|message| message.role == moc_llm::Role::User)
            .filter(|index| *index > 0)
            .unwrap_or(history.len());
    }
    let retained = messages_tokens(&history[start..]);
    let previous_summary = state.summary.as_ref().map(|(_, summary)| summary.as_str());
    let session_digest = digest::prompt_digest(state);
    let base_prompt = summary_prompt(state, instructions, 0, session_digest.as_deref(), None);
    let fixed_summary_tokens = messages_tokens(system).saturating_add(super::tools_tokens(tools));
    let input_budget = model
        .context_window
        .saturating_sub(model.max_output_tokens as u64)
        .saturating_sub(fixed_summary_tokens)
        .saturating_sub(estimate_tokens(&base_prompt))
        .saturating_sub(REPAIR_RESERVE_TOKENS);
    let (selected_history, omitted_prompts) = fit_summary_history(&history[..start], input_budget);
    // 锚点只取"被丢弃历史"的最近一段：checkpoint 的契约是记录当前状态，
    // 要求它复述几十万 token 之前的具体标识符不现实（实测：最近 20 条消息覆盖率
    // 90%，最近 1000 条约 13%）。上一版 checkpoint 的锚点单独全量参与校验。
    let anchor_start = anchor_window_start(&history[..start], ANCHOR_WINDOW_CHARS);
    let anchors = guard::salient_anchors(
        history[anchor_start..start]
            .iter()
            .zip(&bash_flags[anchor_start..start])
            .filter(|(_, is_bash)| !**is_bash)
            .map(|(message, _)| message),
        previous_summary,
    );
    let summary_request = SummaryRequest {
        inner,
        ctx,
        state,
        config,
        model,
        static_prefix,
        tools,
    };
    let mut best: Option<(String, usize)> = None;
    let mut repair: Option<Repair> = None;
    let mut accepted: Option<String> = None;
    for attempt in 1..=SOFT_MAX_ATTEMPTS {
        let prompt = summary_prompt(
            state,
            instructions,
            omitted_prompts,
            session_digest.as_deref(),
            repair.as_ref(),
        );
        let mut messages = Vec::with_capacity(system.len() + selected_history.len() + 1);
        messages.extend_from_slice(system);
        messages.extend_from_slice(&selected_history);
        messages.push(Message::user_text(prompt));
        let raw = summary_request.sample(messages).await?;
        let cleaned = guard::unwrap_checkpoint(&raw);
        let verdict = guard::evaluate(&cleaned, &anchors, SUMMARY_SECTIONS);
        if matches!(verdict, guard::Verdict::Ok) {
            accepted = Some(cleaned);
            break;
        }
        if !verdict.is_structural() {
            let hits = guard::matching(&anchors, &cleaned);
            let better = match &best {
                None => true,
                Some((_, current)) => hits > *current,
            };
            if better {
                best = Some((cleaned, hits));
            }
        }
        tracing::debug!(
            session = %state.session.id,
            attempt,
            verdict = ?verdict,
            "summary checkpoint rejected"
        );
        let missing = match &verdict {
            guard::Verdict::Coverage { missing, .. } => missing.clone(),
            _ => Vec::new(),
        };
        repair = Some(Repair {
            reason: verdict.reason(),
            missing,
        });
    }
    let summary = match accepted {
        Some(summary) => summary,
        None => match best {
            Some((summary, hits)) if hits >= guard::acceptable_hits(anchors.len()) => summary,
            _ => {
                tracing::warn!(
                    session = %state.session.id,
                    attempts = SOFT_MAX_ATTEMPTS,
                    "summary model produced no usable checkpoint; using deterministic fallback"
                );
                digest::fallback_checkpoint(state, &history, previous_summary)
            }
        },
    };
    let first_kept = state
        .transcript
        .iter()
        .skip(visible_start)
        .filter(|entry| entry.in_context())
        .nth(start)
        .map(|entry| entry.id.clone());
    let tokens_after = fixed_tokens
        .saturating_add(estimate_tokens(&summary))
        .saturating_add(retained);
    record_compaction(
        inner,
        ctx,
        &state.session.id,
        super::Checkpoint {
            method: "soft".to_owned(),
            model: Some(model.selector.clone()),
            summary,
            reminder: None,
            first_kept_id: first_kept,
            tokens_before,
            tokens_after,
            instructions: instructions.map(str::to_owned),
            native: None,
            archive: super::HistoryArchive::Write,
        },
    )
    .await
}

/// 从末尾向前累计字符，返回最近 `budget` 字符窗口的起始下标。
fn anchor_window_start(history: &[Message], budget: usize) -> usize {
    let mut total = 0usize;
    let mut start = history.len();
    while start > 0 {
        let chars = message_chars(&history[start - 1]);
        if start < history.len() && total.saturating_add(chars) > budget {
            break;
        }
        total = total.saturating_add(chars);
        start -= 1;
    }
    start
}

/// 估算一条消息的字符量，不额外分配字符串。
fn message_chars(message: &Message) -> usize {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Text(text) => text.len(),
            ContentPart::ToolCall(call) => call.arguments.len(),
            ContentPart::ToolResult(result) => result.content.len(),
            _ => 0,
        })
        .sum()
}

/// 选取能放进 `budget` 的完整轮次后缀；最近一轮自身超预算时退化为有界摘录。
///
/// 返回 `(交给摘要模型的最近历史, 被省略的完整轮次数)`。预算检查必须覆盖最近的
/// 一轮：本轮工具结果可能整组超预算，原样塞进摘要请求会让该请求必然超窗，
/// 之后没有任何压缩方法能成功。
fn fit_summary_history(history: &[Message], budget: u64) -> (Vec<Message>, usize) {
    if history.is_empty() {
        return (Vec::new(), 0);
    }
    let mut end = history.len();
    let mut start = end;
    let mut remaining = budget;
    let mut kept = false;
    while end > 0 {
        let prompt_start = history[..end]
            .iter()
            .rposition(|message| message.role == moc_llm::Role::User)
            .unwrap_or(0);
        let cost = messages_tokens(&history[prompt_start..end]);
        if cost > remaining {
            if kept {
                break;
            }
            return (oversized_excerpt(&history[prompt_start..end], budget), 1);
        }
        start = prompt_start;
        kept = true;
        remaining = remaining.saturating_sub(cost);
        end = prompt_start;
    }
    let omitted_prompts = history[..start]
        .iter()
        .filter(|message| message.role == moc_llm::Role::User)
        .count();
    (history[start..].to_vec(), omitted_prompts)
}

/// 超预算轮次的有界摘录；按 4 字符 1 token 的估算裁剪序列化文本。
fn oversized_excerpt(group: &[Message], budget: u64) -> Vec<Message> {
    let serialized = serde_json::to_string(group).unwrap_or_default();
    let max_chars = usize::try_from(budget.saturating_mul(4))
        .unwrap_or(usize::MAX)
        .max(128);
    let excerpt: String = serialized.chars().take(max_chars).collect();
    vec![Message::user_text(format!(
        "<oversized-prompt>\n{excerpt}\n</oversized-prompt>"
    ))]
}

fn summary_prompt(
    state: &SessionState,
    instructions: Option<&str>,
    omitted_prompts: usize,
    session_digest: Option<&str>,
    repair: Option<&Repair>,
) -> String {
    let sections = sections_block();
    let mut prompt =
        String::with_capacity(SUMMARY_SYSTEM.len() + SUMMARY_CONTRACT.len() + sections.len() + 256);
    prompt.push_str(SUMMARY_SYSTEM);
    prompt.push_str("\n\n<checkpoint-contract>\n");
    prompt.push_str(SUMMARY_CONTRACT);
    prompt.push_str("</checkpoint-contract>\n\n");
    prompt.push_str(&sections);
    if let Some((_, summary)) = &state.summary {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(summary);
        prompt.push_str("\n</previous-summary>\n\n");
    }
    if let Some(digest) = session_digest {
        prompt.push_str(digest);
        prompt.push_str("\n\n");
    }
    if let Some(plan) = state
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == moc_protocol::methods::PlanStatus::Submitted)
    {
        prompt.push_str("<pending-plan>\n");
        prompt.push_str(&plan.markdown);
        prompt.push_str("\n</pending-plan>\n\n");
    }
    if omitted_prompts > 0 {
        prompt.push_str(&format!(
            "<older-history-omitted>{omitted_prompts} earlier prompts omitted; read history://transcript for exact details.</older-history-omitted>\n"
        ));
    }
    if let Some(instructions) = instructions {
        prompt.push_str("<user-instructions>\n");
        prompt.push_str(instructions);
        prompt.push_str("\n</user-instructions>\n");
    }
    if let Some(repair) = repair {
        prompt.push_str(&repair_block(repair));
    }
    prompt
}

/// 固定段落清单；提示词与校验共用同一份顺序。
fn sections_block() -> String {
    let mut out = String::with_capacity(SUMMARY_SECTIONS.len() * 40 + 32);
    out.push_str("Use exactly these sections:\n");
    for title in SUMMARY_SECTIONS {
        out.push_str("## ");
        out.push_str(title);
        out.push('\n');
    }
    out.push('\n');
    out
}

/// 拒收原因与缺失事实；重试时原样回灌，模型才知道该补什么。
fn repair_block(repair: &Repair) -> String {
    let mut out = String::from("<previous-attempt-rejected>\n");
    out.push_str("Your previous reply was rejected: ");
    out.push_str(&repair.reason);
    out.push_str(
        ".\nRewrite the COMPLETE checkpoint (not a patch) and keep every required section.\n",
    );
    if repair.missing.is_empty() {
        out.push_str("Follow <checkpoint-contract> exactly.\n");
    } else {
        out.push_str("These exact facts must appear verbatim in the checkpoint:\n");
        for item in &repair.missing {
            out.push_str("- ");
            out.push_str(item);
            out.push('\n');
        }
    }
    out.push_str("</previous-attempt-rejected>\n");
    out
}

#[cfg(test)]
fn summary_transcript(
    state: &SessionState,
    history: &[Message],
    start: usize,
    instructions: Option<&str>,
) -> String {
    let mut transcript = summary_prompt(state, instructions, 0, None, None);
    for message in &history[..start] {
        transcript.push_str(&serde_json::to_string(message).expect("Message 可序列化"));
        transcript.push_str("\n\n");
    }
    transcript
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::{PlanStatus, SessionMode, SessionPermission};

    use super::*;
    use crate::session::entries::{Entry, EntryPayload, SessionOrigin};

    #[test]
    fn summary_input_carries_checkpoint_plan_instructions_and_required_sections() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Plan,
            SessionPermission::Full,
            SessionOrigin::root(),
            "plan".into(),
            1,
        );
        let parent = header.id().to_owned();
        let summary = Entry::from_payload(
            EntryPayload::Compaction {
                method: "soft".into(),
                summary: "preserve user correction".into(),
                first_kept_id: None,
                tokens_before: 20,
                tokens_after: 10,
                instructions: None,
                native: None,
                model: Some("p/m".into()),
                reminder: None,
            },
            parent,
            2,
        );
        let plan = Entry::from_payload(
            EntryPayload::Plan {
                plan_id: "plan_12345678".into(),
                title: "Keep plan".into(),
                path: "/tmp/plan.md".into(),
                markdown: "# Keep this plan".into(),
                status: PlanStatus::Submitted,
            },
            summary.id().into(),
            3,
        );
        let state = SessionState::from_entries(vec![header, summary, plan]).unwrap();
        let input = summary_transcript(
            &state,
            &[
                Message::user_text("older request"),
                Message::user_text("recent request"),
            ],
            1,
            Some("preserve plan and decisions"),
        );

        assert!(input.contains("preserve user correction"));
        assert!(input.contains("<pending-plan>\n# Keep this plan\n</pending-plan>"));
        assert!(
            input
                .contains("<user-instructions>\npreserve plan and decisions\n</user-instructions>")
        );
        assert!(input.contains("## Primary Request and Intent\n## Key Technical Concepts"));
        assert!(input.contains("older request"));
        assert!(!input.contains("recent request"));
    }

    #[test]
    fn summary_prompt_states_the_merge_contract_and_digest() {
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
        let state = SessionState::from_entries(vec![header]).unwrap();
        let prompt = summary_prompt(
            &state,
            None,
            0,
            Some("<session-digest>\n- a.rs\n</session-digest>"),
            None,
        );
        assert!(prompt.contains("<checkpoint-contract>"));
        assert!(prompt.contains("never restart from scratch"));
        assert!(prompt.contains("<session-digest>\n- a.rs\n</session-digest>"));
        assert!(prompt.contains("## Critical Context"));
    }

    #[test]
    fn repair_block_lists_missing_anchors() {
        let block = repair_block(&Repair {
            reason: "your reply preserves only 12 of the 40 required concrete facts".into(),
            missing: vec!["45_000".into(), "crates/ingest/src/window.rs".into()],
        });
        assert!(block.starts_with("<previous-attempt-rejected>"));
        assert!(block.contains("- 45_000"));
        assert!(block.contains("- crates/ingest/src/window.rs"));
        assert!(block.ends_with("</previous-attempt-rejected>\n"));
    }

    #[test]
    fn anchor_window_keeps_only_the_recent_tail() {
        let history: Vec<Message> = (0..10)
            .map(|index| Message::user_text(format!("{index:0>100}")))
            .collect();
        assert_eq!(anchor_window_start(&history, 350), 7);
        assert_eq!(anchor_window_start(&history, 10_000), 0);
    }

    #[test]
    fn anchor_window_keeps_the_newest_message_even_if_oversized() {
        let history = vec![
            Message::user_text("old"),
            Message::user_text("x".repeat(5_000)),
        ];
        assert_eq!(anchor_window_start(&history, 100), 1);
    }

    #[test]
    fn summary_history_keeps_complete_recent_turns_within_budget() {
        let history = vec![
            Message::user_text("old user"),
            Message::assistant_text("old assistant"),
            Message::user_text("recent user"),
            Message::assistant_text("recent assistant"),
        ];
        let budget = messages_tokens(&history[2..]);
        let (selected, omitted) = fit_summary_history(&history, budget);
        assert_eq!(selected.len(), 2);
        assert_eq!(selected[0].text(), "recent user");
        assert_eq!(omitted, 1);
    }

    #[test]
    fn summary_history_stops_at_the_first_group_that_does_not_fit() {
        let history = vec![
            Message::user_text("older ".repeat(400)),
            Message::user_text("recent"),
        ];
        let budget = messages_tokens(&history[1..]);
        let (selected, omitted) = fit_summary_history(&history, budget);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].text(), "recent");
        assert_eq!(omitted, 1);
    }

    #[test]
    fn oversized_recent_prompt_falls_back_to_a_bounded_excerpt() {
        // 最近一轮（含本轮工具结果）自身超预算：整组塞进摘要请求必然超窗。
        let history = vec![Message::user_text("x".repeat(4_000))];
        let budget = 50;
        let (selected, omitted) = fit_summary_history(&history, budget);
        assert_eq!(selected.len(), 1);
        assert_eq!(omitted, 1);
        let text = selected[0].text();
        assert!(text.starts_with("<oversized-prompt>"), "{text}");
        assert!(text.ends_with("</oversized-prompt>"), "{text}");
        assert!(
            messages_tokens(&selected) <= budget + 32,
            "摘录必须落在预算内，实际 {} token",
            messages_tokens(&selected)
        );
    }

    #[test]
    fn oversized_recent_group_keeps_the_excerpt_of_the_whole_turn() {
        let history = vec![
            Message::user_text("historic prompt ".repeat(50)),
            Message::user_text("current"),
            Message::tool_result("call_1", "y".repeat(8_000), false),
        ];
        let budget = 40;
        let (selected, omitted) = fit_summary_history(&history, budget);
        assert_eq!(selected.len(), 1);
        assert_eq!(omitted, 1);
        let text = selected[0].text();
        assert!(text.starts_with("<oversized-prompt>"), "{text}");
        assert!(text.contains("current"), "摘录必须来自本轮而不是更早的轮次");
        assert!(messages_tokens(&selected) <= budget + 32);
    }
}

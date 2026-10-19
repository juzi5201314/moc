//! soft 压缩：用显式指定的模型生成可迭代更新的历史 checkpoint。
//!
//! 摘要请求是压缩链路里唯一要模型生成的地方，也是最容易出错的一环：模型偶尔会把
//! 「把对话压成 checkpoint」理解成「回一句确认」。流程分四步：
//!
//! 1. 提示词要求模型按固定段落记录**当前状态**，并把上一版 checkpoint 合并进来；
//! 2. [`guard`] 校验段落完整性、必保留短语（被否决的方案名、计划步号）和关键锚点
//!    （路径、标识符、数字）是否齐全；
//! 3. 不合格时把缺了什么列给模型，最多重试 [`SOFT_MAX_ATTEMPTS`] 次；
//! 4. 仍不合格就用 [`digest::fallback_checkpoint`]：保住上一版 checkpoint 和最近一轮
//!    的确定性事实（含被否决方案和计划状态），绝不接受一句话的 checkpoint。

use std::collections::HashSet;
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
// 摘要请求也是模型可见文本，按 `system_prompt` 模块的提示词编写规范编写。
const SUMMARY_SYSTEM: &str = "You are compressing an agent conversation into a checkpoint. Output only the checkpoint; no tool calls, no mention of this request. Use concise engineering English.";
const SUMMARY_CONTRACT: &str = "\
The successor agent sees ONLY this checkpoint plus the most recent turn: anything you leave out is lost.
- Record the current state, not a chronological request log: what is done, what is in flight, what comes next.
- Preserve every file path, command, error string, identifier, number and signature verbatim; quote user constraints and corrections verbatim.
- Keep each decision together with the reason it was made, and mark rejected alternatives as rejected.
- Every rejected alternative goes into `## Decisions and Rejected Options` as `- rejected: <option> — <reason>`; never drop one, never propose it again without new evidence.
- If the conversation has a numbered plan, or an `<active-plan>` block, restate it in `## Plan Status` with every step marked done / current / pending, and name the current step number.
- A `<compaction-summary>` block in the conversation is the prior checkpoint: merge it — keep every still-relevant fact, drop only what later turns completed or contradicted, and never restart from scratch.
- If a `<previous-summary>` block is present, merge it the same way.
- If <session-digest> is present it is runtime-recorded ground truth; carry its files, commands and open items into the checkpoint.
- If <must-preserve> is present, each listed item must appear verbatim in the checkpoint.
";
/// checkpoint 的固定段落，也是 [`guard::evaluate`] 校验完整性的依据。
const SUMMARY_SECTIONS: &[&str] = &[
    "Primary Request and Intent",
    "Key Technical Concepts",
    "Decisions and Rejected Options",
    "Files and Code",
    "Errors and Fixes",
    "Pending Jobs",
    "Plan Status",
    "Current Work",
    "Next Step",
    "Critical Context",
];
/// 最多尝试次数：一次生成 + 两次带缺口清单的修复。
const SOFT_MAX_ATTEMPTS: u32 = 3;
/// 修复提示比首轮长，预算里先留出这部分，免得修复轮自己超窗。
const REPAIR_RESERVE_TOKENS: u64 = 512;
/// 摘要请求的输出上界：checkpoint 只需几千 token，沿用模型的完整输出上限会从
/// 输入预算里扣掉几十万 token，摘要模型只看得到历史末尾的一小段。
const SUMMARY_MAX_OUTPUT_TOKENS: u32 = 32_768;

/// 摘要请求实际申请的输出上限；模型自身上限更小时取模型值。
fn summary_output_tokens(model_max: u32) -> u32 {
    model_max.min(SUMMARY_MAX_OUTPUT_TOKENS)
}

/// 摘要消息（含 system）可用的 token 预算：窗口除去输出上界、工具定义、
/// 提示词与修复余量。
fn summary_input_budget(
    context_window: u64,
    tools_tokens: u64,
    prompt_tokens: u64,
    output_tokens: u32,
) -> u64 {
    context_window
        .saturating_sub(u64::from(output_tokens))
        .saturating_sub(tools_tokens)
        .saturating_sub(prompt_tokens)
        .saturating_sub(REPAIR_RESERVE_TOKENS)
}

/// 校验锚点只看被丢弃历史的末尾这么多个字符（约 6k token）；更早的细节允许被压掉，
/// 模型可以用 `history://transcript` 回查。
const ANCHOR_WINDOW_CHARS: usize = 24_000;

pub(crate) struct SoftContext<'a> {
    pub inner: &'a CoreInner,
    pub ctx: &'a RequestContext,
    pub state: &'a SessionState,
    pub config: &'a ResolvedConfig,
    pub model: &'a ResolvedModel,
    pub system: &'a [Message],
    /// 指令消息开头的静态块数；摘要请求沿用主循环的缓存断点位置。
    pub static_prefix: &'a [usize],
    pub tools: &'a [ToolDef],
    pub instructions: Option<&'a str>,
    /// 审批执行时传入的待执行计划 markdown；`None` 时使用已批准计划。
    pub plan: Option<&'a str>,
    pub budget: CompactionBudget,
}

/// 一次被拒收的 checkpoint；重试时把缺口连同原因一起列给模型。
struct Repair {
    reason: String,
    missing: Vec<String>,
}

/// 发一次摘要请求所需的上下文，多次重试共用。
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
    /// 发一次摘要请求，返回模型的原始输出。
    async fn sample(&self, messages: Vec<Message>) -> Result<String, CoreError> {
        let mut request = crate::service::prompt::request::build_request(
            self.model,
            self.config,
            messages,
            self.tools.to_vec(),
            None,
            &self.state.session.id,
            crate::service::prompt::request::CacheAnchors {
                instruction_static: self.static_prefix,
                // 摘要是本次请求特有的最后一条消息，不能放缓存断点。
                trailing_reminders: 1,
            },
        );
        // checkpoint 只有几千 token：按模型完整上限申请输出会从预算里扣掉几十万 token。
        request.max_output_tokens = Some(summary_output_tokens(self.model.max_output_tokens));
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
        plan,
        budget,
    } = input;
    let CompactionBudget {
        tokens_before,
        fixed_tokens,
    } = budget;
    let visible_start = visible_history_start(state);
    // `bash_flags` 与 `history` 一一对应：手动命令的输出是原文，不参与锚点校验，
    // 否则会要求模型逐字复述一段命令输出，而不是记录工程事实。
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
    // 审批执行（ExecuteCompacted）时计划仍是 Submitted，必须用显式传入的 markdown；
    // 它优先于任何旧的 Approved 计划。
    let plan = plan.or_else(|| digest::approved_plan(state));
    let session_digest = digest::prompt_digest(state);
    // 上一版 checkpoint 在 `native_input` 为空时由 `synthetic_messages` 注入成
    // `<compaction-summary>` 消息；只有原生压缩载荷不在消息视图里，才需要提示词补一份。
    let previous_in_view = state.native_input.is_none();
    // 被否决的方案和计划步号决定「压缩后还能不能接着做」：它们不属于锚点
    // （方案名不是路径或数字，步号不是标识符），必须单独抽出来逐字校验。
    let decisions = digest::decisions(&history[..start], previous_summary);
    let plan_steps = digest::plan_steps(&history[..start], previous_summary, plan);
    let required = required_facts(&decisions, &plan_steps);
    let must_preserve = must_preserve_block(&decisions, &plan_steps);
    let base_prompt = summary_prompt(
        state,
        instructions,
        0,
        session_digest.as_deref(),
        must_preserve.as_deref(),
        None,
        previous_in_view,
    );
    // 摘要请求复用主循环的消息前缀（system + 注入消息）：从第一条消息起就与主循环
    // 逐条相同，prompt cache 才有机会命中；预算只再装被这次压缩丢弃的历史。
    let prefix = super::replay::synthetic_messages(state, Some(system));
    let input_budget = summary_input_budget(
        model.context_window,
        super::tools_tokens(tools),
        estimate_tokens(&base_prompt),
        summary_output_tokens(model.max_output_tokens),
    );
    let (selected_history, omitted_prompts) =
        fit_summary_messages(&prefix, &history[..start], input_budget);
    // 锚点只取被丢弃历史的最近一段：checkpoint 的职责是记录当前状态，要求它复述几十万
    // token 之前的标识符不现实（实测最近 20 条消息覆盖率 90%，最近 1000 条只有 13%）。
    // 上一版 checkpoint 的锚点单独全量校验。
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
            must_preserve.as_deref(),
            repair.as_ref(),
            previous_in_view,
        );
        let mut messages = selected_history.clone();
        messages.push(Message::user_text(prompt));
        let raw = summary_request.sample(messages).await?;
        let cleaned = guard::unwrap_checkpoint(&raw);
        let verdict = guard::evaluate(&cleaned, &anchors, &required, SUMMARY_SECTIONS);
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
                best = Some((cleaned.clone(), hits));
            }
        }
        tracing::debug!(
            session = %state.session.id,
            attempt,
            verdict = ?verdict,
            "summary checkpoint rejected"
        );
        // 修复提示同时列出缺的锚点和缺的必保留短语，一次重试补齐两类。
        let mut missing = match &verdict {
            guard::Verdict::Coverage { missing, .. } => missing.clone(),
            guard::Verdict::Required(missing) => missing.clone(),
            _ => Vec::new(),
        };
        for phrase in guard::missing_required(&required, &cleaned) {
            if !missing.contains(&phrase) {
                missing.push(phrase);
            }
        }
        repair = Some(Repair {
            reason: verdict.reason(),
            missing,
        });
    }
    let summary = match accepted {
        Some(summary) => summary,
        None => match best {
            // 采纳最好的那次尝试：锚点覆盖要达标，必保留短语最多缺四分之一。
            Some((summary, hits))
                if hits >= guard::acceptable_hits(anchors.len())
                    && guard::missing_required(&required, &summary).len() * 4 <= required.len() =>
            {
                summary
            }
            _ => {
                tracing::warn!(
                    session = %state.session.id,
                    attempts = SOFT_MAX_ATTEMPTS,
                    "summary model produced no usable checkpoint; using deterministic fallback"
                );
                digest::fallback_checkpoint(state, &history, previous_summary, plan)
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

/// 从末尾往前累计字符数，返回最近 `budget` 个字符窗口的起始下标。
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

/// 估算一条消息有多少字符，不额外分配字符串。
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

/// 摘要请求的消息：主循环前缀原样保留，随后是能放进预算的最近轮次。
///
/// 返回 `(消息, 被丢弃的用户轮次数)`。前缀自身超预算（例如超大计划文档）时只发前缀：
/// 丢掉前缀比超窗更糟，主循环在同一份状态下也同样会超窗。预算检查必须覆盖最近的一轮：
/// 本轮的工具结果可能整组超预算，原样放进摘要请求必然超窗，之后任何压缩方法都会失败。
fn fit_summary_messages(
    prefix: &[Message],
    history: &[Message],
    budget: u64,
) -> (Vec<Message>, usize) {
    let head = messages_tokens(prefix);
    if head > budget {
        // 整个被丢弃的历史都不在请求里，提示词必须如实说明这一点。
        return (prefix.to_vec(), user_prompts(history));
    }
    let mut view = Vec::with_capacity(prefix.len() + history.len());
    view.extend_from_slice(prefix);
    let mut remaining = budget - head;
    let mut end = history.len();
    let mut start = end;
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
            view.extend(oversized_excerpt(&history[prompt_start..end], remaining));
            return (view, user_prompts(&history[..prompt_start]));
        }
        start = prompt_start;
        kept = true;
        remaining = remaining.saturating_sub(cost);
        end = prompt_start;
    }
    view.extend_from_slice(&history[start..]);
    (view, user_prompts(&history[..start]))
}

/// 被丢弃历史里的用户轮次数；提示词用它说明还有多少内容不在请求里。
fn user_prompts(history: &[Message]) -> usize {
    history
        .iter()
        .filter(|message| message.role == moc_llm::Role::User)
        .count()
}

/// 超预算轮次的有界摘录：把序列化文本按 4 字符 1 token 的估算裁剪。
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
    must_preserve: Option<&str>,
    repair: Option<&Repair>,
    previous_in_view: bool,
) -> String {
    let sections = sections_block();
    let mut prompt =
        String::with_capacity(SUMMARY_SYSTEM.len() + SUMMARY_CONTRACT.len() + sections.len() + 256);
    prompt.push_str(SUMMARY_SYSTEM);
    prompt.push_str("\n\n<checkpoint-contract>\n");
    prompt.push_str(SUMMARY_CONTRACT);
    prompt.push_str("</checkpoint-contract>\n\n");
    prompt.push_str(&sections);
    // 旧 checkpoint 已经在消息前缀里（`<compaction-summary>`）时不再重复一遍：
    // 重复会占预算，也让模型看到两份不一致的副本。
    if !previous_in_view && let Some((_, summary)) = &state.summary {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(summary);
        prompt.push_str("\n</previous-summary>\n\n");
    }
    if let Some(digest) = session_digest {
        prompt.push_str(digest);
        prompt.push_str("\n\n");
    }
    if let Some(must_preserve) = must_preserve {
        prompt.push_str(must_preserve);
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

/// 必保留短语：计划步号在前（决定「做到第几步了」），后面是被否决方案的名字。
fn required_facts(decisions: &[digest::Decision], plan_steps: &[String]) -> Vec<String> {
    /// 校验结果和提示词都不能无限长：超过这个条数后只保留先出现的。
    const MAX_REQUIRED_FACTS: usize = 32;
    let mut out: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    for phrase in plan_steps.iter().chain(
        decisions
            .iter()
            .flat_map(|decision| decision.anchors.iter()),
    ) {
        if !seen.insert(phrase.to_lowercase()) {
            continue;
        }
        out.push(phrase.clone());
        if out.len() >= MAX_REQUIRED_FACTS {
            break;
        }
    }
    out
}

/// 提示词里的 `<must-preserve>`：一份模型可以直接照抄的清单。
fn must_preserve_block(decisions: &[digest::Decision], plan_steps: &[String]) -> Option<String> {
    if decisions.is_empty() && plan_steps.is_empty() {
        return None;
    }
    let mut out = String::from("<must-preserve>\n");
    if !plan_steps.is_empty() {
        out.push_str(
            "Numbered plan steps (each must appear in `## Plan Status` with done/current/pending):\n",
        );
        for step in plan_steps {
            out.push_str("- ");
            out.push_str(step);
            out.push('\n');
        }
    }
    if !decisions.is_empty() {
        out.push_str(
            "Rejected options and hard constraints (each must appear verbatim in `## Decisions and Rejected Options`):\n",
        );
        for decision in decisions {
            out.push_str("- ");
            out.push_str(&decision.clause);
            out.push('\n');
        }
    }
    out.push_str("</must-preserve>");
    Some(out)
}

/// 固定段落清单；提示词和校验用同一份顺序。
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

/// 拒收原因和缺失的事实，重试时原样发给模型，它才知道该补什么。
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
    let mut transcript = summary_prompt(state, instructions, 0, None, None, None, false);
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
            None,
            true,
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
        let (selected, omitted) = fit_summary_messages(&[], &history, budget);
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
        let (selected, omitted) = fit_summary_messages(&[], &history, budget);
        assert_eq!(selected.len(), 1);
        assert_eq!(selected[0].text(), "recent");
        assert_eq!(omitted, 1);
    }

    #[test]
    fn oversized_recent_prompt_falls_back_to_a_bounded_excerpt() {
        // 最近一轮（含本轮工具结果）自己就超预算：整组放进摘要请求必然超窗。
        let history = vec![Message::user_text("x".repeat(4_000))];
        let budget = 50;
        let (selected, omitted) = fit_summary_messages(&[], &history, budget);
        assert_eq!(selected.len(), 1);
        assert_eq!(omitted, 0, "摘录代表的是当前这一轮，不是被省略的更早轮次");
        let text = selected[0].text();
        assert!(text.starts_with("<oversized-prompt>"), "{text}");
        assert!(text.ends_with("</oversized-prompt>"), "{text}");
        assert!(
            messages_tokens(&selected) <= budget + 32,
            "摘录必须在预算内，实际 {} token",
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
        let (selected, omitted) = fit_summary_messages(&[], &history, budget);
        assert_eq!(selected.len(), 1);
        assert_eq!(omitted, 1);
        let text = selected[0].text();
        assert!(text.starts_with("<oversized-prompt>"), "{text}");
        assert!(text.contains("current"), "摘录必须来自本轮而不是更早的轮次");
        assert!(messages_tokens(&selected) <= budget + 32);
    }

    #[test]
    fn summary_budget_covers_a_full_context_window_history() {
        // 500k 窗口的 85% 触发点上，被丢弃的历史仍必须放得下。
        let budget = summary_input_budget(500_000, 20_000, 5_000, summary_output_tokens(384_000));
        assert!(budget >= 435_000, "输入预算只剩 {budget} token");
    }

    #[test]
    fn summary_view_keeps_the_main_loop_prefix_when_it_fits() {
        let prefix = vec![
            Message::user_text("system"),
            Message::user_text("<compaction-summary>prior</compaction-summary>"),
        ];
        let history = vec![
            Message::user_text("u1"),
            Message::assistant_text("a1"),
            Message::user_text("u2"),
        ];
        let budget = messages_tokens(&prefix) + messages_tokens(&history);
        let (view, omitted) = fit_summary_messages(&prefix, &history, budget);
        assert_eq!(view.len(), 5);
        assert_eq!(view[0].text(), "system");
        assert_eq!(view[2].text(), "u1");
        assert_eq!(view[4].text(), "u2");
        assert_eq!(omitted, 0);
    }

    #[test]
    fn summary_view_drops_oldest_turns_when_over_budget() {
        let prefix = vec![
            Message::user_text("system"),
            Message::user_text("<compaction-summary>prior</compaction-summary>"),
        ];
        let history = vec![
            Message::user_text("u1".repeat(40)),
            Message::assistant_text("a1"),
            Message::user_text("u2"),
        ];
        let budget = messages_tokens(&prefix) + messages_tokens(&history[2..]);
        let (view, omitted) = fit_summary_messages(&prefix, &history, budget);
        assert_eq!(view.len(), 3);
        assert_eq!(view[0].text(), "system");
        assert_eq!(view[2].text(), "u2");
        assert_eq!(omitted, 1, "被省略的只有第一轮用户请求");
    }

    #[test]
    fn summary_view_keeps_the_prefix_when_it_exceeds_the_budget() {
        // 超大计划文档撑爆预算：前缀不能丢，否则缓存前缀与主循环分叉。
        let prefix = vec![
            Message::user_text("system"),
            Message::user_text("x".repeat(4_000)),
        ];
        let history = vec![Message::user_text("u1")];
        let (view, omitted) = fit_summary_messages(&prefix, &history, 10);
        assert_eq!(view.len(), prefix.len());
        assert_eq!(view[0].text(), "system");
        assert_eq!(omitted, 1, "整段历史都没进请求，提示词必须如实说明");
    }

    #[test]
    fn summary_prompt_omits_a_replayed_previous_summary() {
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
        let summary = Entry::from_payload(
            EntryPayload::Compaction {
                method: "soft".into(),
                summary: "PRIOR CHECKPOINT".into(),
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
        let state = SessionState::from_entries(vec![header, summary]).unwrap();

        let replayed = summary_prompt(&state, None, 0, None, None, None, true);
        assert!(
            !replayed.contains("<previous-summary>\nPRIOR CHECKPOINT"),
            "上一版 checkpoint 已在消息前缀里，不能再占预算：{replayed}"
        );
        let isolated = summary_prompt(&state, None, 0, None, None, None, false);
        assert!(
            isolated.contains("<previous-summary>\nPRIOR CHECKPOINT\n</previous-summary>"),
            "原生压缩载荷不在消息视图里时必须补上上一版 checkpoint：{isolated}"
        );
    }
}

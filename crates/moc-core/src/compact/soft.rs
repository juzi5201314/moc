//! 用轻量模型生成可更新的历史 checkpoint。

use moc_llm::{ChatRequest, Message, ToolDef};
use moc_protocol::RequestContext;

use super::replay::visible_history_start;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::state::SessionState;

use super::{CompactionBudget, estimate_tokens, messages_tokens, record_compaction};

const SUMMARY_SYSTEM: &str = "You are compressing an agent conversation into a checkpoint. Output only the checkpoint text; do not call tools; do not mention this compression request. Use concise engineering English. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures and code fragments. Record user corrections verbatim.";
const SUMMARY_SECTIONS: &str = "Summarize the older conversation as a checkpoint using these exact sections:\n\
## Primary Request and Intent\n## Key Technical Concepts\n## Files and Code\n\
## Errors and Fixes\n## Pending Jobs\n## Current Work\n## Next Step\n\
## Critical Context\n\n";

pub(crate) struct SoftContext<'a> {
    pub inner: &'a CoreInner,
    pub ctx: &'a RequestContext,
    pub state: &'a SessionState,
    pub config: &'a ResolvedConfig,
    pub system: &'a Message,
    pub tools: &'a [ToolDef],
    pub instructions: Option<&'a str>,
    pub budget: CompactionBudget,
}

pub(crate) async fn compact(
    input: SoftContext<'_>,
) -> Result<moc_protocol::methods::CompactResult, CoreError> {
    let SoftContext {
        inner,
        ctx,
        state,
        config,
        system,
        tools,
        instructions,
        budget,
    } = input;
    let CompactionBudget {
        tokens_before,
        fixed_tokens,
    } = budget;
    let visible_start = visible_history_start(state);
    let history: Vec<Message> = state
        .transcript
        .iter()
        .skip(visible_start)
        .map(|entry| entry.message.clone())
        .collect();
    if history.is_empty() && state.summary.is_none() {
        return Err(CoreError::ContextOverflow("没有可摘要的历史".into()));
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
    let role_model = if config.roles.smol == "inherit" {
        config.resolve_model(&state.model)?
    } else {
        config.resolve_role("smol")?
    };
    let same_model = role_model.selector == state.model;
    let mut summary_history = history[..start].to_vec();
    if !same_model {
        for message in &mut summary_history {
            super::images::strip_images(message);
        }
    }
    let prompt = summary_prompt(state, instructions, 0);
    let fixed_summary_tokens = if same_model {
        messages_tokens(std::slice::from_ref(system)).saturating_add(super::tools_tokens(tools))
    } else {
        estimate_tokens(SUMMARY_SYSTEM)
    };
    let prompt_tokens = estimate_tokens(&prompt);
    let input_budget = role_model
        .context_window
        .saturating_sub(role_model.max_output_tokens as u64)
        .saturating_sub(fixed_summary_tokens)
        .saturating_sub(prompt_tokens);
    let (selected_history, omitted_turns) = fit_summary_history(&summary_history, input_budget);
    let prompt = summary_prompt(state, instructions, omitted_turns);
    let messages = if same_model {
        let mut messages = Vec::with_capacity(selected_history.len() + 2);
        messages.push(system.clone());
        messages.extend(selected_history);
        messages.push(Message::user_text(prompt));
        messages
    } else {
        vec![
            Message::system(SUMMARY_SYSTEM),
            Message::user_text(render_summary_transcript(prompt, &selected_history)),
        ]
    };
    let mut request = if same_model {
        crate::service::turn::request::build_request(
            &role_model,
            config,
            messages,
            tools.to_vec(),
            None,
            &state.session.id,
        )
    } else {
        ChatRequest {
            messages,
            max_output_tokens: Some(role_model.max_output_tokens),
            ..ChatRequest::default()
        }
    };
    if !same_model {
        request.options.cache = None;
    }
    let cancel = ctx.cancel();
    let _permit = inner.provider_permit(&role_model, &cancel).await?;
    let response = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        result = inner.client.complete(&role_model.llm, &request) => result?,
    };
    let summary = response.message.text().trim().to_owned();
    if summary.is_empty() {
        return Err(CoreError::ContextOverflow(
            "摘要模型返回空 checkpoint".into(),
        ));
    }
    let first_kept = state
        .transcript
        .get(visible_start + start)
        .map(|entry| entry.id.clone());
    let tokens_after = fixed_tokens
        .saturating_add(estimate_tokens(&summary))
        .saturating_add(retained);
    record_compaction(
        inner,
        ctx,
        &state.session.id,
        super::Checkpoint {
            method: "soft",
            summary,
            first_kept_id: first_kept,
            tokens_before,
            tokens_after,
            instructions: instructions.map(str::to_owned),
            native: None,
        },
    )
    .await
}

fn fit_summary_history(history: &[Message], budget: u64) -> (Vec<Message>, usize) {
    if history.is_empty() {
        return (Vec::new(), 0);
    }
    let mut end = history.len();
    let mut start = end;
    let mut remaining = budget;
    while end > 0 {
        let turn_start = history[..end]
            .iter()
            .rposition(|message| message.role == moc_llm::Role::User)
            .unwrap_or(0);
        let cost = messages_tokens(&history[turn_start..end]);
        if start != history.len() && cost > remaining {
            break;
        }
        start = turn_start;
        remaining = remaining.saturating_sub(cost);
        end = turn_start;
    }
    if start == history.len() {
        let message = history
            .last()
            .cloned()
            .unwrap_or_else(|| Message::user_text(""));
        let serialized = serde_json::to_string(&message).unwrap_or_default();
        let max_chars = usize::try_from(budget.saturating_mul(4))
            .unwrap_or(usize::MAX)
            .max(128);
        let excerpt: String = serialized.chars().take(max_chars).collect();
        return (
            vec![Message::user_text(format!(
                "<oversized-turn>\n{excerpt}\n</oversized-turn>"
            ))],
            1,
        );
    }
    let omitted_turns = history[..start]
        .iter()
        .filter(|message| message.role == moc_llm::Role::User)
        .count();
    (history[start..].to_vec(), omitted_turns)
}

fn summary_prompt(
    state: &SessionState,
    instructions: Option<&str>,
    omitted_turns: usize,
) -> String {
    let mut prompt = String::with_capacity(SUMMARY_SYSTEM.len() + SUMMARY_SECTIONS.len() + 64);
    prompt.push_str(SUMMARY_SYSTEM);
    prompt.push_str("\n\n");
    prompt.push_str(SUMMARY_SECTIONS);
    if let Some((_, summary)) = &state.summary {
        prompt.push_str("<previous-summary>\n");
        prompt.push_str(summary);
        prompt.push_str("\n</previous-summary>\n\n");
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
    if omitted_turns > 0 {
        prompt.push_str(&format!(
            "<older-history-omitted>已省略 {omitted_turns} 个更早完整轮次；必要时通过历史资源回读。</older-history-omitted>\n"
        ));
    }
    if let Some(instructions) = instructions {
        prompt.push_str("<user-instructions>\n");
        prompt.push_str(instructions);
        prompt.push_str("\n</user-instructions>\n");
    }
    prompt
}

fn render_summary_transcript(prompt: String, history: &[Message]) -> String {
    let mut transcript = prompt;
    for message in history {
        transcript.push_str(&serde_json::to_string(message).expect("Message 可序列化"));
        transcript.push_str("\n\n");
    }
    transcript
}

#[cfg(test)]
fn summary_transcript(
    state: &SessionState,
    history: &[Message],
    start: usize,
    instructions: Option<&str>,
) -> String {
    render_summary_transcript(summary_prompt(state, instructions, 0), &history[..start])
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::{PlanStatus, SessionMode};

    use super::*;
    use crate::session::entries::{Entry, EntryPayload};

    #[test]
    fn summary_input_carries_checkpoint_plan_instructions_and_required_sections() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Plan,
            None,
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
}

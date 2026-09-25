//! 用轻量模型生成可更新的历史 checkpoint。

use moc_llm::{ChatRequest, Message};
use moc_protocol::RequestContext;

use super::replay::visible_history_start;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::state::SessionState;

use super::{estimate_tokens, messages_tokens, record_compaction};

const SUMMARY_SYSTEM: &str = "You are compressing an agent conversation into a checkpoint. Output only the checkpoint text; do not call tools; do not mention this compression request. Use concise engineering English. Preserve exact file paths, commands, error strings, identifiers, numeric values, function signatures and code fragments. Record user corrections verbatim.";

pub(crate) async fn compact(
    inner: &CoreInner,
    ctx: &RequestContext,
    state: &SessionState,
    config: &ResolvedConfig,
    instructions: Option<&str>,
    tokens_before: u64,
) -> Result<moc_protocol::methods::CompactResult, CoreError> {
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
    // 单轮或短历史也支持手动压缩；优先保留最后一个完整用户轮。
    if start == 0 {
        start = history
            .iter()
            .rposition(|message| message.role == moc_llm::Role::User)
            .filter(|index| *index > 0)
            .unwrap_or(history.len());
    }
    let retained = messages_tokens(&history[start..]);
    let transcript = summary_transcript(state, &history, start, instructions);
    let role_model = config.resolve_role("smol")?;
    let request = ChatRequest {
        messages: vec![
            Message::system(SUMMARY_SYSTEM),
            Message::user_text(transcript),
        ],
        max_output_tokens: Some(
            role_model
                .max_output_tokens
                .min(config.limits.max_output_tokens),
        ),
        ..ChatRequest::default()
    };
    let cancel = ctx.cancel();
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
    let tokens_after = estimate_tokens(&summary).saturating_add(retained);
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
fn summary_transcript(
    state: &SessionState,
    history: &[Message],
    start: usize,
    instructions: Option<&str>,
) -> String {
    let mut transcript = String::from(
        "Summarize the older conversation as a checkpoint using these exact sections:\n\
         ## Primary Request and Intent\n## Key Technical Concepts\n## Files and Code\n\
         ## Errors and Fixes\n## Pending Jobs\n## Current Work\n## Next Step\n\
         ## Critical Context\n\n",
    );
    if let Some((_, summary)) = &state.summary {
        transcript.push_str("<previous-summary>\n");
        transcript.push_str(summary);
        transcript.push_str("\n</previous-summary>\n\n");
    }
    if let Some(plan) = state
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == moc_protocol::methods::PlanStatus::Submitted)
    {
        transcript.push_str("<pending-plan>\n");
        transcript.push_str(&plan.markdown);
        transcript.push_str("\n</pending-plan>\n\n");
    }
    for message in &history[..start] {
        transcript.push_str(&serde_json::to_string(message).expect("Message 可序列化"));
        transcript.push_str("\n\n");
    }
    if let Some(instructions) = instructions {
        transcript.push_str("<user-instructions>\n");
        transcript.push_str(instructions);
        transcript.push_str("\n</user-instructions>\n");
    }
    transcript
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
}

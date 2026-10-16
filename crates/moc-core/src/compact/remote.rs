//! Responses 原生压缩、尾部去重与载荷持久化。

use std::time::Instant;

use moc_llm::{ChatRequest, Protocol};
use moc_protocol::RequestContext;
use serde_json::Value;

use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::state::SessionState;

use super::{CompactionBudget, messages_tokens, record_compaction};

pub(crate) async fn compact(
    inner: &CoreInner,
    ctx: &RequestContext,
    state: &SessionState,
    config: &ResolvedConfig,
    model: &crate::ResolvedModel,
    messages: &[moc_llm::Message],
    budget: CompactionBudget,
) -> Result<moc_protocol::methods::CompactResult, CoreError> {
    let CompactionBudget {
        tokens_before,
        fixed_tokens,
    } = budget;
    if !config.compaction.remote.enabled
        || !model.remote_compaction
        || model.llm.protocol != Protocol::OpenAiResponses
        || inner.remote_unavailable.read().contains(&model.selector)
    {
        return Err(CoreError::Tool(
            "Responses native compaction is not supported for this model".into(),
        ));
    }
    let mut full_messages = Vec::with_capacity(messages.len() + 2);
    let skills = inner.project_skills(std::path::Path::new(&state.session.cwd))?;
    let summary = inner.mcp_summary(config).await?;
    let context_files = inner
        .project_context_files(&state.session.id, std::path::Path::new(&state.session.cwd))
        .await;
    full_messages.extend(
        crate::system_prompt::build(
            config,
            model,
            std::path::Path::new(&state.session.cwd),
            &state.session.id,
            &context_files,
            &skills,
            &summary,
            None,
        )
        .messages,
    );
    full_messages.extend_from_slice(messages);
    let request = ChatRequest {
        messages: full_messages,
        tools: inner.tools.definitions(
            state.mode,
            inner.client_supports_ask(ctx.connection_id()),
            config,
            crate::tools::ToolScope {
                control_agents: state.session.parent_session_id.is_none(),
                subagent: state.session.parent_session_id.is_some(),
            },
        ),
        native_input: None,
        ..ChatRequest::default()
    };
    let mut items = state.native_input.clone().unwrap_or_default();
    items.extend(moc_llm::encode_responses_input_items(&model.llm, &request)?);
    let cancel = ctx.cancel();
    let _permit = inner.provider_permit(model, &cancel).await?;
    let request_started = Instant::now();
    let deadline_secs = config.limits.connect_deadline_secs;
    let compacted = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
        result = inner.client.compact(&model.llm, &items, deadline_secs) => result,
    };
    let response = match compacted {
        Ok(response) => response,
        Err(error) => {
            if let moc_llm::LlmError::Guard(violation) = &error
                && let Some(guard) = inner.runtime(&state.session.id).await.guard.lock().clone()
            {
                guard.stop(violation.clone());
            }
            if error
                .api_error()
                .is_some_and(|api| matches!(api.status, Some(400 | 404)))
            {
                inner
                    .remote_unavailable
                    .write()
                    .insert(model.selector.clone());
            }
            return Err(error.into());
        }
    };
    crate::service::record_model_usage(
        inner,
        &state.session.id,
        &model.selector,
        response.usage,
        request_started.elapsed().as_millis().min(u64::MAX as u128) as u64,
    )
    .await?;
    if response.items.is_empty() {
        return Err(CoreError::Tool(
            "the Responses compaction endpoint returned empty output".into(),
        ));
    }
    let retained = config.compaction.remote.retained_message_tokens as u64;
    let (first_kept_id, retained_tokens) = retained_boundary(state, retained);
    let mut tokens_after = fixed_tokens.saturating_add(retained_tokens);
    let tail_start = first_kept_id
        .as_ref()
        .and_then(|id| state.transcript.iter().position(|entry| &entry.id == id))
        .unwrap_or(state.transcript.len());
    let tail_request = ChatRequest {
        messages: state.transcript[tail_start..]
            .iter()
            .filter_map(|entry| entry.context_message())
            .collect(),
        ..ChatRequest::default()
    };
    let tail = moc_llm::encode_responses_input_items(&model.llm, &tail_request)?;
    let mut native_items = response.items;
    // 端点可原样返回真实尾部；移除与本地保留区重复的后缀，仅回放一次。
    if native_items.len() >= tail.len()
        && !tail.is_empty()
        && native_items[native_items.len() - tail.len()..]
            .iter()
            .zip(&tail)
            .all(|(actual, expected)| item_matches(actual, expected))
    {
        native_items.truncate(native_items.len() - tail.len());
    }
    if native_items.is_empty() {
        return Err(CoreError::Tool(
            "the compaction endpoint returned only the raw tail and no checkpoint".into(),
        ));
    }
    let native_value = Value::Array(native_items);
    let native_tokens = serde_json::to_string(&native_value)
        .map_or(0, |serialized| super::estimate_tokens(&serialized));
    tokens_after = tokens_after.saturating_add(native_tokens);
    let native = Some(Box::new(native_value));
    let summary = checkpoint_summary(state);
    record_compaction(
        inner,
        ctx,
        &state.session.id,
        super::Checkpoint {
            method: "remote".to_owned(),
            model: Some(model.selector.clone()),
            summary,
            first_kept_id,
            tokens_before,
            tokens_after,
            instructions: None,
            native,
            archive: super::HistoryArchive::Write,
        },
    )
    .await
}

pub(crate) fn retained_boundary(state: &SessionState, budget: u64) -> (Option<String>, u64) {
    let visible = super::replay::visible_history_start(state);
    let mut start = state.transcript.len();
    let mut tokens = 0u64;
    while start > visible {
        let cost = state.transcript_token_counts[start - 1];
        if start < state.transcript.len() && tokens.saturating_add(cost) > budget {
            break;
        }
        tokens = tokens.saturating_add(cost);
        start -= 1;
    }
    while start > visible && state.transcript[start].message.role == moc_llm::Role::Tool {
        start -= 1;
    }
    let kept: Vec<_> = state.transcript[start..]
        .iter()
        .filter_map(|entry| entry.context_message())
        .collect();
    (
        state.transcript.get(start).map(|entry| entry.id.clone()),
        messages_tokens(&kept),
    )
}

pub(crate) fn checkpoint_summary(state: &SessionState) -> String {
    let completed: std::collections::HashSet<_> = state
        .transcript
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .filter(|result| !result.is_error)
        .map(|result| result.tool_call_id.as_str())
        .collect();
    let mut files = std::collections::BTreeSet::new();
    for call in state
        .transcript
        .iter()
        .flat_map(|entry| entry.message.tool_calls())
    {
        if !completed.contains(call.id.as_str()) || !matches!(call.name.as_str(), "edit" | "write")
        {
            continue;
        }
        let Ok(arguments) = call.arguments_json() else {
            continue;
        };
        if let Some(path) = arguments.get("path").and_then(Value::as_str) {
            files.insert(format!("{}: {path}", call.name));
        }
        if let Some(path) = arguments
            .get("move")
            .and_then(|value| value.get("target_path"))
            .and_then(Value::as_str)
        {
            files.insert(format!("move: {path}"));
        }
    }
    let mut summary = String::from(
        "Saved the server-native compaction payload and retained the recent conversation tail.\nSuccessful file operations (edit/write):",
    );
    if files.is_empty() {
        summary.push_str("none");
    }
    for file in files {
        summary.push_str("\n- ");
        summary.push_str(&file);
    }
    summary
}

fn item_matches(actual: &Value, expected: &Value) -> bool {
    match expected {
        Value::Object(fields) => fields.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| item_matches(actual, value))
        }),
        Value::Array(items) => actual.as_array().is_some_and(|actual| {
            actual.len() == items.len() && actual.iter().zip(items).all(|(a, b)| item_matches(a, b))
        }),
        _ => actual == expected,
    }
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::{SessionMode, SessionPermission};

    use super::*;
    use crate::session::entries::{Entry, EntryPayload};
    use crate::session::state::SessionState;

    #[test]
    fn retained_boundary_preserves_a_suffix_within_the_budget() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "gpt-x".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            None,
            "t".into(),
            1,
        );
        let mut parent = header.id().to_owned();
        let mut entries = vec![header];
        for text in ["old", "newer"] {
            let entry = Entry::from_payload(
                EntryPayload::Message {
                    message: Message::user_text(text),
                    tool: None,
                    finish_reason: None,
                    agent: None,
                },
                parent,
                2,
            );
            parent = entry.id().to_owned();
            entries.push(entry);
        }
        let state = SessionState::from_entries(entries).unwrap();
        let (first, tokens) = retained_boundary(&state, 100);
        assert_eq!(first, Some(state.entries[1].id().to_owned()));
        assert!(tokens > 0);
    }
}

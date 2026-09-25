//! 从追加式日志与有效压缩边界重建模型视图。

use moc_llm::{Message, Role};
use moc_protocol::methods::PlanStatus;

use crate::session::entries::Entry;
use crate::session::state::SessionState;

/// 只让有效 checkpoint 或 reset 决定可见历史起点。
pub fn model_messages(
    state: &SessionState,
    system: Option<Message>,
    model: Option<&crate::ResolvedModel>,
) -> Vec<Message> {
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.push(system);
    }
    let start = visible_history_start(state);
    if let Some((method, summary)) = &state.summary
        && !summary.is_empty()
        && state.native_input.is_none()
    {
        messages.push(Message::user_text(format!(
            "<compaction-summary method=\"{method}\">\n{summary}\n</compaction-summary>"
        )));
    }
    if let Some(plan) = state
        .plans
        .iter()
        .rev()
        .find(|plan| plan.status == PlanStatus::Approved)
    {
        let marker = format!("<active-plan id=\"{}\">", plan.plan_id);
        let already_in_transcript = state
            .transcript
            .iter()
            .skip(start)
            .any(|entry| entry.message.text().contains(&marker));
        if !already_in_transcript {
            messages.push(Message::user_text(format!(
                "<active-plan id=\"{}\">\n{}\n</active-plan>",
                plan.plan_id, plan.markdown
            )));
        }
    }
    messages.extend(
        state.transcript[start..]
            .iter()
            .map(|entry| repair_tool_history(entry.message.clone(), model)),
    );
    messages
}

/// 重置优先于旧压缩；没有保留边界表示该次压缩已覆盖此前全部消息。
pub(crate) fn visible_history_start(state: &SessionState) -> usize {
    let reset = state
        .entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::ResetBoundary { .. }));
    let compact = state.summary.as_ref().and_then(|_| {
        state
            .entries
            .iter()
            .rposition(|entry| matches!(entry, Entry::Compaction { .. }))
    });
    let boundary = match (reset, compact) {
        (Some(reset), Some(compact)) => reset.max(compact),
        (Some(boundary), None) | (None, Some(boundary)) => boundary,
        (None, None) => return 0,
    };
    if matches!(state.entries[boundary], Entry::Compaction { .. })
        && let Some(id) = &state.first_kept_id
        && let Some(index) = state.transcript.iter().position(|entry| &entry.id == id)
    {
        return index;
    }
    state.entries[..=boundary]
        .iter()
        .filter(|entry| matches!(entry, Entry::Message { .. }))
        .count()
}

fn repair_tool_history(mut message: Message, model: Option<&crate::ResolvedModel>) -> Message {
    if message.role != Role::Assistant || message.tool_calls().next().is_none() {
        return message;
    }
    if model.is_some_and(|model| model.requires_assistant_content_for_tool_calls)
        && !message
            .content
            .iter()
            .any(|part| matches!(part, moc_llm::ContentPart::Text(_)))
    {
        message
            .content
            .insert(0, moc_llm::ContentPart::Text(String::new()));
    }
    if model.is_some_and(|model| model.requires_reasoning_content_for_tool_calls)
        && !message
            .content
            .iter()
            .any(|part| matches!(part, moc_llm::ContentPart::Reasoning(_)))
    {
        message.content.insert(
            0,
            moc_llm::ContentPart::Reasoning(moc_llm::ReasoningBlock::text("")),
        );
    }
    message
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::SessionMode;

    use super::*;
    use crate::session::entries::EntryPayload;
    use crate::session::state::SessionState;

    #[test]
    fn latest_compaction_renders_summary_and_only_retained_messages() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            None,
            "title".into(),
            1,
        );
        let first = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("old"),
                tool: None,
                finish_reason: None,
            },
            header.id().into(),
            2,
        );
        let first_id = first.id().to_owned();
        let compact = Entry::from_payload(
            EntryPayload::Compaction {
                method: "soft".into(),
                summary: "keep intent".into(),
                first_kept_id: Some(first_id.clone()),
                tokens_before: 100,
                tokens_after: 50,
                instructions: None,
                native: None,
            },
            first_id,
            3,
        );
        let last = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("new"),
                tool: None,
                finish_reason: None,
            },
            compact.id().into(),
            4,
        );
        let state = SessionState::from_entries(vec![header, first, compact, last]).unwrap();
        let messages = model_messages(&state, None, None);
        assert!(messages[0].text().contains("keep intent"));
        assert_eq!(messages.last().unwrap().text(), "new");
    }

    #[test]
    fn model_changes_and_invalid_native_payload_restore_uncompacted_history() {
        for native in [
            serde_json::json!([{"type":"compaction","encrypted_content":"opaque"}]),
            serde_json::json!("malformed"),
        ] {
            let invalid = !native.is_array();
            let header = Entry::header(
                "ses_123456789abc".into(),
                "/tmp".into(),
                "p/old".into(),
                SessionMode::Normal,
                None,
                "title".into(),
                1,
            );
            let mut state = SessionState::new(header).unwrap();
            for payload in [
                EntryPayload::Message {
                    message: Message::user_text("已清除的历史"),
                    tool: None,
                    finish_reason: None,
                },
                EntryPayload::ResetBoundary {},
                EntryPayload::Message {
                    message: Message::user_text("必须保留的用户纠正"),
                    tool: None,
                    finish_reason: None,
                },
                EntryPayload::Compaction {
                    method: "remote".into(),
                    summary: "原生载荷占位".into(),
                    first_kept_id: None,
                    tokens_before: 100,
                    tokens_after: 10,
                    instructions: None,
                    native: Some(Box::new(native)),
                },
            ] {
                state.apply_entry(Entry::from_payload(payload, state.last_entry_id.clone(), 2));
            }
            if invalid {
                assert_eq!(
                    model_messages(&state, None, None)
                        .iter()
                        .map(Message::text)
                        .collect::<Vec<_>>(),
                    ["必须保留的用户纠正"]
                );
            }
            state.apply_entry(Entry::from_payload(
                EntryPayload::ModelChange {
                    model: "p/new".into(),
                },
                state.last_entry_id.clone(),
                3,
            ));
            let view = model_messages(&state, None, None);
            assert_eq!(
                view.iter().map(Message::text).collect::<Vec<_>>(),
                ["必须保留的用户纠正"]
            );
            assert!(state.native_input.is_none());
        }
    }
}

//! 从追加式日志与有效压缩边界重建模型视图。

use moc_llm::{Message, Role};
use moc_protocol::methods::PlanStatus;

use crate::session::entries::Entry;
use crate::session::state::SessionState;

/// 本地注入上下文消息的固定前缀与正文分块短名：压缩摘要、活动计划、todo 快照。
const INJECTED_PREFIXES: [(&str, &str); 3] = [
    ("<compaction-summary", "compaction summary"),
    ("<active-plan", "active plan"),
    ("<active-todos", "active todos"),
];

/// 消息是否由 moc 注入而非用户输入。
///
/// 这些消息由本模块与计划流程生成并随 transcript 进入上下文；任何要再次引用
/// 其内容的地方（例如子代理任务推导）都必须先跳过，避免整段复制进系统提示。
pub(crate) fn is_injected_context(message: &Message) -> bool {
    injected_name(message).is_some()
}

/// 注入消息的正文分块短名；用户与模型消息返回 `None`。
pub(crate) fn injected_name(message: &Message) -> Option<&'static str> {
    let text = message.text();
    let trimmed = text.trim_start();
    INJECTED_PREFIXES
        .iter()
        .find(|(prefix, _)| trimmed.starts_with(prefix))
        .map(|(_, name)| *name)
}

/// 构造指令消息、checkpoint、活动计划和 todo 等合成前缀，不复制 transcript。
pub(crate) fn synthetic_messages(state: &SessionState, system: Option<&[Message]>) -> Vec<Message> {
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.extend_from_slice(system);
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
            .filter(|entry| entry.in_context())
            .any(|entry| entry.message.text().contains(&marker));
        if !already_in_transcript {
            messages.push(Message::user_text(format!(
                "<active-plan id=\"{}\">\n{}\n</active-plan>",
                plan.plan_id, plan.markdown
            )));
        }
    }
    if !state.todos.is_empty()
        && state
            .entries
            .iter()
            .any(|entry| matches!(entry, Entry::ResetBoundary { .. }))
        && state.summary.is_none()
        && let Ok(todos) = serde_json::to_string(&state.todos)
    {
        messages.push(Message::user_text(format!(
            "<active-todos>\n{todos}\n</active-todos>"
        )));
    }
    messages
}

/// 从追加式日志重建模型消息；transcript 消息只在真正请求模型时复制。
pub fn model_messages(
    state: &SessionState,
    system: Option<&[Message]>,
    model: Option<&crate::ResolvedModel>,
) -> Vec<Message> {
    let mut messages = Vec::with_capacity(2);
    if let Some(system) = system {
        messages.extend_from_slice(system);
    }
    if state.mode == moc_protocol::methods::SessionMode::ClmExperimental
        && let Some(context) = &state.clm_context
    {
        messages.push(crate::clm::message(context));
        return messages;
    }
    messages.extend(synthetic_messages(state, None));
    let start = visible_history_start(state);
    messages.extend(
        state.transcript[start..]
            .iter()
            .filter_map(|entry| entry.context_message())
            .map(|message| repair_tool_history(message, model)),
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

pub(crate) fn repair_tool_history(
    mut message: Message,
    model: Option<&crate::ResolvedModel>,
) -> Message {
    if message.role != Role::Assistant {
        return message;
    }
    let has_tool_calls = message.tool_calls().next().is_some();
    if has_tool_calls
        && model.is_some_and(|model| model.requires_assistant_content_for_tool_calls)
        && !message
            .content
            .iter()
            .any(|part| matches!(part, moc_llm::ContentPart::Text(_)))
    {
        message
            .content
            .insert(0, moc_llm::ContentPart::Text(String::new()));
    }
    if model.is_some_and(|model| {
        model.requires_reasoning_content_for_all_assistant_turns
            || (has_tool_calls && model.requires_reasoning_content_for_tool_calls)
    }) && !message
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
    use moc_protocol::methods::{SessionMode, SessionPermission};

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
            SessionPermission::Full,
            None,
            "title".into(),
            1,
        );
        let first = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("old"),
                tool: None,
                finish_reason: None,
                agent: None,
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
                model: Some("p/model".into()),
            },
            first_id,
            3,
        );
        let last = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("new"),
                tool: None,
                finish_reason: None,
                agent: None,
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
    fn model_messages_reinject_structured_todos_after_compaction() {
        use moc_protocol::methods::{TodoItem, TodoPhase, TodoStatus};

        let header = Entry::header(
            "ses_todo123456".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            None,
            "title".into(),
            1,
        );
        let mut state = SessionState::new(header).unwrap();
        state.todos = vec![TodoPhase {
            phase: "execute".into(),
            items: vec![TodoItem {
                status: TodoStatus::InProgress,
                task: "preserve the current task".into(),
                reason: None,
            }],
        }];
        state.apply_entry(Entry::from_payload(
            EntryPayload::ResetBoundary,
            state.last_entry_id.clone(),
            2,
        ));
        let messages = model_messages(&state, None, None);
        assert!(messages.iter().any(|message| {
            message.text().contains("<active-todos>")
                && message.text().contains("preserve the current task")
        }));
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
                SessionPermission::Full,
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
                    agent: None,
                },
                EntryPayload::ResetBoundary {},
                EntryPayload::Message {
                    message: Message::user_text("必须保留的用户纠正"),
                    tool: None,
                    finish_reason: None,
                    agent: None,
                },
                EntryPayload::Compaction {
                    method: "remote".into(),
                    summary: "原生载荷占位".into(),
                    first_kept_id: None,
                    tokens_before: 100,
                    tokens_after: 10,
                    instructions: None,
                    native: Some(Box::new(native)),
                    // 旧记录没有产出模型：换模型时必须保守作废。
                    model: None,
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

    #[test]
    fn model_change_keeps_a_checkpoint_produced_for_the_target_model() {
        for (checkpoint_model, retained) in
            [(Some("p/new"), true), (Some("p/old"), false), (None, false)]
        {
            let header = Entry::header(
                "ses_123456789abc".into(),
                "/tmp".into(),
                "p/old".into(),
                SessionMode::Normal,
                SessionPermission::Full,
                None,
                "title".into(),
                1,
            );
            let mut state = SessionState::new(header).unwrap();
            let tail = Entry::from_payload(
                EntryPayload::Message {
                    message: Message::user_text("压缩后保留的尾部"),
                    tool: None,
                    finish_reason: None,
                    agent: None,
                },
                state.last_entry_id.clone(),
                2,
            );
            let tail_id = tail.id().to_owned();
            state.apply_entry(tail);
            state.apply_entry(Entry::from_payload(
                EntryPayload::Compaction {
                    method: "remote".into(),
                    summary: "原生载荷占位".into(),
                    first_kept_id: Some(tail_id.clone()),
                    tokens_before: 100,
                    tokens_after: 10,
                    instructions: None,
                    native: Some(Box::new(serde_json::json!([
                        {"type": "compaction", "encrypted_content": "opaque"}
                    ]))),
                    model: checkpoint_model.map(str::to_owned),
                },
                state.last_entry_id.clone(),
                3,
            ));
            state.apply_entry(Entry::from_payload(
                EntryPayload::ModelChange {
                    model: "p/new".into(),
                },
                state.last_entry_id.clone(),
                4,
            ));
            assert_eq!(
                state.native_input.is_some(),
                retained,
                "checkpoint 产出模型 {checkpoint_model:?} 的原生载荷保留判定"
            );
            assert_eq!(state.summary.is_some(), retained);
            assert_eq!(state.first_kept_id.is_some(), retained);
        }
    }

    #[test]
    fn legacy_compaction_entries_without_a_model_keep_the_old_invalidation() {
        // 旧 JSONL 的 Compaction 记录没有 model 字段：必须仍可解析且清理语义与旧版一致。
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/old".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            None,
            "title".into(),
            1,
        );
        let legacy = serde_json::json!({
            "type": "compaction",
            "id": "entry_legacy0001",
            "parent_id": header.id(),
            "at_ms": 2,
            "method": "remote",
            "summary": "原生载荷占位",
            "first_kept_id": null,
            "tokens_before": 100,
            "tokens_after": 10,
            "instructions": null,
            "native": [{"type": "compaction", "encrypted_content": "opaque"}],
        });
        let entry: Entry = serde_json::from_value(legacy).expect("旧记录可解析");
        let mut state = SessionState::from_entries(vec![header, entry.clone()]).unwrap();
        assert!(state.summary.is_some());
        assert!(state.native_input.is_some());
        assert!(state.summary_model.is_none());
        // 缺省产出模型不写入新记录，新旧 JSONL 形状保持一致。
        let round_trip = serde_json::to_value(&entry).expect("序列化");
        assert!(round_trip.get("model").is_none(), "{round_trip}");
        // 换模型时保守作废：与旧版行为等价。
        state.apply_entry(Entry::from_payload(
            EntryPayload::ModelChange {
                model: "p/new".into(),
            },
            state.last_entry_id.clone(),
            3,
        ));
        assert!(state.native_input.is_none());
        assert!(state.summary.is_none());
        assert!(state.first_kept_id.is_none());
    }

    #[test]
    fn clm_projection_replaces_model_view_but_preserves_transcript() {
        let header = Entry::header(
            "ses_clm12345678".into(),
            "/tmp".into(),
            "p/model".into(),
            SessionMode::ClmExperimental,
            SessionPermission::Full,
            None,
            "clm".into(),
            1,
        );
        let user = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("原始用户请求"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            header.id().into(),
            2,
        );
        let context = Entry::from_payload(
            EntryPayload::ClmContext {
                content: "整理后的实时上下文".into(),
            },
            user.id().into(),
            3,
        );
        let state = SessionState::from_entries(vec![header, user, context]).unwrap();
        let messages = model_messages(&state, Some(&[Message::system("system")]), None);

        assert_eq!(state.transcript.len(), 1);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].text(), "system");
        assert!(messages[1].text().contains("整理后的实时上下文"));
        assert!(!messages[1].text().contains("原始用户请求"));
    }
}

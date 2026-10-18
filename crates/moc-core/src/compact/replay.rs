//! 由追加式日志和生效中的压缩边界重建模型看到的历史。

use moc_llm::{Message, Role};
use moc_protocol::methods::PlanStatus;

use crate::session::entries::Entry;
use crate::session::state::SessionState;

/// 本地注入消息的固定前缀与对应的分块短名：压缩摘要、活动计划、todo 快照、
/// Grok Build 的续接前言和运行时提醒。
const INJECTED_PREFIXES: [(&str, &str); 5] = [
    ("<compaction-summary", "compaction summary"),
    ("<active-plan", "active plan"),
    ("<active-todos", "active todos"),
    ("This session is being continued", "grok build summary"),
    ("<system-reminder>", "system reminder"),
];

/// 消息是否是 moc 注入的，而不是用户输入。
///
/// 这些消息由本模块和计划流程生成，会随 transcript 进入上下文。任何需要引用其内容的
/// 地方（比如推导子代理任务）都要先跳过它们，免得整段被复制进系统提示。
pub(crate) fn is_injected_context(message: &Message) -> bool {
    injected_name(message).is_some()
}

/// 注入消息的分块短名；用户和模型的消息返回 `None`。
pub(crate) fn injected_name(message: &Message) -> Option<&'static str> {
    let text = message.text();
    let trimmed = text.trim_start();
    INJECTED_PREFIXES
        .iter()
        .find(|(prefix, _)| trimmed.starts_with(prefix))
        .map(|(_, name)| *name)
}

/// 生成指令消息、checkpoint、活动计划、todo 这类合成消息，不复制 transcript。
pub(crate) fn synthetic_messages(state: &SessionState, system: Option<&[Message]>) -> Vec<Message> {
    let mut messages = Vec::new();
    if let Some(system) = system {
        messages.extend_from_slice(system);
    }
    let start = visible_history_start(state);
    // Grok Build 整体替换历史：回放只有最后一条用户消息、摘要和运行时提醒，不带旧消息。
    if let Some((method, summary)) = &state.summary
        && method == "grok_build"
    {
        if let Some(query) = crate::compact::grok_build::last_user_message(state) {
            messages.push(query);
        }
        if !summary.is_empty() {
            messages.push(Message::user_text(crate::compact::grok_build::carrier(
                summary,
            )));
        }
        if let Some(reminder) = &state.summary_reminder {
            messages.push(Message::user_text(reminder.clone()));
        }
        return messages;
    }
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

/// 由追加式日志重建模型消息；只有真正要请求模型时才复制 transcript 消息。
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

/// 可见历史的起点：重置边界优先于旧的压缩；没有保留边界说明那次压缩已经覆盖了此前的全部消息。
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
    use crate::session::entries::{EntryPayload, SessionOrigin};
    use crate::session::state::SessionState;

    #[test]
    fn latest_compaction_renders_summary_and_only_retained_messages() {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
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
                reminder: None,
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
            SessionOrigin::root(),
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
                SessionOrigin::root(),
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
                    // 旧记录没有生成它的模型：换模型时必须稳妥地作废。
                    model: None,
                    reminder: None,
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
                SessionOrigin::root(),
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
                    reminder: None,
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
            SessionOrigin::root(),
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
    fn grok_build_checkpoint_replays_query_summary_and_reminder_only() {
        let header = Entry::header(
            "ses_grok1234567".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "grok".into(),
            1,
        );
        let query = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("原始需求：实现可切换 soft 压缩"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            header.id().into(),
            2,
        );
        let answer = Entry::from_payload(
            EntryPayload::Message {
                message: Message::assistant_text("旧助手回答不应出现"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            query.id().into(),
            3,
        );
        let compact = Entry::from_payload(
            EntryPayload::Compaction {
                method: "grok_build".into(),
                summary: "Summary:\n1. Primary Request and Intent: 实现可切换 soft 压缩".into(),
                first_kept_id: None,
                tokens_before: 100,
                tokens_after: 40,
                instructions: None,
                native: None,
                model: Some("p/m".into()),
                reminder: Some(
                    "<system-reminder>\n## TODO List\n- [pending] todo-1-1: 接线\n</system-reminder>"
                        .into(),
                ),
            },
            answer.id().into(),
            4,
        );
        let state = SessionState::from_entries(vec![header, query, answer, compact]).unwrap();
        let messages = model_messages(&state, None, None);
        assert_eq!(messages.len(), 3, "{messages:?}");
        assert_eq!(messages[0].text(), "原始需求：实现可切换 soft 压缩");
        assert!(messages[1].text().starts_with(
            "This session is being continued from a previous conversation that ran out of context."
        ));
        assert!(messages[1].text().contains("1. Primary Request and Intent"));
        assert_eq!(
            messages[2].text(),
            "<system-reminder>\n## TODO List\n- [pending] todo-1-1: 接线\n</system-reminder>"
        );
        for message in &messages {
            assert!(
                !message.text().contains("<compaction-summary"),
                "Grok 不走通用摘要包装"
            );
            assert!(
                !message.text().contains("旧助手回答不应出现"),
                "压缩前的旧消息必须被隐藏"
            );
        }
        // 注入消息不参与子代理任务推导，也不该被当成用户输入。
        assert!(is_injected_context(&messages[1]));
        assert!(is_injected_context(&messages[2]));
        assert!(!is_injected_context(&messages[0]));
    }

    #[test]
    fn clm_projection_replaces_model_view_but_preserves_transcript() {
        let header = Entry::header(
            "ses_clm12345678".into(),
            "/tmp".into(),
            "p/model".into(),
            SessionMode::ClmExperimental,
            SessionPermission::Full,
            SessionOrigin::root(),
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

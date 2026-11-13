use moc_llm::Message;
use moc_protocol::methods::{PlanStatus, SessionAutonomy, SessionEngine};

use super::*;
use crate::session::entries::{EntryPayload, SessionOrigin};

/// 工具结果按完成顺序落盘，模型投影必须回到调用顺序。
#[test]
fn out_of_order_tool_results_project_in_call_order() {
    use moc_llm::{ContentPart, Role, ToolCall};

    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp/project".into(),
        "p/model".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "标题".into(),
        10,
    );
    let assistant = Entry::from_payload(
        EntryPayload::Message {
            message: Message {
                role: Role::Assistant,
                content: moc_llm::message_content![
                    ContentPart::ToolCall(ToolCall {
                        id: "call_a".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    }),
                    ContentPart::ToolCall(ToolCall {
                        id: "call_b".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    }),
                ],
            },
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        header.id().into(),
        11,
    );
    // 先完成的是第二个调用。
    let second = Entry::from_payload(
        EntryPayload::Message {
            message: Message::tool_result("call_b", "乙", false),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        assistant.id().into(),
        12,
    );
    let first = Entry::from_payload(
        EntryPayload::Message {
            message: Message::tool_result("call_a", "甲", false),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        second.id().into(),
        13,
    );

    let state = SessionState::from_entries(vec![
        header.clone(),
        assistant.clone(),
        second.clone(),
        first.clone(),
    ])
    .expect("记录链完整");

    let ids: Vec<&str> = state.transcript[1..]
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .map(|result| result.tool_call_id.as_str())
        .collect();
    assert_eq!(ids, ["call_a", "call_b"]);
    for (index, entry) in state.transcript.iter().enumerate() {
        assert_eq!(state.transcript_position(&entry.id), Some(index));
    }
    assert_eq!(state.transcript.len(), state.transcript_token_counts.len());
    let kept_id = second.id().to_owned();
    let mut partial = SessionState::from_entries(vec![header, assistant, second]).unwrap();
    partial.apply_entry(Entry::from_payload(
        EntryPayload::Compaction {
            method: "soft".into(),
            summary: "摘要".into(),
            first_kept_id: Some(kept_id.clone()),
            tokens_before: 1,
            tokens_after: 1,
            instructions: None,
            model: None,
            reminder: None,
        },
        partial.last_entry_id.clone(),
        14,
    ));
    let snapshot = partial.clone();
    assert!(Arc::ptr_eq(&snapshot.entries, &partial.entries));
    assert!(Arc::ptr_eq(&snapshot.transcript, &partial.transcript));
    partial.apply_entry(first);
    assert_eq!(partial.transcript[partial.context_start].id, kept_id);
    assert_eq!(snapshot.transcript.len(), 2, "追加不能改变冻结快照");
    let messages = crate::compact::replay::model_messages(&partial, None, None);
    assert_eq!(
        crate::compact::cached_messages_tokens(&partial, &messages),
        crate::compact::messages_tokens(&messages)
    );
    for (index, parent) in ["call_a", "call_b", "call_a"].into_iter().enumerate() {
        partial.apply_entry(Entry::from_payload(
            EntryPayload::ChildTool {
                call: moc_protocol::TranscriptChildCall {
                    id: format!("child_{index}"),
                    parent_tool_call_id: parent.into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                    result: "结果".into(),
                    status: moc_protocol::ToolStatus::Completed,
                    duration_ms: None,
                    diff: None,
                },
            },
            partial.last_entry_id.clone(),
            20 + index as u64,
        ));
    }
    assert_eq!(
        partial.child_call_indices(&std::collections::HashSet::from(["call_a"]), 64),
        [0, 2]
    );
    assert_eq!(
        partial.child_call_indices(&std::collections::HashSet::from(["call_b", "call_a"]), 2),
        [0, 1]
    );
}

#[test]
fn replay_restores_metadata_todos_plans_and_transcript() {
    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp/project".into(),
        "p/model:high".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "标题".into(),
        10,
    );
    let header_id = header.id().to_owned();
    let message = Entry::from_payload(
        EntryPayload::Message {
            message: Message::user_text("你好"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        header_id,
        11,
    );
    let message_id = message.id().to_owned();
    let todo = Entry::from_payload(EntryPayload::Todos { phases: Vec::new() }, message_id, 12);
    let todo_id = todo.id().to_owned();
    let plan = Entry::from_payload(
        EntryPayload::Plan {
            plan_id: "plan_12345678".into(),
            title: "计划".into(),
            path: "/tmp/plan.md".into(),
            markdown: "# 计划".into(),
            status: PlanStatus::Approved,
            version: 1,
            previous_plan_id: None,
        },
        todo_id,
        13,
    );
    let engine = Entry::from_payload(
        EntryPayload::EngineChange {
            engine: SessionEngine::Standard,
            plan_active: true,
        },
        plan.id().to_owned(),
        14,
    );
    let autonomy = Entry::from_payload(
        EntryPayload::AutonomyChange {
            autonomy: SessionAutonomy::Unattended,
        },
        engine.id().to_owned(),
        15,
    );

    let state = SessionState::from_entries(vec![header, message, todo, plan, engine, autonomy])
        .expect("记录链完整");

    assert_eq!(state.session.title, "标题");
    assert_eq!(state.session.engine, SessionEngine::Standard);
    assert_eq!(state.session.autonomy, SessionAutonomy::Unattended);
    assert!(state.plan_active);
    assert_eq!(state.transcript.len(), 1);
    assert!(matches!(
        &state.transcript[0].message.content[0],
        moc_llm::ContentPart::Text(text) if text == "你好"
    ));
    assert_eq!(state.plans[0].status, PlanStatus::Approved);
    assert_eq!(state.session.updated_at_ms, 15);
    assert_eq!(state.prompt_sequence, 1);
}

/// 计划状态重写不得冲掉批注与版本；`PlanFeedback` 只改批注并把状态推到修订中。
#[test]
fn replay_keeps_plan_version_and_annotations_across_status_rewrites() {
    fn entries(extra: Vec<EntryPayload>) -> Vec<Entry> {
        let header = Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            "p/model".into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            10,
        );
        let mut parent = header.id().to_owned();
        let mut at_ms = 11;
        let mut entries = vec![header];
        let mut push = |entries: &mut Vec<Entry>, payload: EntryPayload| {
            let entry = Entry::from_payload(payload, parent.clone(), at_ms);
            parent = entry.id().to_owned();
            at_ms += 1;
            entries.push(entry);
        };
        let plan_payload = |status, version| EntryPayload::Plan {
            plan_id: "plan_1".into(),
            title: "计划".into(),
            path: "local://plan_1.md".into(),
            markdown: "# 计划\n\n1. 先看代码\n".into(),
            status,
            version,
            previous_plan_id: None,
        };
        push(&mut entries, plan_payload(PlanStatus::Draft, 0));
        push(&mut entries, plan_payload(PlanStatus::Submitted, 1));
        push(
            &mut entries,
            EntryPayload::PlanFeedback {
                plan_id: "plan_1".into(),
                annotations: vec![moc_protocol::methods::PlanAnnotation {
                    id: "a1".into(),
                    start_line: 3,
                    end_line: 3,
                    quote: "先看代码".into(),
                    body: "具体到文件".into(),
                }],
                note: Some("整体保持简洁".into()),
            },
        );
        for payload in extra {
            push(&mut entries, payload);
        }
        entries
    }

    let state = SessionState::from_entries(entries(vec![EntryPayload::Plan {
        plan_id: "plan_1".into(),
        title: "计划".into(),
        path: "local://plan_1.md".into(),
        markdown: "# 计划\n\n1. 先看代码\n".into(),
        status: PlanStatus::Approved,
        version: 1,
        previous_plan_id: None,
    }]))
    .expect("记录链完整");
    assert_eq!(state.plans.len(), 1);
    assert_eq!(state.plans[0].status, PlanStatus::Approved);
    assert_eq!(state.plans[0].version, 1);
    assert_eq!(state.plans[0].annotations.len(), 1);
    assert_eq!(state.plans[0].annotations[0].body, "具体到文件");

    let revised = SessionState::from_entries(entries(Vec::new())).expect("记录链完整");
    assert_eq!(revised.plans[0].status, PlanStatus::Revised);
    assert_eq!(revised.plans[0].annotations.len(), 1);
}

#[test]
fn replay_restores_ptc_state_and_reset_clears_projection() {
    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp/project".into(),
        "p/model".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "标题".into(),
        10,
    );
    let message = Entry::from_payload(
        EntryPayload::Message {
            message: Message::user_text("run"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        header.id().to_owned(),
        11,
    );
    let put = Entry::from_payload(
        EntryPayload::PtcState {
            parent_tool_call_id: "call".into(),
            change: PtcStateChange::Put {
                key: "x".into(),
                value: serde_json::json!({"n":1}),
            },
        },
        message.id().to_owned(),
        12,
    );
    let delete = Entry::from_payload(
        EntryPayload::PtcState {
            parent_tool_call_id: "call".into(),
            change: PtcStateChange::Delete {
                key: "missing".into(),
            },
        },
        put.id().to_owned(),
        13,
    );
    let reset = Entry::from_payload(EntryPayload::ResetBoundary, delete.id().to_owned(), 14);
    let state = SessionState::from_entries(vec![header, message, put, delete, reset])
        .expect("状态日志可回放");
    assert!(state.ptc_state.is_empty());
    assert_eq!(state.ptc_state_bytes, 0);
}

#[test]
fn replay_rejects_broken_parent_chains() {
    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp/project".into(),
        "p/model".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "标题".into(),
        10,
    );
    let entry = Entry::from_payload(EntryPayload::ResetBoundary, "wrong-parent".into(), 11);
    assert!(SessionState::from_entries(vec![header, entry]).is_err());
}

/// Fusion 模型切换的边界：只换 sidekick（或切回同一 lead 的单模型）时
/// 真实发请求的模型没变，checkpoint 与用量基线都必须保留；lead 真变了
/// 只重置用量基线，文本摘要仍然有效。
#[test]
fn fusion_sidekick_switch_preserves_lead_state() {
    fn seeded(model: &str) -> SessionState {
        let mut state = SessionState::from_entries(vec![Entry::header(
            "ses_123456789abc".into(),
            "/tmp/project".into(),
            model.into(),
            SessionEngine::Standard,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            10,
        )])
        .expect("记录链完整");
        state.summary = Some(("soft".into(), "摘要".into()));
        state.summary_model = Some("p/a".into());
        state.usage = Usage {
            input_tokens: 7,
            ..Usage::default()
        };
        state.usage_at = 1;
        state
    }
    let switch = |state: &mut SessionState, next: &str| {
        state.apply_entry(Entry::from_payload(
            EntryPayload::ModelChange { model: next.into() },
            state.last_entry_id.clone(),
            11,
        ));
        assert_eq!(state.model, next);
    };

    // fusion:p/a+p/b → fusion:p/a+p/c → p/a：lead 一直是 p/a。
    let mut state = seeded("fusion:p/a+p/b");
    for next in ["fusion:p/a+p/c", "p/a"] {
        switch(&mut state, next);
        assert!(state.summary.is_some(), "{next} 不应作废 summary");
        assert_eq!(state.usage.input_tokens, 7, "{next} 不应重置 usage");
        assert_eq!(state.usage_at, 1, "{next} 不应重置 usage_at");
    }

    // lead 真变了：只重置用量基线，文本摘要跨模型仍然有效。
    switch(&mut state, "fusion:p/d+p/c");
    assert!(state.summary.is_some(), "文本 checkpoint 跨模型保留");
    assert_eq!(state.usage.input_tokens, 0);
    assert_eq!(state.usage_at, 0);
}

/// 目标快照由 GoalChange 回放；续跑计数只认当前目标 id 的轮次，
/// 换目标（新 id）归零、编辑（同 id）保留、墓碑清空。
#[test]
fn goal_snapshot_and_rounds_replay() {
    use crate::session::entries::GoalSnapshot;
    use moc_protocol::methods::{GoalPhase, GoalRoundMeta};

    fn goal(id: &str, revision: u32) -> GoalSnapshot {
        GoalSnapshot {
            id: id.into(),
            revision,
            objective: "跑到达成".into(),
            phase: GoalPhase::Active,
            blocked_reason: None,
            created_at_ms: 1,
            updated_at_ms: 1,
        }
    }
    fn round(goal_id: &str, round: u32) -> EntryPayload {
        EntryPayload::Message {
            message: Message::user_text(format!("round {round}")),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: Some(GoalRoundMeta {
                goal_id: goal_id.into(),
                revision: 1,
                round,
            }),
        }
    }

    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp/project".into(),
        "p/model".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "标题".into(),
        10,
    );
    let mut state = SessionState::from_entries(vec![header]).expect("记录链完整");
    let push = |state: &mut SessionState, payload: EntryPayload| {
        let entry = Entry::from_payload(payload, state.last_entry_id.clone(), 11);
        state.apply_entry(entry);
    };
    // 一条不相关的用户消息不该计入轮数。
    push(
        &mut state,
        EntryPayload::Message {
            message: Message::user_text("普通提问"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
    );
    push(
        &mut state,
        EntryPayload::GoalChange {
            goal: Some(goal("goal_a", 1)),
        },
    );
    push(&mut state, round("goal_a", 1));
    push(&mut state, round("goal_a", 2));
    assert_eq!(
        state.goal.as_ref().map(|goal| goal.id.as_str()),
        Some("goal_a")
    );
    assert_eq!(state.goal_rounds_started, 2);

    // 同 id 的目标编辑：计数保留（rounds 是这个目标的历史）。
    push(
        &mut state,
        EntryPayload::GoalChange {
            goal: Some(goal("goal_a", 2)),
        },
    );
    assert_eq!(state.goal_rounds_started, 2);
    assert_eq!(state.goal.as_ref().expect("目标").revision, 2);

    // 新目标（新 id）：计数归零重算。
    push(
        &mut state,
        EntryPayload::GoalChange {
            goal: Some(goal("goal_b", 1)),
        },
    );
    assert_eq!(state.goal_rounds_started, 0);
    push(&mut state, round("goal_b", 1));
    // 旧 id 的迟到轮次条目不再计入新目标。
    push(&mut state, round("goal_a", 3));
    assert_eq!(state.goal_rounds_started, 1);

    // 清除墓碑：目标与计数一起消失。
    push(&mut state, EntryPayload::GoalChange { goal: None });
    assert!(state.goal.is_none());
    assert_eq!(state.goal_rounds_started, 0);
}

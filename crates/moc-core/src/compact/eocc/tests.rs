use super::*;
use crate::session::entries::{Entry, EntryPayload, SessionOrigin};
use moc_protocol::{
    AgentMessageMeta, AgentMessageOutcome, GoalRoundMeta, SessionEngine, SessionPermission,
};

fn state() -> SessionState {
    SessionState::new(Entry::header(
        "ses_eocc_test".into(),
        "/tmp".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "交接测试".into(),
        0,
    ))
    .unwrap()
}

fn append(state: &mut SessionState, payload: EntryPayload) {
    state.apply_entry(Entry::from_payload(payload, state.last_entry_id.clone(), 1));
}

fn user(state: &mut SessionState, text: &str) {
    append(
        state,
        EntryPayload::Message {
            message: Message::user_text(text),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
    );
}

fn checkpoint(state: &mut SessionState, kept: Option<String>) {
    append(
        state,
        EntryPayload::Compaction {
            method: "eocc".into(),
            summary: "变更尚未验证".into(),
            first_kept_id: kept,
            tokens_before: 1000,
            tokens_after: 100,
            instructions: None,
            model: Some("p/m".into()),
            reminder: Some("运行任务 job-1".into()),
        },
    );
}

#[test]
fn task_budget_keeps_original_and_latest_correction_without_utf8_damage() {
    let mut state = state();
    user(
        &mut state,
        &format!("原始目标：兼容 XML{}第一条末尾约束", "长资料".repeat(5000)),
    );
    for index in 0..8 {
        user(
            &mut state,
            &format!("中间任务{index}：{}", "不可全部重放".repeat(1000)),
        );
    }
    user(&mut state, "最新纠正：不要更改默认算法");
    let task = task_message(&state, state.transcript.len()).unwrap();
    let text = task.text();
    assert!(message_tokens(&task) <= MAX_TASK_TOKENS);
    assert!(text.contains("原始目标：兼容 XML"));
    assert!(text.contains("第一条末尾约束"));
    assert!(text.contains("最新纠正：不要更改默认算法"));
    assert!(!text.contains("中间任务0"));
    assert!(text.contains("history://transcript"));
}

#[test]
fn task_replay_survives_recompaction_and_reload_without_reviving_reset_history() {
    let mut state = state();
    user(&mut state, "原始需求");
    user(&mut state, "后续纠正");
    checkpoint(&mut state, None);
    checkpoint(&mut state, None);
    let mut state = SessionState::from_entries(state.log_entries()).unwrap();
    user(&mut state, "仍在可见尾部");
    let kept = state.transcript.last().unwrap().id.clone();
    // 模拟切回 moc 后 shake 沿用 EoCC checkpoint，尾部仍可见。
    checkpoint(&mut state, Some(kept));
    let text: String = replay::model_messages(&state, None, None)
        .iter()
        .map(Message::text)
        .collect();
    for request in ["原始需求", "后续纠正", "仍在可见尾部"] {
        assert_eq!(text.matches(request).count(), 1);
    }
    assert!(text.contains("运行任务 job-1"));
    append(&mut state, EntryPayload::ResetBoundary);
    user(&mut state, "新任务");
    checkpoint(&mut state, None);
    let text: String = replay::model_messages(&state, None, None)
        .iter()
        .map(Message::text)
        .collect();
    assert!(text.contains("新任务"));
    assert!(!text.contains("原始需求"));
    assert!(!text.contains("后续纠正"));
}

#[test]
fn task_ledger_distinguishes_parent_instructions_from_reports_and_goal_nudges() {
    let mut state = state();
    state.session.parent_session_id = Some("parent".into());
    user(&mut state, "初始任务");
    user(&mut state, "<active-plan>旧注入计划</active-plan>");
    for (sender, text) in [
        ("parent", "父代理要求保留接口"),
        ("child", "子代理误报已完成"),
    ] {
        append(
            &mut state,
            EntryPayload::Message {
                message: Message::user_text(text),
                tool: None,
                finish_reason: None,
                agent: Some(AgentMessageMeta {
                    from_id: sender.into(),
                    from_title: sender.into(),
                    delivery: AgentMessageOutcome::Injected,
                }),
                goal: None,
            },
        );
    }
    append(
        &mut state,
        EntryPayload::Message {
            message: Message::user_text("自动目标续跑提示"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: Some(GoalRoundMeta {
                goal_id: "goal".into(),
                revision: 1,
                round: 1,
            }),
        },
    );
    let text = task_message(&state, state.transcript.len()).unwrap().text();
    assert!(text.contains("父代理要求保留接口"));
    assert!(!text.contains("子代理误报已完成"));
    assert!(!text.contains("自动目标续跑提示"));
    assert!(!text.contains("旧注入计划"));
}

#[test]
fn recap_budget_rejects_overflow_and_empty_state_without_padding_short_tasks() {
    let short: String = SECTIONS
        .iter()
        .map(|(heading, _)| format!("## {heading}\nNone\n"))
        .collect();
    let tokens = super::super::estimate_tokens(&short);
    assert!(accept(&short, tokens).is_ok());
    assert!(matches!(
        accept(&short, tokens - 1),
        Err(RecapRejection::TooLong(_))
    ));
    let empty: String = SECTIONS
        .iter()
        .map(|(heading, _)| format!("## {heading}\n"))
        .collect();
    assert!(matches!(
        accept(&empty, MAX_RECAP_TOKENS),
        Err(RecapRejection::Empty)
    ));
    assert!(matches!(
        accept("继续工作", MAX_RECAP_TOKENS),
        Err(RecapRejection::Sections(_))
    ));
}

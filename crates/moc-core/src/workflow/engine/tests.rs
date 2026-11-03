//! 引擎单测：重放命中、hash 不一致、settle 顺序、amend 边界、终态竞争、停滞、并发钳制。

use super::*;
use serde_json::json;

fn config(mode: RunMode) -> EngineConfig {
    EngineConfig {
        max_concurrency: 0,
        label: "t".into(),
        mode,
        resumed_from: (mode != RunMode::Fresh).then(|| "prev-run".to_string()),
    }
}

fn engine(mode: RunMode, prior: Vec<JournalRecord>) -> Engine {
    let journal = MemoryJournal { written: Vec::new(), prior };
    Engine::new(Box::new(journal), config(mode))
}

fn step(events: Vec<EngineEvent>, requests: Vec<EngineRequest>, state: ScriptState) -> Step {
    Step { events, requests, state, stall: None }
}

fn ask_req(id: u64, site: &str, seq: u32, instructions: &str) -> EngineRequest {
    EngineRequest::Ask {
        request_id: id,
        site_id: site.into(),
        actor_site_id: "actor#1".into(),
        actor_ordinal: 0,
        actor_seq: seq,
        instructions: instructions.into(),
        schema: None,
    }
}

fn actor_event(name: &str) -> EngineEvent {
    EngineEvent::CreateActor {
        site_id: "actor#1".into(),
        name: Some(name.into()),
        persona: json!({"system": "s"}),
    }
}

/// 与 request_hash 一致的 ask 输入哈希（persona 固定为 actor_event 的 {"system":"s"}）。
fn ask_hash(seq: u32, instructions: &str) -> String {
    let persona_key = input_hash(&json!({"system": "s"}));
    input_hash(&json!({
        "instructions": instructions,
        "schema": Value::Null,
        "actor_seq": seq,
        "persona_key": persona_key,
    }))
}

#[allow(clippy::too_many_arguments)]
fn ask_record(site: &str, ordinal: u32, seq: u32, instructions: &str, actor: &str, persona_key: &str, session: &str, settle: u32, result: Value) -> JournalRecord {
    let mut extra = Map::new();
    extra.insert("actor_site_id".into(), json!("actor#1"));
    extra.insert("actor_ordinal".into(), json!(0));
    extra.insert("actor_seq".into(), json!(seq));
    extra.insert("actor_name".into(), json!(actor));
    extra.insert("persona_key".into(), json!(persona_key));
    extra.insert("instructions".into(), json!(instructions));
    extra.insert("child_session_id".into(), json!(session));
    JournalRecord {
        kind: RecordKind::Ask,
        site_id: site.into(),
        ordinal,
        input_hash: ask_hash(seq, instructions),
        settle,
        // 与 driver 交付形态一致：journal 记全量（value + usage + user_turns）。
        outcome: json!({"status": "completed", "result": {"value": result, "usage": null, "user_turns": seq}}),
        extra,
    }
}

fn actor_record(site: &str, ordinal: u32, name: &str, session: &str) -> JournalRecord {
    let mut extra = Map::new();
    extra.insert("actor_name".into(), json!(name));
    extra.insert("persona_key".into(), json!(input_hash(&json!({"system": "s"}))));
    extra.insert("child_session_id".into(), json!(session));
    JournalRecord {
        kind: RecordKind::Actor,
        site_id: site.into(),
        ordinal,
        input_hash: String::new(),
        settle: 0,
        outcome: json!({"status": "completed"}),
        extra,
    }
}

#[test]
fn done_carries_script_value() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let actions = e.handle_step(step(vec![], vec![], ScriptState::Done { value: json!({"ok": 1}) }));
    match actions.iter().find(|a| matches!(a, EngineAction::Finalized(_))) {
        Some(EngineAction::Finalized(t)) => {
            assert_eq!(t.status, TerminalStatus::Completed);
            assert_eq!(t.result, Some(json!({"ok": 1})));
        }
        _ => panic!("expected finalize"),
    }
}

#[test]
fn resume_replays_world_and_ask_zero_token() {
    let world = JournalRecord {
        kind: RecordKind::World,
        site_id: "world-run#1".into(),
        ordinal: 0,
        input_hash: input_hash(&json!({"cmd": "ls", "args": ["-la"], "timeout_ms": 300_000u64, "output": Value::Null})),
        settle: 0,
        outcome: json!({"status": "completed", "result": {"stdout": "ok"}}),
        extra: Map::new(),
    };
    let persona_key = input_hash(&json!({"system": "s"}));
    let prior = vec![actor_record("actor#1", 0, "a", "sess-1"), world,
        ask_record("ask#1", 0, 1, "q", "a", &persona_key, "sess-1", 1, json!("r"))];
    let mut e = engine(RunMode::Resume, prior);
    let actions = e.handle_step(step(
        vec![actor_event("a")],
        vec![
            ask_req(1, "ask#1", 1, "q"),
            EngineRequest::WorldRun {
                request_id: 2,
                site_id: "world-run#1".into(),
                command: "ls".into(),
                args: vec!["-la".into()],
                timeout_ms: 300_000,
                output: Value::Null,
            },
        ],
        ScriptState::Running,
    ));
    let completes: Vec<u64> = actions
        .iter()
        .filter_map(|a| match a {
            EngineAction::Complete { request_id, .. } => Some(*request_id),
            _ => None,
        })
        .collect();
    assert!(completes.contains(&1), "ask 应被重放命中");
    assert!(completes.contains(&2), "world 应被重放命中");
    assert!(!actions.iter().any(|a| matches!(a, EngineAction::Dispatch(_))), "零 token：不得 dispatch");
    assert_eq!(e.usage.world_calls, 0);
}

#[test]
fn resume_hash_mismatch_errors_run() {
    let prior = vec![JournalRecord {
        kind: RecordKind::World,
        site_id: "world-run#1".into(),
        ordinal: 0,
        input_hash: "deadbeef".into(),
        settle: 0,
        outcome: json!({"status": "completed", "result": 1}),
        extra: Map::new(),
    }];
    let mut e = engine(RunMode::Resume, prior);
    let actions = e.handle_step(step(
        vec![],
        vec![EngineRequest::WorldRun {
            request_id: 1,
            site_id: "world-run#1".into(),
            command: "ls".into(),
            args: vec![],
            timeout_ms: 300_000,
            output: Value::Null,
        }],
        ScriptState::Running,
    ));
    assert_eq!(e.mismatches.len(), 1);
    let terminal = actions.iter().find_map(|a| match a {
        EngineAction::Finalized(t) => Some(t),
        _ => None,
    }).expect("mismatch must finalize");
    assert_eq!(terminal.status, TerminalStatus::Errored);
    assert_eq!(terminal.error_code.as_deref(), Some("InputHashMismatch"));
    assert!(!actions.iter().any(|a| matches!(a, EngineAction::Dispatch(_))), "不得重新执行");
}

#[test]
fn settle_queue_orders_and_dedupes() {
    let mk = |ord: u32, settle: u32, v: i64| JournalRecord {
        kind: RecordKind::World,
        site_id: "w#1".into(),
        ordinal: ord,
        input_hash: String::new(),
        settle,
        outcome: json!({"status": "completed", "result": v}),
        extra: Map::new(),
    };
    let mut q = SettleQueue::new(&[mk(0, 0, 1), mk(1, 1, 2)]);
    q.apply(mk(0, 5, 9));
    assert_eq!(q.next_record().unwrap().outcome["result"], 9);
    assert_eq!(q.next_record().unwrap().outcome["result"], 2);
    assert!(!q.has_pending());
}

#[test]
fn amend_import_matching_rules() {
    let persona_key = input_hash(&json!({"system": "s"}));
    let prior = vec![
        actor_record("actor#1", 0, "rev", "sess-rev"),
        ask_record("ask#1", 0, 1, "q1", "rev", &persona_key, "sess-rev", 1, json!("r1")),
        ask_record("ask#1", 1, 2, "q2", "rev", &persona_key, "sess-rev", 2, json!("r2")),
        ask_record("ask#9", 0, 1, "q1", "other", &persona_key, "sess-x", 3, json!("x")),
    ];
    let mut e = engine(RunMode::Amend, prior);
    let actions = e.handle_step(step(
        vec![actor_event("rev")],
        vec![
            ask_req(1, "ask#1", 1, "q1"),
            ask_req(2, "ask#1", 2, "q2"),
            ask_req(3, "ask#1", 3, "q3"),
        ],
        ScriptState::Running,
    ));
    let completes: Vec<_> = actions.iter().filter(|a| matches!(a, EngineAction::Complete { .. })).collect();
    assert_eq!(completes.len(), 2, "q1/q2 应被导入");
    // 第三个 ask 派发，且 context 是 SeedFrom（前驱会话 + 已导入 2 个）。
    let dispatch = actions.iter().find_map(|a| match a {
        EngineAction::Dispatch(DriverRequest::Ask { context, .. }) => Some(context),
        _ => None,
    }).expect("q3 应 dispatch");
    match dispatch {
        ActorContext::SeedFrom { run_id, child_session_id, user_turns } => {
            assert_eq!(run_id, "prev-run");
            assert_eq!(child_session_id, "sess-rev");
            // 截断点 = 最后一条已导入 ask 的 user_turns（ask_record 里 = seq 2）。
            assert_eq!(*user_turns, 2);
        }
        other => panic!("expected SeedFrom, got {other:?}"),
    }
    // 分叉：persona 不同 → ActorDiverged（非 errored）+ dispatch Fresh。
    let prior2 = vec![ask_record("ask#1", 0, 1, "q1", "rev", "OTHER", "sess-rev", 0, json!("r1"))];
    let mut e2 = engine(RunMode::Amend, prior2);
    let actions = e2.handle_step(step(vec![actor_event("rev")], vec![ask_req(1, "ask#1", 1, "q1")], ScriptState::Running));
    assert!(actions.iter().any(|a| matches!(a, EngineAction::Session(SessionEvent::ActorDiverged { name }) if name == "rev")));
    assert!(!actions.iter().any(|a| matches!(a, EngineAction::Finalized(_))));
    // 指令逐字不同 → 不导入。
    let mut e3 = engine(RunMode::Amend, vec![
        ask_record("ask#1", 0, 1, "DIFFERENT", "rev", &persona_key, "sess-rev", 0, json!("r1")),
    ]);
    let actions = e3.handle_step(step(vec![actor_event("rev")], vec![ask_req(1, "ask#1", 1, "q1")], ScriptState::Running));
    assert!(!actions.iter().any(|a| matches!(a, EngineAction::Complete { request_id: 1, .. })));
    assert!(actions.iter().any(|a| matches!(a, EngineAction::Dispatch(_))));
}

#[test]
fn actor_context_existing_after_session_record() {
    let mut e = engine(RunMode::Fresh, vec![]);
    e.handle_step(step(vec![actor_event("a")], vec![ask_req(1, "ask#1", 1, "q")], ScriptState::Running));
    e.actor_session_created("actor#1", 0, "sess-9");
    // 同 actor FIFO：先 settle 掉在飞 ask，第二个才会派发。
    e.settle(1, DriverOutcome::Completed(json!({"value": "v"})));
    let actions = e.handle_step(step(vec![], vec![ask_req(2, "ask#2", 2, "q2")], ScriptState::Running));
    let ctx = actions.iter().find_map(|a| match a {
        EngineAction::Dispatch(DriverRequest::Ask { context, .. }) => Some(context.clone()),
        _ => None,
    }).expect("second ask must dispatch");
    assert_eq!(ctx, ActorContext::Existing { child_session_id: "sess-9".into() });
    // resume：prior actor 记录 → Existing。
    let mut e2 = engine(RunMode::Resume, vec![actor_record("actor#1", 0, "a", "sess-1")]);
    let actions = e2.handle_step(step(vec![actor_event("a")], vec![ask_req(1, "ask#9", 1, "new q")], ScriptState::Running));
    let ctx = actions.iter().find_map(|a| match a {
        EngineAction::Dispatch(DriverRequest::Ask { context, .. }) => Some(context.clone()),
        _ => None,
    }).expect("new ask dispatches");
    assert_eq!(ctx, ActorContext::Existing { child_session_id: "sess-1".into() });
}

#[test]
fn terminal_first_writer_wins() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let mut actions = Vec::new();
    e.finish(TerminalStatus::Completed, Some(json!("done")), None, None, &mut actions);
    e.finish(TerminalStatus::Errored, None, Some("E".into()), Some("late".into()), &mut actions);
    let terms: Vec<_> = actions.iter().filter_map(|a| match a {
        EngineAction::Finalized(t) => Some(t),
        _ => None,
    }).collect();
    assert_eq!(terms.len(), 1);
    assert_eq!(terms[0].status, TerminalStatus::Completed);
    assert!(e.handle_step(step(vec![], vec![], ScriptState::Running)).is_empty());
}

#[test]
fn runtime_duplicate_actor_name_errors() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let actions = e.handle_step(step(
        vec![
            actor_event("x"),
            EngineEvent::CreateActor { site_id: "actor#2".into(), name: Some("x".into()), persona: json!({}) },
        ],
        vec![],
        ScriptState::Running,
    ));
    let terminal = actions.iter().find_map(|a| match a {
        EngineAction::Finalized(t) => Some(t),
        _ => None,
    }).expect("dup name must finalize");
    assert_eq!(terminal.error_code.as_deref(), Some("DuplicateActorName"));
}

#[test]
fn finish_cancels_in_flight_asks() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let actions = e.handle_step(step(
        vec![actor_event("a")],
        vec![ask_req(1, "ask#1", 1, "q")],
        ScriptState::Running,
    ));
    assert!(actions.iter().any(|a| matches!(a, EngineAction::Dispatch(DriverRequest::Ask { .. }))));
    let mut actions = Vec::new();
    e.finish(TerminalStatus::Completed, Some(json!("done")), None, None, &mut actions);
    assert!(actions.iter().any(|a| matches!(a, EngineAction::Dispatch(DriverRequest::CancelAsk { .. }))));
    assert!(actions.iter().any(|a| matches!(a, EngineAction::Complete { request_id: 1, .. })));
}

#[test]
fn concurrency_cap_queues_asks() {
    let mut e = Engine::new(
        Box::new(MemoryJournal::default()),
        EngineConfig { max_concurrency: 1, label: "t".into(), mode: RunMode::Fresh, resumed_from: None },
    );
    let actions = e.handle_step(step(
        vec![actor_event("a")],
        vec![ask_req(1, "ask#1", 1, "q1"), ask_req(2, "ask#1", 2, "q2")],
        ScriptState::Running,
    ));
    let dispatches = actions.iter().filter(|a| matches!(a, EngineAction::Dispatch(DriverRequest::Ask { .. }))).count();
    assert_eq!(dispatches, 1);
    let actions = e.settle(1, DriverOutcome::Completed(json!("r")));
    assert!(actions.iter().any(|a| matches!(a, EngineAction::Dispatch(DriverRequest::Ask { site_id, ordinal: 1, .. }) if site_id == "ask#1")));
}

#[test]
fn stall_and_deadlock_error_run() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let mut s = step(vec![], vec![], ScriptState::Running);
    s.stall = Some(StallInfo { waiting: vec!["ch-1".into()] });
    let actions = e.handle_step(s);
    let t = actions.iter().find_map(|a| match a {
        EngineAction::Finalized(t) => Some(t),
        _ => None,
    }).expect("deadlock must finalize");
    assert_eq!(t.error_code.as_deref(), Some("ChannelDeadlock"));
    assert!(t.error_message.as_ref().unwrap().contains("ch-1"));

    let mut e = engine(RunMode::Fresh, vec![]);
    let mut s = step(vec![], vec![], ScriptState::Running);
    s.stall = Some(StallInfo { waiting: vec![] });
    let actions = e.handle_step(s);
    let t = actions.iter().find_map(|a| match a {
        EngineAction::Finalized(t) => Some(t),
        _ => None,
    }).expect("stall must finalize");
    assert_eq!(t.error_code.as_deref(), Some("ScriptStalled"));
}

#[test]
fn journal_file_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let mut j = FileJournal::open(dir.path()).unwrap();
    let rec = JournalRecord {
        kind: RecordKind::World,
        site_id: "w#1".into(),
        ordinal: 0,
        input_hash: "aa".into(),
        settle: 0,
        outcome: json!({"status": "completed", "result": 1}),
        extra: Map::new(),
    };
    j.append(&rec).unwrap();
    let j2 = FileJournal::open(dir.path()).unwrap();
    assert_eq!(j2.prior().len(), 1);
    assert_eq!(j2.prior()[0].site_id, "w#1");
}

// —— A1：ask 交付给 guest 的只有 value（journal 记全量）——

#[test]
fn ask_complete_delivers_only_value() {
    let mut e = engine(RunMode::Fresh, vec![]);
    e.handle_step(step(
        vec![actor_event("rev")],
        vec![ask_req(1, "ask#1", 1, "q")],
        ScriptState::Running,
    ));
    let actions = e.settle(
        1,
        DriverOutcome::Completed(json!({
            "value": "v1",
            "usage": {"spent_tokens": 0},
            "user_turns": 2,
        })),
    );
    let outcome = actions
        .iter()
        .find_map(|a| match a {
            EngineAction::Complete { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("complete");
    assert!(
        matches!(outcome, DriverOutcome::Completed(v) if *v == json!("v1")),
        "guest 只应拿到 value，实际 {outcome:?}"
    );
}

#[test]
fn resume_replay_delivers_only_value() {
    let persona_key = input_hash(&json!({"system": "s"}));
    let prior = vec![
        actor_record("actor#1", 0, "rev", "sess-1"),
        ask_record("ask#1", 0, 1, "q", "rev", &persona_key, "sess-1", 1, json!("r1")),
    ];
    let mut e = engine(RunMode::Resume, prior);
    let actions = e.handle_step(step(
        vec![actor_event("rev")],
        vec![ask_req(1, "ask#1", 1, "q")],
        ScriptState::Running,
    ));
    let outcome = actions
        .iter()
        .find_map(|a| match a {
            EngineAction::Complete { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("replay hit");
    assert!(
        matches!(outcome, DriverOutcome::Completed(v) if *v == json!("r1")),
        "重放命中回执只应含 value，实际 {outcome:?}"
    );
}

#[test]
fn amend_import_delivers_only_value() {
    let persona_key = input_hash(&json!({"system": "s"}));
    let prior = vec![
        actor_record("actor#1", 0, "rev", "sess-1"),
        ask_record("ask#1", 0, 1, "q", "rev", &persona_key, "sess-1", 1, json!("r1")),
    ];
    let mut e = engine(RunMode::Amend, prior);
    let actions = e.handle_step(step(
        vec![actor_event("rev")],
        vec![ask_req(1, "ask#1", 1, "q")],
        ScriptState::Running,
    ));
    let outcome = actions
        .iter()
        .find_map(|a| match a {
            EngineAction::Complete { outcome, .. } => Some(outcome),
            _ => None,
        })
        .expect("import hit");
    assert!(
        matches!(outcome, DriverOutcome::Completed(v) if *v == json!("r1")),
        "导入命中回执只应含 value，实际 {outcome:?}"
    );
}

// —— A2/A3：同 actor FIFO + finish 清理 ——

#[test]
fn same_actor_asks_are_fifo() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let actions = e.handle_step(step(
        vec![actor_event("rev")],
        vec![ask_req(1, "ask#1", 1, "q1"), ask_req(2, "ask#2", 1, "q2")],
        ScriptState::Running,
    ));
    assert_eq!(
        actions.iter().filter(|a| matches!(a, EngineAction::Dispatch(_))).count(),
        1,
        "同 actor 并发 ask 只应先 dispatch 一个"
    );
    let actions = e.settle(1, DriverOutcome::Completed(json!({"value": "v"})));
    assert_eq!(
        actions.iter().filter(|a| matches!(a, EngineAction::Dispatch(_))).count(),
        1,
        "settle 后应派发等位的第二个 ask"
    );
}

#[test]
fn finish_drops_pending_asks() {
    let mut e = engine(RunMode::Fresh, vec![]);
    e.handle_step(step(
        vec![actor_event("rev")],
        vec![ask_req(1, "ask#1", 1, "q")],
        ScriptState::Running,
    ));
    let mut actions = Vec::new();
    e.finish(TerminalStatus::Completed, Some(json!("done")), None, None, &mut actions);
    assert!(
        actions.iter().any(|a| matches!(
            a,
            EngineAction::Complete {
                outcome: DriverOutcome::Failed { code, .. },
                ..
            } if code == "CANCELLED"
        )),
        "finish 应取消在飞 ask"
    );
    // 迟到的 settle：pending 已摘除，不得二次回执或追加 journal。
    let late = e.settle(1, DriverOutcome::Completed(json!({"value": "x"})));
    assert!(late.is_empty(), "迟到 settle 不应产生 action");
}

// —— A4/A5：artifact 声明与 report 校验 ——

fn declare_event(id: &str, member: &str, spec: Value) -> EngineEvent {
    EngineEvent::DeclareArtifact {
        site_id: "artifact.table#1".into(),
        member: member.into(),
        spec: json!([id, spec]),
    }
}

#[test]
fn declare_artifact_registers_and_dedups() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let spec = json!({"columns": [{"field": "k", "label": "K"}], "key": "k"});
    let actions = e.handle_step(step(
        vec![declare_event("t1", "table", spec.clone())],
        vec![],
        ScriptState::Running,
    ));
    let declared = actions.iter().find_map(|a| match a {
        EngineAction::Session(SessionEvent::DeclareArtifact { artifact_id, spec, .. }) => {
            Some((artifact_id.clone(), spec.clone()))
        }
        _ => None,
    });
    let (id, emitted) = declared.expect("declare 应发 SessionEvent");
    assert_eq!(id, Some("t1".into()));
    assert_eq!(emitted, spec, "spec 只应是对象部分");
    // 相同重声明：no-op，不再发事件。
    let actions = e.handle_step(step(
        vec![declare_event("t1", "table", spec)],
        vec![],
        ScriptState::Running,
    ));
    assert!(actions.is_empty(), "相同重声明应 no-op");
    // 不同 spec 重声明：整 run errored。
    let mut e = engine(RunMode::Fresh, vec![]);
    e.handle_step(step(vec![declare_event("t1", "table", json!({"key": "k"}))], vec![], ScriptState::Running));
    let actions = e.handle_step(step(
        vec![declare_event("t1", "table", json!({"key": "other"}))],
        vec![],
        ScriptState::Running,
    ));
    assert!(
        actions.iter().any(|a| matches!(a, EngineAction::Finalized(t) if t.status == TerminalStatus::Errored)),
        "不同 spec 重声明应 errored"
    );
}

#[test]
fn report_requires_declared_preset() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let actions = e.handle_step(step(
        vec![EngineEvent::Report {
            site_id: "s".into(),
            item: json!({"k": 1}),
            artifact_id: Some("nope".into()),
        }],
        vec![],
        ScriptState::Running,
    ));
    assert!(
        actions.iter().any(|a| matches!(a, EngineAction::Finalized(t) if t.status == TerminalStatus::Errored)),
        "report 到未声明 id 应 errored"
    );
}

#[test]
fn report_item_cap_errors_run() {
    let mut e = engine(RunMode::Fresh, vec![]);
    let big = "x".repeat(facade::REPORT_MAX_ITEM_BYTES + 1);
    let actions = e.handle_step(step(
        vec![EngineEvent::Report {
            site_id: "s".into(),
            item: json!(big),
            artifact_id: None,
        }],
        vec![],
        ScriptState::Running,
    ));
    assert!(
        actions.iter().any(|a| matches!(a, EngineAction::Finalized(t) if t.status == TerminalStatus::Errored)),
        "单条 report 超 1 MiB 应 errored"
    );
}

#[test]
fn resume_replay_registers_artifact_member() {
    // prior：artifact.markdown("notes") 已发布并落 journal。
    let spec = json!(["notes", "body", null]);
    let prior = vec![JournalRecord {
        kind: RecordKind::Artifact,
        site_id: "artifact.markdown#1".into(),
        ordinal: 0,
        input_hash: input_hash(&json!({"member": "markdown", "spec": spec})),
        settle: 0,
        outcome: json!({"status": "completed", "result": {"id": "notes", "version": 1}}),
        extra: Map::new(),
    }];
    let mut e = engine(RunMode::Resume, prior);
    let actions = e.handle_step(step(
        vec![],
        vec![EngineRequest::PublishArtifact {
            request_id: 1,
            site_id: "artifact.markdown#1".into(),
            member: "markdown".into(),
            spec,
        }],
        ScriptState::Running,
    ));
    // 重放命中（且 id/member 已登记）。
    assert!(
        actions.iter().any(|a| matches!(
            a,
            EngineAction::Complete { outcome: DriverOutcome::Completed(_), .. }
        )),
        "publish 应重放命中"
    );
    // 同 id 再声明 preset → member 冲突 → 整 run errored。
    let actions = e.handle_step(step(
        vec![declare_event("notes", "table", json!({"key": "k"}))],
        vec![],
        ScriptState::Running,
    ));
    assert!(
        actions.iter().any(|a| matches!(a, EngineAction::Finalized(t) if t.status == TerminalStatus::Errored)),
        "重放登记的 member 冲突应 errored"
    );
}

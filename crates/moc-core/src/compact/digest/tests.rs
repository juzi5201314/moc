use moc_llm::{Message, ToolCall};
use moc_protocol::methods::{SessionEngine, SessionPermission, TodoItem, TodoPhase};

use super::*;
use crate::session::entries::{Entry, EntryPayload, SessionOrigin};

fn state_with_calls(
    calls: Vec<(&str, serde_json::Value)>,
    results: Vec<(&str, bool)>,
) -> SessionState {
    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "t".into(),
        1,
    );
    let mut parent = header.id().to_owned();
    let mut entries = vec![header];
    let mut message = Message::assistant_text("");
    for (index, (name, arguments)) in calls.into_iter().enumerate() {
        message
            .content
            .push(moc_llm::ContentPart::ToolCall(ToolCall {
                id: format!("call_{index}_{name}"),
                name: name.into(),
                arguments: arguments.to_string(),
            }));
    }
    entries.push(Entry::from_payload(
        EntryPayload::Message {
            message: message.clone(),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        parent.clone(),
        2,
    ));
    parent = entries.last().expect("entry").id().to_owned();
    for (id, is_error) in results {
        entries.push(Entry::from_payload(
            EntryPayload::Message {
                message: Message::tool_result(id, "ok", is_error),
                tool: None,
                finish_reason: None,
                agent: None,
                goal: None,
            },
            parent.clone(),
            3,
        ));
        parent = entries.last().expect("entry").id().to_owned();
    }
    SessionState::from_entries(entries).expect("state")
}

#[test]
fn edited_files_only_counts_successful_writes() {
    let state = state_with_calls(
        vec![
            ("edit", serde_json::json!({"path": "b.rs"})),
            ("write", serde_json::json!({"path": "a.rs"})),
            ("edit", serde_json::json!({"path": "c.rs"})),
        ],
        vec![("call_0_edit", false), ("call_1_write", false)],
    );
    let files = edited_files(&state);
    assert_eq!(files, vec!["a.rs".to_owned(), "b.rs".to_owned()]);
}

#[test]
fn commands_are_deduplicated_and_ordered() {
    let state = state_with_calls(
        vec![
            ("bash", serde_json::json!({"command": "cargo test"})),
            ("bash", serde_json::json!({"command": "cargo test"})),
            ("bash", serde_json::json!({"command": "cargo fmt"})),
        ],
        vec![
            ("call_0_bash", false),
            ("call_1_bash", false),
            ("call_2_bash", false),
        ],
    );
    assert_eq!(
        commands(&state),
        vec!["cargo test".to_owned(), "cargo fmt".to_owned()]
    );
}

#[test]
fn todo_lines_map_statuses_and_reasons() {
    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "t".into(),
        1,
    );
    let parent = header.id().to_owned();
    let todos = Entry::from_payload(
        EntryPayload::Todos {
            phases: vec![TodoPhase {
                phase: "phase".into(),
                items: vec![
                    TodoItem {
                        task: "done".into(),
                        status: TodoStatus::Completed,
                        reason: None,
                    },
                    TodoItem {
                        task: "open".into(),
                        status: TodoStatus::InProgress,
                        reason: None,
                    },
                    TodoItem {
                        task: "blocked".into(),
                        status: TodoStatus::Blocked,
                        reason: Some("等待评审".into()),
                    },
                    TodoItem {
                        task: "dropped".into(),
                        status: TodoStatus::Abandoned,
                        reason: None,
                    },
                ],
            }],
        },
        parent,
        2,
    );
    let state = SessionState::from_entries(vec![header, todos]).expect("state");
    let lines = todo_lines(&state);
    assert_eq!(
        lines.lines,
        vec![
            "- [in_progress] todo-1-2: open".to_owned(),
            "- [pending] todo-1-3: blocked (等待评审)".to_owned()
        ]
    );
    assert_eq!(lines.completed, 1);
    assert_eq!(lines.cancelled, 1);
}

#[test]
fn fallback_without_previous_builds_required_sections() {
    let state = state_with_calls(vec![], vec![]);
    let history = vec![
        Message::user_text("把 TTL 改成 45 秒"),
        Message::assistant_text("已改好 window.rs"),
    ];
    let checkpoint = fallback_checkpoint(&state, &history, None, None);
    for section in [
        "## Primary Request and Intent",
        "## Files and Code",
        "## Pending Jobs",
        "## Current Work",
        "## Next Step",
        "## Critical Context",
    ] {
        assert!(checkpoint.contains(section), "缺少 {section}");
    }
    assert!(checkpoint.contains("把 TTL 改成 45 秒"));
    assert!(checkpoint.contains("已改好 window.rs"));
}

#[test]
fn decisions_do_not_promote_reference_material_or_status_updates() {
    let history = vec![
        Message::tool_result(
            "page",
            "| Rejected proposal | Social | Anywhere | Affects mood |",
            false,
        ),
        Message::assistant_text("预分配 A* 已实现，它禁止从两侧水障斜切。"),
        Message::user_text("禁止在 legacy.rs 引入 Redis"),
    ];
    let found = decisions(&history, None);
    assert_eq!(found.len(), 1);
    assert!(found[0].clause.contains("legacy.rs"));
}

#[test]
fn new_user_decision_precedes_a_full_old_checkpoint() {
    let previous = format!(
        "## Decisions and Rejected Options\n{}",
        (0..12)
            .map(|i| format!("- rejected: OldOption{i} — 原因\n"))
            .collect::<String>()
    );
    let history = vec![Message::user_text("禁止把 XML mod 物品写死")];
    let found = decisions(&history, Some(&previous));
    assert!(found[0].clause.contains("XML mod"));
}

#[test]
fn distinct_long_questions_are_not_merged_by_their_display_prefix() {
    let common = "共同背景".repeat(60);
    let mut answers = Vec::new();
    push_answer(&mut answers, &format!("{common}数据库？"), "SQLite", true);
    push_answer(&mut answers, &format!("{common}缓存？"), "Redis", true);
    assert_eq!(answers.len(), 2);
    assert_eq!(answers[0].answer, "SQLite");
    assert_eq!(answers[1].answer, "Redis");
}

#[test]
fn same_question_keeps_only_the_latest_user_answer() {
    let previous = "## Decisions and Rejected Options\n- asked: 用哪种存储？ → Redis\n";
    let history = vec![
        ask_call(
            "q",
            serde_json::json!([{"id":"db","question":"用哪种存储？"}]),
        ),
        Message::tool_result("q", "SQLite", false),
    ];
    let found = ask_answers(&history, Some(previous));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].answer, "SQLite");
    assert!(found[0].enforceable);
    let mut corrected = history;
    corrected.push(Message::user_text("撤回，改用 PostgreSQL"));
    assert!(!ask_answers(&corrected, Some(previous))[0].enforceable);
}

#[test]
fn decisions_capture_rejected_options_and_constraints() {
    let history = vec![
        Message::user_text("我们否决了用 lru crate 的 LRU 缓存来管理窗口，pinned 会违反淘汰顺序。"),
        Message::assistant_text("已经记下这次评审结论。"),
        Message::user_text(
            "| 把窗口状态迁到 Redis，多进程共享 | 否决 | 跨进程一致性与 RTT 都不可接受 |",
        ),
    ];
    let decisions = decisions(&history, None);
    let anchors: Vec<&str> = decisions
        .iter()
        .flat_map(|decision| decision.anchors.iter().map(String::as_str))
        .collect();
    assert!(anchors.contains(&"lru"), "{anchors:?}");
    assert!(anchors.contains(&"Redis"), "{anchors:?}");
    assert!(
        decisions
            .iter()
            .any(|decision| decision.clause.contains("否决")),
        "决策原文要能直接照抄"
    );
}

#[test]
fn plan_steps_require_a_numbered_plan() {
    let plan = vec![Message::user_text(
        "收尾计划共 5 步：第 1 步改 TTL，第 2 步加骨架，第 3 步移计数，第 4 步更新文档，第 5 步加日志。",
    )];
    assert_eq!(
        plan_steps(&plan, None, None),
        vec!["第 1 步", "第 2 步", "第 3 步", "第 4 步", "第 5 步"]
    );

    // 零散提一句"第 3 步"不构成计划。
    let scattered = vec![Message::user_text("继续第 3 步。")];
    assert!(plan_steps(&scattered, None, None).is_empty());
}

#[test]
fn plan_steps_come_from_the_approved_plan_markdown() {
    let markdown = "# 重构计划\n\n## 实施步骤\n1. 改 A\n2. 改 B\n3. 改 C\n";
    assert_eq!(
        plan_steps(&[], None, Some(markdown)),
        vec!["第 1 步", "第 2 步", "第 3 步"]
    );
    // 没有计划文档时不会有计划步骤。
    assert!(plan_steps(&[], None, None).is_empty());
}

#[test]
fn decisions_and_plan_survive_from_the_previous_checkpoint() {
    // 第三代压缩时原始请求已不在可见历史里，只剩上一版 checkpoint。
    let previous = "## Plan Status\n- 第 1 步 done；第 2 步 current；第 3 步 pending\n\
                    ## Decisions and Rejected Options\n- rejected: Redis — 一致性不可接受\n";
    let history = vec![Message::user_text("继续")];
    assert_eq!(
        plan_steps(&history, Some(previous), None),
        vec!["第 1 步", "第 2 步", "第 3 步"]
    );
    let decisions = decisions(&history, Some(previous));
    let anchors: Vec<&str> = decisions
        .iter()
        .flat_map(|decision| decision.anchors.iter().map(String::as_str))
        .collect();
    assert!(anchors.contains(&"Redis"), "{anchors:?}");
    assert!(
        !anchors.contains(&"rejected"),
        "识别词本身不该变成必保留事实：{anchors:?}"
    );
}

fn ask_call(id: &str, questions: serde_json::Value) -> Message {
    Message {
        role: Role::Assistant,
        content: moc_llm::message_content![ContentPart::ToolCall(ToolCall::new(
            id,
            "ask",
            serde_json::json!({ "questions": questions }).to_string(),
        ))],
    }
}

#[test]
fn ask_answers_pair_questions_with_user_replies() {
    let call = ask_call(
        "c1",
        serde_json::json!([
            {"id":"db","question":"用哪种存储？","options":[{"label":"PostgreSQL"},{"label":"SQLite"}]},
            {"id":"cache","question":"要不要缓存？","options":[{"label":"要"},{"label":"不要"}]}
        ]),
    );
    let result = Message::tool_result(
        "c1",
        "User answers:\ndb: PostgreSQL\ncache: The user did not answer.",
        false,
    );
    let answers = ask_answers(&[call, result], None);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].question, "用哪种存储？");
    assert_eq!(answers[0].answer, "PostgreSQL");
}

#[test]
fn ask_answers_skip_uninvolved_and_missing_replies() {
    let questions = serde_json::json!([{"id":"q","question":"选哪个？","options":[{"label":"A"},{"label":"B"}]}]);
    let auto = Message::tool_result(
        "c1",
        "B (auto-selected recommended option; the user was not involved)",
        false,
    );
    assert!(ask_answers(&[ask_call("c1", questions.clone()), auto], None).is_empty());
    let missing = Message::tool_result("c2", "The user did not answer.", false);
    assert!(ask_answers(&[ask_call("c2", questions.clone()), missing], None).is_empty());
    let real = Message::tool_result("c3", "B", false);
    let answers = ask_answers(&[ask_call("c3", questions), real], None);
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].answer, "B");
}

#[test]
fn ask_answers_recover_pairs_from_the_previous_checkpoint() {
    // must-preserve 让模型把用户决定记成 `- asked: … → …`；下一代按同一标记回收。
    let previous = "## Decisions and Rejected Options\n\
                    - asked: 用哪种存储？ → PostgreSQL\n\
                    - rejected: lru — 淘汰顺序不可接受\n";
    let answers = ask_answers(&[], Some(previous));
    assert_eq!(answers.len(), 1);
    assert_eq!(answers[0].question, "用哪种存储？");
    assert_eq!(answers[0].answer, "PostgreSQL");
}

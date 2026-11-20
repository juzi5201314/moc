use super::*;
use crate::agents::messaging::resolve_targets;
use crate::agents::spawn::resolve_model;
use crate::agents::waiting::await_any;
use crate::agents::waiting::wait_snapshot;
use moc_protocol::methods::AgentStatus;

mod finishing;
#[path = "../../tests/integration/support/mod.rs"]
mod support;

#[test]
fn explore_model_inherits_parent_and_never_updates_last_selection() {
    let root = tempfile::tempdir().unwrap();
    std::fs::write(root.path().join("config.toml"), "[providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n[providers.p.models.a]\n[providers.p.models.b]\n").unwrap();
    let mut config = crate::ResolvedConfig::load(&crate::CoreConfig {
        home: root.path().to_owned(),
        cwd: root.path().to_owned(),
        ..Default::default()
    })
    .unwrap();
    crate::config::selection::save(root.path(), "p/a:off").unwrap();
    assert_eq!(
        resolve_model(&config, "p/b:off", &config.roles.explore).unwrap(),
        "p/b:off"
    );
    assert_eq!(
        resolve_model(&config, "fusion:p/b:off+p/a:off", "inherit").unwrap(),
        "p/b:off"
    );
    config.roles.explore = "p/a:off".into();
    assert_eq!(
        resolve_model(&config, "p/b:off", &config.roles.explore).unwrap(),
        "p/a:off"
    );
    for removed in ["default", "plan"] {
        assert!(resolve_model(&config, "p/b:off", removed).is_err());
    }
    assert_eq!(
        crate::config::selection::load(root.path())
            .unwrap()
            .as_deref(),
        Some("p/a:off")
    );
}

/// 与 `spawn` 登记的运行态一致，只保留等待语义需要的字段。
fn running_agent(notify: Arc<Notify>) -> AgentRuntime {
    AgentRuntime {
        info: AgentInfo {
            id: "ses_agent00001".into(),
            parent_id: "ses_parent0001".into(),
            title: "子代理".into(),
            model: "p/m".into(),
            cwd: "/tmp/project".into(),
            preset: None,
            status: AgentStatus::Running,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_message: None,
        },
        cancel: CancellationToken::new(),
        notify,
        message: None,
        finish_reason: None,
        yielded: None,
        structured: None,
        awaited: false,
        activity: None,
    }
}

fn target(id: &str) -> SessionInfo {
    SessionInfo {
        id: id.into(),
        parent_session_id: Some("ses_parent0001".into()),
        forked_from: None,
        agent_preset: None,
        delegation_depth: 1,
        title: "子代理".into(),
        model: "p/m".into(),
        cwd: "/tmp/project".into(),
        engine: moc_protocol::methods::SessionEngine::Standard,
        autonomy: moc_protocol::methods::SessionAutonomy::Default,
        plan_active: false,
        fast_mode: false,
        engine_locked: false,
        permission: moc_protocol::methods::SessionPermission::Workspace,
        created_at_ms: 1,
        updated_at_ms: 1,
        lock: None,
    }
}

#[tokio::test]
async fn completion_between_status_check_and_wait_is_not_lost() {
    let notify = Arc::new(Notify::new());
    let mut registry = AgentRegistry::default();
    registry
        .items
        .insert("ses_agent00001".into(), running_agent(Arc::clone(&notify)));
    // 等待者和终态读取在同一临界区内完成；此时代理还在运行。
    let snapshot = wait_snapshot(&registry, &[target("ses_agent00001")]);
    assert!(!snapshot.settled);
    assert_eq!(snapshot.watched.len(), 1);
    assert_eq!(snapshot.rows[0].entry.status, AgentStatus::Running);
    let mut watched = snapshot.watched;
    // 完成路径按 `finish` 的顺序执行：先写终态，再唤醒等待者。
    {
        let runtime = registry
            .items
            .get_mut("ses_agent00001")
            .expect("代理已登记");
        runtime.info.status = AgentStatus::Completed;
        runtime.info.last_message = Some("子代理完成".into());
        runtime.message = Some(Message::assistant_text("子代理完成"));
    }
    notify.notify_waiters();
    // 唤醒发生在等待者建立之后：必须立即就绪，不能依赖后续通知。
    // 上限取 5s，只用来兜住「永远不会被唤醒」这种缺陷，避免并行测试的调度延迟造成误报。
    tokio::time::timeout(Duration::from_secs(5), await_any(&mut watched))
        .await
        .expect("完成通知不得丢失");
}

#[tokio::test]
async fn terminal_agent_returns_its_completion_snapshot() {
    let notify = Arc::new(Notify::new());
    let mut registry = AgentRegistry::default();
    registry
        .items
        .insert("ses_agent00001".into(), running_agent(notify));
    {
        let runtime = registry
            .items
            .get_mut("ses_agent00001")
            .expect("代理已登记");
        runtime.info.status = AgentStatus::Failed;
        runtime.message = Some(Message::assistant_text("上游失败"));
    }
    let snapshot = wait_snapshot(&registry, &[target("ses_agent00001")]);
    assert!(snapshot.settled);
    assert!(snapshot.watched.is_empty());
    let entry = &snapshot.rows[0].entry;
    assert_eq!(entry.status, AgentStatus::Failed);
    assert_eq!(entry.message.as_ref().expect("失败消息").text(), "上游失败");
    // 注册表里没有条目：按 `Idle` 回报。
    let absent = wait_snapshot(&registry, &[target("ses_missing0001")]);
    assert!(absent.settled);
    assert_eq!(absent.rows[0].entry.status, AgentStatus::Idle);
}

#[test]
fn preview_text_truncates_on_char_boundary() {
    assert_eq!(preview_text("abc", 3), ("abc".to_owned(), false));
    assert_eq!(preview_text("abcd", 3), ("abc…".to_owned(), true));
    assert_eq!(preview_text("你好世界", 2), ("你好…".to_owned(), true));
}

#[test]
fn broadcast_and_explicit_targets_resolve_against_peers() {
    let peers = Peers {
        me: SessionInfo {
            parent_session_id: None,
            ..target("ses_parent0001")
        },
        children: vec![
            AgentEntry {
                session: target("ses_child00001"),
                status: AgentStatus::Running,
            },
            AgentEntry {
                session: target("ses_child00002"),
                status: AgentStatus::Completed,
            },
        ],
        parent: None,
        siblings: Vec::new(),
    };
    let broadcast = resolve_targets(&peers, BROADCAST_TARGET, false).expect("广播解析");
    assert_eq!(broadcast.len(), 1, "广播只覆盖运行中的直接子代理");
    assert_eq!(broadcast[0].session.id, "ses_child00001");
    assert!(resolve_targets(&peers, PARENT_TARGET, false).is_err());
    assert!(resolve_targets(&peers, "ses_parent0001", false).is_err());
    assert!(resolve_targets(&peers, "ses_unknown0001", false).is_err());
    let explicit = resolve_targets(&peers, "ses_child00002", false).expect("显式目标");
    assert_eq!(explicit[0].session.id, "ses_child00002");
}

#[test]
fn pruning_keeps_running_agents_and_the_newest_finished_entries() {
    let mut registry = AgentRegistry::default();
    for index in 0..(MAX_TERMINAL_AGENTS + 5) {
        let mut runtime = running_agent(Arc::new(Notify::new()));
        runtime.info.id = format!("ses_done{index:05}");
        runtime.info.status = AgentStatus::Completed;
        runtime.info.updated_at_ms = u64::try_from(index).expect("索引可表示为时间戳");
        registry.items.insert(runtime.info.id.clone(), runtime);
    }
    let mut active = running_agent(Arc::new(Notify::new()));
    active.info.id = "ses_running0001".into();
    registry.items.insert(active.info.id.clone(), active);

    prune_terminal_agents(&mut registry);

    let terminal_count = registry
        .items
        .values()
        .filter(|runtime| terminal(runtime.info.status))
        .count();
    assert_eq!(terminal_count, MAX_TERMINAL_AGENTS);
    assert!(
        registry.items.contains_key("ses_running0001"),
        "运行中的条目必须保留"
    );
    assert!(
        !registry.items.contains_key("ses_done00000"),
        "最旧的终态条目被回收"
    );
    assert!(
        registry
            .items
            .contains_key(&format!("ses_done{:05}", MAX_TERMINAL_AGENTS + 4)),
        "最新的终态条目必须保留"
    );
    // 没超过上限时，再回收也不应删除条目。
    prune_terminal_agents(&mut registry);
    assert_eq!(registry.items.len(), MAX_TERMINAL_AGENTS + 1);
}

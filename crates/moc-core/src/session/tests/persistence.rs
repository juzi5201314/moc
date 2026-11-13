use super::*;

#[tokio::test]
async fn plugin_state_isolated_and_persistent_without_transcript_entries() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let session = create_plugin_state_session(&table).await;
    table
        .plugin_state_set(&session.id, "demo", "count", serde_json::json!(1))
        .await
        .expect("写插件状态");
    assert_eq!(
        table
            .plugin_state_get(&session.id, "demo", "count")
            .await
            .expect("读插件状态"),
        Some(serde_json::json!(1))
    );
    assert_eq!(
        table
            .plugin_state_keys(&session.id, "other")
            .await
            .expect("读另一个插件状态"),
        Vec::<String>::new()
    );
    let record = table.get(&session.id).await.expect("会话记录");
    assert_eq!(
        record.state.lock().await.entries.len(),
        1,
        "状态不进入 transcript"
    );
    let paths = record.paths.clone();
    table.unload(&session.id).await.expect("卸载会话");
    let restored = table
        .plugin_state_get(&session.id, "demo", "count")
        .await
        .expect("恢复插件状态");
    assert_eq!(restored, Some(serde_json::json!(1)));
    assert!(paths.plugin_state("demo").is_file());
}

#[tokio::test]
async fn plugin_state_limit_preserves_previous_file_and_bounds_reads() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let session = create_plugin_state_session(&table).await;
    table
        .plugin_state_set(&session.id, "demo", "count", serde_json::json!(1))
        .await
        .expect("保存原状态");
    let record = table.get(&session.id).await.expect("会话记录");
    let path = record.paths.plugin_state("demo");
    let before = tokio::fs::read(&path).await.expect("原文件字节");
    assert!(
        table
            .plugin_state_set(
                &session.id,
                "demo",
                "count",
                serde_json::json!("x".repeat(1024 * 1024))
            )
            .await
            .is_err()
    );
    assert_eq!(
        tokio::fs::read(&path).await.expect("写入失败后的文件"),
        before
    );
    assert_eq!(
        table
            .plugin_state_get(&session.id, "demo", "count")
            .await
            .expect("原状态仍可读取"),
        Some(serde_json::json!(1))
    );
    tokio::fs::write(
        record.paths.plugin_state("other"),
        vec![b'x'; 1024 * 1024 + 1],
    )
    .await
    .expect("外部写入超限文件");
    assert!(
        table
            .plugin_state_keys(&session.id, "other")
            .await
            .expect_err("读取也有容量上限")
            .to_string()
            .contains("1 MiB")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            tokio::fs::metadata(&path)
                .await
                .expect("文件权限")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn session_lifecycle_persists_transcript_and_true_truncation() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model:high",
            SessionEngine::Standard,
            SessionPermission::Full,
            Some("  标题  "),
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    assert_eq!(info.title, "标题");
    for text in ["第一条", "第二条", "第三条"] {
        table
            .append_message(&info.id, Message::user_text(text), None, None)
            .await
            .expect("追加消息");
    }
    let loaded = table.load(&info.id, Some(2)).await.expect("限制加载");
    assert!(loaded.truncated);
    assert_eq!(loaded.entries.len(), 2);
    assert!(matches!(
        &loaded.entries[0].message.content[0],
        moc_llm::ContentPart::Text(text) if text == "第二条"
    ));
    let plan = moc_protocol::PlanInfo {
        plan_id: "plan-budget".into(),
        title: "预算".into(),
        path: "plan.md".into(),
        markdown: "p".repeat(64 * 1024),
        status: moc_protocol::PlanStatus::Submitted,
        version: 1,
        previous_plan_id: None,
        annotations: Vec::new(),
    };
    table
        .append(&info.id, EntryPayload::plan(&plan, plan.status))
        .await
        .unwrap();
    table
        .append(
            &info.id,
            EntryPayload::ChildTool {
                call: moc_protocol::TranscriptChildCall {
                    id: "child".into(),
                    parent_tool_call_id: "parent".into(),
                    name: "read".into(),
                    arguments: "{}".into(),
                    result: "r".repeat(64 * 1024),
                    status: ToolStatus::Completed,
                    duration_ms: None,
                    diff: None,
                },
            },
        )
        .await
        .unwrap();
    let phases = vec![moc_protocol::TodoPhase {
        phase: "验收".into(),
        items: vec![moc_protocol::TodoItem {
            task: "任务".repeat(16 * 1024),
            status: moc_protocol::TodoStatus::Pending,
            reason: None,
        }],
    }];
    table
        .append(
            &info.id,
            EntryPayload::Todos {
                phases: phases.clone(),
            },
        )
        .await
        .unwrap();
    table
        .append(
            &info.id,
            EntryPayload::GoalChange {
                goal: Some(entries::GoalSnapshot {
                    id: "goal-large".into(),
                    revision: 1,
                    objective: "目标".repeat(16 * 1024),
                    phase: moc_protocol::GoalPhase::Active,
                    blocked_reason: None,
                    created_at_ms: 1,
                    updated_at_ms: 1,
                }),
            },
        )
        .await
        .unwrap();
    table.unload(&info.id).await.expect("卸载");
    let loaded = table.load(&info.id, None).await.expect("从磁盘重载");
    let metadata = table.load_projected(&info.id, Some(0), true).await.unwrap();
    assert!(
        serialized_bytes(&metadata) < 4096,
        "仅取租约的响应不得夹带大状态正文"
    );
    assert!(
        metadata.todos.is_empty()
            && metadata.plans.is_empty()
            && metadata.child_calls.is_empty()
            && metadata.goal.is_none()
    );
    let page = table.load_page(&info.id, None, 0, None).await.unwrap();
    assert_eq!(page.todos, phases);
    assert_eq!(page.todos, loaded.todos);
    assert_eq!(page.goal, loaded.goal);
    assert_eq!(page.plans, loaded.plans);
    assert!(
        table.get(&info.id).await.unwrap().cache_bytes >= 4 * 64 * 1024,
        "计划/子工具正文及其投影副本必须计入只读缓存预算"
    );
    assert!(!loaded.truncated);
    assert_eq!(loaded.entries.len(), 3);
    assert_eq!(loaded.session.engine, SessionEngine::Standard);
    let renamed = table
        .rename(
            &info.id,
            SessionRenameParams {
                session_id: info.id.clone(),
                title: "新标题\n第二行".into(),
            },
        )
        .await
        .expect("重命名");
    assert_eq!(renamed.title, "新标题");
    table.unload(&info.id).await.expect("再次卸载");
    assert_eq!(
        table
            .load(&info.id, None)
            .await
            .expect("重读标题")
            .session
            .title,
        "新标题"
    );
    table.delete(&info.id).await.expect("删除");
    assert!(table.load(&info.id, None).await.is_err());
}

use super::*;

#[tokio::test]
async fn shake_preserves_concurrent_appends_and_replayed_state() {
    use crate::compact::{CompactionBudget, shake};
    use crate::session::state::SessionNotice;
    use moc_protocol::methods::{AgentResultMeta, AgentStatus};
    use std::time::Duration;

    let directory = tempfile::tempdir().expect("临时目录");
    std::fs::write(
        directory.path().join("config.toml"),
        "[models.roles]\ndefault='p/model'\n[providers.p]\napi='openai_chat'\nbase_url='http://127.0.0.1:1'\n[providers.p.models.model]\n[compaction]\nprotect_recent_tool_tokens=0\n[compaction.shake]\nkeep_last_prompts=0\nhard_clear_prompts=0\n",
    )
    .unwrap();
    let core = crate::Core::new_with_sandbox_support(
        crate::CoreConfig {
            home: directory.path().to_owned(),
            cwd: directory.path().to_owned(),
            user_home: Some(directory.path().join("userhome")),
            session_dir: Some(directory.path().join("sessions")),
            ..Default::default()
        },
        crate::sandbox::SandboxSupport::full_only(),
    )
    .unwrap();
    let table = &core.inner.sessions;
    let info = create_plugin_state_session(table).await;
    let original = table
        .append_message(
            &info.id,
            Message::tool_result("read1", "压缩前的工具输出", false),
            None,
            None,
        )
        .await
        .unwrap();
    let record = table.get(&info.id).await.unwrap();
    table
        .ensure_write_lease(&info.id, &record, "test")
        .await
        .unwrap();
    let config = core.inner.config.read().clone();
    let model = config.resolve_model("p/model").unwrap();
    let phases = vec![moc_protocol::TodoPhase {
        phase: "验收".into(),
        items: vec![moc_protocol::TodoItem {
            task: "保留后台交付".into(),
            status: moc_protocol::TodoStatus::Completed,
            reason: None,
        }],
    }];

    // 让后台追加先持有 append_lock、阻塞在文件锁上，固定旧快照与提交的交错。
    let file = record.file.lock().await;
    let append = tokio::spawn({
        let table = Arc::clone(table);
        let id = info.id.clone();
        let phases = phases.clone();
        async move {
            let result = table
                .append(
                    &id,
                    EntryPayload::AgentResult {
                        meta: AgentResultMeta {
                            agent_id: "ses_child".into(),
                            title: "子代理".into(),
                            status: AgentStatus::Completed,
                        },
                        report: "完整交付报告".into(),
                    },
                )
                .await
                .unwrap();
            table
                .append(&id, EntryPayload::Todos { phases })
                .await
                .unwrap();
            result
        }
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while record.append_lock.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("后台追加已取得事务锁");

    let compact = shake::compact(
        &core.inner,
        &info.id,
        &config,
        &model,
        CompactionBudget {
            tokens_before: 100,
            fixed_tokens: 0,
        },
    );
    tokio::pin!(compact);
    assert!(futures_util::poll!(&mut compact).is_pending());
    {
        let mut state = record.state.lock().await;
        state.notices.push(SessionNotice::new("待送达通知".into()));
        state.job_sequence = 7;
    }
    drop(file);
    let (compacted, appended) = tokio::time::timeout(Duration::from_secs(5), async {
        tokio::join!(compact, append)
    })
    .await
    .expect("追加与压缩均应完成");
    assert_eq!(compacted.unwrap().method, "shake");
    let appended = appended.unwrap();
    let state = record.state.lock().await.clone();
    assert!(
        state.log_entries().contains(&appended),
        "后台结果不能被旧快照覆盖"
    );
    let result = state
        .transcript
        .iter()
        .find(|entry| entry.id == appended.id())
        .expect("后台结果保留在内存投影中");
    assert_eq!(result.message.text(), "完整交付报告");
    assert_eq!(result.agent_result.as_ref().unwrap().agent_id, "ses_child");
    assert_eq!(state.todos, phases);
    assert_eq!(state.notices, vec![SessionNotice::new("待送达通知".into())]);
    assert_eq!(state.job_sequence, 7);
    let pruned = state
        .transcript
        .iter()
        .find(|entry| entry.id == original.id())
        .unwrap();
    assert_eq!(
        pruned.message.tool_results().next().unwrap().content,
        "[tool output pruned]"
    );
    let entries = table.read_entries(&info.id).await.unwrap();
    assert_eq!(entries, state.log_entries(), "磁盘与内存条目一致");
    for pair in entries.windows(2) {
        assert_eq!(pair[1].parent_id(), Some(pair[0].id()));
    }
    table.unload(&info.id).await.unwrap();
    let restored = table.get(&info.id).await.unwrap();
    let restored = restored.state.lock().await;
    assert_eq!(restored.log_entries(), entries);
    assert_eq!(restored.transcript, state.transcript);
    assert_eq!(
        restored.transcript_token_counts,
        state.transcript_token_counts
    );
    assert_eq!(restored.todos, phases);
    drop(restored);
    core.shutdown().await;
}

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

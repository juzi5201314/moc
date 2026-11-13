use super::*;

#[tokio::test]
async fn plugin_state_corruption_does_not_affect_other_plugins_or_session_load() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let session = create_plugin_state_session(&table).await;
    for plugin in ["demo", "other"] {
        table
            .plugin_state_set(&session.id, plugin, "count", serde_json::json!(1))
            .await
            .expect("保存插件状态");
    }
    let record = table.get(&session.id).await.expect("会话记录");
    tokio::fs::write(record.paths.plugin_state("demo"), b"{broken")
        .await
        .expect("损坏单个插件状态文件");
    assert!(
        table
            .plugin_state_get(&session.id, "demo", "count")
            .await
            .is_err()
    );
    assert!(table.plugin_state_keys(&session.id, "demo").await.is_err());
    table.unload(&session.id).await.expect("卸载会话");
    table
        .load(&session.id, None)
        .await
        .expect("损坏状态不影响恢复会话");
    table
        .plugin_state_set(&session.id, "other", "count", serde_json::json!(2))
        .await
        .expect("另一个插件仍可写入");
    assert_eq!(
        table
            .plugin_state_get(&session.id, "other", "count")
            .await
            .expect("读取其它插件"),
        Some(serde_json::json!(2))
    );
    assert!(
        table.read_plugin_state(&record, "other").await.is_err(),
        "已卸载的记录不能读取状态"
    );
}

#[tokio::test]
async fn permission_changes_persist_and_reload() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    assert_eq!(info.permission, moc_protocol::SessionPermission::Full);
    table
        .append_message(&info.id, Message::user_text("落盘"), None, None)
        .await
        .expect("追加消息");
    let changed = table
        .set_permission(&info.id, moc_protocol::SessionPermission::Workspace)
        .await
        .expect("切换权限");
    assert_eq!(
        changed.permission,
        moc_protocol::SessionPermission::Workspace
    );
    assert_eq!(
        table.list().await.expect("列出会话")[0].permission,
        moc_protocol::SessionPermission::Workspace
    );
    table.unload(&info.id).await.expect("卸载");
    let loaded = table.load(&info.id, None).await.expect("重载");
    assert_eq!(
        loaded.session.permission,
        moc_protocol::SessionPermission::Workspace
    );
    table
        .set_permission(&info.id, moc_protocol::SessionPermission::Full)
        .await
        .expect("切回权限");
    table.unload(&info.id).await.expect("再次卸载");
    assert_eq!(
        table
            .load(&info.id, None)
            .await
            .expect("再次重载")
            .session
            .permission,
        moc_protocol::SessionPermission::Full
    );
}

#[tokio::test]
async fn load_page_projects_large_messages_and_keeps_cursor_order() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let info = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            false,
        )
        .await
        .expect("创建会话");
    table
        .append_message(&info.id, Message::user_text("第一条"), None, None)
        .await
        .expect("追加第一条");
    table
        .append_message(
            &info.id,
            Message::user_text("x".repeat(200_000)),
            None,
            None,
        )
        .await
        .expect("追加大消息");
    table
        .append(
            &info.id,
            EntryPayload::Compaction {
                method: "soft".into(),
                summary: "摘要".into(),
                first_kept_id: None,
                tokens_before: 50_000,
                tokens_after: 10,
                instructions: None,
                model: None,
                reminder: None,
            },
        )
        .await
        .unwrap();
    let record = table.get(&info.id).await.unwrap();
    let frozen = record.state.lock().await.transcript[1]
        .message
        .content
        .clone();
    assert!(frozen.is_archived(), "失活的大正文应释放堆副本");
    assert!(!frozen.archive_cache_loaded());
    let cold_table = SessionTable::new(directory.path().join("sessions"), true);
    let cold_record = cold_table.get(&info.id).await.unwrap();
    assert!(
        cold_record.state.lock().await.transcript[1]
            .message
            .content
            .is_archived()
    );
    let page = table
        .load_page(&info.id, None, 8, Some(1024))
        .await
        .expect("读取分页");
    assert_eq!(page.entries.len(), 2);
    assert!(page.content_truncated);
    assert!(
        !frozen.archive_cache_loaded(),
        "只读分页不能使共享快照的完整正文驻留"
    );
    assert!(page.entries[1].message.text().len() <= 1024);
    assert!(
        !record.state.lock().await.transcript[1]
            .message
            .content
            .archive_cache_loaded()
    );
    assert_eq!(page.entries[0].message.text(), "第一条");
    assert!(!page.has_more);
    let id = TranscriptEntryId::new(&page.entries[1].id);
    let rewound = table
        .rewind_with_limit(&info.id, &id, Some(0), false)
        .await
        .unwrap();
    assert!(rewound.entries.is_empty());
    assert!(rewound.truncated);
    let selection = moc_protocol::methods::session::RedoPageParams {
        before_entry_id: None,
        max_entries: 1,
    };
    let future = table
        .load_pages(&info.id, None, 1, Some(1024), Some(&selection), None, false)
        .await
        .unwrap();
    assert_eq!(future.redo_entries[0].message.text(), "x".repeat(200_000));
    let restored = table
        .redo_with_limit(&info.id, &id, Some(0), false)
        .await
        .unwrap();
    assert!(restored.entries.is_empty());
    assert_eq!(
        table.load(&info.id, None).await.unwrap().entries[1]
            .message
            .text()
            .len(),
        200_000
    );
    // 控制字符在 JSON 中膨胀为六字节，预算不能只计 markdown.len()。
    let plans: Vec<_> = (0..40)
        .map(|index| moc_protocol::PlanInfo {
            plan_id: format!("plan-{index}"),
            title: format!("版本 {index}"),
            path: "plan.md".into(),
            markdown: "\0".repeat(32 * 1024),
            status: moc_protocol::PlanStatus::Submitted,
            version: index + 1,
            previous_plan_id: None,
            annotations: Vec::new(),
        })
        .collect();
    table.get(&info.id).await.unwrap().state.lock().await.plans = Arc::new(plans.clone());
    assert_eq!(table.load(&info.id, Some(0)).await.unwrap().plans, plans);
    assert!(
        table
            .load_projected(&info.id, Some(0), true)
            .await
            .unwrap()
            .plans
            .is_empty()
    );
    let mut request = moc_protocol::methods::session::PlanPageParams {
        before: None,
        max_entries: 32,
    };
    let no_redo = moc_protocol::methods::session::RedoPageParams::default();
    let mut pages = Vec::new();
    loop {
        let page = table
            .load_pages(
                &info.id,
                None,
                0,
                None,
                Some(&no_redo),
                Some(&request),
                false,
            )
            .await
            .unwrap();
        assert!(page.entries.is_empty(), "独立计划页不重复读取消息");
        assert!(!page.plans.is_empty());
        assert!(page.plans.len() <= 32);
        assert!(serialized_bytes(&page.plans) <= 4 * 1024 * 1024);
        pages.push(page.plans);
        let Some(before) = page.plans_before else {
            break;
        };
        if let Some(previous) = request.before {
            assert!(before < previous);
        }
        request.before = Some(before);
    }
    assert!(pages.len() > 1, "字节预算应在条数上限前生效");
    assert_eq!(pages.into_iter().rev().flatten().collect::<Vec<_>>(), plans);
    let skip = moc_protocol::methods::session::PlanPageParams::default();
    let skipped = table
        .load_pages(
            &info.id,
            None,
            1,
            Some(1024),
            Some(&no_redo),
            Some(&skip),
            false,
        )
        .await
        .unwrap();
    assert!(skipped.plans.is_empty());
    assert_eq!(skipped.plans_before, None);
    request.before = Some(41);
    assert!(
        table
            .load_pages(
                &info.id,
                None,
                0,
                None,
                Some(&no_redo),
                Some(&request),
                false
            )
            .await
            .is_err()
    );
    for limit in 0..32 {
        assert!(
            truncate_text(&"文".repeat(20), limit).0.len() <= limit,
            "截断标记也必须服从字节预算：{limit}"
        );
    }
    for _ in 0..8 {
        table
            .append_message(
                &info.id,
                Message::user_text("\0".repeat(256 * 1024)),
                None,
                None,
            )
            .await
            .unwrap();
    }
    let escaped = table
        .load_pages(
            &info.id,
            None,
            128,
            Some(256 * 1024),
            Some(&no_redo),
            Some(&skip),
            false,
        )
        .await
        .unwrap();
    assert!(
        serialized_bytes(&escaped.entries) <= 8 * 1024 * 1024,
        "消息页必须按实际JSON大小计费"
    );
    assert!(escaped.has_more);
    let preceding = table
        .load_pages(
            &info.id,
            Some(&escaped.entries[0].id),
            128,
            Some(256 * 1024),
            Some(&no_redo),
            Some(&skip),
            false,
        )
        .await
        .unwrap();
    assert_eq!(preceding.entries.len() + escaped.entries.len(), 10);
    assert!(!preceding.has_more);
    assert_eq!(preceding.entries[0].message.text(), "第一条");
    let full = table
        .load_pages(
            &info.id,
            None,
            128,
            Some(1024),
            Some(&no_redo),
            Some(&skip),
            true,
        )
        .await
        .unwrap();
    assert!(!full.content_truncated);
    assert!(full.has_more, "完整消息仍按页总字节预算分批");
    assert!(serialized_bytes(&full.entries) <= 8 * 1024 * 1024);
    assert_eq!(
        full.entries.last().unwrap().message.text().len(),
        256 * 1024
    );
    let image_message = Message {
        role: Role::User,
        content: moc_llm::message_content![
            ContentPart::Text("正文".repeat(20_000)),
            ContentPart::Image {
                source: moc_llm::ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "aGVsbG8=".repeat(1024)
                },
                detail: Some(moc_llm::ImageDetail::High),
            },
        ],
    };
    table
        .append_message(&info.id, image_message.clone(), None, None)
        .await
        .unwrap();
    let full = table
        .load_pages(
            &info.id,
            None,
            1,
            Some(1024),
            Some(&no_redo),
            Some(&skip),
            true,
        )
        .await
        .unwrap();
    assert_eq!(full.entries[0].message, image_message);
    assert!(!full.content_truncated);
    table
        .append_message(
            &info.id,
            Message {
                role: Role::Assistant,
                content: moc_llm::message_content![ContentPart::ToolCall(ToolCall::new(
                    "parent-full",
                    "execute",
                    "{}"
                ))],
            },
            None,
            None,
        )
        .await
        .unwrap();
    let mut children = Vec::new();
    for index in 0..65 {
        let call = TranscriptChildCall {
            id: format!("child-{index}"),
            parent_tool_call_id: "parent-full".into(),
            name: "read".into(),
            arguments: "{}".into(),
            result: if index == 64 {
                "完整子结果".repeat(2048)
            } else {
                index.to_string()
            },
            status: ToolStatus::Completed,
            duration_ms: Some(index),
            diff: None,
        };
        table
            .append(&info.id, EntryPayload::ChildTool { call: call.clone() })
            .await
            .unwrap();
        children.push(call);
    }
    let full = table
        .load_pages(
            &info.id,
            None,
            1,
            Some(1024),
            Some(&no_redo),
            Some(&skip),
            true,
        )
        .await
        .unwrap();
    assert_eq!(full.child_calls, children);
    assert!(!full.content_truncated);
    table
        .append_message(
            &info.id,
            Message::user_text("\0".repeat(1536 * 1024)),
            None,
            None,
        )
        .await
        .unwrap();
    let tail = table
        .append_message(&info.id, Message::user_text("可前滚"), None, None)
        .await
        .unwrap();
    table
        .rewind_with_limit(&info.id, &TranscriptEntryId::new(tail.id()), Some(0), true)
        .await
        .unwrap();
    let redo = moc_protocol::methods::session::RedoPageParams {
        before_entry_id: None,
        max_entries: 1,
    };
    let full = table
        .load_pages(&info.id, None, 1, None, Some(&redo), Some(&skip), true)
        .await
        .unwrap();
    assert!(
        serialized_bytes(&full.entries) > 8 * 1024 * 1024,
        "单个完整条目可独占超预算页"
    );
    assert_eq!(
        full.redo_entries[0].id,
        tail.id(),
        "历史页超预算不能让前滚分页失去进展"
    );
    table.unload(&info.id).await.unwrap();
    let snapshot = Message {
        role: Role::User,
        content: frozen,
    };
    assert_eq!(
        snapshot.text(),
        "x".repeat(200_000),
        "重写和卸载不破坏仍被引用的快照"
    );
}

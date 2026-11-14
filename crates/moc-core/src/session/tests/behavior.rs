use super::*;

/// 引用收集：工具结果 JSON 任意深度的字符串 `agent_id` 与代理消息 `from_id`
/// 都算引用；非法 JSON、非字符串值被忽略。
#[test]
fn referenced_agent_ids_collects_nested_results_and_agent_meta() {
    let tool_entry = |content: &str| {
        Entry::from_payload(
            EntryPayload::Message {
                message: Message {
                    role: Role::Tool,
                    content: moc_llm::message_content![ContentPart::ToolResult(ToolResult {
                        tool_call_id: "call_1".into(),
                        content: content.into(),
                        is_error: false,
                        images: Vec::new(),
                    })],
                },
                tool: None,
                finish_reason: None,
                agent: None,
                goal: None,
            },
            "e0".into(),
            0,
        )
    };
    let agent_entry = Entry::from_payload(
        EntryPayload::Message {
            message: Message::user_text("ping"),
            tool: None,
            finish_reason: None,
            goal: None,
            agent: Some(AgentMessageMeta {
                from_id: "ses_sender".into(),
                from_title: "sender".into(),
                delivery: AgentMessageOutcome::Injected,
            }),
        },
        "e1".into(),
        0,
    );
    let entries = vec![
        tool_entry(r#"{"agents":[{"agent_id":"ses_a"}],"errors":[]}"#),
        tool_entry(r#"{"nested":{"items":[{"agent_id":"ses_b"}]}}"#),
        tool_entry("not json"),
        tool_entry(r#"{"agent_id":123}"#),
        agent_entry,
    ];
    let ids = entries::referenced_agent_ids(&entries);
    assert_eq!(
        ids.iter().map(String::as_str).collect::<Vec<_>>(),
        vec!["ses_a", "ses_b", "ses_sender"],
        "只收集字符串 agent_id 与 agent.from_id：{ids:?}"
    );
}

/// 子代理会话整个放在父会话目录里：transcript 和父会话的 sidecar 同目录，自己的
/// sidecar 是它的子目录。恢复后父链接和来源都在，删除父会话会一并删除子树。
#[tokio::test]
async fn spawned_child_session_lives_inside_the_parent_directory() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let parent = table
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
        .expect("创建父会话");
    table
        .append_message(&parent.id, Message::user_text("父"), None, None)
        .await
        .expect("追加父消息");
    let child = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            Some("子任务"),
            SessionOrigin::spawned(parent.id.clone(), 1, None),
            true,
        )
        .await
        .expect("创建子代理会话");
    table
        .append_message(&child.id, Message::user_text("子"), None, None)
        .await
        .expect("追加子消息");

    let parent_paths = table.get(&parent.id).await.expect("父记录").paths.clone();
    let child_paths = table.get(&child.id).await.expect("子记录").paths.clone();
    assert!(!parent_paths.compressed());
    assert!(child_paths.compressed(), "子代理按本次有效配置选择格式");
    assert_eq!(
        child_paths.transcript.parent(),
        Some(parent_paths.sidecar.as_path())
    );
    assert_eq!(child_paths.sidecar, parent_paths.sidecar.join(&child.id));
    assert!(child_paths.transcript.exists(), "子代理日志已经落盘");

    // 全局会话列表与搜索只返回顶层会话，子代理不能泄露出去；
    // 代理树仍可由内部 known() 与存储层从父会话目录恢复并保留父链接。
    let listed = table.list().await.expect("列出会话");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].id, parent.id);
    assert!(
        table
            .search("子", None, 10)
            .await
            .expect("搜索会话")
            .0
            .is_empty(),
        "子代理正文不应出现在全局搜索里"
    );

    let known = table.known().await.expect("获取已知会话");
    let child_info = known
        .iter()
        .find(|info| info.id == child.id)
        .expect("子代理仍在内部已知集合里");
    assert_eq!(
        child_info.parent_session_id.as_deref(),
        Some(parent.id.as_str())
    );

    table.delete(&parent.id).await.expect("删除父会话");
    assert!(!parent_paths.sidecar.exists());
    assert!(!child_paths.transcript.exists());
}

#[tokio::test]
async fn fresh_session_is_deferred_to_disk_until_first_message() {
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
    // 还没产生 entry 时，磁盘上既没有 transcript 也没有 sidecar，列表里也不会出现。
    let paths = table.store.paths_for(
        std::path::Path::new("/tmp/project"),
        &info.id,
        info.created_at_ms,
    );
    assert!(!paths.transcript.exists());
    assert!(table.list().await.expect("列出会话").is_empty());
    // append 之后：header 和 entry 一起写盘，列表里能看到。
    table
        .append_message(&info.id, Message::user_text("第一条消息"), None, None)
        .await
        .expect("追加消息");
    assert!(paths.transcript.exists());
    let entries = table
        .store
        .read_entries(&paths)
        .await
        .expect("读取 transcript");
    assert_eq!(entries.len(), 2, "header 加一条用户消息");
    assert_eq!(table.list().await.expect("列出会话").len(), 1);
}

#[tokio::test]
async fn bookkeeping_changes_on_fresh_session_stay_off_disk_until_first_message() {
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
    let paths = table.store.paths_for(
        std::path::Path::new("/tmp/project"),
        &info.id,
        info.created_at_ms,
    );

    // 新会话上的配置变更只应用到内存：不落盘、不入列。
    table
        .set_model(&info.id, "p/other".into())
        .await
        .expect("切换模型");
    table
        .set_permission(&info.id, SessionPermission::Workspace)
        .await
        .expect("切换权限");
    table
        .set_engine(&info.id, SessionEngine::Ptc)
        .await
        .expect("切换引擎");
    assert!(!paths.transcript.exists(), "簿记条目不触发落盘");
    assert!(table.list().await.expect("列出会话").is_empty());
    // 内存投影即时生效，前端读到的就是新值。
    let record = table.get(&info.id).await.expect("取会话");
    assert_eq!(record.state.lock().await.session.model, "p/other");
    drop(record);
    // 未落盘会话的 read_entries 能看到内存缓冲的整条链。
    assert_eq!(
        table
            .read_entries(&info.id)
            .await
            .expect("读 entries")
            .len(),
        4,
        "header + 3 条簿记"
    );

    // 首条消息到来：header + 缓冲的簿记 + 消息整链一并落盘。
    table
        .append_message(&info.id, Message::user_text("第一条"), None, None)
        .await
        .expect("追加消息");
    assert!(paths.transcript.exists());
    let entries = table.read_entries(&info.id).await.expect("读 entries");
    assert_eq!(entries.len(), 5, "header + 3 条簿记 + 1 条消息");
    for pair in entries.windows(2) {
        assert_eq!(pair[1].parent_id(), Some(pair[0].id()), "parent_id 链不断");
    }
    let listed = table.list().await.expect("列出会话");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].model, "p/other");

    // 落盘的是完整链：卸载重载后簿记变更仍然生效。
    table.unload(&info.id).await.expect("卸载");
    let reloaded = table.load(&info.id, None).await.expect("重载");
    assert_eq!(reloaded.session.model, "p/other");
    assert_eq!(reloaded.session.permission, SessionPermission::Workspace);
    assert_eq!(reloaded.session.engine, SessionEngine::Ptc);
}

#[tokio::test]
async fn compressed_sessions_use_zstd_only_for_new_sessions() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let compressed = table
        .create(
            "/tmp/project",
            "p/model",
            SessionEngine::Standard,
            SessionPermission::Full,
            None,
            SessionOrigin::root(),
            true,
        )
        .await
        .expect("创建压缩会话");
    table
        .append_message(&compressed.id, Message::user_text("压缩消息"), None, None)
        .await
        .expect("追加消息");
    let record = table.get(&compressed.id).await.expect("取会话");
    assert_eq!(
        record
            .paths
            .transcript
            .extension()
            .and_then(|value| value.to_str()),
        Some("zst")
    );
    let bytes = std::fs::read(&record.paths.transcript).expect("读取日志");
    assert_eq!(
        &bytes[..4],
        &[0x28, 0xb5, 0x2f, 0xfd],
        "文件应以 zstd magic 开头"
    );
    let loaded = table
        .load(&compressed.id, None)
        .await
        .expect("加载压缩会话");
    assert_eq!(loaded.entries.len(), 1);
    assert_eq!(loaded.entries[0].message.text(), "压缩消息");
    assert_eq!(table.list().await.expect("列出会话").len(), 1);
    assert!(
        table
            .store
            .find(&compressed.id)
            .await
            .expect("查找会话")
            .is_some(),
        "压缩日志也要能被 find 发现"
    );

    // 整表重写仍然是一个压缩文件，格式不能退回明文。
    let entries = table
        .store
        .read_entries(&record.paths)
        .await
        .expect("读取 entries");
    table
        .rewrite(&compressed.id, entries)
        .await
        .expect("压缩会话原子重写");
    let bytes = std::fs::read(&record.paths.transcript).expect("读取重写后的日志");
    assert_eq!(&bytes[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
    assert_eq!(
        table
            .load(&compressed.id, None)
            .await
            .unwrap()
            .entries
            .len(),
        1
    );

    // 关闭开关只影响以后创建的会话；已有会话保持压缩格式。
    let plain = table
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
        .expect("创建明文会话");
    table
        .append_message(&plain.id, Message::user_text("明文消息"), None, None)
        .await
        .expect("追加明文消息");
    let plain_record = table.get(&plain.id).await.expect("取明文会话");
    assert_eq!(
        plain_record
            .paths
            .transcript
            .extension()
            .and_then(|value| value.to_str()),
        Some("jsonl")
    );
    assert_eq!(table.list().await.expect("列出会话").len(), 2);
    table.delete(&compressed.id).await.expect("删除压缩会话");
    assert!(!record.paths.transcript.exists());
}

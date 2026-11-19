use super::*;

#[tokio::test]
async fn compressed_write_lease_truncates_a_torn_frame() {
    use tokio::io::AsyncWriteExt;

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
            true,
        )
        .await
        .expect("创建压缩会话");
    table
        .append_message(&info.id, Message::user_text("第一条"), None, None)
        .await
        .expect("追加第一条");
    let record = table.get(&info.id).await.expect("取会话");
    table
        .ensure_write_lease(&info.id, &record, "tui")
        .await
        .expect("取得写租约");
    // 模拟崩溃：只写出下一帧的前半段。
    let frame = crate::session::log::compress_frame(b"{\"partial\":\n").expect("压缩半帧");
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&record.paths.transcript)
        .await
        .expect("打开日志");
    file.write_all(&frame[..frame.len() / 2])
        .await
        .expect("写入半帧");
    file.sync_all().await.expect("刷写半帧");
    assert!(table.release_lease(&record));
    table
        .ensure_write_lease(&info.id, &record, "tui")
        .await
        .expect("重新取得租约时截掉残帧");
    table
        .append_message(&info.id, Message::user_text("第二条"), None, None)
        .await
        .expect("截断后继续追加");
    let loaded = table.load(&info.id, None).await.expect("加载");
    assert_eq!(loaded.entries.len(), 2);
    assert_eq!(loaded.entries[0].message.text(), "第一条");
    assert_eq!(loaded.entries[1].message.text(), "第二条");
}

#[tokio::test]
async fn prompt_and_file_locks_reject_parallel_writers() {
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
        .append_message(&info.id, Message::user_text("落盘"), None, None)
        .await
        .expect("追加消息");
    let record = table.get(&info.id).await.expect("取会话");
    let _prompt = record.try_prompt_lock().expect("取得提示锁");
    assert!(record.try_prompt_lock().is_err());
    table
        .ensure_write_lease(&info.id, &record, "tui")
        .await
        .expect("取得写租约");
    // 同进程重复取租约是幂等的：跨进程互斥由内核文件锁负责，进程内写互斥由提示锁负责。
    table
        .ensure_write_lease(&info.id, &record, "webui")
        .await
        .expect("重复取租约");
    assert!(table.release_lease(&record));
    assert!(!table.release_lease(&record));
    table
        .ensure_write_lease(&info.id, &record, "webui")
        .await
        .expect("释放后可再取");
}

/// 父会话卸载后，结果必须遵守新写者的租约；租约释放后沿最新历史接续。
#[tokio::test]
async fn agent_result_requires_parent_write_lease() {
    let directory = tempfile::tempdir().expect("临时目录");
    let first = SessionTable::new(directory.path(), true);
    let info = first
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
    first
        .append_message(&info.id, Message::user_text("原有历史"), None, None)
        .await
        .expect("持久化父会话");
    first.unload(&info.id).await.expect("卸载父会话");

    let second = SessionTable::new(directory.path(), true);
    let owner = second.get(&info.id).await.expect("新写者加载父会话");
    second
        .ensure_write_lease(&info.id, &owner, "webui")
        .await
        .expect("新写者取得租约");
    let latest = second
        .append_message(&info.id, Message::user_text("新写者历史"), None, None)
        .await
        .expect("新写者追加历史");
    let before = second.read_entries(&info.id).await.expect("读取最新历史");
    let payload = EntryPayload::AgentResult {
        meta: moc_protocol::methods::AgentResultMeta {
            agent_id: "ses_child".into(),
            title: "子代理".into(),
            status: moc_protocol::methods::AgentStatus::Completed,
        },
        report: "完整交付报告".into(),
    };
    let error = first
        .append(&info.id, payload.clone())
        .await
        .expect_err("结果不能绕过新写者的租约");
    assert!(error.to_string().contains("SESSION_LOCKED"), "{error}");
    assert_eq!(
        second.read_entries(&info.id).await.expect("读取磁盘历史"),
        before,
        "租约冲突不能修改父会话历史"
    );

    second.unload(&info.id).await.expect("释放新写者租约");
    let result = first
        .append(&info.id, payload)
        .await
        .expect("租约释放后可交付结果");
    assert_eq!(result.parent_id(), Some(latest.id()));
    let loaded = first.load(&info.id, None).await.expect("恢复交付历史");
    assert_eq!(
        loaded
            .entries
            .iter()
            .map(|entry| entry.message.text())
            .collect::<Vec<_>>(),
        ["原有历史", "新写者历史", "完整交付报告"]
    );
    assert!(
        loaded
            .entries
            .last()
            .expect("结果已落盘")
            .context_message()
            .is_none()
    );
}

#[tokio::test]
async fn a_second_table_cannot_take_the_write_lease_while_the_first_holds_it() {
    let directory = tempfile::tempdir().expect("临时目录");
    let first = SessionTable::new(directory.path(), true);
    let info = first
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
    first
        .append_message(&info.id, Message::user_text("写者"), None, None)
        .await
        .expect("追加消息");
    let first_record = first.get(&info.id).await.expect("取会话");
    first
        .ensure_write_lease(&info.id, &first_record, "tui")
        .await
        .expect("取得写租约");

    // 只读加载不取租约，因此在别人持锁时仍然成功。
    let second = SessionTable::new(directory.path(), true);
    let loaded = second.load(&info.id, None).await.expect("只读加载");
    assert_eq!(loaded.entries[0].message.text(), "写者");
    assert_eq!(
        loaded
            .session
            .lock
            .as_ref()
            .map(|lock| lock.frontend.as_str()),
        Some("tui"),
        "只读加载能看到持有者投影"
    );

    let second_record = second.get(&info.id).await.expect("取第二份会话");
    let error = second
        .ensure_write_lease(&info.id, &second_record, "webui")
        .await
        .expect_err("第二份会话不能取得写租约");
    assert!(
        error.to_string().contains("SESSION_LOCKED"),
        "错误码要能让前端降到只读：{error}"
    );

    assert!(first.release_lease(&first_record));
    second
        .ensure_write_lease(&info.id, &second_record, "webui")
        .await
        .expect("释放后可以接管");
}

#[tokio::test]
async fn lock_refreshes_external_appends_and_replaced_transcript_handles() {
    let directory = tempfile::tempdir().unwrap();
    let first = SessionTable::new(directory.path(), true);
    let info = first
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
        .unwrap();
    first
        .append_message(&info.id, Message::user_text("落盘"), None, None)
        .await
        .unwrap();
    let second = SessionTable::new(directory.path(), true);
    second.load(&info.id, None).await.unwrap();
    let first_record = first.get(&info.id).await.unwrap();
    first
        .ensure_write_lease(&info.id, &first_record, "first")
        .await
        .unwrap();
    first
        .append_message(&info.id, Message::user_text("外部追加"), None, None)
        .await
        .unwrap();
    first
        .rename(
            &info.id,
            SessionRenameParams {
                session_id: info.id.clone(),
                title: "跨进程新标题".into(),
            },
        )
        .await
        .unwrap();
    assert_eq!(second.list().await.unwrap()[0].title, "跨进程新标题");
    let entries = first_record.state.lock().await.log_entries();
    first.rewrite(&info.id, entries).await.unwrap();
    assert!(first.release_lease(&first_record));
    let second_record = second.get(&info.id).await.unwrap();
    second
        .ensure_write_lease(&info.id, &second_record, "second")
        .await
        .unwrap();
    let loaded = second.load(&info.id, None).await.unwrap();
    assert_eq!(
        loaded.entries[1].message.text(),
        "外部追加",
        "取租约后应看到 other 进程追加的消息"
    );
    second
        .append_message(&info.id, Message::assistant_text("续写"), None, None)
        .await
        .unwrap();
    assert!(second.release_lease(&second_record));
    let restored = SessionTable::new(directory.path(), true)
        .load(&info.id, None)
        .await
        .unwrap();
    assert_eq!(restored.entries.len(), 3);
    assert_eq!(restored.entries[2].message.text(), "续写");
}

#[tokio::test]
async fn write_lease_repairs_torn_tail_after_transcript_rewrite() {
    use tokio::io::AsyncWriteExt;

    let directory = tempfile::tempdir().unwrap();
    let first = SessionTable::new(directory.path(), true);
    let info = first
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
        .unwrap();
    first
        .append_message(&info.id, Message::user_text("原有历史"), None, None)
        .await
        .unwrap();
    let first_record = first.get(&info.id).await.unwrap();
    first
        .ensure_write_lease(&info.id, &first_record, "first")
        .await
        .unwrap();
    assert!(first.release_lease(&first_record));

    let second = SessionTable::new(directory.path(), true);
    let second_record = second.get(&info.id).await.unwrap();
    second
        .ensure_write_lease(&info.id, &second_record, "second")
        .await
        .unwrap();
    second
        .append_message(&info.id, Message::user_text("外部追加"), None, None)
        .await
        .unwrap();
    let entries = second_record.state.lock().await.log_entries();
    second.rewrite(&info.id, entries).await.unwrap();
    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&second_record.paths.transcript)
        .await
        .unwrap();
    file.write_all(b"{\"partial\":").await.unwrap();
    file.sync_all().await.unwrap();
    drop(file);
    assert!(second.release_lease(&second_record));

    first
        .ensure_write_lease(&info.id, &first_record, "first")
        .await
        .unwrap();
    first
        .append_message(&info.id, Message::assistant_text("接管续写"), None, None)
        .await
        .unwrap();
    let restored = SessionTable::new(directory.path(), true)
        .load(&info.id, None)
        .await
        .unwrap();
    assert_eq!(
        restored
            .entries
            .iter()
            .map(|entry| entry.message.text())
            .collect::<Vec<_>>(),
        ["原有历史", "外部追加", "接管续写"]
    );
}

#[tokio::test]
async fn engine_locks_after_first_prompt_not_before() {
    let directory = tempfile::tempdir().expect("临时目录");
    let table = SessionTable::new(directory.path().join("sessions"), true);
    let create = |engine| {
        let table = &table;
        async move {
            table
                .create(
                    "/tmp/project",
                    "p/model",
                    engine,
                    SessionPermission::Full,
                    None,
                    SessionOrigin::root(),
                    false,
                )
                .await
                .expect("创建会话")
        }
    };
    // 尚未跑过提示的会话不锁定引擎（CLM 也一样）。
    let untouched = create(SessionEngine::ClmExperimental).await;
    table
        .set_engine(&untouched.id, SessionEngine::Standard)
        .await
        .expect("首次提示前允许切换引擎");
    // 无提示历史时混入的实时上下文不构成锁定理由。
    let forged = create(SessionEngine::Standard).await;
    table
        .append(
            &forged.id,
            EntryPayload::ClmContext {
                content: "伪造的实时上下文".into(),
            },
        )
        .await
        .expect("写入实时上下文");
    table
        .set_engine(&forged.id, SessionEngine::Ptc)
        .await
        .expect("无提示历史的会话不因异常数据锁引擎");
    // 跑过用户提示的会话引擎锁定；同引擎重设不报错。
    let clm = create(SessionEngine::ClmExperimental).await;
    table
        .append(
            &clm.id,
            EntryPayload::ClmContext {
                content: "真实实时上下文".into(),
            },
        )
        .await
        .expect("写入实时上下文");
    table
        .append_message(&clm.id, Message::user_text("提示"), None, None)
        .await
        .expect("写入用户提示");
    let error = table
        .set_engine(&clm.id, SessionEngine::Standard)
        .await
        .expect_err("首次提示后引擎必须锁定");
    assert!(error.to_string().contains("引擎"), "{error}");
    table
        .set_engine(&clm.id, SessionEngine::ClmExperimental)
        .await
        .expect("同引擎重设不报错");
    // 自主程度不受引擎锁定限制：有提示历史后仍随时可改，且随会话持久化。
    let autonomy = table
        .set_autonomy(&clm.id, SessionAutonomy::Unattended)
        .await
        .expect("提示历史后可改自主程度");
    assert_eq!(autonomy.autonomy, SessionAutonomy::Unattended);
    let reloaded = table.load(&clm.id, None).await.expect("重载会话");
    assert_eq!(
        reloaded.session.autonomy,
        SessionAutonomy::Unattended,
        "自主程度必须随 transcript 持久化"
    );
}

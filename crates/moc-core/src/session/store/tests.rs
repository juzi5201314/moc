use moc_protocol::methods::{SessionEngine, SessionPermission};

use super::*;
use crate::session::entries::SessionOrigin;

#[test]
fn cwd_and_timestamp_paths_are_stable() {
    assert_eq!(
        encode_cwd(Path::new("/tmp/project/moc")),
        "-tmp-project-moc"
    );
    assert_eq!(timestamp_name(0), "1970-01-01T00-00-00-000Z");
    assert_eq!(
        timestamp_name(1_728_000_000_123),
        "2024-10-04T00-00-00-123Z"
    );
}

#[tokio::test]
async fn jsonl_store_appends_metadata_lists_and_rewrites_atomically() {
    use super::super::entries::EntryPayload;
    let directory = tempfile::tempdir().expect("临时目录");
    let store = SessionStore::new(directory.path());
    let header = Entry::header(
        "ses_123456789abc".into(),
        "/tmp/project".into(),
        "p/old".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "旧标题".into(),
        1_728_000_000_123,
    );
    let paths = store.paths_for(
        Path::new("/tmp/project"),
        "ses_123456789abc",
        1_728_000_000_123,
    );
    let file = SessionFile::create_with_header(&paths.transcript, &header)
        .await
        .expect("创建会话文件");
    let message = Entry::from_payload(
        EntryPayload::Message {
            message: moc_llm::Message::user_text("第一条"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        header.id().into(),
        1_728_000_000_124,
    );
    file.append(&message, None).await.expect("追加消息");
    let mut parent = message.id().to_owned();
    for (offset, payload) in [
        EntryPayload::TitleChange {
            title: "新标题".into(),
        },
        EntryPayload::ModelChange {
            model: "p/new".into(),
        },
        EntryPayload::EngineChange {
            engine: SessionEngine::Standard,
            plan_active: true,
        },
    ]
    .into_iter()
    .enumerate()
    {
        let entry = Entry::from_payload(payload, parent, 1_728_000_000_125 + offset as u64);
        parent = entry.id().to_owned();
        file.append(&entry, None).await.expect("追加元数据");
    }

    let listed = store.list().await.expect("列出会话");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].title, "新标题");
    assert_eq!(listed[0].model, "p/new");
    assert_eq!(listed[0].engine, SessionEngine::Standard);
    assert!(listed[0].plan_active);
    assert_eq!(listed[0].updated_at_ms, 1_728_000_000_127);
    let cached = read_listing_cache(&directory.path().join(LISTING_CACHE));
    assert_eq!(cached.len(), 1, "缓存已持久化");
    let restarted = SessionStore::new(directory.path());
    assert_eq!(
        restarted.list().await.unwrap(),
        listed,
        "重启后复用同版本元信息"
    );
    std::fs::write(directory.path().join(LISTING_CACHE), b"{broken").unwrap();
    let rebuilt = SessionStore::new(directory.path());
    assert_eq!(
        rebuilt.list().await.unwrap(),
        listed,
        "缓存损坏不影响日志真相"
    );

    let entries = store.read_entries(&paths).await.expect("读取 JSONL");
    assert_eq!(entries.len(), 5);
    assert_eq!(entries[1].parent_id(), Some(entries[0].id()));
    file.rewrite(&entries[..1]).await.expect("原子重写");
    assert!(
        restarted.list().await.unwrap().is_empty(),
        "文件版本变化使缓存失效"
    );
    assert!(
        SessionStore::new(directory.path())
            .list()
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        store.read_entries(&paths).await.expect("重读 JSONL").len(),
        1
    );
    assert!(
        paths
            .transcript
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("2024-10-04T00-00-00-123Z_")
    );
}

#[tokio::test]
async fn list_skips_corrupted_records_without_failing_the_whole_page() {
    use super::super::entries::EntryPayload;
    let directory = tempfile::tempdir().expect("临时目录");
    let store = SessionStore::new(directory.path());
    let cwd = Path::new("/tmp/project");

    let healthy = Entry::header(
        "ses_healthy0001".into(),
        "/tmp/project".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "健康".into(),
        1,
    );
    let healthy_paths = store.paths_for(cwd, "ses_healthy0001", 1);
    let file = SessionFile::create_with_header(&healthy_paths.transcript, &healthy)
        .await
        .expect("创建会话文件");
    let message = Entry::from_payload(
        EntryPayload::Message {
            message: moc_llm::Message::user_text("第一条"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        healthy.id().into(),
        2,
    );
    file.append(&message, None).await.expect("追加消息");

    // 损坏记录后面的合法记录仍然必须生效。
    let broken = Entry::header(
        "ses_broken00001".into(),
        "/tmp/project".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "原始标题".into(),
        3,
    );
    let broken_paths = store.paths_for(cwd, "ses_broken00001", 3);
    SessionFile::create_with_header(&broken_paths.transcript, &broken)
        .await
        .expect("创建会话文件");
    let title = Entry::from_payload(
        EntryPayload::TitleChange {
            title: "损坏行之后的标题".into(),
        },
        broken.id().into(),
        4,
    );
    let mut bytes = std::fs::read(&broken_paths.transcript).expect("读取 JSONL");
    bytes.extend_from_slice(b"{\"type\":\"title_change\",\n");
    bytes.extend_from_slice(serde_json::to_string(&title).expect("序列化").as_bytes());
    bytes.push(b'\n');
    std::fs::write(&broken_paths.transcript, &bytes).expect("写入损坏记录");

    // header 损坏的文件整条跳过，其它文件照常列出。
    let unreadable = store.paths_for(cwd, "ses_unread0001", 5);
    std::fs::write(&unreadable.transcript, b"{\"type\":\n").expect("写入损坏 header");

    let listed = store.list().await.expect("列表必须可用");
    assert_eq!(listed.len(), 2);
    assert!(!listed.iter().any(|info| info.id == "ses_unread0001"));
    let broken = listed
        .iter()
        .find(|info| info.id == "ses_broken00001")
        .expect("含损坏记录的会话仍在列表中");
    assert_eq!(broken.title, "损坏行之后的标题");
    assert_eq!(broken.updated_at_ms, 4);
    assert!(listed.iter().any(|info| info.id == "ses_healthy0001"));
}

#[tokio::test]
async fn listing_discards_all_metadata_from_a_corrupt_multiline_frame() {
    use super::super::entries::EntryPayload;

    let directory = tempfile::tempdir().expect("临时目录");
    let store = SessionStore::new(directory.path());
    let paths = store.paths_for_compressed(Path::new("/tmp/project"), "ses_badframe001", 1);
    let header = Entry::header(
        "ses_badframe001".into(),
        "/tmp/project".into(),
        "p/old".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "已提交标题".into(),
        1,
    );
    let file = SessionFile::create_with_header(&paths.transcript, &header)
        .await
        .unwrap();
    let message = Entry::from_payload(
        EntryPayload::Message {
            message: moc_llm::Message::user_text("已提交消息"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        header.id().into(),
        2,
    );
    file.append(&message, None).await.unwrap();
    let mut parent = message.id().to_owned();
    let mut entries = Vec::new();
    for index in 0..256 {
        let entry = Entry::from_payload(
            EntryPayload::TitleChange {
                title: format!("不应采纳-{index}-{}", "x".repeat(128)),
            },
            parent,
            3 + index,
        );
        parent = entry.id().to_owned();
        entries.push(entry);
    }
    let mut bad_frame = log::encode_zstd_entries(&entries).unwrap();
    *bad_frame.last_mut().unwrap() ^= 0xff;
    let mut bytes = std::fs::read(&paths.transcript).unwrap();
    bytes.extend(bad_frame);
    std::fs::write(&paths.transcript, bytes).unwrap();
    file.append(
        &Entry::from_payload(
            EntryPayload::ModelChange {
                model: "p/new".into(),
            },
            parent,
            500,
        ),
        None,
    )
    .await
    .unwrap();

    // 旧进程可能已经把坏帧投影存入 v2；日志版本不变时也必须重新校验。
    let mut poisoned = header.session_info(None).unwrap();
    poisoned.title = "旧缓存采纳了坏帧标题".into();
    let revision =
        crate::hashline::SnapshotId::from_metadata(&std::fs::metadata(&paths.transcript).unwrap());
    let obsolete = Listings::from([(paths.transcript.clone(), (revision, Some(poisoned)))]);
    write_listing_cache(&directory.path().join(".session-list-v2.json"), &obsolete).unwrap();

    let listed = store.list().await.unwrap();
    let info = listed
        .iter()
        .find(|info| info.id == "ses_badframe001")
        .unwrap();
    assert_eq!(info.title, "已提交标题");
    assert_eq!(info.model, "p/new");
    assert_eq!(info.updated_at_ms, 500);
    assert!(info.engine_locked);
    assert!(store.read_entries(&paths).await.is_err());
}

/// 只有 header 和簿记条目（换模型/权限/引擎等）的 transcript 是「空会话」，
/// 不出现在列表里；追加消息后正常入列。
#[tokio::test]
async fn metadata_only_transcript_is_not_listed_until_first_message() {
    use super::super::entries::EntryPayload;
    let directory = tempfile::tempdir().expect("临时目录");
    let store = SessionStore::new(directory.path());
    let cwd = Path::new("/tmp/project");
    let header = Entry::header(
        "ses_meta0000001".into(),
        "/tmp/project".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "新会话".into(),
        1,
    );
    let paths = store.paths_for(cwd, "ses_meta0000001", 1);
    let file = SessionFile::create_with_header(&paths.transcript, &header)
        .await
        .expect("创建会话文件");
    let mut parent = header.id().to_owned();
    for payload in [
        EntryPayload::ModelChange {
            model: "p/other".into(),
        },
        EntryPayload::PermissionChange {
            permission: SessionPermission::Workspace,
        },
    ] {
        let entry = Entry::from_payload(payload, parent, 2);
        parent = entry.id().to_owned();
        file.append(&entry, None).await.expect("追加簿记条目");
    }
    assert!(
        store.list().await.expect("列出会话").is_empty(),
        "纯簿记会话是幽灵，不入列"
    );
    let message = Entry::from_payload(
        EntryPayload::Message {
            message: moc_llm::Message::user_text("第一条"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        parent,
        3,
    );
    file.append(&message, None).await.expect("追加消息");
    let listed = store.list().await.expect("列出会话");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].model, "p/other");
}

/// 崩溃留下的半条记录：读取时忽略，拿到写租约后截掉才能继续追加。
#[tokio::test]
async fn truncate_partial_tail_discards_an_unterminated_record() {
    let directory = tempfile::tempdir().expect("临时目录");
    let store = SessionStore::new(directory.path());
    let paths = store.paths_for(Path::new("/tmp/project"), "ses_partial00001", 1);
    write_session(&paths, "ses_partial00001").await;

    let mut bytes = std::fs::read(&paths.transcript).expect("读取 JSONL");
    let complete = bytes.len() as u64;
    // 半条 UTF-8 记录：模拟进程在 write_all 中途退出。
    bytes.extend_from_slice("{\"type\":\"message\",\"text\":\"半".as_bytes());
    std::fs::write(&paths.transcript, &bytes).expect("写入残尾");

    let entries = store.read_entries(&paths).await.expect("读取完整前缀");
    assert_eq!(entries.len(), 2, "残尾不参与解析");
    assert_eq!(
        std::fs::metadata(&paths.transcript).unwrap().len(),
        bytes.len() as u64,
        "只读读取不得修改文件"
    );

    let file = store.open(&paths).await.expect("打开可写日志");
    assert!(file.truncate_partial_tail().await.expect("截尾"));
    assert_eq!(
        std::fs::metadata(&paths.transcript).unwrap().len(),
        complete
    );
    assert!(
        !file.truncate_partial_tail().await.expect("再截一次"),
        "完整文件无需修复"
    );

    let header = Entry::header(
        "ses_partial00001".into(),
        "/tmp/project".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        "续写".into(),
        20,
    );
    let appended = Entry::from_payload(
        crate::session::entries::EntryPayload::Message {
            message: moc_llm::Message::user_text("续写"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        entries[1].id().to_owned(),
        21,
    );
    file.append(&appended, Some(&header))
        .await
        .expect("截尾后继续追加");
    let after = store.read_entries(&paths).await.expect("重新解析");
    assert_eq!(after.len(), 3, "续写记录没有粘到残尾上");
    crate::session::state::SessionState::from_entries(after).expect("parent 链完整");
}

/// 写一份最小可列出的会话日志：header 加一条消息。
async fn write_session(paths: &SessionPaths, id: &str) {
    let header = Entry::header(
        id.into(),
        "/tmp/project".into(),
        "p/m".into(),
        SessionEngine::Standard,
        SessionPermission::Full,
        SessionOrigin::root(),
        id.into(),
        10,
    );
    let file = SessionFile::create_with_header(&paths.transcript, &header)
        .await
        .expect("创建会话文件");
    let message = Entry::from_payload(
        crate::session::entries::EntryPayload::Message {
            message: moc_llm::Message::user_text("内容"),
            tool: None,
            finish_reason: None,
            agent: None,
            goal: None,
        },
        header.id().into(),
        11,
    );
    file.append(&message, None).await.expect("追加消息");
}

/// 子代理落在父会话目录里：代理树和目录树同构，列表和查找都要穿过会话目录才能
/// 发现它，删除父会话会一并删除整棵子树；非会话目录里的日志一律不算会话。
#[tokio::test]
async fn child_transcripts_nest_under_the_parent_session_directory() {
    let directory = tempfile::tempdir().expect("临时目录");
    let store = SessionStore::new(directory.path().join("sessions"));
    let cwd = Path::new("/tmp/project");
    let parent = "ses_parent000001";
    let child = "ses_child0000001";

    let parent_paths = store.paths_for(cwd, parent, 10);
    write_session(&parent_paths, parent).await;
    let child_paths = store
        .paths_for_child(cwd, parent, child, 20, false)
        .await
        .expect("子代理路径");
    assert_eq!(
        child_paths.transcript.parent(),
        Some(parent_paths.sidecar.as_path()),
        "transcript 必须和父会话 sidecar 同目录，urlfs 的相对链接才能继续成立"
    );
    assert_eq!(child_paths.sidecar, parent_paths.sidecar.join(child));
    write_session(&child_paths, child).await;
    // 同级的 jobs 不是会话目录，里面的日志不算会话。
    tokio::fs::create_dir_all(parent_paths.jobs())
        .await
        .expect("jobs 目录");
    write_session(
        &store.paths_in(&parent_paths.jobs(), "ses_fake00000001", 30, false),
        "ses_fake00000001",
    )
    .await;

    let listed = store.list().await.expect("列出会话");
    let mut ids: Vec<&str> = listed.iter().map(|info| info.id.as_str()).collect();
    ids.sort_unstable();
    assert_eq!(ids, [child, parent], "只有会话目录里的日志会被列出");
    assert_eq!(
        store
            .find(child)
            .await
            .expect("查找子代理")
            .expect("子代理存在")
            .transcript,
        child_paths.transcript
    );

    // 父会话还没写盘时也按父目录归档：sidecar 目录名永远等于会话 ID。
    let pending = store
        .paths_for_child(cwd, "ses_pending00001", "ses_pendingchild1", 40, false)
        .await
        .expect("未落盘父会话的落点");
    assert_eq!(
        pending.sidecar,
        store
            .paths_for(cwd, "ses_pending00001", 0)
            .sidecar
            .join("ses_pendingchild1")
    );

    store
        .delete(parent, &parent_paths)
        .await
        .expect("删除父会话");
    assert!(!parent_paths.sidecar.exists());
    assert!(!child_paths.transcript.exists());
    assert!(!child_paths.sidecar.exists());
}

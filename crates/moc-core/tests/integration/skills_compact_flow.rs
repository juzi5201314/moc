//! 技能压缩的可见行为：独立上下文、缓存失效、整批原子性、前台取消和空白会话。

use crate::support::{self, Harness};
use moc_protocol::{
    EmptyParams, InputPart, PromptParams, RpcErrorCode, SessionEngine, SessionList,
    SessionListParams, SessionPrompt, SessionSteer, SkillsCancel, SkillsCompact,
    SkillsCompactAction, SkillsCompactParams, SkillsProgress, SkillsProgressPhase, SteerParams,
};
use serde_json::Value;
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const CONFIG: &str = "[skills]\nagents_user=false\nmoc_user=false\n[plugins]\nenabled=false\n[compaction]\nenabled=false\n[providers.test.models.selected]\ncontext_window=1000000\nmax_output_tokens=4096\nreasoning=true\nthinking_levels=['high']";

fn skill(harness: &Harness, name: &str, description: &str) -> PathBuf {
    let dir = harness.root.path().join("work/.moc/skills").join(name);
    std::fs::create_dir_all(dir.join("references")).unwrap();
    let file = dir.join("SKILL.md");
    std::fs::write(&file, format!("---\nname: {name}\ndescription: {description}\n---\nPRIVATE_BODY_SENTINEL\nRead references/details.md as needed.\n")).unwrap();
    std::fs::write(
        dir.join("references/details.md"),
        "PRIVATE_REFERENCE_SENTINEL",
    )
    .unwrap();
    file
}

fn params(action: SkillsCompactAction) -> SkillsCompactParams {
    SkillsCompactParams {
        session_id: None,
        cwd: None,
        model: Some("test/selected:high".into()),
        action,
    }
}

fn cache_path(harness: &Harness) -> PathBuf {
    harness
        .root
        .path()
        .join("home/cache/skill-descriptions.json")
}

fn answer(request: &wiremock::Request) -> ResponseTemplate {
    let body: Value = serde_json::from_slice(&request.body).unwrap();
    let text = body["messages"][0]["content"].as_str().unwrap();
    let input: BTreeMap<String, Value> =
        serde_json::from_str(text.rsplit_once("\n\n").unwrap().1).unwrap();
    let hints: BTreeMap<_, _> = input
        .keys()
        .map(|id| {
            (
                id.clone(),
                "Review source changes; not implementation.".to_owned(),
            )
        })
        .collect();
    ResponseTemplate::new(200)
        .set_body_json(support::complete(&serde_json::to_string(&hints).unwrap()))
}

#[tokio::test]
async fn skills_compaction_is_incremental_isolated_and_does_not_persist_a_draft() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(answer)
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", CONFIG).await;
    let source = skill(
        &harness,
        "review",
        &"Review source changes, analyze correctness and maintainability. ".repeat(8),
    );
    let original = std::fs::read(&source).unwrap();
    let first = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Missing))
        .await
        .unwrap();
    assert_eq!((first.compressed, first.skipped), (1, 0));
    assert!(first.tokens_after < first.tokens_before);
    let cached = std::fs::read(cache_path(&harness)).unwrap();
    let next = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Missing))
        .await
        .unwrap();
    assert_eq!((next.compressed, next.skipped), (0, 1));
    assert_eq!(std::fs::read(cache_path(&harness)).unwrap(), cached);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 1);
    let forced = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Force))
        .await
        .unwrap();
    assert_eq!(forced.compressed, 1);
    assert_eq!(upstream.received_requests().await.unwrap().len(), 2);
    assert_eq!(std::fs::read(&source).unwrap(), original);

    skill(
        &harness,
        "review",
        "Review Rust changes only, not frontend changes.",
    );
    let changed = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Missing))
        .await
        .unwrap();
    assert_eq!((changed.compressed, changed.skipped), (1, 0));
    let requests = upstream.received_requests().await.unwrap();
    for request in &requests {
        let body: Value = serde_json::from_slice(&request.body).unwrap();
        assert_eq!(body["model"], "selected");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert!(
            body.get("tools")
                .is_none_or(|tools| tools.as_array().is_some_and(Vec::is_empty))
        );
        assert!(!body.to_string().contains("PRIVATE_BODY_SENTINEL"));
        assert!(!body.to_string().contains("PRIVATE_REFERENCE_SENTINEL"));
    }
    // 另一个项目中仍存在的缓存不能因为当前目录不可见而被清理。
    let other = harness.root.path().join("other/SKILL.md");
    std::fs::create_dir_all(other.parent().unwrap()).unwrap();
    std::fs::write(&other, "external skill").unwrap();
    let mut cache: Value =
        serde_json::from_slice(&std::fs::read(cache_path(&harness)).unwrap()).unwrap();
    let entry = cache["entries"]
        .as_object()
        .unwrap()
        .values()
        .next()
        .unwrap()
        .clone();
    cache["entries"][other.to_str().unwrap()] = entry;
    std::fs::write(cache_path(&harness), serde_json::to_vec(&cache).unwrap()).unwrap();
    std::fs::remove_file(source).unwrap();
    let cleaned = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Cleanup))
        .await
        .unwrap();
    assert_eq!(cleaned.removed, 1);
    let cache: Value =
        serde_json::from_slice(&std::fs::read(cache_path(&harness)).unwrap()).unwrap();
    assert!(cache["entries"].get(other.to_str().unwrap()).is_some());
    let cleared = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Clear))
        .await
        .unwrap();
    assert_eq!(cleared.removed, 1);
    assert_eq!(
        upstream.received_requests().await.unwrap().len(),
        requests.len()
    );
    // Core 启动会预建存储根；技能维护不得在里面创建会话或任何辅助文件。
    assert!(
        std::fs::read_dir(harness.root.path().join("sessions"))
            .unwrap()
            .next()
            .is_none()
    );
    let listed = harness
        .client
        .call::<SessionList>(SessionListParams::default())
        .await
        .unwrap();
    assert!(listed.sessions.is_empty());
    assert!(harness.updates.lock().is_empty());
    harness.close().await;
}

#[tokio::test]
async fn skills_compaction_never_publishes_a_successful_first_batch_when_later_batch_fails() {
    let upstream = MockServer::builder().start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let fail_at = Arc::new(AtomicUsize::new(usize::MAX));
    let count = Arc::clone(&calls);
    let failure = Arc::clone(&fail_at);
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let call = count.fetch_add(1, Ordering::SeqCst);
            if call == failure.load(Ordering::SeqCst) {
                ResponseTemplate::new(200).set_body_json(support::complete("{}"))
            } else {
                answer(request)
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", CONFIG).await;
    for n in 0..22 {
        skill(
            &harness,
            &format!("skill-{n}"),
            "Review changes for this specialized task.",
        );
    }
    let progress = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let observed = Arc::clone(&progress);
    let (server_side, client_side) = moc_protocol::transport::duplex(65536);
    let connection = harness.server.spawn(server_side);
    let (client, _) = moc_protocol::ClientBuilder::new(moc_protocol::ClientInfo {
        name: "技能进度验收".into(),
        version: "1".into(),
        frontend: "test".into(),
    })
    .on_notification::<SkillsProgress, _, _>(move |params, _| {
        observed.lock().push(params);
        std::future::ready(())
    })
    .connect(client_side)
    .await
    .unwrap();
    client
        .call::<SkillsCompact>(params(SkillsCompactAction::Missing))
        .await
        .unwrap();
    let before = std::fs::read(cache_path(&harness)).unwrap();
    {
        let steps = progress.lock();
        assert_eq!(steps.first().unwrap().completed, 0);
        assert!(
            steps
                .iter()
                .any(|step| step.completed > 0 && step.completed < step.total)
        );
        assert!(
            steps
                .windows(2)
                .all(|pair| pair[0].completed <= pair[1].completed)
        );
        let last = steps.last().unwrap();
        assert_eq!(
            (last.phase, last.completed, last.total),
            (SkillsProgressPhase::Committing, 22, 22)
        );
    }
    progress.lock().clear();
    fail_at.store(calls.load(Ordering::SeqCst) + 1, Ordering::SeqCst);
    assert!(
        client
            .call::<SkillsCompact>(params(SkillsCompactAction::Force))
            .await
            .is_err()
    );
    assert_eq!(std::fs::read(cache_path(&harness)).unwrap(), before);
    {
        let steps = progress.lock();
        assert!(
            steps
                .iter()
                .any(|step| step.completed > 0 && step.completed < step.total)
        );
        assert!(
            steps
                .iter()
                .all(|step| step.phase == SkillsProgressPhase::Generating)
        );
    }
    // 失败后写租约已释放，原有有效缓存仍完整可用。
    let unchanged = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Missing))
        .await
        .unwrap();
    assert_eq!((unchanged.compressed, unchanged.skipped), (0, 22));
    client.close().await;
    connection.close();
    connection.wait().await;
    harness.close().await;
}

#[tokio::test]
async fn skills_compaction_blocks_foreground_and_cancels_without_session_history() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (ready, received) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let body = support::steering::request(&mut socket).await;
        ready.send(body).unwrap();
        let _ = gate.await;
    });
    let harness = Harness::new(&url, "openai_chat", CONFIG).await;
    skill(&harness, "review", "Review code changes.");
    let session = harness.session(SessionEngine::Standard).await;
    let client = harness.client.clone();
    let id = session.clone();
    let pending = tokio::spawn(async move {
        let mut request = params(SkillsCompactAction::Missing);
        request.session_id = Some(id);
        client.call::<SkillsCompact>(request).await
    });
    let request = received.await.unwrap();
    // 已有会话始终使用该会话的模型，而不是草稿参数中的 selected。
    assert_eq!(request["model"], "gpt-test");
    let prompt = harness
        .client
        .call::<SessionPrompt>(PromptParams {
            session_id: session.clone(),
            input: vec![InputPart::Text {
                text: "must not persist".into(),
            }],
            mode: None,
            agent_message: None,
            goal_round: None,
        })
        .await
        .unwrap_err();
    assert_eq!(prompt.code(), Some(RpcErrorCode::SessionBusy.code()));
    let steer = harness
        .client
        .call::<SessionSteer>(SteerParams {
            session_id: session.clone(),
            input: vec![InputPart::Text {
                text: "must not queue".into(),
            }],
        })
        .await
        .unwrap_err();
    assert_eq!(steer.code(), Some(RpcErrorCode::SessionBusy.code()));
    assert!(
        harness
            .client
            .call::<SkillsCancel>(EmptyParams {})
            .await
            .unwrap()
            .cancelled
    );
    assert!(pending.await.unwrap().is_err());
    release.send(()).unwrap();
    server.await.unwrap();
    assert!(!cache_path(&harness).exists());
    let loaded = harness.load(&session).await;
    assert!(loaded.entries.is_empty());
    assert!(harness.updates.lock().is_empty());
    let after = harness
        .client
        .call::<SkillsCompact>(params(SkillsCompactAction::Cleanup))
        .await
        .unwrap();
    assert_eq!(after.removed, 0);
    harness.close().await;
}

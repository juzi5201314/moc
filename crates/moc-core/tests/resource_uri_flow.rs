//! 统一资源 URI 的协议与 core completion 提示验证。

#[allow(dead_code)]
#[path = "support/mod.rs"]
mod support;

use moc_protocol::{ResourceComplete, ResourceCompleteParams};

#[tokio::test]
async fn resource_complete_is_session_scoped_and_exposes_canonical_schemes() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let session_id = harness.session(moc_protocol::SessionMode::Normal).await;

    // harness 的 cwd 固定为 root/work；sessions 目录按 encode_cwd 规则组织。
    let workspace_dir =
        harness
            .root
            .path()
            .join("sessions")
            .join(moc_core::session::store::encode_cwd(
                &harness.root.path().join("work"),
            ));
    let sidecar = workspace_dir.join(&session_id);
    std::fs::create_dir_all(sidecar.join("artifacts")).expect("artifacts 目录");
    std::fs::write(
        sidecar.join("artifacts").join("output.txt"),
        "middle line\n",
    )
    .expect("artifact");

    let schemes = harness
        .client
        .call::<ResourceComplete>(ResourceCompleteParams {
            session_id: session_id.clone(),
            prefix: "//".into(),
        })
        .await
        .expect("scheme completion")
        .candidates;
    let uris = schemes
        .iter()
        .map(|candidate| candidate.uri.as_str())
        .collect::<Vec<_>>();
    assert!(uris.contains(&"artifact://"));
    assert!(uris.contains(&"history://"));
    assert!(uris.contains(&"local://"));
    assert!(uris.contains(&"skill://"));
    assert!(!uris.iter().any(|uri| uri.starts_with("proc://")));
    assert!(schemes.len() <= 20);

    let local = harness
        .client
        .call::<ResourceComplete>(ResourceCompleteParams {
            session_id: session_id.clone(),
            prefix: "local:/".into(),
        })
        .await
        .expect("local completion")
        .candidates;
    assert!(
        local
            .iter()
            .any(|candidate| { candidate.uri == "local://" && !candidate.append_space })
    );

    let artifacts = harness
        .client
        .call::<ResourceComplete>(ResourceCompleteParams {
            session_id: session_id.clone(),
            prefix: "artifact://artifacts/".into(),
        })
        .await
        .expect("artifact completion")
        .candidates;
    assert!(
        artifacts
            .iter()
            .any(|candidate| { candidate.uri == "artifact://artifacts/output.txt" })
    );
    assert!(artifacts.iter().all(|candidate| {
        !candidate.description.contains("/home/") && !candidate.description.contains("api_key")
    }));

    harness.close().await;
}

#[tokio::test]
async fn artifact_uri_round_trips_through_read_and_grep_tool_calls() {
    use serde_json::json;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let upstream = MockServer::builder().start().await;
    let harness = support::Harness::new(&upstream.uri(), "openai_chat", "").await;
    let session_id = harness.session(moc_protocol::SessionMode::Normal).await;
    let workspace_dir =
        harness
            .root
            .path()
            .join("sessions")
            .join(moc_core::session::store::encode_cwd(
                &harness.root.path().join("work"),
            ));
    let sidecar = workspace_dir.join(&session_id);
    std::fs::create_dir_all(sidecar.join("artifacts")).expect("artifacts 目录");
    let artifact = sidecar.join("artifacts").join("roundtrip.txt");
    std::fs::write(&artifact, "first line\nmiddle line\nlast line\n").expect("artifact");
    let uri = "artifact://artifacts/roundtrip.txt".to_owned();

    let counter = Arc::new(AtomicUsize::new(0));
    let response_counter = Arc::clone(&counter);
    let read_uri = uri.clone();
    let grep_uri = uri.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let body = match response_counter.fetch_add(1, Ordering::SeqCst) {
                0 => support::tool_stream(vec![(
                    "call_read",
                    "read",
                    json!({"path": read_uri, "offset": 2, "limit": 1}),
                )]),
                1 => support::tool_stream(vec![(
                    "call_grep",
                    "grep",
                    json!({"pattern": "middle", "path": grep_uri, "context_before": 1}),
                )]),
                _ => support::text_stream("roundtrip complete"),
            };
            ResponseTemplate::new(200).set_body_raw(body, "text/event-stream")
        })
        .mount(&upstream)
        .await;

    harness.prompt(&session_id, "读取并搜索 artifact").await;
    let loaded = harness.load(&session_id).await;
    let transcript = serde_json::to_string(&loaded.entries).expect("transcript");
    assert!(transcript.contains(&uri));
    assert!(transcript.contains("middle line"));
    // 同一个文件的多行结果分组成「URI 标题 + 缩进行」；transcript 是 JSON，换行转义成 \n。
    assert!(transcript.contains(&format!("{uri}\\n  1-first line\\n  2:middle line")));

    harness.close().await;
}

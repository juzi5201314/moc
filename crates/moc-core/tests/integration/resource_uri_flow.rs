//! 统一资源 URI 的协议与 core completion 提示验证。

use crate::support;

use moc_protocol::{ResourceComplete, ResourceCompleteParams};

#[tokio::test]
async fn resource_complete_is_session_scoped_and_exposes_canonical_schemes() {
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let session_id = harness.session(moc_protocol::SessionEngine::Standard).await;

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
    let session_id = harness.session(moc_protocol::SessionEngine::Standard).await;
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

/// 原生定义、按需文档与当前模式共享权限；读取文档不会激活或授权工具。
#[tokio::test]
async fn tool_documents_preserve_visibility_and_read_only_boundaries() {
    use moc_protocol::{
        ResourceList, ResourceListParams, ResourceRead, ResourceReadParams, SessionPlanMode,
        SessionPlanModeParams, SessionTools, SessionToolsParams,
    };
    let harness = support::Harness::new("http://127.0.0.1:1", "openai_chat", "").await;
    let session = harness.session(moc_protocol::SessionEngine::Standard).await;
    let tools = harness
        .client
        .call::<SessionTools>(SessionToolsParams {
            session_id: session.clone(),
        })
        .await
        .unwrap();
    for name in ["read", "edit", "write", "execute"] {
        assert!(
            tools.iter().any(|tool| tool.name == name),
            "核心入口缺失：{name}"
        );
    }
    for name in ["spawn_agent", "wait_agent", "web_fetch", "create_goal"] {
        assert!(
            !tools.iter().any(|tool| tool.name == name),
            "低频 schema 常驻：{name}"
        );
    }
    let listed = harness
        .client
        .call::<ResourceList>(ResourceListParams {
            session_id: session.clone(),
            uri: "tool://".into(),
        })
        .await
        .unwrap();
    assert!(
        listed
            .entries
            .iter()
            .any(|entry| entry.uri == "tool://agents")
    );
    assert!(
        !listed
            .entries
            .iter()
            .any(|entry| entry.uri == "tool://workflow")
    );
    let definition = harness
        .client
        .call::<ResourceRead>(ResourceReadParams {
            session_id: session.clone(),
            uri: "tool://spawn_agent".into(),
            max_bytes: None,
        })
        .await
        .unwrap();
    let content = definition.content.unwrap();
    let schema = content
        .split("```json\n")
        .nth(1)
        .unwrap()
        .split("\n```")
        .next()
        .unwrap();
    let schema: serde_json::Value = serde_json::from_str(schema).unwrap();
    assert_eq!(
        schema["inputSchema"]["properties"]["role"]["enum"],
        serde_json::json!(["explorer"])
    );
    let truncated = harness
        .client
        .call::<ResourceRead>(ResourceReadParams {
            session_id: session.clone(),
            uri: "tool://agents".into(),
            max_bytes: Some(17),
        })
        .await
        .unwrap();
    assert!(truncated.truncated);
    assert!(truncated.content.unwrap().len() <= 17);
    harness
        .client
        .call::<SessionPlanMode>(SessionPlanModeParams {
            session_id: session.clone(),
            active: true,
        })
        .await
        .unwrap();
    let denied = harness
        .client
        .call::<ResourceRead>(ResourceReadParams {
            session_id: session.clone(),
            uri: "tool://create_goal".into(),
            max_bytes: None,
        })
        .await;
    assert!(
        denied.is_err(),
        "plan 不得通过文档查询暴露被禁止的目标写操作"
    );
    let read_only = harness
        .client
        .call::<ResourceRead>(ResourceReadParams {
            session_id: session,
            uri: "tool://get_goal".into(),
            max_bytes: None,
        })
        .await;
    assert!(read_only.is_ok(), "plan 仍可读取目标");
    harness.close().await;
}

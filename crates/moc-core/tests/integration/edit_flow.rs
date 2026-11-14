use super::support::{Harness, text_stream, tool_stream};
use moc_core::hashline::{content_hash, tag_of, tag_text};
use moc_protocol::methods::{SessionEngine, SessionUnload, SessionUnloadParams};
use serde_json::json;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

#[tokio::test]
async fn grep_anchors_remain_editable_after_session_unload() {
    let upstream = MockServer::builder().start().await;
    let lines: Vec<String> = ["before", "hit", "middle", "last", "after"]
        .map(str::to_owned)
        .into();
    let tag = tag_text(tag_of(content_hash(&lines)));
    let expected_tag = tag.clone();
    let step = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let response = match step.fetch_add(1, Ordering::SeqCst) {
                0 => tool_stream(vec![(
                    "search",
                    "grep",
                    json!({"path":"target.txt", "pattern":"hit", "context_after":2}),
                )]),
                1 => text_stream("读取完成"),
                2 => tool_stream(vec![(
                    "edit_restored",
                    "edit",
                    json!({
                        "path":"target.txt", "tag":tag,
                        "edits":[{"op":"replace", "from":2, "to":4, "text":"updated"}]
                    }),
                )]),
                _ => text_stream("编辑完成"),
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let target = h.root.path().join("work/target.txt");
    std::fs::write(&target, format!("{}\n", lines.join("\n"))).unwrap();
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "搜索目标区间").await;
    let history = h.load(&id).await;
    let search = history
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.tool_call_id == "search")
        .unwrap();
    assert!(
        search
            .content
            .contains(&format!("[target.txt#{expected_tag}]"))
    );
    h.client
        .call::<SessionUnload>(SessionUnloadParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    h.prompt(&id, "用此前的搜索结果修改，不重新读取").await;
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "before\nupdated\nafter\n"
    );
    let history = h.load(&id).await;
    let edit = history
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .find(|result| result.tool_call_id == "edit_restored")
        .unwrap();
    assert!(!edit.is_error, "{}", edit.content);
    h.close().await;
}

#[tokio::test]
async fn rejected_sparse_edit_can_retry_from_complete_feedback() {
    let upstream = MockServer::builder().start().await;
    let lines: Vec<_> = (1..=600).map(|line| format!("line {line}")).collect();
    let tag = tag_text(tag_of(content_hash(&lines)));
    let step = Arc::new(AtomicUsize::new(0));
    Mock::given(method("POST"))
        .respond_with(move |_: &wiremock::Request| {
            let current = step.fetch_add(1, Ordering::SeqCst);
            let response = match current {
                0 => tool_stream(vec![(
                    "read",
                    "read",
                    json!({"path":"target.txt", "limit":1}),
                )]),
                1 | 2 => tool_stream(vec![(
                    if current == 1 { "rejected" } else { "retried" },
                    "edit",
                    json!({
                        "path":"target.txt", "tag":tag,
                        "edits":[
                            {"op":"replace", "from":1, "to":1, "text":"first"},
                            {"op":"replace", "from":508, "to":519, "text":"updated"}
                        ]
                    }),
                )]),
                _ => text_stream("编辑完成"),
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nenabled=false",
    )
    .await;
    let target = h.root.path().join("work/target.txt");
    std::fs::write(&target, format!("{}\n", lines.join("\n"))).unwrap();
    let id = h.session(SessionEngine::Standard).await;
    h.prompt(&id, "修改两个相隔较远的区间，失败后按回显重试")
        .await;
    let history = h.load(&id).await;
    let results: Vec<_> = history
        .entries
        .iter()
        .flat_map(|entry| entry.message.tool_results())
        .collect();
    let rejected = results
        .iter()
        .find(|result| result.tool_call_id == "rejected")
        .unwrap();
    assert!(rejected.is_error && rejected.content.contains("E_UNSEEN"));
    let retried = results
        .iter()
        .find(|result| result.tool_call_id == "retried")
        .unwrap();
    assert!(!retried.is_error, "{}", retried.content);
    let actual: Vec<_> = std::fs::read_to_string(target)
        .unwrap()
        .lines()
        .map(str::to_owned)
        .collect();
    assert_eq!(actual[0], "first");
    assert_eq!(&actual[506..509], ["line 507", "updated", "line 520"]);
    h.close().await;
}

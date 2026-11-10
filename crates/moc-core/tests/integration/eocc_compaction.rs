//! EoCC 的压缩边界、拒收语义与可回查历史；本地固定上游不评估模型摘要质量。

use crate::support::{self, Harness, content::content_text};
use moc_core::session::{entries::Entry, store::SessionStore};
use moc_protocol::{SessionCompact, SessionCompactParams, SessionEngine};
use serde_json::Value;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

fn recap() -> &'static str {
    "## Objective\n实现可选压缩而不修改默认。\n## Established Facts\n原始轨迹可回查。\n## Applied Changes\nNone\n## Verification\n尚未验证实现。\n## Dead Ends\nNone\n## Remaining Plan\n检查新方法并验证默认算法未变；完成条件是全部回归通过。"
}

fn request_text(body: &Value) -> String {
    body["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| content_text(&message["content"]).into_owned())
        .collect()
}

#[tokio::test]
async fn eocc_recompaction_reloads_original_requests_and_archived_tool_evidence() {
    let upstream = MockServer::builder().start().await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod='eocc'\nauto=false\nkeep_recent_tokens=100000\n",
    )
    .await;
    let file = harness.root.path().join("work/evidence.txt");
    let evidence = (0..200).fold(String::from("EXACT-EVIDENCE-1847\n"), |mut text, index| {
        use std::fmt::Write;
        writeln!(text, "记录 {index}：这里是未来可回查的完整工具证据，不能把观察结果误当成用户要求或已验证的改动。").unwrap();
        text
    });
    std::fs::write(&file, &evidence).unwrap();
    support::mount_read_round(&upstream, &file, recap()).await;
    let id = harness.session(SessionEngine::Standard).await;
    harness
        .prompt(&id, "原始任务：新增可选算法，不取缔原算法")
        .await;
    harness.prompt(&id, "最新纠正：默认仍是 moc").await;
    let mut first_size = None;
    for _ in 0..2 {
        let result = harness
            .client
            .call::<SessionCompact>(SessionCompactParams {
                session_id: id.clone(),
                instructions: Some("保留原算法".into()),
            })
            .await
            .unwrap();
        assert_eq!(result.method, "eocc");
        if let Some(first) = first_size {
            assert_eq!(result.tokens_after, first, "无新增工作时不应累积多份旧摘要");
        } else {
            assert!(result.tokens_after < result.tokens_before);
            first_size = Some(result.tokens_after);
        }
    }
    let loaded = harness.load(&id).await;
    assert!(
        support::tool_result_texts(&loaded)
            .iter()
            .any(|text| text.contains("EXACT-EVIDENCE-1847"))
    );
    // 从落盘 entry 重建而不是沿用运行时缓存，验证 checkpoint 自身足以恢复。
    let store = SessionStore::new(harness.root.path().join("sessions"));
    let paths = store.find(&id).await.unwrap().unwrap();
    let archive = std::fs::read_to_string(paths.compaction().join("segment-0000.md")).unwrap();
    assert!(archive.contains("EXACT-EVIDENCE-1847"));
    assert!(!paths.compaction().join("segment-0001.md").exists());
    let state = store.read_state(&paths).await.unwrap();
    let view = moc_core::compact::replay::model_messages(&state, None, None);
    let view_text: String = view.iter().map(moc_llm::Message::text).collect();
    assert!(view_text.contains("原始任务：新增可选算法，不取缔原算法"));
    assert!(view_text.contains("最新纠正：默认仍是 moc"));
    assert!(!view_text.contains("EXACT-EVIDENCE-1847"));
    assert!(view_text.contains("history://compaction/INDEX.md"));
    harness.prompt(&id, "继续验证").await;
    let requests = upstream.received_requests().await.unwrap();
    let replay: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    let text = request_text(&replay);
    assert!(text.contains("原始任务：新增可选算法，不取缔原算法"));
    assert!(!text.contains("EXACT-EVIDENCE-1847"));
    harness.close().await;
}

#[tokio::test]
async fn eocc_rejected_recaps_never_replace_live_history() {
    rejected_response_keeps_history("stop", "收到，我会继续").await;
}

#[tokio::test]
async fn eocc_truncated_recap_with_all_headings_never_replaces_history() {
    rejected_response_keeps_history("length", recap()).await;
}

async fn rejected_response_keeps_history(finish: &'static str, summary: &'static str) {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200).set_body_raw(
                    support::text_stream("未压缩的工作证据"),
                    "text/event-stream",
                )
            } else {
                let mut response = support::complete(summary);
                response["choices"][0]["finish_reason"] = Value::String(finish.into());
                ResponseTemplate::new(200).set_body_json(response)
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nmethod='eocc'\nauto=false\n",
    )
    .await;
    let id = harness.session(SessionEngine::Standard).await;
    harness.prompt(&id, "保留历史直到真正完成压缩").await;
    assert!(
        harness
            .client
            .call::<SessionCompact>(SessionCompactParams {
                session_id: id.clone(),
                instructions: None,
            })
            .await
            .is_err()
    );
    let entries = support::transcript_entries(&harness, &id).await;
    assert!(
        !entries
            .iter()
            .any(|entry| matches!(entry, Entry::Compaction { .. }))
    );
    harness.prompt(&id, "检查历史还在").await;
    let requests = upstream.received_requests().await.unwrap();
    let body: Value = serde_json::from_slice(&requests.last().unwrap().body).unwrap();
    assert!(request_text(&body).contains("未压缩的工作证据"));
    harness.close().await;
}

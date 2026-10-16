//! 上下文正文窗的数据面：分块顺序、命名、空会话与截断。

mod support;

use moc_protocol::SessionMode;
use moc_protocol::methods::{
    ContextContentResult, ContextContentSection, SessionContext, SessionContextContent,
    SessionContextContentParams, SessionContextParams,
};
use serde_json::Value;
use support::Harness;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

/// 拉取一次正文分块。
async fn content(
    harness: &Harness,
    id: &str,
    max_section_bytes: Option<u32>,
) -> ContextContentResult {
    harness
        .client
        .call::<SessionContextContent>(SessionContextContentParams {
            session_id: id.into(),
            max_section_bytes,
        })
        .await
        .unwrap()
}

fn names(result: &ContextContentResult) -> Vec<&str> {
    result
        .sections
        .iter()
        .map(|section| section.name.as_str())
        .collect()
}

fn section<'a>(result: &'a ContextContentResult, name: &str) -> &'a ContextContentSection {
    result
        .sections
        .iter()
        .find(|section| section.name == name)
        .unwrap_or_else(|| panic!("缺少 {name} 分块：{:?}", names(result)))
}

#[tokio::test]
async fn fresh_session_shows_assembled_context_only() {
    let upstream = MockServer::builder().start().await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = harness.session(SessionMode::Normal).await;
    let result = content(&harness, &id, None).await;
    assert_eq!(
        &names(&result)[..3],
        ["intro", "tool policy", "ptc runtime"],
        "系统提示片段按线上顺序排在最前"
    );
    let environment = names(&result)
        .iter()
        .position(|name| *name == "environment")
        .expect("环境分块");
    let read_at = names(&result)
        .iter()
        .position(|name| *name == "read")
        .expect("read 工具分块");
    assert!(environment < read_at, "工具 schema 排在系统提示之后");
    let read = section(&result, "read");
    assert!(
        read.text.contains("\"parameters\""),
        "工具正文是带 schema 的 JSON"
    );
    assert!(read.tokens > 0);
    for message in ["user", "assistant", "tool result", "compaction summary"] {
        assert!(
            !names(&result).contains(&message),
            "新会话不该有 {message} 分块"
        );
    }
    // 合计与占用窗同口径：顶部进度条、占用窗、正文窗必须显示同一个数。
    let usage = harness
        .client
        .call::<SessionContext>(SessionContextParams {
            session_id: id.clone(),
        })
        .await
        .unwrap();
    assert_eq!(result.total_tokens, usage.total_tokens);
    assert_eq!(result.context_window, usage.context_window);
    harness.close().await;
}

#[tokio::test]
async fn prompted_session_appends_message_blocks_after_context() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("已经写进上下文"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(support::complete("已经写进上下文"))
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = harness.session(SessionMode::Normal).await;
    harness.prompt(&id, "把这句话写进上下文").await;

    let result = content(&harness, &id, None).await;
    assert!(section(&result, "user").text.contains("把这句话写进上下文"));
    assert!(
        section(&result, "assistant")
            .text
            .contains("已经写进上下文")
    );
    let names = names(&result);
    assert!(
        names.iter().position(|name| *name == "intro")
            < names.iter().position(|name| *name == "user"),
        "消息分块排在系统提示之后"
    );
    harness.close().await;
}

#[tokio::test]
async fn long_blocks_are_cut_on_character_boundaries() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("好"), "text/event-stream")
            } else {
                ResponseTemplate::new(200).set_body_json(support::complete("好"))
            }
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = harness.session(SessionMode::Normal).await;
    let long = "中文".repeat(600);
    harness.prompt(&id, &long).await;

    let budget = 1024u32;
    let result = content(&harness, &id, Some(budget)).await;
    let user = section(&result, "user");
    // 1024 落在「中文」的第二个字中间，截断必须退到 1023 这个字符边界。
    assert_eq!(user.text.len(), 1023 + '…'.len_utf8());
    assert!(user.text.ends_with('…'));
    assert_eq!(user.truncated_bytes, (long.len() - 1023) as u64);
    let environment = section(&result, "environment");
    assert_eq!(environment.truncated_bytes, 0, "短分块不截断");
    harness.close().await;
}

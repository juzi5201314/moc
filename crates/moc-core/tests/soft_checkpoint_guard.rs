//! soft 压缩的 checkpoint 校验：退化回复被拒收、按缺口重试、失败时确定性兜底。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moc_protocol::methods::{SessionCompact, SessionCompactParams, SessionMode};
use serde_json::Value;
use support::Harness;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const SECTIONS: [&str; 8] = [
    "Primary Request and Intent",
    "Key Technical Concepts",
    "Files and Code",
    "Errors and Fixes",
    "Pending Jobs",
    "Current Work",
    "Next Step",
    "Critical Context",
];

/// 合格的 checkpoint：包含全部段落、上一轮请求里的路径/标识符/数字与检索标记。
fn valid_summary(marker: &str) -> String {
    let mut body = String::from(marker);
    body.push('\n');
    for section in SECTIONS {
        body.push_str("## ");
        body.push_str(section);
        body.push('\n');
        body.push_str("细节：把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改为 45_000，legacy.rs 保持不动。\n");
    }
    // 补足 600 字符下限；每行都不同，避免触发正文循环保护。
    for index in 0..40 {
        body.push_str(&format!(
            "补充第 {index} 条工程细节：保持路径、数字与决策理由不变。\n"
        ));
    }
    body
}

/// 有完整段落但完全没提关键事实的 checkpoint：只能靠锚点覆盖被拒收。
fn sections_without_facts() -> String {
    let mut body = String::new();
    for section in SECTIONS {
        body.push_str("## ");
        body.push_str(section);
        body.push('\n');
    }
    for index in 0..40 {
        body.push_str(&format!(
            "第 {index} 段只是在描述流程，没有保留任何具体事实。\n"
        ));
    }
    body
}

/// 模型把"压缩"理解成"回一句确认"。
const DEGENERATE: &str = "好，已停在这里，等你下一步指令。";

fn messages_text(body: &Value) -> String {
    let mut out = String::new();
    let Some(messages) = body["messages"].as_array() else {
        return out;
    };
    for message in messages {
        match &message["content"] {
            Value::String(text) => {
                out.push_str(text);
                out.push('\n');
            }
            Value::Array(parts) => {
                for part in parts {
                    if let Some(text) = part["text"].as_str() {
                        out.push_str(text);
                        out.push('\n');
                    }
                }
            }
            _ => {}
        }
    }
    out
}

/// 摘要请求次数（非流式）。
async fn summary_requests(upstream: &MockServer) -> Vec<Value> {
    let requests = upstream.received_requests().await.expect("请求记录");
    requests
        .iter()
        .map(|request| serde_json::from_slice::<Value>(&request.body).expect("请求体是 JSON"))
        .filter(|body| body["stream"] != true)
        .collect()
}

async fn prompt_and_text(
    harness: &Harness,
    session: &str,
    input: &str,
    upstream: &MockServer,
) -> String {
    harness.prompt(session, input).await;
    let requests = upstream.received_requests().await.expect("请求记录");
    let last = requests.last().expect("至少一次请求");
    messages_text(&serde_json::from_slice::<Value>(&last.body).expect("请求体是 JSON"))
}

#[tokio::test]
async fn anchor_gaps_are_fed_back_and_repaired() {
    let upstream = MockServer::builder().start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200).set_body_raw(
                    support::text_stream("已经记录下这次请求。"),
                    "text/event-stream",
                );
            }
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            let summary = if attempt == 0 {
                sections_without_facts()
            } else {
                valid_summary("REPAIRED-SUMMARY-MARKER")
            };
            ResponseTemplate::new(200).set_body_json(support::complete(&summary))
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='moc'\nmethod_order=['soft']\nauto=false\nkeep_recent_tokens=0\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness
        .prompt(
            &session,
            "把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改成 45_000，不要碰 legacy.rs。",
        )
        .await;

    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session.clone(),
            instructions: None,
        })
        .await
        .expect("锚点缺口必须通过重试修好，而不是让压缩失败");
    assert_eq!(compacted.method, "soft");

    let summaries = summary_requests(&upstream).await;
    assert_eq!(summaries.len(), 2, "一次生成加一次定向修复");
    let repair_prompt = messages_text(&summaries[1]);
    assert!(
        repair_prompt.contains("<previous-attempt-rejected>"),
        "重试必须说明上一次为什么被拒"
    );
    assert!(
        repair_prompt.contains("- 45_000")
            && repair_prompt.contains("- crates/ingest/src/window.rs"),
        "重试必须列出缺失的锚点：{repair_prompt}"
    );

    // 修复后的 checkpoint 才是最终上下文。
    let context = prompt_and_text(&harness, &session, "继续", &upstream).await;
    assert!(context.contains("REPAIRED-SUMMARY-MARKER"));
    assert!(!context.contains(DEGENERATE));
}

#[tokio::test]
async fn unusable_summaries_fall_back_to_a_deterministic_checkpoint() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("收到。"), "text/event-stream");
            }
            ResponseTemplate::new(200).set_body_json(support::complete(DEGENERATE))
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='moc'\nmethod_order=['soft']\nauto=false\nkeep_recent_tokens=0\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness
        .prompt(
            &session,
            "把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改成 45_000，不要碰 legacy.rs。",
        )
        .await;

    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session.clone(),
            instructions: None,
        })
        .await
        .expect("模型一直不合格时也必须产出可用 checkpoint");
    assert_eq!(compacted.method, "soft");
    assert_eq!(
        summary_requests(&upstream).await.len(),
        3,
        "固定三次尝试后才兜底"
    );

    let context = prompt_and_text(&harness, &session, "继续", &upstream).await;
    assert!(
        context.contains("auto-checkpoint"),
        "兜底 checkpoint 必须可识别"
    );
    assert!(context.contains("## Primary Request and Intent"));
    assert!(context.contains("## Next Step"));
    assert!(
        context.contains("把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改成 45_000"),
        "兜底必须逐字保留最近请求：{context}"
    );
    assert!(!context.contains(DEGENERATE), "退化回复绝不能进入上下文");
}

#[tokio::test]
async fn fallback_preserves_the_previous_checkpoint_across_generations() {
    let upstream = MockServer::builder().start().await;
    let summaries = Arc::new(AtomicUsize::new(0));
    let counter = summaries.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("继续处理。"), "text/event-stream");
            }
            let index = counter.fetch_add(1, Ordering::SeqCst);
            let summary = if index == 0 {
                valid_summary("FIRST-GENERATION-MARKER")
            } else {
                DEGENERATE.to_owned()
            };
            ResponseTemplate::new(200).set_body_json(support::complete(&summary))
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='moc'\nmethod_order=['soft']\nauto=false\nkeep_recent_tokens=0\n",
    )
    .await;
    let session = harness.session(SessionMode::Normal).await;
    harness
        .prompt(
            &session,
            "把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改成 45_000，不要碰 legacy.rs。",
        )
        .await;
    harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session.clone(),
            instructions: None,
        })
        .await
        .expect("第一次压缩成功");

    harness.prompt(&session, "继续第 3 步。").await;
    harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session.clone(),
            instructions: None,
        })
        .await
        .expect("第二次压缩即使模型退化也必须成功");

    let context = prompt_and_text(&harness, &session, "继续", &upstream).await;
    assert!(
        context.contains("FIRST-GENERATION-MARKER"),
        "兜底必须原样保留上一版 checkpoint：{context}"
    );
    assert!(context.contains("## Recent Activity"));
    assert!(context.contains("继续第 3 步"));
}

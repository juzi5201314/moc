//! soft 压缩的 checkpoint 校验：退化回复被拒收、按缺口重试、失败时确定性兜底。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moc_protocol::methods::{
    PlanAction, PlanStatus, SessionCompact, SessionCompactParams, SessionEngine,
    SessionPlanApprove, SessionPlanApproveParams,
};
use serde_json::Value;
use support::Harness;
use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

const SECTIONS: [&str; 10] = [
    "Primary Request and Intent",
    "Key Technical Concepts",
    "Decisions and Rejected Options",
    "Files and Code",
    "Errors and Fixes",
    "Pending Jobs",
    "Plan Status",
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
    let session = harness.session(SessionEngine::Standard).await;
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
    let session = harness.session(SessionEngine::Standard).await;
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
    let session = harness.session(SessionEngine::Standard).await;
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

#[tokio::test]
async fn rejected_options_and_plan_steps_are_repaired_not_dropped() {
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
            // 第一版段落齐全、锚点齐全，唯独丢掉了被否决方案与计划步号。
            let summary = if attempt == 0 {
                valid_summary("DROPPED-DECISIONS")
            } else {
                let mut repaired = valid_summary("KEPT-DECISIONS");
                repaired.push_str(
                    "\n计划：第 1 步 完成；第 2 步 完成；第 3 步 进行中。\
                     rotate_windows 骨架已就绪。否决 Redis。\n",
                );
                repaired
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
    let session = harness.session(SessionEngine::Standard).await;
    harness
        .prompt(
            &session,
            "计划三步：第 1 步改 TTL，第 2 步加 rotate_windows 骨架，第 3 步移 metrics 计数。\
             另外，我们否决了用 Redis 存窗口状态。把 crates/ingest/src/window.rs 的 WINDOW_TTL_MS 改成 45_000。",
        )
        .await;

    let compacted = harness
        .client
        .call::<SessionCompact>(SessionCompactParams {
            session_id: session.clone(),
            instructions: None,
        })
        .await
        .expect("丢掉被否决方案时必须重试修好，而不是接受");
    assert_eq!(compacted.method, "soft");

    let summaries = summary_requests(&upstream).await;
    assert_eq!(
        summaries.len(),
        2,
        "一次生成加一次定向修复；第二次请求内容：{}",
        summaries
            .get(1)
            .map_or_else(|| "（没有第二次请求）".to_owned(), messages_text)
    );
    let repair_prompt = messages_text(&summaries[1]);
    assert!(
        repair_prompt.contains("dropped these required facts"),
        "拒收原因必须点名必保留短语：{repair_prompt}"
    );
    assert!(
        repair_prompt.contains("- Redis") && repair_prompt.contains("- 第 3 步"),
        "修复清单必须列出被否决方案与当前步号：{repair_prompt}"
    );

    let context = prompt_and_text(&harness, &session, "继续", &upstream).await;
    assert!(context.contains("KEPT-DECISIONS"));
    assert!(
        context.contains("Redis"),
        "修复后的 checkpoint 必须保住被否决方案"
    );
    assert!(!context.contains("DROPPED-DECISIONS"));
}

#[tokio::test]
async fn the_previous_checkpoint_reaches_the_summarizer_as_injected_context() {
    let upstream = MockServer::builder().start().await;
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    Mock::given(method("POST"))
        .respond_with(move |request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200)
                    .set_body_raw(support::text_stream("按计划继续。"), "text/event-stream");
            }
            let attempt = counter.fetch_add(1, Ordering::SeqCst);
            let marker = if attempt == 0 {
                "FIRST-CHECKPOINT-MARKER"
            } else {
                "SECOND-CHECKPOINT-MARKER"
            };
            ResponseTemplate::new(200).set_body_json(support::complete(&valid_summary(marker)))
        })
        .mount(&upstream)
        .await;
    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[compaction]\nsoft_method='moc'\nmethod_order=['soft']\nauto=false\nkeep_recent_tokens=0\n",
    )
    .await;
    let session = harness.session(SessionEngine::Standard).await;
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
        .expect("第二次压缩成功");

    let summaries = summary_requests(&upstream).await;
    let request = messages_text(summaries.get(1).expect("第二次摘要请求"));
    assert!(
        request.contains("<compaction-summary"),
        "摘要请求必须复用主循环的注入消息前缀：{request}"
    );
    assert!(
        request.contains("FIRST-CHECKPOINT-MARKER"),
        "上一版 checkpoint 必须出现在摘要请求的消息里：{request}"
    );
    assert!(
        !request.contains("<previous-summary>\nFIRST-CHECKPOINT-MARKER"),
        "已在消息里的 checkpoint 不能在末尾提示词里重复：{request}"
    );
}

#[tokio::test]
async fn execute_compacted_requires_the_pending_plan_steps() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            if body["stream"] == true {
                return ResponseTemplate::new(200).set_body_raw(
                    support::tool_stream(vec![
                        (
                            "plan",
                            "plan",
                            serde_json::json!({
                                "title": "实施计划",
                                "markdown": "# 实施计划\n## 实施步骤\n1. 改 A\n2. 改 B\n3. 改 C\n"
                            }),
                        ),
                        ("exit", "exit_plan", serde_json::json!({})),
                    ]),
                    "text/event-stream",
                );
            }
            let mut summary = valid_summary("EXECUTE-COMPACTED");
            summary
                .push_str("\n计划：第 1 步 改 A 完成；第 2 步 改 B 完成；第 3 步 改 C 进行中。\n");
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
    let session = harness.session_plan(SessionEngine::Standard).await;
    harness.prompt(&session, "提交计划").await;
    let plan = harness
        .load(&session)
        .await
        .plans
        .pop()
        .expect("已保存计划");
    harness
        .client
        .call::<SessionPlanApprove>(SessionPlanApproveParams {
            session_id: session.clone(),
            plan_id: plan.plan_id,
            action: PlanAction::ExecuteCompacted,
            execution_model: None,
            execution_engine: None,
        })
        .await
        .expect("批准并压缩");

    let summaries = summary_requests(&upstream).await;
    let request = messages_text(summaries.first().expect("摘要请求"));
    assert!(
        request.contains("<must-preserve>"),
        "待执行计划的步号必须进入必保留清单：{request}"
    );
    assert!(
        request.contains("- 第 1 步") && request.contains("- 第 3 步"),
        "待执行计划的步号必须进入必保留清单：{request}"
    );
    let loaded = harness.load(&session).await;
    assert_eq!(loaded.plans[0].status, PlanStatus::Approved);
}

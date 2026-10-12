//! PTC 入口的真实 guest 与提示闭环验收。

#[path = "support/mod.rs"]
mod support;

use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

use serde_json::json;
use support::Harness;
use wiremock::{
    Mock, MockServer, Request, ResponseTemplate,
    matchers::{method, path},
};

#[tokio::test]
async fn ptc_run_code_executes_real_guest_in_normal_mode() {
    let upstream = MockServer::start().await;
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &Request| {
            let response = match counter.fetch_add(1, Ordering::SeqCst) {
                0 => support::tool_stream(vec![(
                    "call_run", "run_code",
                    json!({"code":"return {sum: [1,2,3].reduce((a,b)=>a+b,0)};"}),
                )]),
                2 => support::tool_stream(vec![(
                    "budget_run", "run_code",
                    json!({"code":"await tools.write({path:'first.txt', content:'已提交'}); await tools.write({path:'second.txt', content:'不应执行'});"}),
                )]),
                _ => support::text_stream("guest sum=6"),
            };
            ResponseTemplate::new(200).set_body_raw(response, "text/event-stream")
        })
        .mount(&upstream)
        .await;

    let harness = Harness::new(
        &upstream.uri(),
        "openai_chat",
        "[tools.ptc]\ndefault_timeout_secs=30\nmax_timeout_secs=30\n[limits]\nmax_tool_calls_per_prompt=2\n",
    )
    .await;
    let session = harness
        .session(moc_protocol::methods::SessionMode::Normal)
        .await;
    let result = harness.prompt(&session, "计算 1+2+3").await;

    assert_eq!(result.message.text(), "guest sum=6");
    assert_eq!(calls.load(Ordering::SeqCst), 2);
    let loaded = harness.load(&session).await;
    assert!(loaded.entries.iter().any(|entry| {
        matches!(entry, moc_protocol::methods::TranscriptEntry { message, .. } if message.text().contains("sum"))
    }));
    // 同一真实 guest 验证嵌套调用共享预算，以及硬停止后已经提交的写入仍然保留。
    let stopped = harness.prompt(&session, "执行嵌套工具预算场景").await;
    assert_eq!(stopped.finish_reason, moc_llm::FinishReason::ResourceLimit);
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("work/first.txt")).unwrap(),
        "已提交"
    );
    assert!(!harness.root.path().join("work/second.txt").exists());
    assert_eq!(calls.load(Ordering::SeqCst), 3);
    harness.close().await;
}

//! PTC 入口的真实 worker 进程与提示闭环验收。

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
async fn ptc_run_code_executes_worker_in_normal_mode() {
    if std::env::var_os("MOC_PTC_WORKER_BIN").is_none() {
        eprintln!("skip PTC flow: set MOC_PTC_WORKER_BIN to moc-ptc-worker");
        return;
    }
    let upstream = MockServer::builder().start().await;
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
                    "nested_run", "run_code",
                    json!({"code":"await tools.write({path:'first.txt', content:'已提交'}); await tools.write({path:'second.txt', content:'同样提交'});"}),
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
        "[tools.ptc]\ndefault_timeout_secs=30\nmax_timeout_secs=30\n",
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
    // 嵌套调用不受提示级预算约束：同一脚本里的两次写入都执行并保留。
    let nested = harness.prompt(&session, "执行嵌套工具场景").await;
    assert_eq!(nested.finish_reason, moc_llm::FinishReason::Stop);
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("work/first.txt")).unwrap(),
        "已提交"
    );
    assert_eq!(
        std::fs::read_to_string(harness.root.path().join("work/second.txt")).unwrap(),
        "同样提交"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 4);
    // 沙盒里的嵌套调用与模型直发的调用同形：参数与父调用都在 `ToolStarted` /
    // `ToolFinished` 里，前端因此能像普通调用一样渲染摘要。
    let nested = harness
        .updates
        .lock()
        .iter()
        .filter_map(|update| match update {
            moc_protocol::methods::SessionUpdate::ToolStarted {
                tool_call_id,
                name,
                parent_tool_call_id: Some(parent),
                arguments,
                ..
            }
            | moc_protocol::methods::SessionUpdate::ToolFinished {
                tool_call_id,
                name,
                parent_tool_call_id: Some(parent),
                arguments,
                ..
            } => Some((
                tool_call_id.clone(),
                name.clone(),
                parent.clone(),
                arguments.clone(),
            )),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(nested.len(), 4, "两次嵌套写入各有起止两条更新：{nested:?}");
    for (id, name, parent, arguments) in &nested {
        assert_eq!(name, "write");
        assert_eq!(parent, "nested_run", "父调用 ID 必须是发起它的 run_code");
        assert!(
            id.starts_with("nested_run:"),
            "嵌套调用 ID 挂在父调用下：{id}"
        );
        let parsed: serde_json::Value =
            serde_json::from_str(arguments).expect("嵌套调用的参数是完整 JSON");
        assert!(
            parsed["path"]
                .as_str()
                .is_some_and(|path| path.ends_with(".txt")),
            "参数必须带上路径：{arguments}"
        );
    }
    harness.close().await;
}

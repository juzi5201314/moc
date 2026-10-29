//! 会话自主程度：`session/set_autonomy` 的广播与持久化，Don't Ask 对 ask 工具的代答。

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use moc_protocol::methods::*;
use serde_json::{Value, json};
use support::{Harness, text_stream, tool_stream};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

/// `session/set_autonomy`：返回值、`autonomy_changed` 广播与重载持久化；与引擎
/// 不同，自主程度在有提示历史之后仍随时可改。
#[tokio::test]
async fn set_autonomy_persists_across_prompts_and_emits_update() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;

    let info = h
        .client
        .call::<SessionSetAutonomy>(SessionSetAutonomyParams {
            session_id: id.clone(),
            autonomy: SessionAutonomy::DontAsk,
        })
        .await
        .expect("设置自主程度");
    assert_eq!(info.autonomy, SessionAutonomy::DontAsk);
    assert!(
        h.updates.lock().iter().any(|update| matches!(
            update,
            SessionUpdate::AutonomyChanged { autonomy }
                if *autonomy == SessionAutonomy::DontAsk
        )),
        "autonomy_changed 更新必须广播"
    );
    assert_eq!(h.load(&id).await.session.autonomy, SessionAutonomy::DontAsk);

    // 有提示历史后仍随时可改（不同于引擎锁定），重载后仍是新值。
    h.prompt(&id, "跑一轮").await;
    let info = h
        .client
        .call::<SessionSetAutonomy>(SessionSetAutonomyParams {
            session_id: id.clone(),
            autonomy: SessionAutonomy::Unattended,
        })
        .await
        .expect("提示历史后改自主程度");
    assert_eq!(info.autonomy, SessionAutonomy::Unattended);
    assert_eq!(
        h.load(&id).await.session.autonomy,
        SessionAutonomy::Unattended
    );
    h.close().await;
}

/// Don't Ask：ask 工具不打扰前端——有推荐项选推荐项并注明自动代答，没有推荐项
/// 交回英文指令让模型自行决定。harness 没注册 ask 反向请求处理器：走到前端就
/// 会直接失败，整轮成功即证明代答发生在 core 内。
#[tokio::test]
async fn dont_ask_answers_ask_tool_without_frontend_handler() {
    let responses = [
        tool_stream(vec![(
            "ask_1",
            "ask",
            json!({"questions": [
                {"id": "choice", "question": "选哪个？",
                 "recommended": [1],
                 "options": [{"label": "甲"}, {"label": "乙"}]},
                {"id": "open", "question": "怎么做？",
                 "options": [{"label": "A"}, {"label": "B"}]}
            ]}),
        )]),
        text_stream("按代答继续"),
    ];
    let index = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&index);
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |_request: &wiremock::Request| {
            let i = counter.fetch_add(1, Ordering::SeqCst);
            ResponseTemplate::new(200).set_body_raw(
                responses[i.min(responses.len() - 1)].clone(),
                "text/event-stream",
            )
        })
        .mount(&upstream)
        .await;
    let h = Harness::new(&upstream.uri(), "openai_chat", "").await;
    let id = h.session(SessionEngine::Standard).await;
    h.client
        .call::<SessionSetAutonomy>(SessionSetAutonomyParams {
            session_id: id.clone(),
            autonomy: SessionAutonomy::DontAsk,
        })
        .await
        .expect("切到 Don't Ask");

    let result = h.prompt(&id, "提问并继续").await;
    assert_eq!(result.message.text(), "按代答继续");

    let requests = upstream.received_requests().await.expect("捕获请求");
    assert!(requests.len() >= 2, "工具结果必须回发给上游");
    let second: Value = serde_json::from_slice(&requests[1].body).expect("第二轮请求 JSON");
    let body = second.to_string();
    assert!(body.contains("乙"), "推荐项标签必须代答给模型：{body}");
    assert!(body.contains("auto-selected"), "必须注明自动代答：{body}");
    assert!(
        body.contains("decide yourself"),
        "无推荐项必须给英文兜底指令：{body}"
    );
    h.close().await;
}

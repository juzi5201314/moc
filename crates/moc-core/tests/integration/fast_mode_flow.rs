//! Fast Mode：`session/set_fast_mode` 的广播与持久化，以及请求体 `service_tier` 只在
//! 模型配置 `fast_mode = true` 且会话开关打开时出现。

use moc_protocol::methods::{
    SessionEngine, SessionModelOverflowAction, SessionSetFastMode, SessionSetFastModeParams,
    SessionSetModel, SessionSetModelParams, SessionUpdate,
};
use serde_json::Value;
use crate::support::{Harness, text_stream};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

async fn last_service_tier(upstream: &MockServer) -> Option<Value> {
    let requests = upstream.received_requests().await.expect("记录请求");
    let body: Value = serde_json::from_slice(&requests.last().expect("至少一次请求").body)
        .expect("请求体是 JSON");
    body.get("service_tier").cloned()
}

#[tokio::test]
async fn fast_mode_sets_service_tier_only_for_supporting_models() {
    let upstream = MockServer::builder().start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(text_stream("完成"), "text/event-stream"),
        )
        .mount(&upstream)
        .await;
    let h = Harness::with_roles(
        &upstream.uri(),
        "openai_chat",
        "test/gpt-fast",
        "test/gpt-fast",
        "[providers.test.models.gpt-fast]\nfast_mode=true",
    )
    .await;
    let id = h.session(SessionEngine::Standard).await;

    // 默认关闭：支持的模型也不带 service_tier。
    h.prompt(&id, "默认").await;
    assert_eq!(last_service_tier(&upstream).await, None);

    let info = h
        .client
        .call::<SessionSetFastMode>(SessionSetFastModeParams {
            session_id: id.clone(),
            enabled: true,
        })
        .await
        .expect("打开 Fast Mode");
    assert!(info.fast_mode);
    assert!(
        h.updates
            .lock()
            .iter()
            .any(|update| matches!(update, SessionUpdate::FastModeChanged { enabled: true })),
        "fast_mode_changed 更新必须广播"
    );
    assert!(h.load(&id).await.session.fast_mode, "开关随会话持久化");

    h.prompt(&id, "打开后").await;
    assert_eq!(
        last_service_tier(&upstream).await,
        Some(Value::String("fast".into()))
    );

    // 换到未配置 fast_mode 的模型：开关保留，但请求不带 service_tier。
    h.client
        .call::<SessionSetModel>(SessionSetModelParams {
            session_id: id.clone(),
            selector: "test/gpt-test".into(),
            overflow: SessionModelOverflowAction::Switch,
        })
        .await
        .expect("切换模型");
    h.prompt(&id, "不支持的模型").await;
    assert!(h.load(&id).await.session.fast_mode);
    assert_eq!(last_service_tier(&upstream).await, None);
    h.close().await;
}

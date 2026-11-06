//! HTTP 字节计数：complete 与 stream 两条读取路径都应把 body 计入
//! [`HTTP_RX_BYTES`] / [`HTTP_TX_BYTES`]。同一测试二进制内的用例并发执行，
//! 计数器是全进程共享的，因此断言只能给下界，不能要精确值。

use std::sync::atomic::Ordering;

use moc_llm::{
    Auth, ChatRequest, Client, HTTP_RX_BYTES, HTTP_TX_BYTES, LlmConfig, Message, Protocol,
    RetryPolicy,
};
use serde_json::json;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn start() -> MockServer {
    MockServer::builder().start().await
}

fn config(server: &MockServer) -> LlmConfig {
    LlmConfig {
        protocol: Protocol::OpenAiChat,
        base_url: format!("{}/v1", server.uri()),
        model: "test-model".into(),
        auth: Auth::Bearer("test-key".into()),
        retry: RetryPolicy {
            max_attempts: 1,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    }
}

#[tokio::test]
async fn complete_counts_request_and_response_bytes() {
    let server = start().await;
    let body = json!({
        "choices": [{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}]
    });
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body.to_string(), "application/json"))
        .mount(&server)
        .await;
    let (rx0, tx0) = (
        HTTP_RX_BYTES.load(Ordering::Relaxed),
        HTTP_TX_BYTES.load(Ordering::Relaxed),
    );
    Client::new()
        .complete(&config(&server), &request())
        .await
        .unwrap();
    let rx = HTTP_RX_BYTES.load(Ordering::Relaxed) - rx0;
    let tx = HTTP_TX_BYTES.load(Ordering::Relaxed) - tx0;
    assert!(rx >= body.to_string().len() as u64, "rx delta {rx}");
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(tx >= sent.body.len() as u64, "tx delta {tx}");
}

#[tokio::test]
async fn stream_counts_response_bytes() {
    let server = start().await;
    let body = concat!(
        "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"index\":0}]}\n\n",
        "data: [DONE]\n\n"
    );
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(&server)
        .await;
    let (rx0, tx0) = (
        HTTP_RX_BYTES.load(Ordering::Relaxed),
        HTTP_TX_BYTES.load(Ordering::Relaxed),
    );
    let mut stream = Client::new()
        .stream(&config(&server), &request())
        .await
        .unwrap();
    while stream.next_event().await.is_some() {}
    let rx = HTTP_RX_BYTES.load(Ordering::Relaxed) - rx0;
    let tx = HTTP_TX_BYTES.load(Ordering::Relaxed) - tx0;
    assert!(rx >= body.len() as u64, "rx delta {rx}");
    let sent = &server.received_requests().await.unwrap()[0];
    assert!(tx >= sent.body.len() as u64, "tx delta {tx}");
}

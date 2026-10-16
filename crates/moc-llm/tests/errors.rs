//! 错误分类与重试行为：状态码 → [`moc_llm::LlmError`]、重试与不重试、流中错误。

use moc_llm::{
    ApiErrorKind, ChatRequest, Client, LlmConfig, Message, Protocol, RetryPolicy, TransportKind,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 启动 mock 服务器。
async fn start() -> MockServer {
    MockServer::start().await
}

/// 指向 mock 服务器的配置。
fn config(protocol: Protocol, server: &MockServer, retry: RetryPolicy) -> LlmConfig {
    LlmConfig {
        protocol,
        base_url: format!("{}/v1", server.uri()),
        model: "test-model".into(),
        retry,
        ..Default::default()
    }
}

/// 只重试一次且不等待的策略：测试里退避时长必须可忽略。
fn fast_retry(max_attempts: u32) -> RetryPolicy {
    RetryPolicy {
        max_attempts,
        initial_backoff_secs: 0.001,
        max_backoff_secs: 0.01,
        jitter: false,
    }
}

fn ok_body() -> Value {
    json!({"choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]})
}

/// 挂载固定 JSON 响应；`times` 为 `None` 表示不限次数。
async fn mount(
    server: &MockServer,
    route: &str,
    status: u16,
    body: Value,
    headers: &[(&str, &str)],
    times: Option<u64>,
    priority: u8,
) {
    let mut template =
        ResponseTemplate::new(status).set_body_raw(body.to_string(), "application/json");
    for (name, value) in headers {
        template = template.insert_header(*name, *value);
    }
    let mut mock = Mock::given(method("POST"))
        .and(path(route))
        .respond_with(template)
        .with_priority(priority);
    if let Some(times) = times {
        mock = mock.up_to_n_times(times);
    }
    mock.mount(server).await;
}

/// 挂载固定 SSE 响应。
async fn mount_sse(server: &MockServer, route: &str, frames: &[&str]) {
    let body = frames.concat();
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(server)
        .await;
}

async fn request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("mock 服务器在测试期间存活")
        .len()
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    }
}

#[tokio::test]
async fn no_retry_on_400() {
    let server = start().await;
    mount(
        &server,
        "/v1/chat/completions",
        400,
        json!({"error": {"message": "bad temperature", "type": "invalid_request_error", "code": null}}),
        &[],
        None,
        1,
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(3));
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    let api = error.api_error().expect("应携带上游错误");
    assert_eq!(api.kind, ApiErrorKind::InvalidRequest);
    assert_eq!(api.status, Some(400));
    assert_eq!(api.message, "bad temperature");
    assert!(!error.is_retryable());
    assert_eq!(request_count(&server).await, 1, "400 不重试");
}

#[tokio::test]
async fn retry_on_429_then_success() {
    let server = start().await;
    mount(
        &server,
        "/v1/chat/completions",
        429,
        json!({"error": {"message": "slow down", "type": "rate_limit_error"}}),
        &[("retry-after", "0")],
        Some(1),
        1,
    )
    .await;
    mount(
        &server,
        "/v1/chat/completions",
        200,
        ok_body(),
        &[],
        None,
        2,
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(3));
    let response = Client::new().complete(&cfg, &request()).await.unwrap();
    assert_eq!(response.message.text(), "ok");
    assert_eq!(request_count(&server).await, 2, "429 后重试一次即成功");
}

#[tokio::test]
async fn retries_exhausted_reports_attempts() {
    let server = start().await;
    mount(
        &server,
        "/v1/chat/completions",
        500,
        json!({"error": {"message": "boom", "type": "server_error"}}),
        &[],
        None,
        1,
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(3));
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    match error {
        moc_llm::LlmError::RetriesExhausted { attempts, last } => {
            assert_eq!(attempts, 3);
            assert_eq!(last.status(), Some(500));
            assert_eq!(
                last.api_error().map(|api| api.kind),
                Some(ApiErrorKind::ServerError)
            );
        }
        other => panic!("期望 RetriesExhausted，实际 {other:?}"),
    }
    assert_eq!(request_count(&server).await, 3);
}

#[tokio::test]
async fn quota_exceeded_is_not_retried() {
    let server = start().await;
    mount(
        &server,
        "/v1/chat/completions",
        429,
        json!({"error": {"message": "You exceeded your current quota", "type": "insufficient_quota", "code": "insufficient_quota"}}),
        &[],
        None,
        1,
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(3));
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    assert_eq!(
        error.api_error().map(|api| api.kind),
        Some(ApiErrorKind::QuotaExceeded)
    );
    assert_eq!(
        request_count(&server).await,
        1,
        "额度耗尽等待无意义，不重试"
    );
}

#[tokio::test]
async fn context_length_is_detected_from_body() {
    let server = start().await;
    mount(
        &server,
        "/v1/chat/completions",
        400,
        json!({
            "error": {
                "message": "This model's maximum context length is 65536 tokens",
                "type": "invalid_request_error",
                "code": "context_length_exceeded",
            }
        }),
        &[],
        None,
        1,
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(1));
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    assert_eq!(
        error.api_error().map(|api| api.kind),
        Some(ApiErrorKind::ContextLength)
    );
}

#[tokio::test]
async fn error_body_and_metadata_are_preserved() {
    let server = start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(
            ResponseTemplate::new(502)
                .insert_header("x-request-id", "req_42")
                .insert_header("retry-after", "3")
                .set_body_raw("<html>bad gateway</html>", "text/html"),
        )
        .mount(&server)
        .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(1));
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    let api = error.api_error().expect("应携带上游错误");
    assert_eq!(api.kind, ApiErrorKind::ServerError);
    assert_eq!(
        api.message, "<html>bad gateway</html>",
        "解析失败时降级为原始体"
    );
    assert_eq!(api.raw_body.as_ref(), "<html>bad gateway</html>");
    assert_eq!(api.request_id.as_deref(), Some("req_42"));
    assert_eq!(api.retry_after, Some(std::time::Duration::from_secs(3)));
    assert!(error.is_retryable());
}

#[tokio::test]
async fn error_body_is_truncated_at_local_limit() {
    let server = start().await;
    // 上限 1 MiB，上游给两倍：错误体必须被截断，而不是整份读进内存。
    let payload = "<html>".to_string() + &"x".repeat(2 * 1024 * 1024);
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(502).set_body_raw(payload, "text/html"))
        .mount(&server)
        .await;

    let cfg = config(Protocol::OpenAiChat, &server, fast_retry(1));
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    let api = error.api_error().expect("应携带上游错误");
    assert_eq!(api.status, Some(502));
    assert_eq!(api.kind, ApiErrorKind::ServerError);
    assert_eq!(api.raw_body.len(), 1024 * 1024);
    assert!(
        api.raw_body.starts_with("<html>x"),
        "{}",
        &api.raw_body[..16]
    );
    assert!(api.message.starts_with("<html>x"), "{}", api.message);
}

#[tokio::test]
async fn mid_stream_error_is_not_retried() {
    let server = start().await;
    mount_sse(
        &server,
        "/v1/messages",
        &[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"test\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"写到一半\"}}\n\n",
            "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"overloaded_error\",\"message\":\"Overloaded\"}}\n\n",
        ],
    )
    .await;

    let cfg = config(Protocol::AnthropicMessages, &server, fast_retry(3));
    let req = ChatRequest {
        max_output_tokens: Some(256),
        ..request()
    };
    let mut stream = Client::new().stream(&cfg, &req).await.unwrap();
    let mut text = String::new();
    while let Some(event) = stream.next_event().await {
        if let Ok(moc_llm::StreamEvent::TextDelta { text: delta, .. }) = event {
            text.push_str(&delta);
        }
    }
    let error = stream.finish().await.unwrap_err();
    assert_eq!(text, "写到一半", "错误之前的增量已经交付");
    match error {
        moc_llm::LlmError::Api(api) => {
            assert_eq!(api.kind, ApiErrorKind::Overloaded);
            assert_eq!(api.status, None);
            assert!(api.is_retryable(), "过载本身可重试，但流中不重放");
        }
        other => panic!("期望 Api 错误，实际 {other:?}"),
    }
    assert_eq!(request_count(&server).await, 1, "流中途错误不重放");
}

#[tokio::test]
async fn transport_error_is_classified_and_retryable() {
    // 1 端口必然无人监听：连接被拒绝是最典型的可重试传输错误。
    // 主机名必须用 localhost 而不是 127.0.0.1：环境代理会吞掉对裸 IP 的连接
    // 拒绝并回 502（no_proxy 的 `127.*` 通配语法 reqwest 不识别），而
    // `localhost` 条目能被命中、直接旁路代理。
    let cfg = LlmConfig {
        protocol: Protocol::OpenAiChat,
        base_url: "http://localhost:1/v1".into(),
        model: "test-model".into(),
        retry: fast_retry(1),
        ..Default::default()
    };
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    match &error {
        moc_llm::LlmError::Transport(transport) => {
            assert_eq!(transport.kind(), TransportKind::Connect);
            assert!(transport.is_retryable());
        }
        other => panic!("期望 Transport 错误，实际 {other:?}"),
    }

    // 允许重试时会重放并最终以 RetriesExhausted 收尾。
    let cfg = LlmConfig {
        retry: fast_retry(2),
        ..cfg
    };
    let error = Client::new().complete(&cfg, &request()).await.unwrap_err();
    match error {
        moc_llm::LlmError::RetriesExhausted { attempts, last } => {
            assert_eq!(attempts, 2);
            assert!(matches!(*last, moc_llm::LlmError::Transport(_)), "{last:?}");
        }
        other => panic!("期望 RetriesExhausted，实际 {other:?}"),
    }
}

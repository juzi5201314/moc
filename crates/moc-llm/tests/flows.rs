//! 三协议的非流式与流式全流程，以及请求塑形（鉴权头、Azure 端点、extra_body、
//! capture_raw）。全部为固定输入 → 固定输出，不访问任何外部服务。

use moc_llm::{
    Auth, ChatRequest, Client, ContentPart, EndpointStyle, FinishReason, LlmConfig, LlmError,
    Message, Protocol, ReasoningBlock, RequestOptions, RetryPolicy, Role, StreamEvent, ToolCall,
    Usage,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// 启动 mock 服务器。
async fn start() -> MockServer {
    MockServer::builder().start().await
}

/// 指向 mock 服务器的配置；测试里默认不重试，重试行为由 errors.rs 专门验证。
fn config(protocol: Protocol, server: &MockServer) -> LlmConfig {
    LlmConfig {
        protocol,
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

/// 挂载固定 JSON 响应。
async fn mount_json(server: &MockServer, route: &str, status: u16, body: Value) {
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(
            ResponseTemplate::new(status).set_body_raw(body.to_string(), "application/json"),
        )
        .mount(server)
        .await;
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

/// 收到的全部请求。
async fn requests(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .expect("mock 服务器在测试期间存活")
}

/// 最后一次请求的 JSON 体。
async fn last_body(server: &MockServer) -> Value {
    let requests = requests(server).await;
    let last = requests.last().expect("至少收到一次请求");
    serde_json::from_slice(&last.body).expect("请求体是 JSON")
}

#[tokio::test]
async fn configured_transport_preserves_user_agent_when_reused_with_provider_timeout() {
    let server = start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .and(wiremock::matchers::header("user-agent", "moc-pool-contract"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "choices": [{"index":0,"message":{"role":"assistant","content":"传输选项保留"},"finish_reason":"stop"}]
        })))
        .expect(2)
        .mount(&server).await;
    let client = Client::builder()
        .user_agent("moc-pool-contract")
        .connect_timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap();
    let mut cfg = config(Protocol::OpenAiChat, &server);
    cfg.connect_timeout_secs = Some(20.0);
    let request = ChatRequest {
        messages: vec![Message::user_text("检查连接池")],
        ..Default::default()
    };
    assert_eq!(
        client
            .complete(&cfg, &request)
            .await
            .unwrap()
            .message
            .text(),
        "传输选项保留"
    );
    assert_eq!(
        client
            .clone()
            .complete(&cfg, &request)
            .await
            .unwrap()
            .message
            .text(),
        "传输选项保留"
    );
}

#[tokio::test]
async fn connected_nonstream_request_can_wait_beyond_connect_timeout() {
    use std::time::Duration;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (entered, connected) = tokio::sync::oneshot::channel();
    let (release, gate) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut reader = BufReader::new(socket);
        let mut length = 0;
        loop {
            let mut line = String::new();
            assert!(reader.read_line(&mut line).await.unwrap() > 0);
            if line == "\r\n" {
                break;
            }
            if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                length = value.trim().parse::<usize>().unwrap();
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).await.unwrap();
        entered.send(()).unwrap();
        gate.await.unwrap();
        let body = json!({"model":"test-model","choices":[{"index":0,"message":{"role":"assistant","content":"完整摘要"},"finish_reason":"stop"}]}).to_string();
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = reader.get_mut().write_all(response.as_bytes()).await;
    });
    let response = tokio::spawn(async move {
        Client::new()
            .complete(
                &LlmConfig {
                    base_url: format!("http://{address}/v1"),
                    model: "test-model".into(),
                    connect_timeout_secs: Some(20.0),
                    timeout_secs: Some(600.0),
                    retry: RetryPolicy {
                        max_attempts: 1,
                        ..Default::default()
                    },
                    ..Default::default()
                },
                &ChatRequest {
                    messages: vec![Message::user_text("压缩历史")],
                    ..Default::default()
                },
            )
            .await
    });
    connected.await.unwrap();
    // 已收到完整请求后才推进虚拟时钟；不依赖 TCP 调度或真实 sleep。
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(30)).await;
    tokio::task::yield_now().await;
    tokio::time::resume();
    release.send(()).unwrap();
    assert_eq!(response.await.unwrap().unwrap().message.text(), "完整摘要");
    server.await.unwrap();
}

#[tokio::test]
async fn complete_chat_nonstream() {
    let server = start().await;
    mount_json(
        &server,
        "/v1/chat/completions",
        200,
        json!({
            "id": "chatcmpl-1",
            "model": "test-model",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "你好", "reasoning_content": "想一想"},
                "finish_reason": "stop",
            }],
            "usage": {"prompt_tokens": 7, "completion_tokens": 3, "total_tokens": 10},
        }),
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server);
    let req = ChatRequest {
        messages: vec![Message::system("sys"), Message::user_text("hi")],
        max_output_tokens: Some(64),
        ..Default::default()
    };
    let response = Client::new().complete(&cfg, &req).await.unwrap();

    assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(response.model.as_deref(), Some("test-model"));
    assert_eq!(response.message.text(), "你好");
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 7,
            output_tokens: 3,
            total_tokens: Some(10),
            ..Default::default()
        }
    );
    assert_eq!(
        response.message.content[0],
        ContentPart::Reasoning(ReasoningBlock::text("想一想"))
    );
    assert!(response.raw.is_none(), "默认不保留原始响应体");

    let body = last_body(&server).await;
    assert_eq!(body["model"], json!("test-model"));
    assert_eq!(body["messages"].as_array().unwrap().len(), 2);
    assert_eq!(body["max_tokens"], json!(64));
    assert!(body.get("stream").is_none());
    assert_eq!(requests(&server).await.len(), 1);
}

#[tokio::test]
async fn stream_chat() {
    let server = start().await;
    mount_sse(
        &server,
        "/v1/chat/completions",
        &[
            "data: {\"id\":\"chatcmpl-9\",\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2,\"total_tokens\":7}}\n\n",
            "data: [DONE]\n\n",
        ],
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server);
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let mut stream = Client::new().stream(&cfg, &req).await.unwrap();

    let mut text = String::new();
    let mut done = None;
    while let Some(event) = stream.next_event().await {
        match event.unwrap() {
            StreamEvent::TextDelta { text: delta, .. } => text.push_str(&delta),
            StreamEvent::Done { finish_reason } => done = Some(finish_reason),
            _ => {}
        }
    }
    assert_eq!(text, "你好");
    assert_eq!(done, Some(FinishReason::Stop));

    let response = stream.finish().await.unwrap();
    assert_eq!(response.message.text(), "你好");
    assert_eq!(response.id.as_deref(), Some("chatcmpl-9"));
    assert_eq!(response.usage.total_tokens(), 7);

    let body = last_body(&server).await;
    assert_eq!(body["stream"], json!(true));
    assert_eq!(body["stream_options"], json!({"include_usage": true}));
}

#[tokio::test]
async fn complete_anthropic_nonstream() {
    let server = start().await;
    mount_json(
        &server,
        "/v1/messages",
        200,
        json!({
            "id": "msg_1",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "thinking", "thinking": "先算", "signature": "sig-1"},
                {"type": "text", "text": "答案是 42"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "北京"}},
            ],
            "stop_reason": "tool_use",
            "usage": {"input_tokens": 20, "output_tokens": 8, "cache_read_input_tokens": 16},
        }),
    )
    .await;

    let cfg = LlmConfig {
        auth: Auth::Header {
            name: "x-api-key".into(),
            value: "sk-ant-test".into(),
        },
        ..config(Protocol::AnthropicMessages, &server)
    };
    let req = ChatRequest {
        messages: vec![Message::system("sys"), Message::user_text("hi")],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let response = Client::new().complete(&cfg, &req).await.unwrap();

    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.usage.input_tokens, 36);
    assert_eq!(response.usage.cached_input_tokens, Some(16));
    assert_eq!(response.usage.total_tokens, None);
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Reasoning(ReasoningBlock::Text {
                text: "先算".into(),
                signature: Some("sig-1".into()),
            }),
            ContentPart::Text("答案是 42".into()),
            ContentPart::ToolCall(ToolCall::new(
                "toolu_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );

    let request = &requests(&server).await[0];
    let headers: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(name, value)| (name.as_str().to_owned(), value.to_str().unwrap().to_owned()))
        .collect();
    assert!(
        headers.contains(&("x-api-key".to_string(), "sk-ant-test".to_string())),
        "{headers:?}"
    );
    assert!(
        headers.contains(&("anthropic-version".to_string(), "2023-06-01".to_string())),
        "{headers:?}"
    );

    let body = last_body(&server).await;
    assert_eq!(body["max_tokens"], json!(1024));
    assert_eq!(body["system"], json!("sys"));
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn stream_anthropic() {
    let server = start().await;
    mount_sse(
        &server,
        "/v1/messages",
        &[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"claude-sonnet-4-5\",\"usage\":{\"input_tokens\":9,\"output_tokens\":1}}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"北京\\\"}\"}}\n\n",
            "event: message_delta\ndata: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"tool_use\"},\"usage\":{\"output_tokens\":12}}\n\n",
            "event: message_stop\ndata: {\"type\":\"message_stop\"}\n\n",
        ],
    )
    .await;

    let cfg = LlmConfig {
        auth: Auth::Header {
            name: "x-api-key".into(),
            value: "sk-ant-test".into(),
        },
        ..config(Protocol::AnthropicMessages, &server)
    };
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        max_output_tokens: Some(512),
        ..Default::default()
    };
    let mut stream = Client::new().stream(&cfg, &req).await.unwrap();
    while stream.next_event().await.is_some() {}

    let response = stream.finish().await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.message.content,
        vec![ContentPart::ToolCall(ToolCall::new(
            "toolu_1",
            "get_weather",
            "{\"city\":\"北京\"}"
        ))]
    );
    assert_eq!(response.usage.input_tokens, 9);
    assert_eq!(response.usage.output_tokens, 12);

    assert_eq!(last_body(&server).await["stream"], json!(true));
}

#[tokio::test]
async fn complete_responses_nonstream() {
    let server = start().await;
    mount_json(
        &server,
        "/v1/responses",
        200,
        json!({
            "id": "resp_1",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "blob", "summary": []},
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "答案是 42"}]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "{}"},
            ],
            "usage": {"input_tokens": 30, "output_tokens": 5, "total_tokens": 35},
        }),
    )
    .await;

    let cfg = config(Protocol::OpenAiResponses, &server);
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        max_output_tokens: Some(256),
        ..Default::default()
    };
    let response = Client::new().complete(&cfg, &req).await.unwrap();

    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.message.content.len(), 3);
    assert!(matches!(
        response.message.content[0],
        ContentPart::Reasoning(ReasoningBlock::Item(_))
    ));
    assert_eq!(
        response.message.content[1],
        ContentPart::Text("答案是 42".into())
    );

    let body = last_body(&server).await;
    assert_eq!(body["max_output_tokens"], json!(256));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    assert!(body.get("stream").is_none());
}

#[tokio::test]
async fn stream_responses() {
    let server = start().await;
    mount_sse(
        &server,
        "/v1/responses",
        &[
            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_1\",\"model\":\"gpt-5\"}}\n\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\"}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"想一想\"}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"blob\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"想一想\"}]}}\n\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":2,\"item\":{\"type\":\"compaction\",\"id\":\"cmp_1\",\"encrypted_content\":\"opaque\"}}\n\n",
            "data: {\"type\":\"response.output_text.delta\",\"output_index\":1,\"delta\":\"好\"}\n\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[{\"type\":\"message\"},{\"type\":\"compaction\",\"id\":\"cmp_1\",\"encrypted_content\":\"opaque\"}],\"usage\":{\"input_tokens\":4,\"output_tokens\":2,\"total_tokens\":6}}}\n\n",
        ],
    )
    .await;

    let cfg = config(Protocol::OpenAiResponses, &server);
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let mut stream = Client::new().stream(&cfg, &req).await.unwrap();

    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await {
        events.push(event.unwrap());
    }
    assert!(
        matches!(events[0], StreamEvent::Start { .. }),
        "{:?}",
        events[0]
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::ReasoningItem { .. }))
            .count(),
        1
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::CompactionItem { .. }))
            .count(),
        1
    );

    let response = stream.finish().await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.message.text(), "好");
    assert!(matches!(
        response.message.content[0],
        ContentPart::Reasoning(ReasoningBlock::Item(_))
    ));
    assert_eq!(
        response.compaction.as_deref(),
        Some(&json!({ "type": "compaction", "id": "cmp_1", "encrypted_content": "opaque" }))
    );
    assert_eq!(response.usage.total_tokens(), 6);
}

#[tokio::test]
async fn capture_raw_response() {
    let server = start().await;
    let payload = json!({
        "id": "chatcmpl-raw",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
        "vendor_extension": {"unknown": [1, 2, 3]},
    });
    mount_json(&server, "/v1/chat/completions", 200, payload.clone()).await;

    let cfg = config(Protocol::OpenAiChat, &server);
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: RequestOptions {
            capture_raw: true,
            ..Default::default()
        },
        ..Default::default()
    };
    let response = Client::new().complete(&cfg, &req).await.unwrap();
    let raw = response.raw.expect("capture_raw 应保留原始响应体");
    assert_eq!(*raw, payload);
    assert_eq!(raw["vendor_extension"]["unknown"], json!([1, 2, 3]));
}

#[tokio::test]
async fn extra_body_merged_with_standard_fields_winning() {
    let server = start().await;
    mount_json(
        &server,
        "/v1/chat/completions",
        200,
        json!({"choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]}),
    )
    .await;

    let cfg = LlmConfig {
        extra_body: serde_json::Map::from_iter([
            ("thinking_budget".into(), json!(16384)),
            ("temperature".into(), json!(9.9)),
        ]),
        ..config(Protocol::OpenAiChat, &server)
    };
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        temperature: Some(0.2),
        ..Default::default()
    };
    Client::new().complete(&cfg, &req).await.unwrap();

    let body = last_body(&server).await;
    assert_eq!(body["thinking_budget"], json!(16384));
    assert_eq!(
        body["temperature"],
        json!(0.2),
        "标准字段覆盖 extra_body 同名键"
    );
}

#[tokio::test]
async fn azure_endpoint_url_and_header() {
    let server = start().await;
    let route = "/openai/deployments/gpt-4o-deploy/chat/completions";
    Mock::given(method("POST"))
        .and(path(route))
        .and(query_param("api-version", "2025-04-01-preview"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({"choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]})
                .to_string(),
            "application/json",
        ))
        .mount(&server)
        .await;

    let cfg = LlmConfig {
        base_url: server.uri(),
        model: "gpt-4o-deploy".into(),
        auth: Auth::Header {
            name: "api-key".into(),
            value: "azure-key".into(),
        },
        endpoint: EndpointStyle::Azure {
            api_version: "2025-04-01-preview".into(),
        },
        retry: RetryPolicy {
            max_attempts: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let response = Client::new().complete(&cfg, &req).await.unwrap();
    assert_eq!(response.message.text(), "ok");

    let request = &requests(&server).await[0];
    assert_eq!(request.url.path(), route);
    let api_key = request
        .headers
        .get("api-key")
        .expect("Azure 用 api-key 头鉴权")
        .to_str()
        .unwrap();
    assert_eq!(api_key, "azure-key");
    assert!(request.headers.get("authorization").is_none());
}

#[tokio::test]
async fn request_options_headers_override_config_headers() {
    let server = start().await;
    mount_json(
        &server,
        "/v1/chat/completions",
        200,
        json!({"choices": [{"message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}]}),
    )
    .await;

    let cfg = LlmConfig {
        headers: std::collections::BTreeMap::from([("x-trace".into(), "from-config".into())]),
        ..config(Protocol::OpenAiChat, &server)
    };
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: RequestOptions {
            headers: std::collections::BTreeMap::from([("x-trace".into(), "from-request".into())]),
            ..Default::default()
        },
        ..Default::default()
    };
    Client::new().complete(&cfg, &req).await.unwrap();

    let request = &requests(&server).await[0];
    assert_eq!(
        request.headers.get("x-trace").unwrap().to_str().unwrap(),
        "from-request"
    );
}

#[tokio::test]
async fn role_variants_round_trip_through_the_wire() {
    // system / developer / user / assistant（含工具调用与思维链）/ tool 五类消息
    // 全部能编码，且响应能直接回灌历史。
    let server = start().await;
    mount_json(
        &server,
        "/v1/chat/completions",
        200,
        json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "查到了",
                    "reasoning_content": "调用工具",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "get_date", "arguments": "{}"}},
                    ],
                },
                "finish_reason": "tool_calls",
            }],
        }),
    )
    .await;

    let cfg = LlmConfig {
        wire: moc_llm::WireConfig {
            developer_role: true,
            ..Default::default()
        },
        ..config(Protocol::OpenAiChat, &server)
    };
    let req = ChatRequest {
        messages: vec![
            Message::system("系统"),
            Message::developer("开发者"),
            Message::user_text("查天气和日期"),
        ],
        tools: vec![moc_llm::ToolDef::new(
            "get_weather",
            json!({"type": "object"}),
        )],
        ..Default::default()
    };
    let response = Client::new().complete(&cfg, &req).await.unwrap();
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.message.tool_calls().count(), 2);

    // 把响应与两条工具结果组成下一轮历史：编码必须通过历史完整性校验。
    let next = ChatRequest {
        messages: vec![
            Message::system("系统"),
            Message::developer("开发者"),
            Message::user_text("查天气和日期"),
            response.message.clone(),
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_2", "2026-09-24", false),
        ],
        tools: vec![moc_llm::ToolDef::new(
            "get_weather",
            json!({"type": "object"}),
        )],
        ..Default::default()
    };
    let body = {
        mount_json(
            &server,
            "/v1/chat/completions",
            200,
            json!({"choices": [{"message": {"role": "assistant", "content": "完成"}, "finish_reason": "stop"}]}),
        )
        .await;
        Client::new().complete(&cfg, &next).await.unwrap();
        last_body(&server).await
    };
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[1]["role"], json!("developer"));
    assert_eq!(messages[3]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[3]["reasoning_content"], json!("调用工具"));
    assert_eq!(messages[4]["role"], json!("tool"));
    assert_eq!(messages[5]["role"], json!("tool"));
    assert_eq!(messages[3]["role"], json!("assistant"));
    assert_eq!(
        Role::Assistant.as_str(),
        messages[3]["role"].as_str().unwrap()
    );
}

#[tokio::test]
async fn responses_compact_uses_endpoint_and_preserves_items() {
    let server = start().await;
    let output = vec![
        json!({ "type": "compaction", "id": "cmp_1" }),
        json!({ "type": "message" }),
    ];
    mount_json(
        &server,
        "/v1/responses/compact",
        200,
        json!({ "output": output, "usage": { "input_tokens": 40, "output_tokens": 2 } }),
    )
    .await;
    let cfg = LlmConfig {
        extra_body: serde_json::Map::from_iter([("metadata".into(), json!({ "source": "test" }))]),
        ..config(Protocol::OpenAiResponses, &server)
    };
    let input = vec![json!({ "type": "message", "role": "user" })];

    let response = Client::new()
        .compact(&cfg, &input, 30)
        .await
        .expect("压缩成功");

    assert_eq!(response.items, output);
    assert_eq!(response.usage.input_tokens, 40);
    assert_eq!(response.raw.as_deref().unwrap()["output"], json!(output));
    let requests = requests(&server).await;
    let sent: Value = serde_json::from_slice(&requests[0].body).expect("请求体是 JSON");
    assert_eq!(sent["model"], "test-model");
    assert_eq!(sent["input"], json!(input));
    assert_eq!(sent["metadata"], json!({ "source": "test" }));
}

#[tokio::test]
async fn responses_compact_requires_output_and_reports_body() {
    let server = start().await;
    mount_json(
        &server,
        "/v1/responses/compact",
        200,
        json!({ "status": "completed" }),
    )
    .await;
    let cfg = config(Protocol::OpenAiResponses, &server);
    let error = Client::new()
        .compact(&cfg, &[], 30)
        .await
        .expect_err("缺少 output 不应被接受");
    match error {
        LlmError::Decode { message, body } => {
            assert!(message.contains("output"));
            assert!(
                body.as_deref()
                    .is_some_and(|body| body.contains("completed"))
            );
        }
        other => panic!("期望 Decode，实际 {other:?}"),
    }
}

//! 跨厂商方言矩阵与两条端到端往返。
//!
//! 本 crate 不含任何以厂商身份为键的内置知识；这里用「core 侧风格构造的配置体 +
//! 请求」断言线上形态，证明同一套机制能表达真实厂商组合。

use moc_llm::{
    Auth, CacheConfig, CacheTtl, ChatRequest, Client, ContentPart, Effort, FinishReason, LlmConfig,
    MaxTokensField, Message, Protocol, ReasoningBlock, ReasoningConfig, ReasoningEcho,
    RequestOptions, RetryPolicy, StreamEvent, ToolCall, ToolDef, WireConfig,
};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

async fn start() -> MockServer {
    MockServer::start().await
}

fn path_of(protocol: Protocol) -> &'static str {
    match protocol {
        Protocol::OpenAiChat => "/v1/chat/completions",
        Protocol::OpenAiResponses => "/v1/responses",
        Protocol::AnthropicMessages => "/v1/messages",
    }
}

/// 各协议的最小成功响应。
fn ok_response(protocol: Protocol) -> Value {
    match protocol {
        Protocol::OpenAiChat => json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": "ok"}, "finish_reason": "stop"}],
        }),
        Protocol::OpenAiResponses => json!({
            "status": "completed",
            "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}],
        }),
        Protocol::AnthropicMessages => json!({
            "content": [{"type": "text", "text": "ok"}],
            "stop_reason": "end_prompt",
        }),
    }
}

fn config(protocol: Protocol, server: &MockServer) -> LlmConfig {
    LlmConfig {
        protocol,
        base_url: format!("{}/v1", server.uri()),
        model: "test-model".into(),
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

/// 挂载只生效一次的 SSE 响应：往返用例先收流式响应，随后同一路径要返回非流式响应。
async fn mount_sse_once(server: &MockServer, route: &str, frames: &[&str]) {
    let body = frames.concat();
    Mock::given(method("POST"))
        .and(path(route))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(server)
        .await;
}

async fn received(server: &MockServer) -> Vec<wiremock::Request> {
    server
        .received_requests()
        .await
        .expect("mock 服务器在测试期间存活")
}

/// 发一次请求并返回实际发出的请求体。
async fn sent_body(server: &MockServer, cfg: &LlmConfig, req: &ChatRequest) -> Value {
    mount_json(
        server,
        path_of(cfg.protocol),
        200,
        ok_response(cfg.protocol),
    )
    .await;
    Client::new().complete(cfg, req).await.unwrap();
    let requests = received(server).await;
    let last = requests.last().expect("至少收到一次请求");
    serde_json::from_slice(&last.body).expect("请求体是 JSON")
}

fn assistant_with_tools(calls: &[(&str, &str)]) -> Message {
    Message {
        role: moc_llm::Role::Assistant,
        content: calls
            .iter()
            .map(|(id, name)| ContentPart::ToolCall(ToolCall::new(*id, *name, "{}")))
            .collect(),
    }
}

fn weather_tool() -> ToolDef {
    ToolDef::new(
        "get_weather",
        json!({"type": "object", "properties": {"city": {"type": "string"}}}),
    )
}

#[tokio::test]
async fn deepseek_thinking_mode_with_tools() {
    let server = start().await;
    let cfg = LlmConfig {
        protocol: Protocol::OpenAiChat,
        base_url: format!("{}/v1", server.uri()),
        model: "deepseek-reasoner".into(),
        auth: Auth::Bearer("sk-deepseek".into()),
        wire: WireConfig {
            max_output_tokens_field: MaxTokensField::MaxTokens,
            reasoning_field: "reasoning_content".into(),
            ..Default::default()
        },
        retry: RetryPolicy {
            max_attempts: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let req = ChatRequest {
        messages: vec![
            Message::user_text("查天气"),
            Message {
                role: moc_llm::Role::Assistant,
                content: vec![
                    ContentPart::Reasoning(ReasoningBlock::text("上一轮思路")),
                    ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                ],
            },
            Message::tool_result("call_1", "晴", false),
        ],
        tools: vec![weather_tool()],
        max_output_tokens: Some(4096),
        reasoning: Some(ReasoningConfig {
            enabled: true,
            effort: Some(Effort::High),
            ..Default::default()
        }),
        ..Default::default()
    };
    let body = sent_body(&server, &cfg, &req).await;

    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    assert_eq!(body["reasoning_effort"], json!("high"));
    assert_eq!(body["max_tokens"], json!(4096));
    assert_eq!(
        body["messages"][1]["reasoning_content"],
        json!("上一轮思路"),
        "携带 tools 时思维链必须完整回传"
    );
}

#[tokio::test]
async fn claude_adaptive_thinking_with_cache_control() {
    let server = start().await;
    let cfg = LlmConfig {
        auth: Auth::Header {
            name: "x-api-key".into(),
            value: "sk-ant".into(),
        },
        ..config(Protocol::AnthropicMessages, &server)
    };
    let req = ChatRequest {
        messages: vec![
            Message::system("系统提示"),
            Message::user_text("hi"),
            assistant_with_tools(&[("tool_1", "get_weather")]),
            Message::tool_result("tool_1", "晴", false),
        ],
        tools: vec![weather_tool()],
        max_output_tokens: Some(8192),
        reasoning: Some(ReasoningConfig {
            enabled: true,
            effort: Some(Effort::High),
            ..Default::default()
        }),
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![3],
                ttl: Some(CacheTtl::FiveMinutes),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let body = sent_body(&server, &cfg, &req).await;

    assert_eq!(body["thinking"], json!({"type": "adaptive"}));
    assert_eq!(body["output_config"], json!({"effort": "high"}));
    assert_eq!(body["system"], json!("系统提示"));
    assert_eq!(
        body["messages"][2]["content"][0],
        json!({"type": "tool_result", "tool_use_id": "tool_1", "content": "晴", "is_error": false,
               "cache_control": {"type": "ephemeral", "ttl": "5m"}})
    );
}

#[tokio::test]
async fn responses_stateless_continuation() {
    let server = start().await;
    let cfg = config(Protocol::OpenAiResponses, &server);
    let reasoning_item = json!({
        "type": "reasoning",
        "id": "rs_1",
        "encrypted_content": "blob",
        "summary": [],
    });
    let req = ChatRequest {
        messages: vec![
            Message::user_text("查天气"),
            Message {
                role: moc_llm::Role::Assistant,
                content: vec![
                    ContentPart::Reasoning(ReasoningBlock::Item(Box::new(reasoning_item.clone()))),
                    ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                ],
            },
            Message::tool_result("call_1", "晴", false),
        ],
        tools: vec![weather_tool()],
        options: RequestOptions {
            store: Some(false),
            ..Default::default()
        },
        ..Default::default()
    };
    let body = sent_body(&server, &cfg, &req).await;

    assert_eq!(body["store"], json!(false));
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 4);
    assert_eq!(input[1], reasoning_item, "reasoning item 原样回传");
    assert_eq!(input[2]["type"], json!("function_call"));
    assert_eq!(input[3]["type"], json!("function_call_output"));
    assert_eq!(input[3]["call_id"], json!("call_1"));
}

#[tokio::test]
async fn parallel_tool_roundtrip_chat() {
    let server = start().await;
    // 第一次响应：一条 assistant 消息带 2 个工具调用。
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({
                "choices": [{
                    "index": 0,
                    "message": {
                        "role": "assistant",
                        "content": null,
                        "reasoning_content": "并行调用",
                        "tool_calls": [
                            {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}},
                            {"id": "call_2", "type": "function", "function": {"name": "get_date", "arguments": "{}"}},
                        ],
                    },
                    "finish_reason": "tool_calls",
                }],
            })
            .to_string(),
            "application/json",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_json(
        &server,
        "/v1/chat/completions",
        200,
        ok_response(Protocol::OpenAiChat),
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server);
    let tools = vec![weather_tool()];
    let first = Client::new()
        .complete(
            &cfg,
            &ChatRequest {
                messages: vec![Message::user_text("并行查")],
                tools: tools.clone(),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.finish_reason, FinishReason::ToolCalls);
    assert_eq!(first.message.tool_calls().count(), 2);

    // 响应直接回灌历史，工具结果按调用顺序各一条消息。
    let second = ChatRequest {
        messages: vec![
            Message::user_text("并行查"),
            first.message.clone(),
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_2", "2026-09-24", false),
        ],
        tools,
        ..Default::default()
    };
    Client::new().complete(&cfg, &second).await.unwrap();

    let requests = received(&server).await;
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 2);
    assert_eq!(messages[1]["tool_calls"][0]["id"], json!("call_1"));
    assert_eq!(messages[1]["tool_calls"][1]["id"], json!("call_2"));
    assert_eq!(
        messages[1]["reasoning_content"],
        json!("并行调用"),
        "携带 tools 时思维链随响应一起回灌"
    );
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[2]["tool_call_id"], json!("call_1"));
    assert_eq!(messages[3]["role"], json!("tool"));
    assert_eq!(messages[3]["tool_call_id"], json!("call_2"));
}

#[tokio::test]
async fn parallel_tool_roundtrip_anthropic() {
    let server = start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            json!({
                "id": "msg_1",
                "content": [
                    {"type": "thinking", "thinking": "并行调用", "signature": "sig-1"},
                    {"type": "text", "text": "这就查"},
                    {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "北京"}},
                    {"type": "tool_use", "id": "toolu_2", "name": "get_date", "input": {}},
                ],
                "stop_reason": "tool_use",
            })
            .to_string(),
            "application/json",
        ))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    mount_json(
        &server,
        "/v1/messages",
        200,
        ok_response(Protocol::AnthropicMessages),
    )
    .await;

    let cfg = LlmConfig {
        auth: Auth::Header {
            name: "x-api-key".into(),
            value: "sk-ant".into(),
        },
        ..config(Protocol::AnthropicMessages, &server)
    };
    let tools = vec![weather_tool()];
    let first = Client::new()
        .complete(
            &cfg,
            &ChatRequest {
                messages: vec![Message::user_text("并行查")],
                tools: tools.clone(),
                max_output_tokens: Some(4096),
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(first.message.tool_calls().count(), 2);

    let second = ChatRequest {
        messages: vec![
            Message::user_text("并行查"),
            first.message.clone(),
            Message::tool_result("toolu_1", "晴", false),
            Message::tool_result("toolu_2", "2026-09-24", false),
        ],
        tools,
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    Client::new().complete(&cfg, &second).await.unwrap();

    let requests = received(&server).await;
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3, "工具结果合并进一条 user 消息");
    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(messages[1]["content"][0]["type"], json!("thinking"));
    assert_eq!(messages[1]["content"][1]["type"], json!("text"));
    assert_eq!(messages[1]["content"][2]["type"], json!("tool_use"));
    assert_eq!(messages[1]["content"][3]["type"], json!("tool_use"));
    assert_eq!(messages[2]["role"], json!("user"));
    assert_eq!(messages[2]["content"].as_array().unwrap().len(), 2);
    assert_eq!(messages[2]["content"][0]["type"], json!("tool_result"));
    assert_eq!(messages[2]["content"][0]["tool_use_id"], json!("toolu_1"));
    assert_eq!(messages[2]["content"][1]["tool_use_id"], json!("toolu_2"));
}

#[tokio::test]
async fn steering_interrupt_roundtrip_chat() {
    let server = start().await;
    mount_sse_once(
        &server,
        "/v1/chat/completions",
        &[
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"test-model\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"正在查\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"get_weather\",\"arguments\":\"{\\\"city\\\":\\\"北京\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"call_2\",\"function\":{\"name\":\"get_date\",\"arguments\":\"{\\\"day\\\":\"}}]},\"finish_reason\":null}]}\n\n",
        ],
    )
    .await;

    let cfg = config(Protocol::OpenAiChat, &server);
    let req = ChatRequest {
        messages: vec![Message::user_text("查天气")],
        tools: vec![weather_tool()],
        ..Default::default()
    };
    let mut stream = Client::new().stream(&cfg, &req).await.unwrap();

    // 读到第二个调用的参数增量出现（说明它是半截的）就打断。
    loop {
        if let StreamEvent::ToolCallArgumentsDelta { index: 1, .. } = stream
            .next_event()
            .await
            .expect("流应持续产出事件")
            .unwrap()
        {
            break;
        }
    }
    let partial = stream.interrupt();
    assert_eq!(partial.finish_reason, FinishReason::Interrupted);
    assert_eq!(partial.message.text(), "正在查");
    assert_eq!(
        partial.message.tool_calls().count(),
        1,
        "半截的工具调用被丢弃"
    );

    // core 为保留的调用补结果、追加用户插话，然后重新发起请求。
    let next = ChatRequest {
        messages: vec![
            Message::user_text("查天气"),
            partial.message.clone(),
            Message::tool_result("call_1", "晴", false),
            Message::user_text("改用另一种方案"),
        ],
        tools: vec![weather_tool()],
        ..Default::default()
    };
    mount_json(
        &server,
        "/v1/chat/completions",
        200,
        ok_response(Protocol::OpenAiChat),
    )
    .await;
    Client::new().complete(&cfg, &next).await.unwrap();

    let requests = received(&server).await;
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 4);
    assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 1);
    assert_eq!(messages[1]["tool_calls"][0]["id"], json!("call_1"));
    assert_eq!(messages[2]["role"], json!("tool"));
    assert_eq!(messages[3]["role"], json!("user"));
    assert_eq!(messages[3]["content"], json!("改用另一种方案"));
}

#[tokio::test]
async fn steering_interrupt_roundtrip_anthropic() {
    let server = start().await;
    mount_sse_once(
        &server,
        "/v1/messages",
        &[
            "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"test\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"半截思考\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"正在查\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"get_weather\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"city\\\":\\\"北京\\\"}\"}}\n\n",
            "event: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":3,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_2\",\"name\":\"get_date\"}}\n\n",
            "event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":3,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"day\\\":\"}}\n\n",
        ],
    )
    .await;

    let cfg = LlmConfig {
        auth: Auth::Header {
            name: "x-api-key".into(),
            value: "sk-ant".into(),
        },
        ..config(Protocol::AnthropicMessages, &server)
    };
    let req = ChatRequest {
        messages: vec![Message::user_text("查天气")],
        tools: vec![weather_tool()],
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    let mut stream = Client::new().stream(&cfg, &req).await.unwrap();
    loop {
        if let StreamEvent::ToolCallArgumentsDelta { index: 3, .. } = stream
            .next_event()
            .await
            .expect("流应持续产出事件")
            .unwrap()
        {
            break;
        }
    }
    let partial = stream.interrupt();

    // 无签名的 thinking 块与半截工具调用都被修剪掉。
    assert_eq!(
        partial.message.content,
        vec![
            ContentPart::Text("正在查".into()),
            ContentPart::ToolCall(ToolCall::new(
                "toolu_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );

    let next = ChatRequest {
        messages: vec![
            Message::user_text("查天气"),
            partial.message.clone(),
            Message::tool_result("toolu_1", "晴", false),
            Message::user_text("改用另一种方案"),
        ],
        tools: vec![weather_tool()],
        max_output_tokens: Some(4096),
        ..Default::default()
    };
    mount_json(
        &server,
        "/v1/messages",
        200,
        ok_response(Protocol::AnthropicMessages),
    )
    .await;
    Client::new().complete(&cfg, &next).await.unwrap();

    let requests = received(&server).await;
    let body: Value = serde_json::from_slice(&requests[1].body).unwrap();
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(messages[1]["content"].as_array().unwrap().len(), 2);
    assert_eq!(messages[1]["content"][1]["type"], json!("tool_use"));
    assert_eq!(messages[2]["role"], json!("user"));
    assert_eq!(
        messages[2]["content"],
        json!([
            {"type": "tool_result", "tool_use_id": "toolu_1", "content": "晴", "is_error": false},
            {"type": "text", "text": "改用另一种方案"},
        ]),
        "工具结果与用户插话落在同一条 user 消息内"
    );
}

#[tokio::test]
async fn reasoning_echo_switch_applies_per_protocol() {
    let server = start().await;
    let history = vec![
        Message::user_text("问题"),
        Message {
            role: moc_llm::Role::Assistant,
            content: vec![
                ContentPart::Reasoning(ReasoningBlock::text("历史思维链")),
                ContentPart::Text("回答".into()),
            ],
        },
    ];

    // Chat + 无工具 + Protocol：思维链不回传。
    let cfg = config(Protocol::OpenAiChat, &server);
    let body = sent_body(
        &server,
        &cfg,
        &ChatRequest {
            messages: history.clone(),
            ..Default::default()
        },
    )
    .await;
    assert!(body["messages"][1].get("reasoning_content").is_none());

    // Always：无论有无工具都回传。
    let body = sent_body(
        &server,
        &cfg,
        &ChatRequest {
            messages: history.clone(),
            options: RequestOptions {
                echo_reasoning: ReasoningEcho::Always,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    assert_eq!(
        body["messages"][1]["reasoning_content"],
        json!("历史思维链")
    );

    // Never：一律不回传。
    let body = sent_body(
        &server,
        &cfg,
        &ChatRequest {
            messages: history.clone(),
            options: RequestOptions {
                echo_reasoning: ReasoningEcho::Never,
                ..Default::default()
            },
            ..Default::default()
        },
    )
    .await;
    assert!(body["messages"][1].get("reasoning_content").is_none());
}

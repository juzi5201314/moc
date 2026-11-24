use super::*;
use crate::config::WireConfig;
use crate::types::{RequestOptions, Usage};
use serde_json::json;

fn config() -> LlmConfig {
    LlmConfig {
        protocol: crate::config::Protocol::OpenAiChat,
        base_url: "https://api.deepseek.com/v1".into(),
        model: "test-model".into(),
        ..Default::default()
    }
}

fn encode_body(cfg: &LlmConfig, req: &ChatRequest, stream: bool) -> Value {
    let encoded = encode(cfg, req, stream).unwrap();
    assert_eq!(encoded.url, format!("{}/chat/completions", cfg.base_url));
    serde_json::from_slice(&encoded.body).unwrap()
}

fn sse(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.into(),
        id: None,
    }
}

fn feed(decoder: &mut ChatStreamDecoder, frames: &[&str]) -> Vec<Result<StreamEvent, LlmError>> {
    let mut out = Vec::new();
    for frame in frames {
        decoder.decode(sse(frame), &mut out);
    }
    decoder.finish(&mut out);
    out
}

fn events(decoder: &mut ChatStreamDecoder, frames: &[&str]) -> Vec<StreamEvent> {
    feed(decoder, frames)
        .into_iter()
        .map(|event| event.unwrap())
        .collect()
}

#[test]
fn custom_messages_ride_as_developer_when_the_wire_supports_it() {
    let req = ChatRequest {
        messages: vec![
            Message::user_text("做事"),
            Message::custom("<system-reminder>Sandbox mode: full.</system-reminder>"),
        ],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": "做事"},
            {"role": "user", "content": "<system-reminder>Sandbox mode: full.</system-reminder>"},
        ]),
        "默认不带 developer 角色时降级为 user"
    );

    let mut with_developer = config();
    with_developer.wire.developer_role = true;
    let body = encode_body(&with_developer, &req, false);
    assert_eq!(
        body["messages"][1]["role"], "developer",
        "支持 developer 的供应商用独立角色承载提醒"
    );
}

#[test]
fn request_minimal_sends_only_model_and_messages() {
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "messages": [{"role": "user", "content": "hi"}],
        })
    );
}

#[test]
fn explicit_empty_assistant_blocks_preserve_required_wire_fields() {
    let mut request = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![
                ContentPart::Text(String::new()),
                ContentPart::Reasoning(ReasoningBlock::text("")),
            ],
        }],
        options: RequestOptions {
            echo_reasoning: crate::types::ReasoningEcho::Always,
            ..Default::default()
        },
        ..Default::default()
    };
    let body = encode_body(&config(), &request, false);
    assert_eq!(body["messages"][0]["content"], "");
    assert_eq!(body["messages"][0]["reasoning_content"], "");
    request.options.echo_reasoning = crate::types::ReasoningEcho::Never;
    let stripped = encode_body(&config(), &request, false);
    assert!(stripped["messages"][0].get("reasoning_content").is_none());
    assert_eq!(stripped["messages"][0]["content"], "");
}

#[test]
fn request_full_maps_every_field() {
    let cfg = LlmConfig {
        wire: WireConfig {
            developer_role: true,
            thinking: crate::ThinkingWire {
                toggle_field: Some("thinking".into()),
                ..Default::default()
            },
            ..Default::default()
        },
        ..config()
    };
    let req = ChatRequest {
        messages: vec![
            Message::system("系统提示"),
            Message::developer("开发者提示"),
            Message {
                role: Role::Assistant,
                content: crate::message_content![
                    ContentPart::Reasoning(ReasoningBlock::text("上一轮思路")),
                    ContentPart::Text("调用工具".into()),
                    ContentPart::ToolCall(ToolCall::new(
                        "call_1",
                        "get_weather",
                        r#"{"city":"北京"}"#,
                    )),
                ],
            },
            Message::tool_result("call_1", "晴", false),
            Message::user_text("谢谢"),
        ],
        tools: vec![ToolDef {
            name: "get_weather".into(),
            description: Some("查天气".into()),
            parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
            strict: Some(true),
        }],
        tool_choice: Some(ToolChoice::Auto),
        max_output_tokens: Some(2048),
        temperature: Some(0.3),
        top_p: Some(0.9),
        // Chat 没有 top_k 标准字段：核心想传时用 extra_body 表达，
        // 本 crate 不猜测厂商是否支持。
        top_k: Some(20),
        stop: vec!["\n\n".into()],
        reasoning: Some(ReasoningConfig {
            enabled: true,
            effort: Some(crate::types::Effort::High),
            ..Default::default()
        }),
        response_format: Some(ResponseFormat::JsonObject),
        parallel_tool_calls: Some(false),
        options: RequestOptions {
            store: Some(false),
            ..Default::default()
        },
    };
    let body = encode_body(&cfg, &req, false);
    assert_eq!(
        body,
        json!({
            "model": "test-model",
            "messages": [
                {"role": "system", "content": "系统提示"},
                {"role": "developer", "content": "开发者提示"},
                {
                    "role": "assistant",
                    "content": "调用工具",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
                    }],
                    "reasoning_content": "上一轮思路",
                },
                {"role": "tool", "tool_call_id": "call_1", "content": "晴"},
                {"role": "user", "content": "谢谢"},
            ],
            "tools": [{
                "type": "function",
                "function": {
                    "name": "get_weather",
                    "description": "查天气",
                    "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                    "strict": true,
                },
            }],
            "tool_choice": "auto",
            "max_tokens": 2048,
            "temperature": 0.3,
            "top_p": 0.9,
            "stop": ["\n\n"],
            "thinking": {"type": "enabled"},
            "reasoning_effort": "high",
            "response_format": {"type": "json_object"},
            "parallel_tool_calls": false,
            "store": false,
        })
    );
    assert!(body.get("top_k").is_none(), "top_k 不属于 Chat 的标准字段");
}

#[test]
fn thinking_wire_variants() {
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        reasoning: Some(ReasoningConfig {
            enabled: true,
            effort: Some(crate::types::Effort::Medium),
            ..Default::default()
        }),
        ..Default::default()
    };

    // 只有 toggle（Kimi 风格：无强度字段）。
    let cfg = LlmConfig {
        wire: WireConfig {
            thinking: crate::config::ThinkingWire {
                toggle_field: Some("thinking".into()),
                effort_field: None,
                ..Default::default()
            },
            ..Default::default()
        },
        ..config()
    };
    let body = encode_body(&cfg, &req, false);
    assert_eq!(body["thinking"], json!({"type": "enabled"}));
    assert!(body.get("reasoning_effort").is_none());

    // toggle_extra 并入 thinking 对象。
    let cfg = LlmConfig {
        wire: WireConfig {
            thinking: crate::config::ThinkingWire {
                toggle_field: Some("thinking".into()),
                toggle_extra: serde_json::Map::from_iter([
                    ("keep".into(), json!("all")),
                    ("clear_thinking".into(), json!(false)),
                ]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..config()
    };
    let body = encode_body(&cfg, &req, false);
    assert_eq!(
        body["thinking"],
        json!({"type": "enabled", "keep": "all", "clear_thinking": false})
    );

    // effort_map 改写强度取值。
    let cfg = LlmConfig {
        wire: WireConfig {
            thinking: crate::config::ThinkingWire {
                effort_map: std::collections::BTreeMap::from([(
                    crate::types::Effort::Medium,
                    "medium-effort".into(),
                )]),
                ..Default::default()
            },
            ..Default::default()
        },
        ..config()
    };
    let body = encode_body(&cfg, &req, false);
    assert_eq!(body["reasoning_effort"], json!("medium-effort"));
    let standard = config();
    let body = encode_body(&standard, &req, true);
    assert!(
        body.get("thinking").is_none(),
        "标准 Chat 不接受厂商 thinking 对象"
    );
    assert_eq!(body["reasoning_effort"], "medium");
    let mut disabled = req;
    disabled.reasoning.as_mut().unwrap().enabled = false;
    let body = encode_body(&standard, &disabled, false);
    assert_eq!(body["reasoning_effort"], "none");
}

#[test]
fn response_format_json_schema_and_unsupported() {
    let req = ChatRequest {
        messages: vec![Message::user_text("hi")],
        response_format: Some(ResponseFormat::JsonSchema {
            name: "weather".into(),
            description: Some("天气结构".into()),
            schema: json!({"type": "object"}),
            strict: Some(true),
        }),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["response_format"],
        json!({
            "type": "json_schema",
            "json_schema": {
                "name": "weather",
                "description": "天气结构",
                "schema": {"type": "object"},
                "strict": true,
            },
        })
    );

    let cfg = LlmConfig {
        wire: WireConfig {
            response_format_dialect: ResponseFormatDialect::Unsupported,
            ..Default::default()
        },
        ..config()
    };
    let body = encode_body(&cfg, &req, false);
    assert!(body.get("response_format").is_none());
}

#[test]
fn image_url_and_base64() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: crate::message_content![
                ContentPart::Text("看图".into()),
                ContentPart::Image {
                    source: ImageSource::Url("https://example.com/a.png".into()),
                    detail: Some(ImageDetail::High),
                },
                ContentPart::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    },
                    detail: None,
                },
            ],
        }],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "text", "text": "看图"},
            {"type": "image_url", "image_url": {"url": "https://example.com/a.png", "detail": "high"}},
            {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
        ])
    );
}

#[test]
fn cache_key_and_breakpoints() {
    moc_i18n::set_locale("zh");
    let req = ChatRequest {
        messages: vec![Message::system("系统"), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(crate::config::CacheConfig {
                key: Some("session-1".into()),
                breakpoints: vec![0],
                instruction_blocks: Vec::new(),
                ttl: Some(CacheTtl::ThirtyMinutes),
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(body["prompt_cache_key"], json!("session-1"));
    assert_eq!(
        body["prompt_cache_options"],
        json!({"mode": "explicit", "ttl": "30m"})
    );
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type": "text", "text": "系统", "prompt_cache_breakpoint": {"mode": "explicit"}}])
    );
    assert_eq!(body["messages"][1]["content"], json!("hi"));

    // 不支持的 TTL 直接报错，不静默降级。
    let bad = ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(crate::config::CacheConfig {
                ttl: Some(CacheTtl::FiveMinutes),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("只支持 30m"), "{err}");

    // 越界断点报错。
    let bad = ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(crate::config::CacheConfig {
                breakpoints: vec![7],
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let err = encode(&config(), &bad, false).unwrap_err();
    assert!(err.to_string().contains("超出消息范围"), "{err}");
}

#[test]
fn response_decode_reads_text_reasoning_tools_and_usage() {
    let body = json!({
        "id": "chatcmpl-1",
        "object": "chat.completion",
        "model": "deepseek-reasoner",
        "choices": [{
            "index": 0,
            "message": {
                "role": "assistant",
                "content": "答案是 42",
                "reasoning_content": "先算一下",
                "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}},
                    {"id": "call_2", "type": "function", "function": {"name": "get_date", "arguments": "{}"}},
                ],
            },
            "finish_reason": "tool_calls",
        }],
        "usage": {
            "prompt_tokens": 100,
            "completion_tokens": 40,
            "total_tokens": 140,
            "prompt_tokens_details": {"cached_tokens": 64},
            "completion_tokens_details": {"reasoning_tokens": 12},
        },
    });
    let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
    assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(response.model.as_deref(), Some("deepseek-reasoner"));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 100,
            output_tokens: 40,
            total_tokens: Some(140),
            cached_input_tokens: Some(64),
            cache_creation_input_tokens: None,
            reasoning_tokens: Some(12),
        }
    );
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Reasoning(ReasoningBlock::Text {
                text: "先算一下".into(),
                signature: None,
            }),
            ContentPart::Text("答案是 42".into()),
            ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
            ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
        ]
    );
}

#[test]
fn finish_reason_table_via_decode() {
    let cases = [
        ("stop", FinishReason::Stop),
        ("length", FinishReason::Length),
        ("tool_calls", FinishReason::ToolCalls),
        ("function_call", FinishReason::ToolCalls),
        ("content_filter", FinishReason::ContentFilter),
        ("something_new", FinishReason::Other("something_new".into())),
        (
            "model_context_window_exceeded",
            FinishReason::Other("model_context_window_exceeded".into()),
        ),
    ];
    for (wire, expected) in cases {
        let body = json!({
            "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": wire}],
        });
        let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
        assert_eq!(response.finish_reason, expected, "{wire}");
    }

    // finish_reason 为 null（未结束的增量形态）时回落到默认值。
    let body = json!({"choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": null}]});
    let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
    assert_eq!(
        response.finish_reason,
        FinishReason::Other("unknown".into())
    );
}

#[test]
fn parallel_tool_calls_encoded_as_one_message_and_n_results() {
    let req = ChatRequest {
        messages: vec![
            Message::user_text("并行查"),
            Message {
                role: Role::Assistant,
                content: crate::message_content![
                    ContentPart::ToolCall(ToolCall::new(
                        "call_1",
                        "get_weather",
                        "{\"city\":\"北京\"}",
                    )),
                    ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
                    ContentPart::ToolCall(ToolCall::new("call_3", "get_time", "{}")),
                ],
            },
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_2", "2026-09-24", false),
            Message::tool_result("call_3", "12:00", false),
        ],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 5);
    assert_eq!(messages[1]["content"], json!(null));
    assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 3);
    assert_eq!(messages[1]["tool_calls"][0]["id"], json!("call_1"));
    assert_eq!(messages[1]["tool_calls"][2]["id"], json!("call_3"));
    for (offset, (id, content)) in [
        ("call_1", "晴"),
        ("call_2", "2026-09-24"),
        ("call_3", "12:00"),
    ]
    .iter()
    .enumerate()
    {
        let message = &messages[2 + offset];
        assert_eq!(message["role"], json!("tool"));
        assert_eq!(message["tool_call_id"], json!(id));
        assert_eq!(message["content"], json!(content));
    }
}

#[test]
fn parallel_tool_calls_streaming_interleaved() {
    let mut decoder = ChatStreamDecoder::new(&config());
    let events = events(
        &mut decoder,
        &[
            r#"{"id":"chatcmpl-1","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"get_date","arguments":""}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"北京\"}"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
            "[DONE]",
        ],
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: Some("chatcmpl-1".into()),
                model: Some("test-model".into()),
                effort: None,
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "call_2".into(),
                name: "get_date".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "{\"city\":".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 1,
                json: "{}".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "\"北京\"}".into(),
            },
            StreamEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: Some(15),
                ..Default::default()
            }),
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    );
}

#[test]
fn tool_call_start_waits_for_real_id() {
    // 网关可能先发名称、后发 ID：Start 必须推迟到 ID 到位，并携带真实 ID；
    // 若提前用空 ID 发射，下游会把该调用的参数增量落到空 ID 上（参数丢失）。
    let mut decoder = ChatStreamDecoder::new(&config());
    let events = events(
        &mut decoder,
        &[
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"bash"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"arguments":"{\"command\":"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ls\"}"}}]},"finish_reason":"tool_calls"}]}"#,
            "[DONE]",
        ],
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: None,
                model: None,
                effort: None,
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "bash".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "{\"command\":".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "\"ls\"}".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    );
}

#[test]
fn stream_tool_call_index_is_bounded_by_local_slots() {
    // `index` 直接用作 `Vec` 下标：巨大值会驱动一次超大分配，回绕值
    // （`usize::MAX`）会在 `index + 1` 处溢出，两者都必须以流错误结束。
    for raw in ["1024", "4294967296", "18446744073709551615"] {
        let mut decoder = ChatStreamDecoder::new(&config());
        let out = feed(
            &mut decoder,
            &[&format!(
                r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{raw},"id":"call_1","function":{{"name":"get_weather","arguments":"{{}}"}}}}]}},"finish_reason":null}}]}}"#
            )],
        );
        assert_eq!(
            out.iter().filter(|event| event.is_err()).count(),
            1,
            "index {raw}: {out:?}"
        );
        assert!(
            matches!(out.last(), Some(Err(LlmError::Stream(_)))),
            "index {raw}: {:?}",
            out.last()
        );
        assert!(
            decoder.tool_ids.is_empty(),
            "index {raw}: 不应为越界下标分配槽位"
        );
    }

    // 上限内的最后一个下标仍然可用。
    let last = MAX_TOOL_CALL_SLOTS - 1;
    let mut decoder = ChatStreamDecoder::new(&config());
    let out = events(
        &mut decoder,
        &[&format!(
            r#"{{"choices":[{{"index":0,"delta":{{"tool_calls":[{{"index":{last},"id":"call_1","function":{{"name":"get_weather","arguments":"{{}}"}}}}]}},"finish_reason":"tool_calls"}}]}}"#
        )],
    );
    assert_eq!(
        out,
        vec![
            StreamEvent::Start {
                id: None,
                model: None,
                effort: None,
            },
            StreamEvent::ToolCallStart {
                index: last,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: last,
                json: "{}".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    );
}

#[test]
fn stream_accepts_block_array_content() {
    // 少数网关在流式增量里给内容块数组：与非流式路径一样必须取出文本，
    // 否则响应「成功」但正文为空。
    let mut decoder = ChatStreamDecoder::new(&config());
    let events = events(
        &mut decoder,
        &[
            r#"{"id":"chatcmpl-1","choices":[{"index":0,"delta":{"role":"assistant","content":[{"type":"text","text":"你"},{"type":"text","text":"好"}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":["再","见"]},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ],
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: Some("chatcmpl-1".into()),
                model: None,
                effort: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "你好".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "再见".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ]
    );
}

#[test]
fn stream_text_and_reasoning_deltas() {
    let mut decoder = ChatStreamDecoder::new(&config());
    let events = events(
        &mut decoder,
        &[
            r#"{"id":"chatcmpl-9","model":"m","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"想","content":""},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"reasoning_content":"一下","content":"你"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{"content":"好"},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ],
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: Some("chatcmpl-9".into()),
                model: Some("m".into()),
                effort: None,
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "想".into(),
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "一下".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "你".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "好".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ]
    );
}

#[test]
fn stream_without_done_sentinel_finishes_at_eof() {
    let mut decoder = ChatStreamDecoder::new(&config());
    let events = events(
        &mut decoder,
        &[r#"{"choices":[{"index":0,"delta":{"content":"完"},"finish_reason":"length"}]}"#],
    );
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: None,
                model: None,
                effort: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "完".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Length,
            },
        ]
    );
    assert!(decoder.is_terminated());
}

#[test]
fn stream_malformed_json_terminates_with_error() {
    let mut decoder = ChatStreamDecoder::new(&config());
    let out = feed(&mut decoder, &["{oops", r#"{"choices":[]}"#]);
    assert!(matches!(out[0], Err(LlmError::Stream(_))), "{:?}", out[0]);
    assert_eq!(out.len(), 1, "错误后不再产出事件");
    assert!(decoder.is_terminated());
}

#[test]
fn stream_refusal_is_emitted_as_text() {
    let mut decoder = ChatStreamDecoder::new(&config());
    let events = events(
        &mut decoder,
        &[
            r#"{"choices":[{"index":0,"delta":{"refusal":"不能回答"},"finish_reason":null}]}"#,
            r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
            "[DONE]",
        ],
    );
    assert_eq!(
        events[1],
        StreamEvent::TextDelta {
            index: 0,
            text: "不能回答".into(),
        }
    );
}

#[test]
fn empty_tool_result_encodes_as_empty_string() {
    let req = ChatRequest {
        messages: vec![
            Message {
                role: Role::Assistant,
                content: crate::message_content![ContentPart::ToolCall(ToolCall::new(
                    "call_1", "noop", "{}"
                ))],
            },
            Message::tool_result("call_1", "", false),
        ],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(body["messages"][1]["content"], json!(""));
}

#[test]
fn tool_result_images_are_bypassed_as_user_message() {
    let req = ChatRequest {
        messages: vec![
            Message {
                role: Role::Assistant,
                content: crate::message_content![ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "screenshot",
                    "{}",
                ))],
            },
            Message::tool_result_with_images(
                "call_1",
                "截图如下",
                false,
                vec![ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                }],
            ),
        ],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "assistant", "content": null, "tool_calls": [
                {"id": "call_1", "type": "function", "function": {"name": "screenshot", "arguments": "{}"}},
            ]},
            {"role": "tool", "tool_call_id": "call_1", "content": "截图如下"},
            {"role": "user", "content": [
                {"type": "text", "text": "[image returned by tool call_1]"},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ]},
        ])
    );
}

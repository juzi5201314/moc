use super::*;
use crate::config::{CacheConfig, Protocol, WireConfig};
use crate::types::{ReasoningEcho, RequestOptions, Usage};
use serde_json::json;

fn config() -> LlmConfig {
    LlmConfig {
        protocol: Protocol::OpenAiResponses,
        base_url: "https://api.openai.com/v1".into(),
        model: "gpt-5".into(),
        auth: crate::config::Auth::Bearer("sk-test".into()),
        ..Default::default()
    }
}

fn base_request() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    }
}

fn encode_body(cfg: &LlmConfig, req: &ChatRequest, stream: bool) -> Value {
    let encoded = encode(cfg, req, stream).unwrap();
    assert_eq!(encoded.url, format!("{}/responses", cfg.base_url));
    serde_json::from_slice(&encoded.body).unwrap()
}

fn sse(data: &str) -> SseEvent {
    SseEvent {
        event: None,
        data: data.into(),
        id: None,
    }
}

fn events(
    decoder: &mut ResponsesStreamDecoder,
    frames: &[&str],
) -> Vec<Result<StreamEvent, LlmError>> {
    let mut out = Vec::new();
    for frame in frames {
        decoder.decode(sse(frame), &mut out);
    }
    decoder.finish(&mut out);
    out
}

fn unwrapped(events: Vec<Result<StreamEvent, LlmError>>) -> Vec<StreamEvent> {
    events.into_iter().map(|event| event.unwrap()).collect()
}

fn assistant_with_tools(calls: &[(&str, &str)]) -> Message {
    Message {
        role: Role::Assistant,
        content: calls
            .iter()
            .map(|(id, name)| ContentPart::ToolCall(ToolCall::new(*id, *name, "{}")))
            .collect(),
    }
}

#[test]
fn request_minimal_has_no_optional_fields() {
    let body = encode_body(&config(), &base_request(), false);
    // `include` 不是可选项：默认回传思维链时它保证加密 reasoning 可续跑。
    assert_eq!(
        body,
        json!({
            "model": "gpt-5",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "hi"}],
            }],
            "include": ["reasoning.encrypted_content"],
        })
    );
}

#[test]
fn request_input_items_order() {
    let req = ChatRequest {
        messages: vec![
            Message::system("系统"),
            Message::user_text("查天气"),
            Message {
                role: Role::Assistant,
                content: crate::message_content![
                    ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                        json!({
                            "type": "reasoning",
                            "id": "rs_1",
                            "encrypted_content": "blob",
                            "summary": [],
                        })
                        .into()
                    ))),
                    ContentPart::Text("这就去查".into()),
                    ContentPart::ToolCall(ToolCall::new(
                        "call_1",
                        "get_weather",
                        "{\"city\":\"北京\"}",
                    )),
                ],
            },
            Message::tool_result("call_1", "晴", false),
        ],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["input"],
        json!([
            {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "系统"}]},
            {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "查天气"}]},
            {
                "type": "reasoning",
                "id": "rs_1",
                "encrypted_content": "blob",
                "summary": [],
            },
            {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "这就去查"}]},
            {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "晴"},
        ])
    );
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
        body["input"],
        json!([
            {"type": "function_call", "call_id": "call_1", "name": "screenshot", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "截图如下"},
            {"type": "message", "role": "user", "content": [
                {"type": "input_text", "text": "[image returned by tool call_1]"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
            ]},
        ])
    );
}

#[test]
fn reasoning_item_echoed_verbatim() {
    let item = json!({
        "type": "reasoning",
        "id": "rs_1",
        "encrypted_content": "opaque-blob",
        "summary": [{"type": "summary_text", "text": "摘要"}],
        "future_field": {"nested": [1, 2, 3]},
    });
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![ContentPart::Reasoning(ReasoningBlock::Item(
                Box::new(item.clone().into())
            ))],
        }],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(body["input"], json!([item]));

    // echo = Never 时被剔除。
    let req = ChatRequest {
        options: RequestOptions {
            echo_reasoning: ReasoningEcho::Never,
            ..Default::default()
        },
        ..req
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(body["input"], json!([]));
}

#[test]
fn parallel_function_call_outputs_pair_by_call_id() {
    let req = ChatRequest {
        messages: vec![
            Message::user_text("并行查"),
            Message {
                role: Role::Assistant,
                content: crate::message_content![
                    ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                        json!({
                            "type": "reasoning",
                            "id": "rs_1",
                        })
                        .into()
                    ))),
                    ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                    ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
                ],
            },
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_2", "2026-09-24", false),
        ],
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    let input = body["input"].as_array().unwrap();
    assert_eq!(input.len(), 6);
    // reasoning item 位于全部 function_call 之前。
    assert_eq!(input[1]["type"], json!("reasoning"));
    assert_eq!(input[2]["call_id"], json!("call_1"));
    assert_eq!(input[3]["call_id"], json!("call_2"));
    assert_eq!(input[4]["type"], json!("function_call_output"));
    assert_eq!(input[4]["call_id"], json!("call_1"));
    assert_eq!(input[4]["output"], json!("晴"));
    assert_eq!(input[5]["call_id"], json!("call_2"));
    assert_eq!(input[5]["output"], json!("2026-09-24"));
}

#[test]
fn max_tokens_parallel_and_store() {
    let req = ChatRequest {
        max_output_tokens: Some(2048),
        temperature: Some(0.2),
        top_p: Some(0.8),
        parallel_tool_calls: Some(false),
        options: RequestOptions {
            store: Some(false),
            ..Default::default()
        },
        ..base_request()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(body["max_output_tokens"], json!(2048));
    assert_eq!(body["temperature"], json!(0.2));
    assert_eq!(body["top_p"], json!(0.8));
    assert_eq!(body["parallel_tool_calls"], json!(false));
    assert_eq!(body["store"], json!(false));

    // 线格式不支持并行开关时不发送。
    let cfg = LlmConfig {
        wire: WireConfig {
            parallel_tool_calls: false,
            ..Default::default()
        },
        ..config()
    };
    let body = encode_body(&cfg, &req, false);
    assert!(body.get("parallel_tool_calls").is_none());

    // stop 无对应字段，忽略。
    let with_stop = ChatRequest {
        stop: vec!["STOP".into()],
        ..base_request()
    };
    let body = encode_body(&config(), &with_stop, false);
    assert!(body.get("stop").is_none());
    assert!(body.get("stop_sequences").is_none());
}

#[test]
fn reasoning_config_mapping() {
    let enabled = ChatRequest {
        reasoning: Some(ReasoningConfig {
            enabled: true,
            effort: Some(crate::types::Effort::Medium),
            summary: Some(ReasoningSummary::Detailed),
        }),
        ..base_request()
    };
    let body = encode_body(&config(), &enabled, false);
    assert_eq!(
        body["reasoning"],
        json!({"effort": "medium", "summary": "detailed"})
    );

    // 关闭：发送 effort=none。
    let disabled = ChatRequest {
        reasoning: Some(ReasoningConfig {
            enabled: false,
            effort: Some(crate::types::Effort::High),
            ..Default::default()
        }),
        ..base_request()
    };
    let body = encode_body(&config(), &disabled, false);
    assert_eq!(body["reasoning"], json!({"effort": "none"}));

    // Omitted 不发送 summary（Responses 没有关闭摘要的取值）。
    let omitted = ChatRequest {
        reasoning: Some(ReasoningConfig {
            enabled: true,
            summary: Some(ReasoningSummary::Omitted),
            ..Default::default()
        }),
        ..base_request()
    };
    let body = encode_body(&config(), &omitted, false);
    assert!(body.get("reasoning").is_none(), "{body}");

    // 未配置 reasoning 时完全不发送。
    let body = encode_body(&config(), &base_request(), false);
    assert!(body.get("reasoning").is_none());
}

#[test]
fn include_encrypted_reasoning() {
    let body = encode_body(&config(), &base_request(), false);
    assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

    let never = ChatRequest {
        options: RequestOptions {
            echo_reasoning: ReasoningEcho::Never,
            ..Default::default()
        },
        ..base_request()
    };
    let body = encode_body(&config(), &never, false);
    assert!(body.get("include").is_none());

    // 与 extra_body 里的 include 合并去重。
    let cfg = LlmConfig {
        extra_body: serde_json::Map::from_iter([(
            "include".into(),
            json!([
                "message.output_text.logprobs",
                "reasoning.encrypted_content"
            ]),
        )]),
        ..config()
    };
    let body = encode_body(&cfg, &base_request(), false);
    assert_eq!(
        body["include"],
        json!([
            "message.output_text.logprobs",
            "reasoning.encrypted_content"
        ])
    );
}

#[test]
fn image_input_block() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: crate::message_content![
                ContentPart::Text("看图".into()),
                ContentPart::Image {
                    source: ImageSource::Url("https://example.com/a.png".into()),
                    detail: Some(ImageDetail::Low),
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
        body["input"][0]["content"],
        json!([
            {"type": "input_text", "text": "看图"},
            {"type": "input_image", "image_url": "https://example.com/a.png", "detail": "low"},
            {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
        ])
    );
}

#[test]
fn cache_breakpoints_and_ttl_validation() {
    let req = ChatRequest {
        messages: vec![Message::system("系统"), Message::user_text("hi")],
        options: RequestOptions {
            cache: Some(CacheConfig {
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
        body["input"][0]["content"],
        json!([{"type": "input_text", "text": "系统", "prompt_cache_breakpoint": {"mode": "explicit"}}])
    );

    let bad_ttl = ChatRequest {
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![0],
                ttl: Some(CacheTtl::OneHour),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..req
    };
    let err = encode(&config(), &bad_ttl, false).unwrap_err();
    assert!(err.to_string().contains("只支持 30m"), "{err}");

    // 断点落在没有文本内容块的 item 上：报错而不是静默丢弃。
    let bad_target = ChatRequest {
        messages: vec![
            Message::user_text("hi"),
            assistant_with_tools(&[("call_1", "get_weather")]),
            Message::tool_result("call_1", "晴", false),
        ],
        options: RequestOptions {
            cache: Some(CacheConfig {
                breakpoints: vec![2],
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    };
    let err = encode(&config(), &bad_target, false).unwrap_err();
    assert!(err.to_string().contains("只能落在文本内容块上"), "{err}");
}

#[test]
fn response_decode_output_items() {
    let body = json!({
        "id": "resp_1",
        "object": "response",
        "model": "gpt-5",
        "status": "completed",
        "output": [
            {"type": "reasoning", "id": "rs_1", "encrypted_content": "blob", "summary": []},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "答案是 42"},
                {"type": "refusal", "refusal": "拒绝部分"},
            ]},
            {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
        ],
        "usage": {
            "input_tokens": 100,
            "output_tokens": 40,
            "total_tokens": 140,
            "input_tokens_details": {"cached_tokens": 64},
            "output_tokens_details": {"reasoning_tokens": 12},
        },
    });
    let response = decode_response(&config(), &base_request(), &body).unwrap();
    assert_eq!(response.id.as_deref(), Some("resp_1"));
    assert_eq!(response.model.as_deref(), Some("gpt-5"));
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
            ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "blob",
                    "summary": [],
                })
                .into()
            ))),
            ContentPart::Text("答案是 42".into()),
            ContentPart::Text("拒绝部分".into()),
            ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );
}

#[test]
fn status_to_finish_reason() {
    let cases = [
        (
            json!({"status": "completed", "output": []}),
            FinishReason::Stop,
        ),
        (
            json!({"status": "completed", "output": [{"type": "function_call"}]}),
            FinishReason::ToolCalls,
        ),
        (
            json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}),
            FinishReason::Length,
        ),
        (
            json!({"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}),
            FinishReason::ContentFilter,
        ),
        (
            json!({"status": "incomplete", "incomplete_details": {"reason": "something_new"}}),
            FinishReason::Other("something_new".into()),
        ),
        (json!({"status": "cancelled"}), FinishReason::Cancelled),
        (
            json!({"status": "queued"}),
            FinishReason::Other("queued".into()),
        ),
        (json!({}), FinishReason::Other("unknown".into())),
    ];
    for (body, expected) in cases {
        let response = decode_response(&config(), &base_request(), &body).unwrap();
        assert_eq!(response.finish_reason, expected, "{body}");
    }
}

#[test]
fn failed_status_becomes_api_error() {
    let body = json!({
        "status": "failed",
        "error": {"code": "server_error", "message": "upstream exploded"},
    });
    let err = decode_response(&config(), &base_request(), &body).unwrap_err();
    match err {
        LlmError::Api(error) => {
            assert_eq!(error.message, "upstream exploded");
            assert_eq!(error.code.as_deref(), Some("server_error"));
            assert_eq!(error.status, None);
            assert!(error.raw_body.contains("upstream exploded"));
        }
        other => panic!("期望 Api 错误，实际 {other:?}"),
    }
}

#[test]
fn stream_failed_and_error_terminate() {
    let mut decoder = ResponsesStreamDecoder::new(&config());
    let out = events(
        &mut decoder,
        &[
            r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
            r#"{"type":"response.completed","response":{"status":"completed"}}"#,
        ],
    );
    assert_eq!(out.len(), 1);
    match &out[0] {
        Err(LlmError::Api(error)) => {
            assert_eq!(error.message, "boom");
            assert_eq!(error.code.as_deref(), Some("server_error"));
        }
        other => panic!("期望 Api 错误，实际 {other:?}"),
    }
    assert!(decoder.is_terminated());

    let mut decoder = ResponsesStreamDecoder::new(&config());
    let out = events(
        &mut decoder,
        &[r#"{"type":"error","code":"invalid_request","message":"bad request"}"#],
    );
    assert_eq!(out.len(), 1);
    assert!(matches!(out[0], Err(LlmError::Api(_))), "{:?}", out[0]);
}

#[test]
fn stream_without_terminal_event_finishes_at_eof() {
    let mut decoder = ResponsesStreamDecoder::new(&config());
    let events = unwrapped(events(
        &mut decoder,
        &[r#"{"type":"response.output_text.delta","output_index":0,"delta":"完"}"#],
    ));
    assert_eq!(
        events,
        vec![
            StreamEvent::TextDelta {
                index: 0,
                text: "完".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Other("unknown".into()),
            },
        ]
    );
}

#[test]
fn stream_part_boundaries_insert_paragraph_separator() {
    let mut decoder = ResponsesStreamDecoder::new(&config());
    let events = unwrapped(events(
        &mut decoder,
        &[
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":0,"delta":"**A**"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"**B**"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"summary_index":1,"delta":"续"}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"content_index":0,"delta":"甲"}"#,
            r#"{"type":"response.output_text.delta","output_index":1,"content_index":1,"delta":"乙"}"#,
            r#"{"type":"response.output_text.delta","output_index":2,"content_index":0,"delta":"丙"}"#,
        ],
    ));
    assert_eq!(
        events,
        vec![
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "**A**".into(),
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "\n\n**B**".into(),
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "续".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "甲".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "\n\n乙".into(),
            },
            StreamEvent::TextDelta {
                index: 2,
                text: "丙".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Other("unknown".into()),
            },
        ]
    );
}

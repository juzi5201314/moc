use super::*;

#[test]
fn json_object_format_is_rejected_instead_of_sending_an_invalid_request() {
    let mut request = base_request();
    request.response_format = Some(ResponseFormat::JsonObject);
    assert!(matches!(
        encode(&config(), &request, false),
        Err(LlmError::Encode(_))
    ));
}

#[test]
fn request_minimal_has_no_optional_fields() {
    let body = encode_body(&config(), &base_request(), false);
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 1024,
            "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
        })
    );
}

#[test]
fn request_full_maps_every_field() {
    let cfg = LlmConfig {
        wire: WireConfig {
            supports_stop: true,
            ..Default::default()
        },
        ..config()
    };
    let req = ChatRequest {
        messages: vec![
            Message::system("系统提示"),
            Message {
                role: Role::User,
                content: crate::message_content![
                    ContentPart::Text("看图".into()),
                    ContentPart::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "AAAA".into(),
                        },
                        detail: None,
                    },
                ],
            },
        ],
        tools: vec![ToolDef {
            name: "get_weather".into(),
            description: Some("查天气".into()),
            parameters: json!({"type": "object"}),
            strict: Some(true),
        }],
        tool_choice: Some(ToolChoice::Auto),
        max_output_tokens: Some(4096),
        temperature: Some(0.4),
        top_p: Some(0.9),
        top_k: Some(20),
        stop: vec!["\n\n".into()],
        reasoning: Some(ReasoningConfig {
            enabled: true,
            effort: Some(crate::types::Effort::High),
            summary: Some(ReasoningSummary::Detailed),
        }),
        response_format: Some(ResponseFormat::JsonSchema {
            name: "weather".into(),
            description: Some("结构".into()),
            schema: json!({"type": "object"}),
            strict: Some(true),
        }),
        options: RequestOptions {
            user_id: Some("user-42".into()),
            ..Default::default()
        },
        ..Default::default()
    };
    let body = encode_body(&cfg, &req, true);
    assert_eq!(
        body,
        json!({
            "model": "claude-sonnet-4-5",
            "max_tokens": 4096,
            "system": "系统提示",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "看图"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ],
            }],
            "tools": [{
                "name": "get_weather",
                "description": "查天气",
                "input_schema": {"type": "object"},
                "strict": true,
            }],
            "tool_choice": {"type": "auto"},
            "temperature": 0.4,
            "top_p": 0.9,
            "top_k": 20,
            "stop_sequences": ["\n\n"],
            "metadata": {"user_id": "user-42"},
            "thinking": {"type": "adaptive", "display": "summarized"},
            "output_config": {
                "effort": "high",
                "format": {"type": "json_schema", "schema": {"type": "object"}},
            },
            "stream": true,
        })
    );
}

#[test]
fn system_and_developer_never_merged_into_messages() {
    let req = ChatRequest {
        messages: vec![
            Message::system("s"),
            Message::developer("d"),
            Message::user_text("u"),
        ],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    let messages = body["messages"].as_array().unwrap();
    assert_eq!(messages.len(), 1);
    for message in messages {
        assert_ne!(message["role"], json!("system"));
        assert_ne!(message["role"], json!("developer"));
    }
}

#[test]
fn image_base64_and_url() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::User,
            content: crate::message_content![
                ContentPart::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/jpeg".into(),
                        data: "BBBB".into(),
                    },
                    detail: Some(crate::types::ImageDetail::High),
                },
                ContentPart::Image {
                    source: ImageSource::Url("https://example.com/a.png".into()),
                    detail: None,
                },
            ],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
            {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
        ])
    );
}

/// reasoning 配置 → thinking 与 output_config.effort 的映射。
#[test]
fn thinking_and_effort_map_to_wire_fields() {
    let cases: [(Option<ReasoningConfig>, Option<Value>, Option<Value>); 4] = [
        (
            Some(ReasoningConfig {
                enabled: true,
                effort: Some(Effort::High),
                summary: Some(ReasoningSummary::Omitted),
            }),
            Some(json!({"type": "adaptive", "display": "omitted"})),
            Some(json!({"effort": "high"})),
        ),
        (
            Some(ReasoningConfig {
                enabled: true,
                effort: Some(Effort::Medium),
                ..Default::default()
            }),
            Some(json!({"type": "adaptive"})),
            Some(json!({"effort": "medium"})),
        ),
        (
            Some(ReasoningConfig {
                enabled: false,
                effort: Some(Effort::Medium),
                ..Default::default()
            }),
            Some(json!({"type": "disabled"})),
            None,
        ),
        (None, None, None),
    ];
    for (reasoning, thinking, effort) in cases {
        let req = ChatRequest {
            reasoning,
            ..base_request()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body.get("thinking"), thinking.as_ref(), "{body}");
        match effort {
            Some(effort) => assert_eq!(body.get("output_config"), Some(&effort), "{body}"),
            None => assert!(body.get("output_config").is_none(), "{body}"),
        }
    }
}

#[test]
fn response_decode_reads_thinking_text_tools_and_usage() {
    let body = json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-5",
        "content": [
            {"type": "thinking", "thinking": "先算一下", "signature": "sig-1"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "答案是 42"},
            {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "北京"}},
        ],
        "stop_reason": "tool_use",
        "usage": {
            "input_tokens": 100,
            "output_tokens": 40,
            "cache_read_input_tokens": 64,
            "cache_creation_input_tokens": 16,
            "output_tokens_details": {"thinking_tokens": 12},
        },
    });
    let response = decode_response(&config(), &base_request(), &body).unwrap();
    assert_eq!(response.id.as_deref(), Some("msg_1"));
    assert_eq!(response.model.as_deref(), Some("claude-sonnet-4-5"));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.usage,
        Usage {
            input_tokens: 180,
            output_tokens: 40,
            total_tokens: None,
            cached_input_tokens: Some(64),
            cache_creation_input_tokens: Some(16),
            reasoning_tokens: Some(12),
        }
    );
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Reasoning(ReasoningBlock::Text {
                text: "先算一下".into(),
                signature: Some("sig-1".into()),
            }),
            ContentPart::Reasoning(ReasoningBlock::Redacted {
                data: "opaque".into(),
            }),
            ContentPart::Text("答案是 42".into()),
            ContentPart::ToolCall(ToolCall::new(
                "toolu_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );
}

#[test]
fn tool_result_error_flag_is_encoded() {
    let req = ChatRequest {
        messages: vec![
            assistant_with_tools(&[("call_1", "get_weather")]),
            Message {
                role: Role::Tool,
                content: crate::message_content![ContentPart::ToolResult(ToolResult {
                    tool_call_id: "call_1".into(),
                    content: "工具炸了".into(),
                    is_error: true,
                    images: Vec::new(),
                })],
            },
        ],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": "工具炸了",
            "is_error": true,
        })
    );
}

use super::*;

#[test]
fn reasoning_blocks_round_trip_with_signature() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![
                ContentPart::Reasoning(ReasoningBlock::Text {
                    text: "先想一想".into(),
                    signature: Some("sig-1".into()),
                }),
                ContentPart::Reasoning(ReasoningBlock::Redacted {
                    data: "opaque".into(),
                }),
                ContentPart::Text("答案".into()),
            ],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][0]["content"],
        json!([
            {"type": "thinking", "thinking": "先想一想", "signature": "sig-1"},
            {"type": "redacted_thinking", "data": "opaque"},
            {"type": "text", "text": "答案"},
        ])
    );

    // echo = Never 时 thinking 与 redacted_thinking 一并剔除，其余块保持顺序。
    let req = ChatRequest {
        options: RequestOptions {
            echo_reasoning: crate::types::ReasoningEcho::Never,
            ..Default::default()
        },
        ..req
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type": "text", "text": "答案"}])
    );
}

#[test]
fn unsigned_thinking_block_is_dropped() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![
                ContentPart::Reasoning(ReasoningBlock::text("无签名的思考")),
                ContentPart::Text("正文".into()),
            ],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][0]["content"],
        json!([{"type": "text", "text": "正文"}])
    );
}

#[test]
fn stream_maps_official_stop_reason_values() {
    // `end_turn` 必须落成自然结束；落成 `Other` 会让 core 把正常结束当异常。
    for (wire, expected) in [
        ("end_turn", FinishReason::Stop),
        ("pause_turn", FinishReason::PauseTurn),
    ] {
        let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"写完"}}"#,
                &format!(
                    r#"{{"type":"message_delta","delta":{{"stop_reason":"{wire}"}},"usage":{{"output_tokens":4}}}}"#
                ),
                r#"{"type":"message_stop"}"#,
            ],
        ));
        assert_eq!(
            events.last(),
            Some(&StreamEvent::Done {
                finish_reason: expected,
            }),
            "{wire}"
        );
    }
}

#[test]
fn stream_event_sequence() {
    let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
    let events = unwrapped(events(
        &mut decoder,
        &[
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":1}}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}}"#,
            r#"{"type":"content_block_stop","index":1}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"北京\"}"}}"#,
            r#"{"type":"content_block_stop","index":2}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
            r#"{"type":"message_stop"}"#,
            // 终态之后的帧一律丢弃。
            r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"多余"}}"#,
        ],
    ));
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: Some("msg_1".into()),
                model: Some("claude-sonnet-4-5".into()),
                effort: None,
            },
            StreamEvent::Usage(Usage {
                input_tokens: 10,
                output_tokens: 1,
                ..Default::default()
            }),
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "想".into(),
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: "sig".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "答案".into(),
            },
            StreamEvent::ToolCallStart {
                index: 2,
                id: "toolu_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 2,
                json: "{\"city\":".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 2,
                json: "\"北京\"}".into(),
            },
            StreamEvent::Usage(Usage {
                output_tokens: 40,
                ..Default::default()
            }),
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    );
}

#[test]
fn stream_skips_server_tool_arguments() {
    // 服务端工具（web_search / MCP）由上游自己执行，其 input_json_delta
    // 不是客户端工具调用；产出 id/name 全空的 ToolCall 后下一轮必 400。
    let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
    let events = unwrapped(events(
        &mut decoder,
        &[
            r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5"}}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"moc\"}"}}"#,
            r#"{"type":"content_block_stop","index":0}"#,
            r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[]}}"#,
            r#"{"type":"content_block_start","index":2,"content_block":{"type":"mcp_tool_use","id":"mcptoolu_1","name":"remote"}}"#,
            r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
            r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
            r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"北京\"}"}}"#,
            r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    ));
    assert_eq!(
        events,
        vec![
            StreamEvent::Start {
                id: Some("msg_1".into()),
                model: Some("claude-sonnet-4-5".into()),
                effort: None,
            },
            StreamEvent::ToolCallStart {
                index: 3,
                id: "toolu_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 3,
                json: "{\"city\":\"北京\"}".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ]
    );
}

#[test]
fn stream_redacted_thinking_and_ping() {
    let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
    let events = unwrapped(events(
        &mut decoder,
        &[
            r#"{"type":"ping"}"#,
            r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
            r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{}}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    ));
    assert_eq!(
        events,
        vec![
            StreamEvent::ReasoningItem {
                index: 0,
                item: Box::new(json!({"type": "redacted_thinking", "data": "opaque"})),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Other("unknown".into()),
            },
        ]
    );
}

#[test]
fn stream_error_event_terminates() {
    let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
    let out = events(
        &mut decoder,
        &[
            r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
            r#"{"type":"message_stop"}"#,
        ],
    );
    assert_eq!(out.len(), 1);
    match &out[0] {
        Err(LlmError::Api(error)) => {
            assert_eq!(error.kind, ApiErrorKind::Overloaded);
            assert_eq!(error.message, "Overloaded");
            assert_eq!(error.status, None);
            assert!(error.is_retryable());
        }
        other => panic!("期望 Api 错误，实际 {other:?}"),
    }
    assert!(decoder.is_terminated());
}

#[test]
fn stream_without_message_stop_finishes_at_eof() {
    let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
    let events = unwrapped(events(
        &mut decoder,
        &[
            r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9}}"#,
        ],
    ));
    assert_eq!(
        events,
        vec![
            StreamEvent::Usage(Usage {
                output_tokens: 9,
                ..Default::default()
            }),
            StreamEvent::Done {
                finish_reason: FinishReason::Length,
            },
        ]
    );
}

#[test]
fn tool_result_images_upgrade_content_to_blocks() {
    let req = ChatRequest {
        messages: vec![
            assistant_with_tools(&[("call_1", "screenshot")]),
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
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][1]["content"][0],
        json!({
            "type": "tool_result",
            "tool_use_id": "call_1",
            "content": [
                {"type": "text", "text": "截图如下"},
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
            ],
            "is_error": false,
        })
    );

    // 无图结果保持字符串形态，不引入结构变化。
    let plain = ChatRequest {
        messages: vec![
            assistant_with_tools(&[("call_1", "screenshot")]),
            Message::tool_result("call_1", "纯文本", false),
        ],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &plain, false);
    assert_eq!(
        body["messages"][1]["content"][0]["content"],
        json!("纯文本")
    );
}

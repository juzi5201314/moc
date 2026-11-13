use super::*;

#[test]
fn accumulates_text_and_tool_calls() {
    let accumulator = accumulate(
        Protocol::OpenAiChat,
        &[
            StreamEvent::Start {
                id: Some("chatcmpl-1".into()),
                model: Some("test".into()),
                effort: None,
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "答案".into(),
            },
            StreamEvent::TextDelta {
                index: 0,
                text: "是 42".into(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "{\"city\":\"北京\"}".into(),
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
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiChat, false);
    assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
    assert_eq!(response.model.as_deref(), Some("test"));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.usage.total_tokens(), 15);
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Text("答案是 42".into()),
            ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );
}

#[test]
fn accumulates_reasoning_with_signature() {
    let accumulator = accumulate(
        Protocol::AnthropicMessages,
        &[
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "先".into(),
            },
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "想一想".into(),
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: "sig-".into(),
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: "1".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "答案".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    );
    let response = accumulator.fold(Protocol::AnthropicMessages, false);
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Reasoning(ReasoningBlock::Text {
                text: "先想一想".into(),
                signature: Some("sig-1".into()),
            }),
            ContentPart::Text("答案".into()),
        ]
    );
}

#[test]
fn keeps_reasoning_item_from_event() {
    let item = serde_json::json!({
        "type": "reasoning",
        "id": "rs_1",
        "encrypted_content": "blob",
        "summary": [{"type": "summary_text", "text": "想一下"}],
    });
    let accumulator = accumulate(
        Protocol::OpenAiResponses,
        &[
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "想一下".into(),
            },
            StreamEvent::ReasoningItem {
                index: 0,
                item: Box::new(item.clone()),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiResponses, false);
    assert_eq!(
        response.message.content,
        vec![ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
            item.into()
        )))]
    );
}

#[test]
fn interleaved_tool_calls_by_index() {
    let accumulator = accumulate(
        Protocol::OpenAiChat,
        &[
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
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiChat, false);
    assert_eq!(
        response.message.content,
        vec![
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
fn partial_arguments_preserved_raw() {
    let accumulator = accumulate(
        Protocol::OpenAiChat,
        &[
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "{\"city\":".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::ToolCalls,
            },
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiChat, false);
    let ContentPart::ToolCall(call) = &response.message.content[0] else {
        panic!("期望工具调用块");
    };
    assert_eq!(call.arguments, "{\"city\":");
    assert!(call.arguments_json().is_err());
}

#[test]
fn interrupt_keeps_text_and_complete_tool_call() {
    let accumulator = accumulate(
        Protocol::OpenAiChat,
        &[
            StreamEvent::TextDelta {
                index: 0,
                text: "正在查".into(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "{\"city\":\"北京\"}".into(),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "call_2".into(),
                name: "get_date".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 1,
                json: "{\"day\":".into(),
            },
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiChat, true);
    assert_eq!(response.finish_reason, FinishReason::Interrupted);
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Text("正在查".into()),
            ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );
}

#[test]
fn interrupt_drops_unsigned_thinking() {
    let accumulator = accumulate(
        Protocol::AnthropicMessages,
        &[
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "半截思考".into(),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "正文".into(),
            },
        ],
    );
    let response = accumulator.fold(Protocol::AnthropicMessages, true);
    assert_eq!(
        response.message.content,
        vec![ContentPart::Text("正文".into())]
    );

    // 带签名时保留。
    let accumulator = accumulate(
        Protocol::AnthropicMessages,
        &[
            StreamEvent::ReasoningDelta {
                index: 0,
                text: "完整思考".into(),
            },
            StreamEvent::ReasoningSignature {
                index: 0,
                signature: "sig".into(),
            },
        ],
    );
    let response = accumulator.fold(Protocol::AnthropicMessages, true);
    assert_eq!(
        response.message.content,
        vec![ContentPart::Reasoning(ReasoningBlock::Text {
            text: "完整思考".into(),
            signature: Some("sig".into()),
        })]
    );
}

#[test]
fn interrupt_drops_orphan_reasoning_item() {
    let item = serde_json::json!({"type": "reasoning", "id": "rs_1"});
    let accumulator = accumulate(
        Protocol::OpenAiResponses,
        &[
            StreamEvent::ReasoningItem {
                index: 0,
                item: Box::new(item.clone()),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 1,
                json: "{\"city\":".into(),
            },
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiResponses, true);
    assert!(
        response.message.content.is_empty(),
        "{:?}",
        response.message.content
    );

    // 有完整工具调用与之配对时保留。
    let accumulator = accumulate(
        Protocol::OpenAiResponses,
        &[
            StreamEvent::ReasoningItem {
                index: 0,
                item: Box::new(item.clone()),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 1,
                json: "{\"city\":\"北京\"}".into(),
            },
        ],
    );
    let response = accumulator.fold(Protocol::OpenAiResponses, true);
    assert_eq!(
        response.message.content,
        vec![
            ContentPart::Reasoning(ReasoningBlock::Item(Box::new(item.into()))),
            ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{\"city\":\"北京\"}"
            )),
        ]
    );
}

#[test]
fn redacted_thinking_folds_into_redacted_block() {
    // Anthropic 的 `redacted_thinking` 折成 `ReasoningBlock::Item` 时编码器
    // 硬拒绝：产物一旦落库，该会话后续所有请求都以不可重试的 Encode 失败。
    let accumulator = accumulate(
        Protocol::AnthropicMessages,
        &[
            StreamEvent::ReasoningItem {
                index: 0,
                item: Box::new(serde_json::json!({
                    "type": "redacted_thinking",
                    "data": "opaque",
                })),
            },
            StreamEvent::TextDelta {
                index: 1,
                text: "答案".into(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    );
    for interrupted in [false, true] {
        let response = accumulator.fold(Protocol::AnthropicMessages, interrupted);
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Reasoning(ReasoningBlock::Redacted {
                    data: "opaque".into(),
                }),
                ContentPart::Text("答案".into()),
            ],
            "interrupted={interrupted}"
        );
    }

    // 与非流式一致地可再编码：同一条消息放回历史，下一个请求必须能构建。
    let response = accumulator.fold(Protocol::AnthropicMessages, true);
    let request = ChatRequest {
        messages: vec![
            Message::user_text("查天气"),
            response.message,
            Message::user_text("继续"),
        ],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let encoded =
        crate::codec::encode(&config(Protocol::AnthropicMessages), &request, false).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&encoded.body).unwrap();
    assert_eq!(
        body["messages"][1]["content"][0],
        serde_json::json!({"type": "redacted_thinking", "data": "opaque"})
    );

    // 与协议不匹配的 reasoning item 同样不能留下：Anthropic 编码器对它硬拒绝。
    let accumulator = accumulate(
        Protocol::AnthropicMessages,
        &[StreamEvent::ReasoningItem {
            index: 0,
            item: Box::new(serde_json::json!({"type": "reasoning", "id": "rs_1"})),
        }],
    );
    assert!(
        accumulator
            .fold(Protocol::AnthropicMessages, false)
            .message
            .content
            .is_empty()
    );
}

#[tokio::test]
async fn interrupted_summary_is_visible_but_not_replayed_as_a_reasoning_item() {
    let mut stream = stream_from_chunks(
        &config(Protocol::OpenAiResponses),
        &request(),
        vec![
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"output_index\":0,\"delta\":\"想了一半\"}\n\n",
        ],
    );
    assert!(matches!(
        stream.next_event().await.unwrap().unwrap(),
        StreamEvent::ReasoningDelta { .. }
    ));
    let response = stream.interrupt();
    let ContentPart::Reasoning(block) = &response.message.content[0] else {
        panic!("缺少摘要")
    };
    assert_eq!(block.display_text().as_deref(), Some("想了一半"));
    let request = ChatRequest {
        messages: vec![response.message],
        ..Default::default()
    };
    let wire = crate::inspect_request(&config(Protocol::OpenAiResponses), &request, false).unwrap();
    assert_eq!(wire.body["input"], serde_json::json!([]));
}

#[test]
fn interrupt_then_history_is_valid() {
    let accumulator = accumulate(
        Protocol::OpenAiChat,
        &[
            StreamEvent::TextDelta {
                index: 0,
                text: "先查天气".into(),
            },
            StreamEvent::ToolCallStart {
                index: 0,
                id: "call_1".into(),
                name: "get_weather".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 0,
                json: "{\"city\":\"北京\"}".into(),
            },
            StreamEvent::ToolCallStart {
                index: 1,
                id: "call_2".into(),
                name: "get_date".into(),
            },
            StreamEvent::ToolCallArgumentsDelta {
                index: 1,
                json: "{\"day\":".into(),
            },
        ],
    );
    let partial = accumulator.fold(Protocol::OpenAiChat, true);

    // core 为保留的工具调用补结果、追加用户插话，然后重新编码：不应触发
    // 历史完整性校验。
    let request = ChatRequest {
        messages: vec![
            Message::user_text("查天气"),
            partial.message,
            Message::tool_result("call_1", "晴", false),
            Message::user_text("改用另一种方案"),
        ],
        ..Default::default()
    };
    let encoded = crate::codec::encode(&config(Protocol::OpenAiChat), &request, false).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&encoded.body).unwrap();
    assert_eq!(body["messages"].as_array().unwrap().len(), 4);
    assert_eq!(
        body["messages"][1]["tool_calls"].as_array().unwrap().len(),
        1
    );
}

#[tokio::test]
async fn chat_keeps_choice_zero_and_complete_fragmented_tool_name() {
    let stream = stream_ending_after(
        &config(Protocol::OpenAiChat),
        &request(),
        vec![
            "data: {\"choices\":[{\"index\":1,\"delta\":{\"content\":\"不能混入\",\"tool_calls\":[{\"index\":0,\"id\":\"other\",\"function\":{\"name\":\"wrong\",\"arguments\":\"{}\"}}]}},{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"get\",\"arguments\":\"{\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_1\",\"function\":{\"name\":\"_weather\",\"arguments\":\"\\\"city\\\":\\\"北京\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"name\":\"_now\",\"arguments\":\"}\"}}]},\"finish_reason\":\"tool_calls\"},{\"index\":1,\"delta\":{},\"finish_reason\":\"length\"}]}\n\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
            "data: [DONE]\n\n",
        ],
    );
    let response = stream.finish().await.unwrap();
    assert_eq!(
        response.message.content.as_slice(),
        &[ContentPart::ToolCall(ToolCall::new(
            "call_1",
            "get_weather_now",
            r#"{"city":"北京"}"#
        ))]
    );
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(response.usage.total_tokens(), 15);
}

#[tokio::test]
async fn finish_rejects_a_stream_that_ends_without_a_terminal_event() {
    let cfg = config(Protocol::OpenAiChat);
    let req = request();
    let stream = stream_ending_after(
        &cfg,
        &req,
        vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"半截\"},\"finish_reason\":null}]}\n\n",
        ],
    );
    let error = stream.finish().await.unwrap_err();
    assert!(matches!(error, LlmError::Stream(_)), "{error:?}");
}

#[tokio::test]
async fn finish_accepts_a_stream_that_reaches_the_terminal_event() {
    let cfg = config(Protocol::OpenAiChat);
    let req = request();
    let stream = stream_ending_after(
        &cfg,
        &req,
        vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"完整\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ],
    );
    let response = stream.finish().await.expect("有终态的响应");
    assert!(matches!(
        &response.message.content[0],
        crate::ContentPart::Text(text) if text == "完整"
    ));
}

#[tokio::test]
async fn finish_folds_all_events() {
    let cfg = config(Protocol::OpenAiChat);
    let req = request();
    let mut stream = stream_from_chunks(
        &cfg,
        &req,
        vec![
            "data: {\"id\":\"chatcmpl-1\",\"model\":\"test\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你\"},\"finish_reason\":null}]}\n\n",
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"好\"},\"finish_reason\":\"stop\"}]}\n\n",
            "data: [DONE]\n\n",
        ],
    );
    // 消费一个事件后再 finish：剩余事件由 finish 读尽。
    let first = stream.next_event().await.unwrap().unwrap();
    assert!(matches!(first, StreamEvent::Start { .. }), "{first:?}");
    let response = stream.finish().await.unwrap();
    assert_eq!(response.message.text(), "你好");
    assert_eq!(response.finish_reason, FinishReason::Stop);
    assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
}

#[tokio::test]
async fn interrupt_returns_partial_response_without_await() {
    let cfg = config(Protocol::AnthropicMessages);
    let req = request();
    let mut stream = stream_from_chunks(
        &cfg,
        &req,
        vec![
            "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_1\",\"model\":\"test\"}}\n\n",
            "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"写到一半\"}}\n\n",
        ],
    );
    // 只读到中途就打断：这正是用户插话的时序。
    assert!(stream.next_event().await.is_some());
    assert!(stream.next_event().await.is_some());
    let partial = stream.interrupt();
    assert_eq!(partial.finish_reason, FinishReason::Interrupted);
    assert_eq!(partial.message.text(), "写到一半");
    assert_eq!(partial.id.as_deref(), Some("msg_1"));
}

#[test]
fn usage_is_merged_across_events() {
    // Anthropic 的用量分两次给出：message_start 只有输入，message_delta 只有输出。
    let accumulator = accumulate(
        Protocol::AnthropicMessages,
        &[
            StreamEvent::Usage(Usage {
                input_tokens: 9,
                output_tokens: 1,
                cached_input_tokens: Some(4),
                ..Default::default()
            }),
            StreamEvent::Usage(Usage {
                output_tokens: 12,
                ..Default::default()
            }),
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    );
    let response = accumulator.fold(Protocol::AnthropicMessages, false);
    assert_eq!(response.usage.input_tokens, 9);
    assert_eq!(response.usage.output_tokens, 12);
    assert_eq!(response.usage.cached_input_tokens, Some(4));
}

#[test]
fn accumulator_ignores_empty_text_blocks() {
    let accumulator = accumulate(
        Protocol::OpenAiChat,
        &[
            StreamEvent::TextDelta {
                index: 0,
                text: String::new(),
            },
            StreamEvent::Done {
                finish_reason: FinishReason::Stop,
            },
        ],
    );
    assert!(
        accumulator
            .fold(Protocol::OpenAiChat, false)
            .message
            .content
            .is_empty()
    );
}

#[test]
fn chat_stream_is_send_and_unpin() {
    // 契约：ChatStream 可以 `tokio::spawn` 到独立任务中消费。
    fn assert_send_unpin<T: Send + Unpin>() {}
    assert_send_unpin::<ChatStream>();
}

#[tokio::test]
async fn guard_drops_upstream_and_discards_reasoning_and_tools_in_same_chunk() {
    struct DroppedBody {
        data: Option<Bytes>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Stream for DroppedBody {
        type Item = reqwest::Result<Bytes>;
        fn poll_next(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<Option<Self::Item>> {
            match self.data.take() {
                Some(data) => Poll::Ready(Some(Ok(data))),
                None => Poll::Pending,
            }
        }
    }
    impl Drop for DroppedBody {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }
    let dropped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let frame = serde_json::json!({"choices":[{"index":0,"delta":{"reasoning_content":"再次检查结果并确认当前判断。".repeat(100)},"finish_reason":null}]});
    let body = DroppedBody {
        data: Some(Bytes::from(format!("data: {frame}\n\ndata: [DONE]\n\n"))),
        dropped: std::sync::Arc::clone(&dropped),
    };
    let mut stream = ChatStream::new(Box::pin(body), &config(Protocol::OpenAiChat), &request());
    let mut events = Vec::new();
    while let Some(event) = stream.next_event().await {
        events.push(event);
    }
    assert!(dropped.load(std::sync::atomic::Ordering::Relaxed));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, Err(LlmError::Guard(_))))
    );
    let partial = stream.interrupt();
    assert!(partial.message.content.is_empty());
    assert_eq!(partial.finish_reason, FinishReason::LoopDetected);
}

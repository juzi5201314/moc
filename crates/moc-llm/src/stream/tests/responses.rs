use super::*;

#[tokio::test]
async fn responses_replay_preserves_phase_and_interleaved_text_order() {
    use serde_json::json;
    let output = [
        json!({"type":"message", "phase":"commentary", "role":"assistant", "content":[{"type":"output_text","text":"先查"}]}),
        json!({"type":"function_call", "call_id":"call_1", "name":"read", "arguments":"{}"}),
        json!({"type":"message", "phase":"final_answer", "role":"assistant", "content":[{"type":"output_text","text":"结论"}]}),
    ];
    let response = decode_frames(
        Protocol::OpenAiResponses,
        &[json!({"type":"response.completed", "response":{"status":"completed", "output":output}})],
    )
    .await;
    assert_eq!(response.message.text(), "先查结论");
    let restored =
        serde_json::from_str(&serde_json::to_string(&response.message).unwrap()).unwrap();
    let request = ChatRequest {
        messages: vec![restored, Message::tool_result("call_1", "结果", false)],
        ..Default::default()
    };
    let wire = crate::inspect_request(&config(Protocol::OpenAiResponses), &request, false).unwrap();
    assert_eq!(wire.body["input"][0], output[0]);
    assert_eq!(wire.body["input"][1], output[1]);
    assert_eq!(wire.body["input"][2], output[2]);
}

#[tokio::test]
async fn responses_tool_snapshots_preserve_arguments_without_deltas() {
    use serde_json::json;
    let arguments = r#"{"path":"README.md"}"#;
    let call = json!({"type":"function_call", "id":"fc_1", "call_id":"call_1", "name":"read", "arguments":arguments});
    for carrier in ["added", "arguments_done", "item_done", "completed"] {
        let mut added = call.clone();
        added["arguments"] = json!(if carrier == "added" { arguments } else { "" });
        let mut frames =
            vec![json!({"type":"response.output_item.added", "output_index":0, "item":added})];
        match carrier {
            "arguments_done" => frames.push(json!({"type":"response.function_call_arguments.done", "output_index":0, "arguments":arguments})),
            "item_done" => frames.push(json!({"type":"response.output_item.done", "output_index":0, "item":call})),
            _ => {}
        }
        frames.push(json!({"type":"response.completed", "response":{"status":"completed", "output": if carrier == "completed" { vec![call.clone()] } else { Vec::new() }}}));
        let response = decode_frames(Protocol::OpenAiResponses, &frames).await;
        assert_eq!(
            response.message.tool_calls().collect::<Vec<_>>(),
            vec![&ToolCall::new("call_1", "read", arguments)],
            "{carrier}"
        );
    }
}

#[tokio::test]
async fn responses_final_items_replace_partial_arguments_without_duplicates() {
    use serde_json::json;
    let calls = [
        json!({"type":"function_call", "call_id":"call_read", "name":"read", "arguments":"{\"path\":\"README.md\"}"}),
        json!({"type":"function_call", "call_id":"call_find", "name":"find", "arguments":"{\"glob\":[\"*.rs\"]}"}),
        json!({"type":"function_call", "call_id":"call_bash", "name":"bash", "arguments":"{\"command\":\"pwd\"}"}),
    ];
    let mut frames = Vec::new();
    for (index, call) in calls.iter().enumerate() {
        let mut added = call.clone();
        added["arguments"] = json!("");
        frames
            .push(json!({"type":"response.output_item.added", "output_index":index, "item":added}));
        frames.push(json!({"type":"response.function_call_arguments.delta", "output_index":index, "delta":"{"}));
    }
    for (index, call) in calls.iter().enumerate().rev() {
        frames.push(json!({"type":"response.function_call_arguments.done", "output_index":index, "arguments":call["arguments"]}));
        frames.push(json!({"type":"response.output_item.done", "output_index":index, "item":call}));
    }
    frames.push(json!({"type":"response.completed", "response":{"id":"resp_1", "model":"test", "status":"completed", "output":calls}}));
    let response = decode_frames(Protocol::OpenAiResponses, &frames).await;
    assert_eq!(response.id.as_deref(), Some("resp_1"));
    assert_eq!(response.finish_reason, FinishReason::ToolCalls);
    assert_eq!(
        response.message.tool_calls().cloned().collect::<Vec<_>>(),
        vec![
            ToolCall::new("call_read", "read", r#"{"path":"README.md"}"#),
            ToolCall::new("call_find", "find", r#"{"glob":["*.rs"]}"#),
            ToolCall::new("call_bash", "bash", r#"{"command":"pwd"}"#),
        ]
    );
}

#[tokio::test]
async fn responses_final_message_preserves_text_refusal_and_reasoning() {
    use serde_json::json;
    let reasoning =
        json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":[]});
    let message = json!({"type":"message", "content":[{"type":"output_text", "text":"完整正文"}, {"type":"refusal", "refusal":"拒绝内容"}]});
    let response = decode_frames(Protocol::OpenAiResponses, &[
        json!({"type":"response.output_item.done", "output_index":0, "item":reasoning}),
        json!({"type":"response.output_text.delta", "output_index":1, "content_index":0, "delta":"完整"}),
        json!({"type":"response.output_item.done", "output_index":1, "item":message}),
        json!({"type":"response.completed", "response":{"status":"completed", "output":[reasoning, message]}}),
    ]).await;
    assert_eq!(response.message.text(), "完整正文\n\n拒绝内容");
    assert_eq!(response.message.content.len(), 2);
    assert_eq!(
        response.message.content[0],
        ContentPart::Reasoning(ReasoningBlock::Item(Box::new(reasoning.into())))
    );
}

#[tokio::test]
async fn responses_summary_survives_snapshots_persistence_and_replay() {
    use serde_json::json;
    for (delta, summary) in [
        ("摘要", json!([])),
        ("", json!([{"type":"summary_text", "text":"摘要"}])),
        ("摘", json!([{"type":"summary_text", "text":"摘要"}])),
    ] {
        let item = json!({"type":"reasoning", "id":"rs_1", "encrypted_content":"opaque", "summary":summary});
        let frames = [
            json!({"type":"response.reasoning_summary_text.delta", "output_index":0, "delta":delta}),
            json!({"type":"response.output_item.done", "output_index":0, "item":item}),
            json!({"type":"response.completed", "response":{"status":"completed", "output":[item]}}),
        ];
        let body = EndedStream {
            chunks: frames
                .iter()
                .map(|frame| Bytes::from(format!("data: {frame}\n\n")))
                .collect(),
        };
        let cfg = config(Protocol::OpenAiResponses);
        let mut stream = ChatStream::new(Box::pin(body), &cfg, &request());
        let mut visible = String::new();
        while let Some(event) = stream.next_event().await {
            if let StreamEvent::ReasoningDelta { text, .. } = event.unwrap() {
                visible.push_str(&text);
            }
        }
        assert_eq!(visible, "摘要", "实时摘要必须完整且不重复");
        let response = stream.finish().await.unwrap();
        let persisted = serde_json::to_string(&response.message).unwrap();
        let restored: Message = serde_json::from_str(&persisted).unwrap();
        let ContentPart::Reasoning(block) = &restored.content[0] else {
            panic!("缺少思考块")
        };
        assert_eq!(block.display_text().as_deref(), Some("摘要"));
        let request = ChatRequest {
            messages: vec![restored],
            ..Default::default()
        };
        let wire = crate::inspect_request(&cfg, &request, false).unwrap();
        assert_eq!(wire.body["input"][0], item, "本地摘要不能改写原始回传对象");
    }
}

#[tokio::test]
async fn responses_incomplete_preserves_refusal_usage_and_stop_reason() {
    use serde_json::json;
    let response = decode_frames(Protocol::OpenAiResponses, &[
        json!({"type":"response.refusal.delta", "output_index":0, "delta":"不能回答"}),
        json!({"type":"response.incomplete", "response":{"status":"incomplete", "incomplete_details":{"reason":"max_output_tokens"}, "usage":{"input_tokens":3,"output_tokens":1}}}),
    ]).await;
    assert_eq!(response.message.text(), "不能回答");
    assert_eq!(response.finish_reason, FinishReason::Length);
    assert_eq!(response.usage.input_tokens, 3);
    assert_eq!(response.usage.output_tokens, 1);
}

#[tokio::test]
async fn responses_text_survives_multi_message_and_mismatched_snapshots() {
    use serde_json::json;
    // 多 message 输出项不应向后续输出项补前导空行而破坏快照前缀校验
    let msg1 = json!({"type":"message", "phase":"commentary", "role":"assistant", "content":[{"type":"output_text", "text":"先查"}]});
    let call = json!({"type":"function_call", "call_id":"call_1", "name":"read", "arguments":"{}"});
    let msg2 = json!({"type":"message", "phase":"final_answer", "role":"assistant", "content":[{"type":"output_text", "text":"结论"}]});
    let multi = decode_frames(
        Protocol::OpenAiResponses,
        &[
            json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"先查"}),
            json!({"type":"response.output_item.done", "output_index":0, "item":msg1}),
            json!({"type":"response.output_item.done", "output_index":1, "item":call}),
            json!({"type":"response.output_text.delta", "output_index":2, "content_index":0, "delta":"结论"}),
            json!({"type":"response.output_item.done", "output_index":2, "item":msg2}),
            json!({"type":"response.completed", "response":{"status":"completed", "output":[msg1, call, msg2]}}),
        ],
    )
    .await;
    assert_eq!(multi.message.text(), "先查结论");

    // 兼容网关终态 content 为空或与已收增量不一致时，保留已收增量而不中止流
    for done_text in [json!([]), json!([{"type":"output_text", "text":"改写后的正文"}])] {
        let item = json!({"type":"message", "role":"assistant", "content":done_text});
        let response = decode_frames(
            Protocol::OpenAiResponses,
            &[
                json!({"type":"response.output_text.delta", "output_index":0, "content_index":0, "delta":"原始增量"}),
                json!({"type":"response.output_item.done", "output_index":0, "item":item}),
                json!({"type":"response.completed", "response":{"status":"completed", "output":[item]}}),
            ],
        )
        .await;
        assert_eq!(response.message.text(), "原始增量");
    }
}

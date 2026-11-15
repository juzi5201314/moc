use super::*;

#[test]
fn merge_by_wire_role() {
    let req = ChatRequest {
        messages: vec![
            Message::user_text("先查"),
            assistant_with_tools(&[("call_1", "get_weather"), ("call_2", "get_date")]),
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_2", "2026-09-24", false),
            Message::user_text("再看这个"),
        ],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"],
        json!([
            {"role": "user", "content": [{"type": "text", "text": "先查"}]},
            {"role": "assistant", "content": [
                {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {}},
                {"type": "tool_use", "id": "call_2", "name": "get_date", "input": {}},
            ]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "call_1", "content": "晴", "is_error": false},
                {"type": "tool_result", "tool_use_id": "call_2", "content": "2026-09-24", "is_error": false},
                {"type": "text", "text": "再看这个"},
            ]},
        ])
    );
}

#[test]
fn tool_use_arguments_become_object() {
    rust_i18n::set_locale("zh");
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                r#"{"city":"北京","days":2}"#,
            ))],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    assert_eq!(
        body["messages"][0]["content"][0],
        json!({
            "type": "tool_use",
            "id": "call_1",
            "name": "get_weather",
            "input": {"city": "北京", "days": 2},
        })
    );

    let broken = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "{",
            ))],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &broken, false).unwrap_err();
    assert!(err.to_string().contains("不是合法 JSON"), "{err}");

    let not_object = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![ContentPart::ToolCall(ToolCall::new(
                "call_1",
                "get_weather",
                "[1,2]",
            ))],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &not_object, false).unwrap_err();
    assert!(err.to_string().contains("必须是 JSON 对象"), "{err}");
}

#[test]
fn reasoning_only_assistant_message_is_dropped() {
    let req = ChatRequest {
        messages: vec![
            Message::user_text("问题"),
            Message {
                role: Role::Assistant,
                content: crate::message_content![ContentPart::Reasoning(ReasoningBlock::text(
                    "只有思考"
                ))],
            },
            Message::user_text("继续"),
        ],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let body = encode_body(&config(), &req, false);
    // 空 assistant 消息在线上非法，直接丢弃；两侧 user 消息因此合并。
    assert_eq!(
        body["messages"],
        json!([{
            "role": "user",
            "content": [
                {"type": "text", "text": "问题"},
                {"type": "text", "text": "继续"},
            ],
        }])
    );
}

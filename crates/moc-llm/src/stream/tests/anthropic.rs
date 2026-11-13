use super::*;

#[tokio::test]
async fn anthropic_tool_input_survives_without_json_deltas() {
    use serde_json::json;
    for input in [json!({}), json!({"path":"README.md"})] {
        let response = decode_frames(Protocol::AnthropicMessages, &[
            json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"toolu_1", "name":"read", "input":input}}),
            json!({"type":"content_block_stop", "index":0}),
            json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}}),
            json!({"type":"message_stop"}),
        ]).await;
        let call = response.message.tool_calls().next().expect("工具调用存在");
        assert_eq!(call.arguments_json().unwrap(), input);
    }
}

#[tokio::test]
async fn anthropic_json_deltas_replace_initial_input() {
    use serde_json::json;
    let response = decode_frames(Protocol::AnthropicMessages, &[
        json!({"type":"content_block_start", "index":0, "content_block":{"type":"tool_use", "id":"toolu_1", "name":"read", "input":{}}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"{\"path\":"}}),
        json!({"type":"content_block_delta", "index":0, "delta":{"type":"input_json_delta", "partial_json":"\"README.md\"}"}}),
        json!({"type":"content_block_stop", "index":0}),
        json!({"type":"message_delta", "delta":{"stop_reason":"tool_use"}}),
        json!({"type":"message_stop"}),
    ]).await;
    assert_eq!(
        response.message.tool_calls().next().unwrap().arguments,
        r#"{"path":"README.md"}"#
    );
}

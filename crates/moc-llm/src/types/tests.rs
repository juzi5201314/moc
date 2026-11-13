use super::*;
use serde_json::json;

#[test]
fn tool_result_images_round_trip_and_default_when_absent() {
    use serde_json::json;
    let message = Message::tool_result_with_images(
        "call_1",
        "截图如下",
        false,
        vec![ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "AAAA".into(),
        }],
    );
    let encoded = serde_json::to_string(&message).unwrap();
    let decoded: Message = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, message);

    // 旧会话记录没有 images 字段：反序列化为空而不是报错。
    let legacy = json!({
        "role": "Tool",
        "content": [{"ToolResult": {
            "tool_call_id": "call_1",
            "content": "旧结果",
            "is_error": false,
        }}]
    });
    let decoded: Message = serde_json::from_value(legacy).unwrap();
    assert_eq!(decoded.tool_results().next().unwrap().images, Vec::new());
}

#[test]
fn message_text_concatenates_only_text_parts() {
    let message = Message {
        role: Role::Assistant,
        content: crate::message_content![
            ContentPart::Reasoning(ReasoningBlock::text("想一想")),
            ContentPart::Text("前半".into()),
            ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
            ContentPart::Text("后半".into()),
        ],
    };
    assert_eq!(message.text(), "前半后半");
    assert_eq!(message.tool_calls().count(), 1);
    let mut copy = message.clone();
    assert_eq!(
        message.content.as_ptr(),
        copy.content.as_ptr(),
        "只读克隆共享正文"
    );
    copy.content.push(ContentPart::Text("新尾部".into()));
    assert_eq!(message.text(), "前半后半", "修改克隆不能影响原历史");
    assert_eq!(copy.text(), "前半后半新尾部");
    let restored: Message =
        serde_json::from_value(serde_json::to_value(&message).unwrap()).unwrap();
    assert_eq!(restored, message, "磁盘与协议 JSON 形式保持兼容");
    let encoded = serde_json::to_vec(&message.content).unwrap();
    let content = MessageContent::from_archive(encoded.into()).unwrap();
    assert!(!content.archive_cache_loaded());
    assert_eq!(content.tool_call_ids().collect::<Vec<_>>(), ["call_1"]);
    assert_eq!(content.tool_result_ids().count(), 0);
    assert!(!content.archive_cache_loaded(), "读取工具 ID 不还原正文");
    assert_eq!(
        serde_json::to_value(&content).unwrap(),
        serde_json::to_value(&message.content).unwrap()
    );
    assert!(!content.archive_cache_loaded(), "序列化不永久加热旧正文");
    let mut archived = Message {
        role: message.role,
        content,
    };
    assert_eq!(archived.text(), message.text());
    assert!(
        !archived.content.archive_cache_loaded(),
        "派生文本不保留解码副本"
    );
    assert_eq!(archived.content.len(), 4);
    assert!(archived.content.archive_cache_loaded());
    let cold = archived.content.cold_clone();
    assert!(!cold.archive_cache_loaded());
    archived
        .content
        .push(ContentPart::Text("归档后修改".into()));
    assert!(!archived.content.is_archived());
    assert_eq!(cold, message.content);
    assert_eq!(archived.text(), "前半后半归档后修改");
    assert!(MessageContent::from_archive(bytes::Bytes::from_static(b"[{\"Text\":42}]")).is_err());
}

#[test]
fn usage_total_tokens_falls_back_to_sum() {
    let explicit = Usage {
        input_tokens: 10,
        output_tokens: 3,
        total_tokens: Some(999),
        ..Default::default()
    };
    assert_eq!(explicit.total_tokens(), 999);

    let derived = Usage {
        input_tokens: 10,
        output_tokens: 3,
        ..Default::default()
    };
    assert_eq!(derived.total_tokens(), 13);
}

#[test]
fn tool_call_arguments_json_parses_or_errors() {
    let call = ToolCall::new("call_1", "get_weather", r#"{"city":"北京"}"#);
    assert_eq!(call.arguments_json().unwrap(), json!({"city": "北京"}));

    let broken = ToolCall::new("call_2", "get_weather", r#"{"city":"#);
    let err = broken.arguments_json().unwrap_err();
    assert!(matches!(err, LlmError::Encode(_)), "{err:?}");
    assert!(err.to_string().contains("get_weather"), "{err}");
}

#[test]
fn reasoning_display_text_reads_summary_entries() {
    let plain = ReasoningBlock::Text {
        text: "思考中".into(),
        signature: Some("sig".into()),
    };
    assert_eq!(plain.display_text().as_deref(), Some("思考中"));

    let redacted = ReasoningBlock::Redacted {
        data: "opaque".into(),
    };
    assert_eq!(redacted.display_text(), None);

    let item = ReasoningBlock::Item(Box::new(
        (json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "blob",
            "summary": [
                {"type": "summary_text", "text": "第一段"},
                {"type": "summary_text", "text": "第二段"},
            ],
        }))
        .into(),
    ));
    assert_eq!(item.display_text().as_deref(), Some("第一段\n\n第二段"));

    let empty = ReasoningBlock::Item(Box::new(
        (json!({"type": "reasoning", "summary": []})).into(),
    ));
    assert_eq!(empty.display_text(), None);
}

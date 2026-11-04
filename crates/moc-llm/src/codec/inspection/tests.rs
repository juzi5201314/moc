use super::*;
use crate::{ImageSource, ToolCall, ToolResult};

#[test]
fn inspection_matches_wire_and_preserves_injection_coordinates() {
    for protocol in [
        Protocol::OpenAiChat,
        Protocol::OpenAiResponses,
        Protocol::AnthropicMessages,
    ] {
        let cfg = LlmConfig {
            protocol,
            ..Default::default()
        };
        let request = ChatRequest {
            max_output_tokens: Some(512),
            messages: vec![
                Message {
                    role: Role::System,
                    content: vec![
                        ContentPart::Text("默认提示".into()),
                        ContentPart::Text("工具规则".into()),
                    ],
                },
                Message::user_text("<system-reminder>用户自己写的标签</system-reminder>"),
                Message::custom("<system-reminder>真正的运行时提醒</system-reminder>"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall(ToolCall {
                        id: "call1".into(),
                        name: "read".into(),
                        arguments: "{}".into(),
                    })],
                },
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult(ToolResult {
                        tool_call_id: "call1".into(),
                        content: "结果".into(),
                        is_error: false,
                        images: vec![ImageSource::Url("https://example.com/image.png".into())],
                    })],
                },
            ],
            ..Default::default()
        };
        let wire = super::super::encode(&cfg, &request, true).unwrap();
        let inspection = inspect_request(&cfg, &request, true).unwrap();
        assert_eq!(
            inspection.body,
            serde_json::from_slice::<Value>(&wire.body).unwrap()
        );
        for source in &inspection.sources {
            let value = inspection
                .body
                .pointer(&source.pointer)
                .expect("每个来源坐标必须存在");
            if let Some(range) = &source.text_range {
                let text = value.as_str().expect("文本坐标指向字符串");
                let ContentPart::Text(expected) =
                    &request.messages[source.message_index].content[source.part_index]
                else {
                    panic!("文本来源")
                };
                assert_eq!(&text[range.clone()], expected);
            }
        }
        let user = inspection
            .sources
            .iter()
            .find(|s| s.message_index == 1 && s.text_range.is_some())
            .unwrap();
        let reminder = inspection
            .sources
            .iter()
            .find(|s| s.message_index == 2 && s.text_range.is_some())
            .unwrap();
        assert_ne!(user.pointer, reminder.pointer);
        if protocol == Protocol::AnthropicMessages {
            assert!(user.pointer.starts_with("/messages/0/content/0"));
            assert!(reminder.pointer.starts_with("/messages/0/content/1"));
        } else {
            assert!(
                inspection
                    .sources
                    .iter()
                    .filter(|s| s.message_index == 4 && s.text_range.is_none())
                    .count()
                    >= 2,
                "工具图片拆为额外条目仍保留来源"
            );
        }
    }
}

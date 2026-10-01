//! 图片 token 与请求体预算。
//!
//! 图片的 Base64 字节数不应直接当作 token；请求体又有独立于上下文窗口的
//! HTTP 大小上限。这里在构造请求的拥有副本上执行高低水位淘汰，不改写会话原始历史。

use moc_llm::{ContentPart, Message, ToolDef};
use serde::Serialize;
use serde_json::Value;

/// 没有供应商专属配置时采用的请求体上限。
const DEFAULT_MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;
/// 触发图片淘汰前为请求封套与工具定义预留的空间。
const TRIGGER_HEADROOM_BYTES: usize = 3 * 1024 * 1024;
/// 单张图片的上下文 token 估算，沿用 Grok Build 的低分辨率图片估算值。
pub const IMAGE_TOKEN_ESTIMATE: u64 = 765;
const USER_IMAGE_PLACEHOLDER: &str = "[较早图片已移除以满足请求大小限制；若仍需要，请重新提供图片]";
const TOOL_IMAGE_NOTE: &str = "[工具结果中的图片已移除以满足请求大小限制；不要根据记忆描述其内容]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudgetOutcome {
    pub body_bytes: usize,
    pub body_bytes_after: usize,
    pub evicted: usize,
}

/// 在请求副本上按旧到新的顺序移除图片，直到降到低水位。
///
/// 该接口只改变 `messages`，因此不会损坏可回放的 transcript；同样的输入会得到同样
/// 的输出，后续请求可以继续复用稳定前缀。若正文本身超过上限但没有图片，调用方仍会
/// 交给上游返回上下文或请求体错误。
pub fn apply(
    mut messages: Vec<Message>,
    tools: &[ToolDef],
    native_input: Option<&[Value]>,
    configured_max_bytes: Option<u64>,
) -> (Vec<Message>, ImageBudgetOutcome) {
    let max_bytes = configured_max_bytes
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > TRIGGER_HEADROOM_BYTES * 4)
        .unwrap_or(DEFAULT_MAX_REQUEST_BYTES);
    let trigger = max_bytes.saturating_sub(TRIGGER_HEADROOM_BYTES);
    let reclaim = max_bytes / 2;
    let fixed_bytes = serialized_bytes(tools).saturating_add(serialized_bytes(&native_input));
    let body_bytes = fixed_bytes.saturating_add(serialized_bytes(&messages));
    if body_bytes <= trigger {
        return (
            messages,
            ImageBudgetOutcome {
                body_bytes,
                body_bytes_after: body_bytes,
                evicted: 0,
            },
        );
    }

    let mut evicted = 0;
    for index in 0..messages.len() {
        evicted += strip_images(&mut messages[index]);
        if evicted > 0 && fixed_bytes.saturating_add(serialized_bytes(&messages)) <= reclaim {
            break;
        }
    }
    let body_bytes_after = fixed_bytes.saturating_add(serialized_bytes(&messages));
    (
        messages,
        ImageBudgetOutcome {
            body_bytes,
            body_bytes_after,
            evicted,
        },
    )
}

pub fn message_image_tokens(message: &Message) -> u64 {
    message
        .content
        .iter()
        .map(|part| match part {
            ContentPart::Image { .. } => IMAGE_TOKEN_ESTIMATE,
            ContentPart::ToolResult(result) => {
                (result.images.len() as u64).saturating_mul(IMAGE_TOKEN_ESTIMATE)
            }
            _ => 0,
        })
        .sum()
}

pub fn strip_images(message: &mut Message) -> usize {
    match message.role {
        moc_llm::Role::User => {
            let mut evicted = 0;
            for part in &mut message.content {
                if matches!(part, ContentPart::Image { .. }) {
                    *part = ContentPart::Text(USER_IMAGE_PLACEHOLDER.into());
                    evicted += 1;
                }
            }
            evicted
        }
        moc_llm::Role::Tool => {
            let mut evicted = 0;
            for part in &mut message.content {
                if let ContentPart::ToolResult(result) = part
                    && !result.images.is_empty()
                {
                    evicted += result.images.len();
                    result.images.clear();
                    if !result.content.contains(TOOL_IMAGE_NOTE) {
                        result.content.push('\n');
                        result.content.push_str(TOOL_IMAGE_NOTE);
                    }
                }
            }
            evicted
        }
        _ => 0,
    }
}

fn serialized_bytes<T: Serialize + ?Sized>(value: &T) -> usize {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.saturating_add(bytes.len());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let mut counter = Counter(0);
    let _ = serde_json::to_writer(&mut counter, value);
    counter.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_llm::ImageSource;

    #[test]
    fn image_budget_replaces_old_images_without_mutating_the_input_copy() {
        let image = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "x".repeat(50 * 1024 * 1024),
        };
        let messages = vec![Message {
            role: moc_llm::Role::User,
            content: vec![ContentPart::Image {
                source: image,
                detail: None,
            }],
        }];
        let (messages, outcome) = apply(messages, &[], None, Some(8 * 1024 * 1024));
        assert_eq!(outcome.evicted, 1);
        assert!(outcome.body_bytes_after < outcome.body_bytes);
        assert!(matches!(messages[0].content[0], ContentPart::Text(_)));
    }

    #[test]
    fn image_tokens_are_fixed_independent_of_base64_length() {
        let short = Message {
            role: moc_llm::Role::User,
            content: vec![ContentPart::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "x".into(),
                },
                detail: None,
            }],
        };
        let long = Message {
            role: moc_llm::Role::User,
            content: vec![ContentPart::Image {
                source: ImageSource::Base64 {
                    media_type: "image/png".into(),
                    data: "x".repeat(100_000),
                },
                detail: None,
            }],
        };
        assert_eq!(message_image_tokens(&short), IMAGE_TOKEN_ESTIMATE);
        assert_eq!(message_image_tokens(&long), IMAGE_TOKEN_ESTIMATE);
    }
}

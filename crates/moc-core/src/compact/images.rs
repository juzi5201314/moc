//! 图片的 token 估算与请求体大小预算。
//!
//! 图片的 Base64 字节数不能直接当 token 算；请求体还有一个与上下文窗口无关的 HTTP
//! 大小上限。这里在构造请求用的副本上按高低水位淘汰图片，不改动会话的原始历史。

use moc_llm::{ContentPart, Message, ToolDef};
use serde::Serialize;
use serde_json::Value;

/// 供应商没有单独配置时使用的请求体上限。
const DEFAULT_MAX_REQUEST_BYTES: usize = 50 * 1024 * 1024;
/// 触发图片淘汰前，为请求外层结构和工具定义预留的空间。
const TRIGGER_HEADROOM_BYTES: usize = 3 * 1024 * 1024;
/// 单张图片的上下文 token 估算，沿用 Grok Build 对低分辨率图片的估算值。
pub const IMAGE_TOKEN_ESTIMATE: u64 = 765;
const USER_IMAGE_PLACEHOLDER: &str =
    "[Earlier image removed to meet the request size limit; provide it again if needed]";
const TOOL_IMAGE_NOTE: &str =
    "[Image in tool result removed to meet the request size limit; do not describe it from memory]";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudgetOutcome {
    pub body_bytes: usize,
    pub body_bytes_after: usize,
    pub evicted: usize,
}

/// 在请求副本上从旧到新移除图片，直到体积降到低水位。
///
/// 只改动 `messages`，所以不会破坏可回放的 transcript；相同输入得到相同输出，后续请求
/// 仍能复用稳定前缀。体积只完整计算一次，之后按被改动消息的差值增量更新，不会每一步都
/// 重新序列化整个消息列表。如果正文本身超限但没有图片，照样交给上游，由上游返回上下文
/// 或请求体错误。
pub fn apply(
    mut messages: Vec<Message>,
    tools: &[ToolDef],
    native_input: Option<&[Value]>,
    configured_max_bytes: Option<u64>,
) -> (Vec<Message>, ImageBudgetOutcome) {
    // 配置的上限原样生效：小于触发余量的上限会让图片全部被淘汰（0 已在校验时拒绝）。
    // 悄悄退回默认值会让用户设置的请求体上限失效。
    let max_bytes = configured_max_bytes
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(DEFAULT_MAX_REQUEST_BYTES);
    let trigger = max_bytes.saturating_sub(TRIGGER_HEADROOM_BYTES);
    let reclaim = max_bytes / 2;
    let fixed_bytes = serialized_bytes(tools).saturating_add(serialized_bytes(&native_input));
    let body_bytes =
        fixed_bytes.saturating_add(messages.iter().map(serialized_bytes).sum::<usize>());
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

    let mut total_bytes = body_bytes;
    let mut evicted = 0;
    for index in 0..messages.len() {
        if !has_images(&messages[index]) {
            continue;
        }
        let before = serialized_bytes(&messages[index]);
        evicted += strip_images(&mut messages[index]);
        let after = serialized_bytes(&messages[index]);
        total_bytes = total_bytes.saturating_sub(before).saturating_add(after);
        if total_bytes <= reclaim {
            break;
        }
    }
    (
        messages,
        ImageBudgetOutcome {
            body_bytes,
            body_bytes_after: total_bytes,
            evicted,
        },
    )
}

/// 消息是否带图片；不带图片的消息跳过序列化。
fn has_images(message: &Message) -> bool {
    message.content.iter().any(|part| match part {
        ContentPart::Image { .. } => true,
        ContentPart::ToolResult(result) => !result.images.is_empty(),
        _ => false,
    })
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

#[cfg(test)]
thread_local! {
    /// 测试用的序列化次数计数，保证「体积只完整计算一次、之后按差值增量更新」不被改坏。
    static SERIALIZED_CALLS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn serialized_bytes<T: Serialize + ?Sized>(value: &T) -> usize {
    #[cfg(test)]
    SERIALIZED_CALLS.with(|calls| calls.set(calls.get() + 1));
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
    fn configured_limit_below_the_trigger_headroom_is_honored() {
        let image = ImageSource::Base64 {
            media_type: "image/png".into(),
            data: "x".repeat(5 * 1024 * 1024),
        };
        let messages = vec![Message {
            role: moc_llm::Role::User,
            content: vec![ContentPart::Image {
                source: image,
                detail: None,
            }],
        }];
        // 4 MiB 上限低于默认值：必须按配置淘汰，不能悄悄退回 50 MiB。
        let (messages, outcome) = apply(messages, &[], None, Some(4 * 1024 * 1024));
        assert_eq!(outcome.evicted, 1);
        assert!(outcome.body_bytes > 4 * 1024 * 1024);
        assert!(outcome.body_bytes_after < outcome.body_bytes);
        assert!(matches!(messages[0].content[0], ContentPart::Text(_)));
    }

    #[test]
    fn image_eviction_stops_at_the_low_watermark() {
        let messages: Vec<Message> = (0..10)
            .map(|_| Message {
                role: moc_llm::Role::User,
                content: vec![ContentPart::Image {
                    source: ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "y".repeat(1024 * 1024),
                    },
                    detail: None,
                }],
            })
            .collect();
        let (messages, outcome) = apply(messages, &[], None, Some(8 * 1024 * 1024));
        assert!(
            outcome.evicted > 0 && outcome.evicted < 10,
            "够用就停，实际淘汰了 {} 张",
            outcome.evicted
        );
        assert!(outcome.body_bytes_after <= 4 * 1024 * 1024 + 4096);
        assert!(
            messages
                .iter()
                .any(|message| matches!(message.content[0], ContentPart::Image { .. })),
            "较新的图片必须保留"
        );
    }

    #[test]
    fn image_free_bodies_are_serialized_once_per_message() {
        let messages: Vec<Message> = (0..200)
            .map(|_| Message::user_text("z".repeat(64 * 1024)))
            .collect();
        let count = messages.len();
        SERIALIZED_CALLS.with(|calls| calls.set(0));
        let (_, outcome) = apply(messages, &[], None, Some(4 * 1024 * 1024));
        assert_eq!(outcome.evicted, 0);
        assert_eq!(outcome.body_bytes, outcome.body_bytes_after);
        // 额外两次是请求封套（工具定义与原生载荷）；旧实现会跑满全表 O(n²)。
        let calls = SERIALIZED_CALLS.with(std::cell::Cell::get);
        assert!(
            calls <= count + 4,
            "无图片时每条消息只允许序列化一次，实际 {calls} 次"
        );
    }
}

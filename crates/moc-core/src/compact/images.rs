//! 图片 token 估算与显式历史归档的图片清除。
//!
//! HTTP 请求图片压缩与字节预算统一由 moc-llm 发送边界处理；这里不再按估算体积静默删图。
use moc_llm::{ContentPart, Message};

/// 沿用 Grok Build 对低分辨率图片的 token 估算值。
pub const IMAGE_TOKEN_ESTIMATE: u64 = 765;
const USER_IMAGE_PLACEHOLDER: &str =
    "[Earlier image removed during history compaction; provide it again if needed]";
const TOOL_IMAGE_NOTE: &str =
    "[Image in tool result removed during history compaction; do not describe it from memory]";

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
mod tests {
    use super::*;

    #[test]
    fn explicit_compaction_keeps_text_and_marks_removed_images() {
        let source = moc_llm::ImageSource::Url("https://example.com/image.png".into());
        let mut message = Message::tool_result_with_images("id", "正文", false, vec![source]);
        let original = message.clone();
        assert_eq!(strip_images(&mut message), 1);
        assert_eq!(strip_images(&mut message), 0);
        assert_eq!(original.tool_results().next().unwrap().images.len(), 1);
        let result = message.tool_results().next().unwrap();
        assert!(result.content.starts_with("正文"));
        assert!(result.content.contains("history compaction"));
    }
}

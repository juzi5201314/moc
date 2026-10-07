//! MCP `tools/call` 结果到 moc 富内容表示的映射。
//!
//! 文本与图片是模型可直接消费的形态；音频与内嵌资源降级为带元信息的文本占位，
//! 与 moc-llm「core 负责把多模态结果转写」的既有约定一致。

use rmcp::model::{CallToolResult, ContentBlock, ResourceContents};

/// 内联图片载荷；仅保留主流视觉模型支持的格式，其余降级为文本占位。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub media_type: String,
    /// 不含 `data:` 前缀的 base64 载荷。
    pub data: String,
}

impl ImageData {
    pub fn is_supported(media_type: &str) -> bool {
        matches!(
            media_type,
            "image/png" | "image/jpeg" | "image/webp" | "image/gif"
        )
    }
}

/// 一次成功送达的调用输出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    /// 全部文本块按序以换行连接，占位文本混排其中。
    pub text: String,
    /// 服务端返回的原始结构化结果，保持 JSON 类型与形状。
    pub structured_content: Option<serde_json::Value>,
    /// 图片块（PNG/JPEG/WebP/GIF）。
    pub images: Vec<ImageData>,
    /// 服务端声明的执行失败（`isError: true`）；内容仍返回给模型以便自纠。
    pub is_error: bool,
}

/// 映射一次调用结果。
pub fn map_call_result(result: &CallToolResult) -> ToolOutput {
    let mut lines: Vec<String> = Vec::new();
    let mut images = Vec::new();
    for block in &result.content {
        match block {
            ContentBlock::Text(text) => {
                lines.push(text.text.clone());
            }
            ContentBlock::Image(image) => {
                if ImageData::is_supported(&image.mime_type) {
                    images.push(ImageData {
                        media_type: image.mime_type.clone(),
                        data: image.data.clone(),
                    });
                } else {
                    lines.push(format!(
                        "[图片 {} {} 字节（base64），格式不受支持，已省略]",
                        image.mime_type,
                        image.data.len()
                    ));
                }
            }
            ContentBlock::Audio(audio) => {
                lines.push(format!(
                    "[音频 {} {} 字节（base64），音频播放不受支持]",
                    audio.mime_type,
                    audio.data.len()
                ));
            }
            ContentBlock::ResourceLink(link) => {
                lines.push(format!("[资源链接：{} ({})]", link.name, link.uri));
            }
            ContentBlock::Resource(embedded) => match &embedded.resource {
                ResourceContents::TextResourceContents { uri, text, .. } => {
                    lines.push(format!("[内嵌资源 {uri}]\n{text}"));
                }
                ResourceContents::BlobResourceContents {
                    uri,
                    mime_type,
                    blob,
                    ..
                } => lines.push(format!(
                    "[内嵌资源 {uri}（{}，{} 字节 blob），二进制内容已省略]",
                    mime_type.as_deref().unwrap_or("未知类型"),
                    blob.len()
                )),
                // rmcp 标注 non_exhaustive：未来新增的资源形态按未知占位处理。
                _ => {}
            },
            // rmcp 标注 non_exhaustive：未知内容块直接忽略。
            _ => {}
        }
    }
    ToolOutput {
        text: lines.join("\n"),
        structured_content: result.structured_content.clone(),
        images,
        is_error: result.is_error == Some(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{Resource, ResourceContents};
    use serde_json::json;

    #[test]
    fn joins_text_and_collects_supported_images() {
        let result = CallToolResult::success(vec![
            ContentBlock::text("第一行"),
            ContentBlock::image("aGVsbG8=", "image/png"),
            ContentBlock::image("eHVl", "image/x-icon"),
            ContentBlock::text("最后一行"),
        ]);
        let output = map_call_result(&result);
        assert_eq!(
            output.text,
            "第一行\n[图片 image/x-icon 4 字节（base64），格式不受支持，已省略]\n最后一行"
        );
        assert_eq!(
            output.images,
            vec![ImageData {
                media_type: "image/png".into(),
                data: "aGVsbG8=".into()
            }]
        );
        assert!(!output.is_error);
    }

    #[test]
    fn preserves_structured_content_and_marks_errors() {
        let mut result = CallToolResult::success(vec![ContentBlock::text("失败原因")]);
        result.structured_content = Some(json!({"count": 3}));
        result.is_error = Some(true);
        let output = map_call_result(&result);
        assert!(output.is_error);
        assert_eq!(output.text, "失败原因");
        assert_eq!(output.structured_content, Some(json!({"count": 3})));
    }

    #[test]
    fn demotes_audio_and_resources_to_text_placeholders() {
        let result = CallToolResult::success(vec![
            ContentBlock::audio("QUFB", "audio/wav"),
            ContentBlock::resource_link(Resource::new("file:///tmp/a.bin", "a.bin")),
            ContentBlock::resource(ResourceContents::TextResourceContents {
                uri: "file:///tmp/note.md".into(),
                mime_type: Some("text/markdown".into()),
                text: "正文内容".into(),
                meta: None,
            }),
        ]);
        let output = map_call_result(&result);
        assert!(output.text.contains("音频 audio/wav"));
        assert!(output.text.contains("资源链接：a.bin (file:///tmp/a.bin)"));
        assert!(output.text.contains("正文内容"));
        assert!(output.images.is_empty());
    }
}

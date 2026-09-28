//! 输入投影、推理档位与稳定缓存断点。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use moc_llm::{
    CacheConfig, ChatRequest, ContentPart, Message, ReasoningConfig, ReasoningEcho, RequestOptions,
    Role,
};
use moc_protocol::methods::InputPart;
use serde_json::Value;

pub(super) async fn project_input(
    input: Vec<InputPart>,
    accepts_image: bool,
    cwd: &Path,
) -> Result<Message, crate::CoreError> {
    let mut content = Vec::with_capacity(input.len());
    for part in input {
        match part {
            InputPart::Text { text } if accepts_image => {
                append_text_with_images(&mut content, &text, cwd).await?;
            }
            InputPart::Text { text } => content.push(ContentPart::Text(text)),
            InputPart::Image { source, detail } if accepts_image => {
                content.push(ContentPart::Image {
                    source: normalize_image_source(source, cwd).await?,
                    detail,
                });
            }
            InputPart::Image { .. } => content.push(ContentPart::Text(
                "[用户提供了图片（当前模型不支持图片输入）]".into(),
            )),
        }
    }
    Ok(Message {
        role: Role::User,
        content,
    })
}
async fn normalize_image_source(
    source: moc_llm::ImageSource,
    cwd: &Path,
) -> Result<moc_llm::ImageSource, crate::CoreError> {
    match source {
        moc_llm::ImageSource::Url(url) if url.starts_with('@') => load_local_image(&url, cwd)
            .await?
            .ok_or_else(|| crate::CoreError::Tool(format!("图片文件不存在：{}", &url[1..]))),
        source => Ok(source),
    }
}

async fn append_text_with_images(
    content: &mut Vec<ContentPart>,
    text: &str,
    cwd: &Path,
) -> Result<(), crate::CoreError> {
    let mut cursor = 0;
    let mut index = 0;
    while index < text.len() {
        while index < text.len() && text.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        let start = index;
        while index < text.len() && !text.as_bytes()[index].is_ascii_whitespace() {
            index += 1;
        }
        let end = index;
        if start == end || !text[start..end].starts_with('@') {
            continue;
        }
        let Some(image) = load_local_image(&text[start..end], cwd).await? else {
            continue;
        };
        push_text(content, &text[cursor..start]);
        content.push(ContentPart::Image {
            source: image,
            detail: None,
        });
        cursor = end;
    }
    push_text(content, &text[cursor..]);
    Ok(())
}

fn push_text(content: &mut Vec<ContentPart>, text: &str) {
    if !text.is_empty() {
        content.push(ContentPart::Text(text.to_owned()));
    }
}

async fn load_local_image(
    token: &str,
    cwd: &Path,
) -> Result<Option<moc_llm::ImageSource>, crate::CoreError> {
    let raw_path = token.strip_prefix('@').unwrap_or_default();
    let Some(media_type) = image_media_type(raw_path) else {
        return Ok(None);
    };
    let path = resolve_image_path(raw_path, cwd);
    let bytes = match tokio::fs::read(&path).await {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == ErrorKind::NotFound => {
            return Err(crate::CoreError::Tool(format!(
                "图片文件不存在：{}",
                path.display()
            )));
        }
        Err(error) => {
            return Err(crate::CoreError::Tool(format!(
                "读取图片 {} 失败：{error}",
                path.display()
            )));
        }
    };
    Ok(Some(moc_llm::ImageSource::Base64 {
        media_type: media_type.to_owned(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    }))
}

fn resolve_image_path(raw_path: &str, cwd: &Path) -> PathBuf {
    let path = crate::config::expand_user_path(raw_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
}

fn image_media_type(path: &str) -> Option<&'static str> {
    match Path::new(path)
        .extension()
        .and_then(|extension| extension.to_str())
        .map(|extension| extension.to_ascii_lowercase())
        .as_deref()
    {
        Some("png") => Some("image/png"),
        Some("jpg" | "jpeg") => Some("image/jpeg"),
        Some("gif") => Some("image/gif"),
        Some("webp") => Some("image/webp"),
        Some("bmp") => Some("image/bmp"),
        Some("svg") => Some("image/svg+xml"),
        Some("avif") => Some("image/avif"),
        Some("heic") => Some("image/heic"),
        Some("heif") => Some("image/heif"),
        _ => None,
    }
}

pub(super) fn build_request(
    model: &crate::ResolvedModel,
    _config: &crate::ResolvedConfig,
    messages: Vec<Message>,
    tools: Vec<moc_llm::ToolDef>,
    native_input: Option<Vec<Value>>,
    session_id: &str,
) -> ChatRequest {
    let echo_reasoning = if model.requires_reasoning_content_for_tool_calls
        || model.requires_reasoning_content_for_all_assistant_turns
    {
        ReasoningEcho::Always
    } else {
        model.echo_reasoning
    };
    let mut breakpoints = vec![0];
    if messages.len() > 1 {
        breakpoints.push(messages.len() - 1);
    }
    ChatRequest {
        messages,
        native_input,
        tools,
        tool_choice: None,
        max_output_tokens: Some(model.max_output_tokens),
        temperature: None,
        top_p: None,
        top_k: None,
        stop: Vec::new(),
        reasoning: Some(ReasoningConfig {
            enabled: model.capabilities.reasoning
                && model.thinking_level != crate::ThinkingLevel::Off,
            effort: model.thinking_level.effort(),
            summary: None,
        }),
        response_format: None,
        parallel_tool_calls: Some(model.llm.wire.parallel_tool_calls),
        options: RequestOptions {
            echo_reasoning,
            cache: (model.supports_prompt_cache_key
                || model.llm.protocol == moc_llm::Protocol::AnthropicMessages)
                .then(|| CacheConfig {
                    key: Some(session_id.to_owned()),
                    breakpoints,
                    ttl: None,
                }),
            ..RequestOptions::default()
        },
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use moc_llm::ImageSource;

    #[tokio::test]
    async fn unsupported_images_become_text_and_local_paths_become_base64() {
        let image = || InputPart::Image {
            source: ImageSource::Url("https://example.invalid/image.png".into()),
            detail: None,
        };
        let unsupported = project_input(vec![image()], false, Path::new("/tmp"))
            .await
            .expect("图片投影成功");
        assert_eq!(
            unsupported.text(),
            "[用户提供了图片（当前模型不支持图片输入）]"
        );
        let supported = project_input(vec![image()], true, Path::new("/tmp"))
            .await
            .expect("图片投影成功");
        assert!(matches!(supported.content[0], ContentPart::Image { .. }));

        let dir = tempfile::tempdir().expect("临时目录");
        tokio::fs::write(dir.path().join("shot.png"), b"png-bytes")
            .await
            .expect("写入图片");
        let local = project_input(
            vec![InputPart::Text {
                text: "请查看 @shot.png".into(),
            }],
            true,
            dir.path(),
        )
        .await
        .expect("本地图片投影成功");
        let ContentPart::Image {
            source: ImageSource::Base64 {
                media_type, data, ..
            },
            ..
        } = &local.content[1]
        else {
            panic!("本地图片应转换为 Base64");
        };
        assert_eq!(media_type, "image/png");
        assert_eq!(data, "cG5nLWJ5dGVz");
    }
}

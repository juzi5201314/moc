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

pub(in crate::service) async fn project_input(
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

pub(crate) fn build_request(
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
    let (messages, image_budget) = crate::compact::images::apply(
        messages,
        &tools,
        native_input.as_deref(),
        model.max_request_bytes,
    );
    if image_budget.evicted > 0 {
        tracing::info!(
            body_bytes = image_budget.body_bytes,
            body_bytes_after = image_budget.body_bytes_after,
            evicted = image_budget.evicted,
            "请求图片已按字节预算淘汰"
        );
    }
    let message_count = messages.len();
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
            cache: cache_options(model, message_count, session_id),
            ..RequestOptions::default()
        },
    }
}

pub(crate) fn cache_options(
    model: &crate::ResolvedModel,
    message_count: usize,
    session_id: &str,
) -> Option<CacheConfig> {
    use crate::config::model::CachePolicy;
    let anthropic = model.llm.protocol == moc_llm::Protocol::AnthropicMessages;
    if model.cache == CachePolicy::Off || (!model.supports_prompt_cache_key && !anthropic) {
        return None;
    }
    let breakpoints = if anthropic {
        let mut points = vec![0];
        for index in message_count.saturating_sub(2)..message_count {
            if index != 0 {
                points.push(index);
            }
        }
        points
    } else {
        Vec::new()
    };
    Some(CacheConfig {
        key: (!anthropic).then(|| session_id.to_owned()),
        breakpoints,
        ttl: match (anthropic, model.cache) {
            (true, CachePolicy::Long) => Some(moc_llm::CacheTtl::OneHour),
            (true, CachePolicy::Short) => Some(moc_llm::CacheTtl::FiveMinutes),
            _ => None,
        },
    })
}

/// 比对上游回显与本地请求，列出所有不一致的字段。
///
/// 只比较**双方都给出**的字段：上游没回显（`None` 或空白）不产生结论，避免把
/// 网关的沉默误报成改写。模型 id 逐字（去首尾空白后）比较——网关降级、别名映射、
/// 混淆 id 解包正是要暴露的目标；推理强度按 ASCII 大小写不敏感比较，因为上游
/// 常把强度归一化成小写。
pub(in crate::service) fn upstream_mismatches(
    model: &crate::ResolvedModel,
    request: &ChatRequest,
    response: &moc_llm::ChatResponse,
) -> Vec<moc_protocol::methods::UpstreamFieldMismatch> {
    use moc_protocol::methods::{UpstreamField, UpstreamFieldMismatch};

    let mut mismatches = Vec::new();
    let reported_model = response
        .model
        .as_deref()
        .map(str::trim)
        .filter(|model| !model.is_empty());
    if let Some(reported) = reported_model
        && reported != model.model_id
    {
        mismatches.push(UpstreamFieldMismatch {
            field: UpstreamField::Model,
            requested: model.model_id.clone(),
            reported: reported.to_owned(),
        });
    }
    let reported_effort = response
        .reasoning_effort
        .as_deref()
        .map(str::trim)
        .filter(|effort| !effort.is_empty());
    if let (Some(requested), Some(reported)) = (
        model.llm.wire_effort(request.reasoning.as_ref()),
        reported_effort,
    ) && !requested.eq_ignore_ascii_case(reported)
    {
        mismatches.push(UpstreamFieldMismatch {
            field: UpstreamField::Effort,
            requested,
            reported: reported.to_owned(),
        });
    }
    mismatches
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

//! 构建请求：输入消息、推理等级和稳定的缓存断点。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use moc_llm::{
    CacheConfig, ChatRequest, ContentPart, Message, ReasoningConfig, ReasoningEcho, RequestOptions,
    Role,
};
use moc_protocol::methods::InputPart;

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
                "[The user provided an image, but the current model does not support image input]"
                    .into(),
            )),
        }
    }
    Ok(Message {
        role: Role::User,
        content: content.into(),
    })
}
async fn normalize_image_source(
    source: moc_llm::ImageSource,
    cwd: &Path,
) -> Result<moc_llm::ImageSource, crate::CoreError> {
    match source {
        moc_llm::ImageSource::Url(url) if url.starts_with('@') => load_local_image(&url, cwd)
            .await?
            .ok_or_else(|| crate::CoreError::Tool(format!("image file not found: {}", &url[1..]))),
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
                "image file not found: {}",
                path.display()
            )));
        }
        Err(error) => {
            return Err(crate::CoreError::Tool(format!(
                "failed to read image {}: {error}",
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

/// 缓存断点的位置信息，由提示编排给出，编码器据此放置断点。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CacheAnchors<'a> {
    /// 每条指令消息开头的静态块数。请求开头的几条消息就是指令消息，下标一一对应；
    /// `0` 表示该消息没有静态前缀。
    pub instruction_static: &'a [usize],
    /// 追加在请求末尾、不进入会话历史的提醒消息条数。
    ///
    /// 提醒下一轮不会出现在同一位置，放在它上面的断点永远不会命中，所以尾部断点
    /// 要放在最后一条会进入历史的 IR 消息上。
    pub trailing_reminders: usize,
}

pub(crate) fn build_request(
    model: &crate::ResolvedModel,
    config: &crate::ResolvedConfig,
    messages: Vec<Message>,
    tools: Vec<moc_llm::ToolDef>,
    session_id: &str,
    anchors: CacheAnchors<'_>,
) -> ChatRequest {
    let echo_reasoning = if model.requires_reasoning_content_for_tool_calls
        || model.requires_reasoning_content_for_all_assistant_turns
    {
        ReasoningEcho::Always
    } else {
        model.echo_reasoning
    };
    let message_count = messages.len();
    ChatRequest {
        messages,
        tools,
        tool_choice: None,
        max_output_tokens: Some(model.max_output_tokens),
        temperature: None,
        top_p: None,
        top_k: None,
        stop: Vec::new(),
        reasoning: model.capabilities.reasoning.then(|| ReasoningConfig {
            enabled: model.thinking_level != crate::ThinkingLevel::Off,
            effort: model.thinking_level.effort(),
            summary: Some(crate::config::parameters::summary(
                model.llm.protocol,
                &model.llm.extra_body,
            )),
        }),
        response_format: None,
        parallel_tool_calls: Some(
            model
                .llm
                .extra_body
                .get("parallel_tool_calls")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(model.llm.wire.parallel_tool_calls),
        ),
        options: RequestOptions {
            echo_reasoning,
            cache: cache_options(model, message_count, anchors, session_id),
            stream_guard: config.limits.stream_guard(),
            connect_deadline_secs: config.limits.connect_deadline_secs,
            max_request_bytes: model.max_request_bytes,
            ..RequestOptions::default()
        },
    }
}

pub(crate) fn cache_options(
    model: &crate::ResolvedModel,
    message_count: usize,
    anchors: CacheAnchors,
    session_id: &str,
) -> Option<CacheConfig> {
    use crate::config::model::CachePolicy;
    let anthropic = model.llm.protocol == moc_llm::Protocol::AnthropicMessages;
    if model.cache == CachePolicy::Off || (!model.supports_prompt_cache_key && !anthropic) {
        return None;
    }
    let (breakpoints, instruction_blocks) = if anthropic {
        // 第一个断点：整段指令。Anthropic 的最小缓存单元是「系统提示 + 工具定义」。
        let mut points = vec![0];
        // 尾部断点：最后一条会进入历史的 IR 消息。不能放在本轮提醒上，否则永远不会命中。
        let tail = message_count.saturating_sub(1 + anchors.trailing_reminders);
        if tail != 0 && tail < message_count {
            points.push(tail);
        }
        // 静态前缀断点：与工作目录无关的部分单独设断点，跨目录、跨会话复用。
        // 仅取最后一个静态前缀，给系统边界和历史尾部保留断点额度。
        let blocks = anchors
            .instruction_static
            .iter()
            .enumerate()
            .rev()
            .find(|(_, count)| **count > 0)
            .map(|(message, count)| {
                vec![moc_llm::InstructionBlock {
                    message,
                    block: *count - 1,
                }]
            })
            .unwrap_or_default();
        (points, blocks)
    } else {
        (Vec::new(), Vec::new())
    };
    Some(CacheConfig {
        key: (!anthropic).then(|| {
            model
                .llm
                .extra_body
                .get("prompt_cache_key")
                .and_then(serde_json::Value::as_str)
                .unwrap_or(session_id)
                .to_owned()
        }),
        breakpoints,
        instruction_blocks,
        ttl: match (anthropic, model.cache) {
            (true, CachePolicy::Long) => Some(moc_llm::CacheTtl::OneHour),
            (true, CachePolicy::Short) => Some(moc_llm::CacheTtl::FiveMinutes),
            _ => None,
        },
    })
}

/// 比较上游回显与本地请求，列出所有不一致的字段。
///
/// 只比较**两边都有值**的字段：上游没回显（`None` 或空白）就不下结论，以免把网关
/// 不回显误报成改写。模型 id 去掉首尾空白后逐字比较，网关降级、别名映射、混淆 id
/// 解包正是要发现的情况；推理强度按 ASCII 忽略大小写比较，因为上游常把它转成小写。
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

    #[test]
    fn configured_api_parameters_reach_requests_and_model_overrides_preserve_siblings() {
        use serde_json::json;
        for (api, provider_body, model_body, expected) in [
            (
                "openai_responses",
                json!({"reasoning":{"summary":"detailed","context":"all_turns","effort":"medium"},"text":{"verbosity":"low"},"prompt_cache_key":"shared"}),
                json!({"reasoning":{"mode":"pro"},"parallel_tool_calls":false}),
                json!({"reasoning":{"summary":"detailed","context":"all_turns","mode":"pro","effort":"high"}}),
            ),
            (
                "anthropic_messages",
                json!({"thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"},"output_config":{"effort":"medium"},"metadata":{"user_id":"opaque"}}),
                json!({"temperature":0.5,"top_k":20}),
                json!({"thinking":{"type":"enabled","budget_tokens":2048,"display":"omitted"}}),
            ),
            (
                "openai_chat",
                json!({"reasoning_effort":"medium","frequency_penalty":-0.5,"tool_choice":"none","stream_options":{"include_usage":false}}),
                json!({"seed":-42,"stop":["END"],"parallel_tool_calls":false}),
                json!({"frequency_penalty":-0.5,"seed":-42,"stop":["END"],"tool_choice":"none","stream_options":{"include_usage":false}}),
            ),
        ] {
            let directory = tempfile::tempdir().expect("临时目录");
            let boot = crate::CoreConfig {
                home: directory.path().join("home"),
                cwd: directory.path().join("repo"),
                ..Default::default()
            };
            std::fs::create_dir_all(&boot.home).expect("创建配置目录");
            std::fs::create_dir_all(&boot.cwd).expect("创建工作区");
            let view =
                crate::config::editor::inspect(&boot, moc_protocol::ConfigScope::Global, &boot.cwd)
                    .expect("读取配置");
            let config = crate::config::editor::save(
                &boot,
                moc_protocol::ConfigScope::Global,
                &boot.cwd,
                json!({"providers":{"p":{"api":api,"base_url":"https://example.invalid","headers":{"X-Shared":"provider","x-provider":"keep"},"extra_body":provider_body,"models":{"m":{"reasoning":true,"compat":{"headers":{"x-shared":"model"},"extra_body":model_body}}}}}}),
                &view.revision,
            ).expect("保存参数");
            assert_eq!(
                config
                    .resolve_model("p/m")
                    .expect("默认模型")
                    .thinking_level,
                crate::ThinkingLevel::Medium
            );
            let model = config.resolve_model("p/m:high").expect("解析模型");
            assert_eq!(model.llm.headers["x-shared"], "model");
            assert_eq!(model.llm.headers["x-provider"], "keep");
            let request = build_request(
                &model,
                &config,
                vec![Message::user_text("问题")],
                vec![moc_llm::ToolDef {
                    name: "read".into(),
                    description: Some("Read a file".into()),
                    parameters: json!({"type":"object"}),
                    strict: None,
                }],
                "session",
                CacheAnchors::default(),
            );
            let body = moc_llm::inspect_request(&model.llm, &request, true)
                .expect("编码请求")
                .body;
            for (key, value) in expected.as_object().expect("预期对象") {
                assert_eq!(&body[key], value, "{api}: {key}");
            }
            if api == "openai_responses" {
                assert_eq!(body["text"]["verbosity"], "low");
                assert_eq!(body["prompt_cache_key"], "shared");
            }
            if api != "anthropic_messages" {
                assert_eq!(body["parallel_tool_calls"], false);
            }
            let off_model = config.resolve_model("p/m:off").expect("关闭思考");
            let off_request = build_request(
                &off_model,
                &config,
                vec![Message::user_text("问题")],
                Vec::new(),
                "session",
                CacheAnchors::default(),
            );
            let off = moc_llm::inspect_request(&off_model.llm, &off_request, true)
                .expect("编码关闭思考的请求")
                .body;
            match api {
                "openai_responses" => {
                    assert_eq!(off["reasoning"]["effort"], "none");
                    assert!(off["reasoning"].get("summary").is_none());
                }
                "anthropic_messages" => {
                    assert_eq!(off["thinking"], json!({"type":"disabled"}));
                    assert!(
                        off.get("output_config")
                            .and_then(|v| v.get("effort"))
                            .is_none()
                    );
                }
                _ => assert_eq!(off["reasoning_effort"], "none"),
            }
        }
    }
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
            "[The user provided an image, but the current model does not support image input]"
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

    #[test]
    fn anthropic_cache_breakpoints_skip_tail_reminders_and_lift_static_prefix() {
        let directory = tempfile::tempdir().expect("临时目录");
        let cwd = directory.path().join("repo");
        let home = directory.path().join("home");
        std::fs::create_dir_all(&cwd).expect("创建仓库");
        std::fs::create_dir_all(&home).expect("创建 home");
        std::fs::write(
            home.join("config.toml"),
            "[models.roles]\ndefault='p/model'\nexplore='p/model'\nplan='p/model'\n[providers.p]\napi='anthropic_messages'\nbase_url='https://example.invalid'\ncache='short'\n[providers.p.models.model]\n",
        )
        .expect("写配置");
        let core = crate::CoreConfig {
            home: home.clone(),
            cwd: cwd.clone(),
            user_home: Some(directory.path().join("userhome")),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
            ptc_worker: None,
        };
        let config = crate::ResolvedConfig::load(&core)
            .expect("载入配置")
            .for_project(&cwd)
            .expect("解析项目");
        let model = config.resolve_model("p/model").expect("解析模型");
        // 7 条消息，其中最后 1 条是本轮的尾部提醒：断点必须退回第 5 条。
        let options = cache_options(
            &model,
            7,
            CacheAnchors {
                instruction_static: &[3],
                trailing_reminders: 1,
            },
            "session-1",
        )
        .expect("缓存配置");
        assert_eq!(options.breakpoints, vec![0, 5]);
        assert_eq!(
            options.instruction_blocks,
            vec![moc_llm::InstructionBlock {
                message: 0,
                block: 2
            }]
        );
        assert_eq!(options.key, None);
        assert_eq!(options.ttl, Some(moc_llm::CacheTtl::FiveMinutes));
    }
}

//! 构建请求：输入消息、推理等级和稳定的缓存断点。

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use base64::Engine as _;
use moc_llm::{
    CacheConfig, ChatRequest, ContentPart, Message, ReasoningConfig, ReasoningEcho, RequestOptions,
    Role,
};
use moc_protocol::methods::InputPart;
use tokio::io::AsyncReadExt;

pub(in crate::service) async fn project_input(
    input: Vec<InputPart>,
    accepts_image: bool,
    cwd: &Path,
    anchors: &crate::hashline::AnchorStore,
    config: &crate::ResolvedConfig,
) -> Result<Message, crate::CoreError> {
    let mut content = Vec::with_capacity(input.len());
    for part in input {
        match part {
            InputPart::Text { text } => {
                append_text_with_references(
                    &mut content,
                    &text,
                    cwd,
                    accepts_image,
                    anchors,
                    config,
                )
                .await?;
            }
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

/// `@` 引用展开：`@path` 就地保留在正文里，紧跟一个 `<moc-attachment …>`
/// 附件块带注入内容；`@@` 转义为字面 `@`；图片沿用原有内联逻辑。
async fn append_text_with_references(
    content: &mut Vec<ContentPart>,
    text: &str,
    cwd: &Path,
    accepts_image: bool,
    anchors: &crate::hashline::AnchorStore,
    config: &crate::ResolvedConfig,
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
        let token = &text[start..end];
        // `@@` 转义：字面保留单个 `@`。
        if let Some(rest) = token.strip_prefix("@@") {
            push_text(content, &text[cursor..start]);
            content.push(ContentPart::Text(format!("@{rest}")));
            cursor = end;
            continue;
        }
        // 含空格的路径：向后按词贪心扩展，取能解析出文件/目录的最长前缀
        //（补全插进来的 `@dir file.md`，`@dir` 单独也可能是已存在的目录）。
        let end = extend_reference_end(text, start, end, cwd);
        index = end;
        let token = &text[start..end];
        // 图片附件沿用原逻辑：token 从正文移除，换成图片块。
        if accepts_image && let Some(image) = load_local_image(token, cwd).await? {
            push_text(content, &text[cursor..start]);
            content.push(ContentPart::Image {
                source: image,
                detail: None,
            });
            cursor = end;
            continue;
        }
        let Some(attachment) = attachment_for_token(token, cwd, anchors, config) else {
            continue;
        };
        // 文件/目录：`@path` 保留在正文作引用，附件块就地跟在后面。
        push_text(content, &text[cursor..end]);
        content.push(ContentPart::Text(attachment));
        cursor = end;
    }
    push_text(content, &text[cursor..]);
    Ok(())
}

/// 把一个 `@token` 解析成附件块文本；路径不存在或不适用时返回 `None`，
/// 调用方保留 token 原样。
fn attachment_for_token(
    token: &str,
    cwd: &Path,
    anchors: &crate::hashline::AnchorStore,
    config: &crate::ResolvedConfig,
) -> Option<String> {
    let raw = token.strip_prefix('@')?;
    if raw.is_empty() {
        return None;
    }
    // 规范化对齐 read 工具的展示相对化：cwd 带符号链接段时不归一就剥不掉前缀。
    let path = crate::tools::normalized(&resolve_reference_path(raw, cwd));
    let metadata = std::fs::metadata(&path).ok()?;
    let normalized = crate::tools::normalized(cwd);
    let display = path
        .strip_prefix(&normalized)
        .map(|relative| relative.display().to_string())
        .unwrap_or_else(|_| path.display().to_string());
    let (kind, body) = if metadata.is_dir() {
        match crate::tools::read::list_attachment(&path, &normalized) {
            Ok(listing) => ("dir", listing),
            Err(error) => ("dir", format!("[{display}]\n({error})")),
        }
    } else if metadata.is_file() {
        match crate::tools::read::attachment_text(&path, &display, anchors, config) {
            Ok(text) => ("file", text),
            Err(reason) => (
                "file",
                format!(
                    "[{display}]\n({} bytes; not attached: {reason}; read it with the read tool)",
                    metadata.len()
                ),
            ),
        }
    } else {
        return None;
    };
    Some(crate::resources::attach::wrap_attachment(
        &path.display().to_string(),
        kind,
        &body,
    ))
}

/// 从 `end` 向后按词扩展 `@` 引用：路径含空格时 token 只是前缀，
/// 逐词试探 `metadata`，记录最后一个能解析的边界；末段超过 255 个字符即停，
/// 因为该段已不可能是有效文件名，继续扩展也无法成为有效路径。
/// 新增词含路径分隔符时，若父目录不存在也立即停止，避免扫描后续引用。
fn extend_reference_end(text: &str, start: usize, mut end: usize, cwd: &Path) -> usize {
    let bytes = text.as_bytes();
    let mut scan = end;
    loop {
        let mut i = scan;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i == bytes.len() || i == scan {
            break;
        }
        while i < bytes.len() && !bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        let path = resolve_reference_path(&text[start + 1..i], cwd);
        if path
            .components()
            .next_back()
            .is_some_and(|component| component.as_os_str().to_string_lossy().chars().count() > 255)
        {
            break;
        }
        if text[scan..i].contains(std::path::is_separator)
            && path.parent().is_some_and(|parent| !parent.is_dir())
        {
            break;
        }
        if std::fs::metadata(path).is_ok() {
            end = i;
        }
        scan = i;
    }
    end
}

fn resolve_reference_path(raw_path: &str, cwd: &Path) -> PathBuf {
    let path = crate::config::expand_user_path(raw_path);
    if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    }
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
    let metadata = match tokio::fs::metadata(&path).await {
        Ok(metadata) => metadata,
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
    if metadata.len() > crate::tools::read::MAX_IMAGE_FILE_BYTES {
        return Err(crate::CoreError::Tool(format!(
            "{}: image is {} bytes and exceeds the 20 MiB image input limit; crop or downscale it first",
            path.display(),
            metadata.len()
        )));
    }
    let file = match tokio::fs::File::open(&path).await {
        Ok(file) => file,
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
    let mut bytes = Vec::new();
    if let Err(error) = file
        .take(crate::tools::read::MAX_IMAGE_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
    {
        return Err(crate::CoreError::Tool(format!(
            "failed to read image {}: {error}",
            path.display()
        )));
    }
    if bytes.len() as u64 > crate::tools::read::MAX_IMAGE_FILE_BYTES {
        return Err(crate::CoreError::Tool(format!(
            "{}: image is {} bytes and exceeds the 20 MiB image input limit; crop or downscale it first",
            path.display(),
            bytes.len()
        )));
    }
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
    /// project_input 所需的 anchors 与解析配置；返回临时目录保活。
    fn attach_fixture() -> (
        tempfile::TempDir,
        crate::hashline::AnchorStore,
        crate::ResolvedConfig,
    ) {
        let dir = tempfile::tempdir().expect("临时目录");
        let home = dir.path().join("home");
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(&home).expect("创建 home");
        std::fs::create_dir_all(&cwd).expect("创建工作区");
        let config = crate::ResolvedConfig::load(&crate::CoreConfig {
            home,
            cwd,
            ..Default::default()
        })
        .expect("加载配置");
        (dir, crate::hashline::AnchorStore::new(), config)
    }

    #[tokio::test]
    async fn unsupported_images_become_text_and_local_paths_become_base64() {
        let image = || InputPart::Image {
            source: ImageSource::Url("https://example.invalid/image.png".into()),
            detail: None,
        };
        let (dir, anchors, config) = attach_fixture();
        let cwd = dir.path().join("repo");
        let unsupported = project_input(vec![image()], false, &cwd, &anchors, &config)
            .await
            .expect("图片投影成功");
        assert_eq!(
            unsupported.text(),
            "[The user provided an image, but the current model does not support image input]"
        );
        let supported = project_input(vec![image()], true, &cwd, &anchors, &config)
            .await
            .expect("图片投影成功");
        assert!(matches!(supported.content[0], ContentPart::Image { .. }));

        tokio::fs::write(cwd.join("shot.png"), b"png-bytes")
            .await
            .expect("写入图片");
        let local = project_input(
            vec![InputPart::Text {
                text: "请查看 @shot.png".into(),
            }],
            true,
            &cwd,
            &anchors,
            &config,
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

    #[tokio::test]
    async fn oversized_at_image_is_rejected_before_reading() {
        let (dir, anchors, config) = attach_fixture();
        let cwd = dir.path().join("repo");
        let path = cwd.join("large.png");
        let file = std::fs::File::create(&path).expect("创建图片");
        file.set_len(crate::tools::read::MAX_IMAGE_FILE_BYTES + 1)
            .expect("设置图片大小");

        let error = project_input(
            vec![InputPart::Text {
                text: "查看 @large.png".into(),
            }],
            true,
            &cwd,
            &anchors,
            &config,
        )
        .await
        .unwrap_err();

        let message = error.to_string();
        assert!(message.contains("image is 20971521 bytes"), "{message}");
        assert!(message.contains("20 MiB image input limit"), "{message}");
    }

    /// `@` 文件/目录引用：正文保留 token、紧跟 `<moc-attachment>` 注入块；
    /// 缺失路径与 `@@` 转义都不产生附件。
    #[tokio::test]
    async fn at_references_inject_attachment_parts_and_stay_in_text() {
        let (dir, anchors, config) = attach_fixture();
        let cwd = dir.path().join("repo");
        std::fs::write(cwd.join("note.txt"), "hello\nworld\n").expect("写文件");
        std::fs::create_dir_all(cwd.join("pkg")).expect("建目录");
        std::fs::write(cwd.join("pkg").join("mod.rs"), "pub mod x {}\n").expect("写子文件");

        let message = project_input(
            vec![InputPart::Text {
                text: "看 @note.txt 和 @pkg 还有 @missing 与 @@literal".into(),
            }],
            false,
            &cwd,
            &anchors,
            &config,
        )
        .await
        .expect("投影成功");
        let text = message.text();
        for kept in ["@note.txt", "@pkg", "@missing", "@literal"] {
            assert!(text.contains(kept), "正文应保留 {kept}：{text}");
        }
        assert!(!text.contains("@@literal"), "转义只留一个 @：{text}");
        let attachments = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => crate::resources::attach::parse_attachment(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(attachments.len(), 2, "{attachments:?}");
        let (file_path, file_kind, file_body) = attachments[0];
        assert_eq!(file_kind, "file");
        assert!(file_path.ends_with("note.txt"), "{file_path}");
        assert!(file_body.contains("note.txt"), "{file_body}");
        assert!(file_body.contains("hello"), "{file_body}");
        let (dir_path, dir_kind, dir_body) = attachments[1];
        assert_eq!(dir_kind, "dir");
        assert!(dir_path.ends_with("pkg"), "{dir_path}");
        assert!(dir_body.contains("mod.rs"), "{dir_body}");
    }

    /// 含空格路径：token 只到第一个空格，向后按词贪心取最长可解析前缀；
    /// 前缀恰好是已存在目录时，更长的文件路径仍然胜出。
    #[tokio::test]
    async fn at_references_resolve_paths_with_spaces_by_longest_match() {
        let (dir, anchors, config) = attach_fixture();
        let cwd = dir.path().join("repo");
        std::fs::create_dir_all(cwd.join("design")).expect("建目录");
        std::fs::write(cwd.join("design notes.md"), "# notes\n").expect("写文件");

        let message = project_input(
            vec![InputPart::Text {
                text: "看 @design notes.md 这一段".into(),
            }],
            false,
            &cwd,
            &anchors,
            &config,
        )
        .await
        .expect("投影成功");
        let attachments = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => crate::resources::attach::parse_attachment(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(attachments.len(), 1, "{attachments:?}");
        let (path, kind, body) = attachments[0];
        assert_eq!(kind, "file", "应命中文件而不是前缀目录：{path}");
        assert!(path.ends_with("design notes.md"), "{path}");
        assert!(body.contains("# notes"), "{body}");
        assert!(message.text().contains("这一段"), "正文尾部不能丢");
    }

    #[tokio::test]
    async fn at_references_attach_filename_with_many_spaces() {
        let (dir, anchors, config) = attach_fixture();
        let cwd = dir.path().join("repo");
        let filename = "one two three four five six seven eight nine ten eleven.txt";
        std::fs::write(cwd.join(filename), "many spaces\n").expect("写文件");

        let message = project_input(
            vec![InputPart::Text {
                text: format!("看 @{filename} 这一段"),
            }],
            false,
            &cwd,
            &anchors,
            &config,
        )
        .await
        .expect("投影成功");
        let attachments = message
            .content
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text(text) => crate::resources::attach::parse_attachment(text),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(attachments.len(), 1, "{attachments:?}");
        let (path, kind, body) = attachments[0];
        assert!(path.ends_with(filename), "{path}");
        assert_eq!(kind, "file");
        assert!(body.contains("many spaces"), "{body}");
        assert!(message.text().contains(&format!("@{filename}")));
        assert!(message.text().ends_with(" 这一段"), "正文尾部不能丢");
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

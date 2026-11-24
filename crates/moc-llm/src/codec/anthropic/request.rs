use moc_i18n::t;

use super::super::EncodedRequest;
use super::super::common_headers;
use super::super::echo;
use super::super::echo::ReasoningEchoPlan;
use super::ANTHROPIC_VERSION;
use super::CacheOutcome;
use super::EXTENDED_TTL_BETA;
use super::Normalized;
use super::Placement;
use crate::config::CacheTtl;
use crate::config::LlmConfig;
use crate::config::ToolChoiceDialect;
use crate::error::LlmError;
use crate::types::ChatRequest;
use crate::types::ContentPart;
use crate::types::ImageSource;
use crate::types::Message;
use crate::types::ReasoningBlock;
use crate::types::ReasoningConfig;
use crate::types::ReasoningSummary;
use crate::types::ResponseFormat;
use crate::types::Role;
use crate::types::ToolCall;
use crate::types::ToolChoice;
use crate::types::ToolDef;
use serde_json::Map;
use serde_json::Value;
use serde_json::json;

/// 编码请求。
pub(crate) fn encode(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
) -> Result<EncodedRequest, LlmError> {
    encode_inspected(cfg, req, stream, None)?.into_wire("Anthropic")
}

pub(in super::super) fn encode_inspected(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
    sources: Option<&mut Vec<super::super::RequestSource>>,
) -> Result<EncodedRequest<Value>, LlmError> {
    let max_tokens = req
        .max_output_tokens
        .ok_or_else(|| LlmError::Encode(t!("llm.err.anthropic_max_tokens").into_owned()))?;
    let plan = echo::plan(cfg, req);
    let mut normalized = normalize(cfg, req, plan)?;
    let cache = apply_cache(req, &mut normalized)?;
    if let Some(sources) = sources {
        inspection_sources(req, &normalized, sources);
    }

    let mut body = cfg.extra_body.clone();
    if !cfg.wire.supports_stop {
        body.remove("stop_sequences");
    }
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::Unsupported {
        body.remove("tool_choice");
    }
    body.insert("model".into(), Value::String(cfg.model.clone()));
    body.insert("max_tokens".into(), Value::from(max_tokens));
    if let Some(system) = normalized.system_field() {
        body.insert("system".into(), system);
    }
    body.insert("messages".into(), Value::Array(normalized.messages));
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(encode_tool_def).collect()),
        );
        if let Some(choice) =
            encode_tool_choice(cfg, req.tool_choice.as_ref(), req.parallel_tool_calls)
        {
            body.insert("tool_choice".into(), choice);
        }
    }
    if let Some(temperature) = req.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = req.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if let Some(top_k) = req.top_k {
        body.insert("top_k".into(), Value::from(top_k));
    }
    if !req.stop.is_empty() && cfg.wire.supports_stop {
        body.insert(
            "stop_sequences".into(),
            Value::Array(req.stop.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if let Some(user_id) = &req.options.user_id {
        body.insert("metadata".into(), json!({"user_id": user_id}));
    }
    if let Some(reasoning) = &req.reasoning {
        encode_thinking(cfg, reasoning, &mut body);
    }
    let output_config = encode_output_config(cfg, req)?;
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    } else {
        body.remove("output_config");
    }
    if stream {
        body.insert("stream".into(), Value::Bool(true));
    }

    let mut headers = common_headers(cfg, req);
    headers.set_default("anthropic-version", ANTHROPIC_VERSION);
    if cache.needs_extended_ttl_beta {
        headers.set_default("anthropic-beta", EXTENDED_TTL_BETA);
    }

    Ok(EncodedRequest {
        url: cfg.endpoint_url(),
        headers: headers.into_vec(),
        body: Value::Object(body),
    })
}

/// 请求体顶层 `output_config`：推理强度与响应格式。
pub(super) fn encode_output_config(
    cfg: &LlmConfig,
    req: &ChatRequest,
) -> Result<Map<String, Value>, LlmError> {
    let mut output_config = cfg
        .extra_body
        .get("output_config")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if req
        .reasoning
        .as_ref()
        .is_some_and(|reasoning| !reasoning.enabled)
    {
        output_config.remove("effort");
    }
    if cfg.wire.response_format_dialect == crate::config::ResponseFormatDialect::Unsupported {
        output_config.remove("format");
    }
    if let Some(reasoning) = &req.reasoning
        && let Some(effort) = cfg.wire_effort(Some(reasoning))
    {
        output_config.insert("effort".into(), Value::String(effort));
    }
    if cfg.wire.response_format_dialect != crate::config::ResponseFormatDialect::Unsupported
        && let Some(format) = &req.response_format
    {
        match format {
            ResponseFormat::Text => {
                output_config.remove("format");
            }
            ResponseFormat::JsonObject => {
                return Err(LlmError::Encode(
                    t!("llm.err.anthropic_json_object").into_owned(),
                ));
            }
            // format 只接受 type + schema，普通文本省略该字段。
            ResponseFormat::JsonSchema { schema, .. } => {
                output_config.insert(
                    "format".into(),
                    json!({"type": "json_schema", "schema": schema}),
                );
            }
        }
    }
    Ok(output_config)
}

/// 从缓存坐标复用原消息到线上内容块的映射；检查时才分配路径。
pub(super) fn inspection_sources(
    req: &ChatRequest,
    normalized: &Normalized,
    sources: &mut Vec<super::super::RequestSource>,
) {
    for (index, message) in req.messages.iter().enumerate() {
        let temporary;
        let message = if message.content.is_archived() {
            temporary = Message {
                role: message.role,
                content: message.content.detached_archive(),
            };
            &temporary
        } else {
            message
        };
        let Some(placement) = normalized.placements[index] else {
            continue;
        };
        match placement {
            Placement::System { first_block, .. } => {
                let mut offset = normalized.system_parts[..first_block]
                    .iter()
                    .map(|text| text.len() + 2)
                    .sum::<usize>();
                for (part, content) in message.content.iter().enumerate() {
                    let ContentPart::Text(text) = content else {
                        continue;
                    };
                    let (pointer, range) = if normalized.system_override.is_some() {
                        (
                            format!("/system/{}/text", first_block + part),
                            0..text.len(),
                        )
                    } else {
                        ("/system".to_owned(), offset..offset + text.len())
                    };
                    super::super::inspection::source(sources, index, part, pointer, Some(range));
                    offset += text.len() + 2;
                }
            }
            Placement::Block {
                message: wire,
                first_block,
                ..
            } => {
                // Assistant 可能过滤思考块，整条标记来源即可；用户/工具/提醒逐块一一映射。
                if message.role == Role::Assistant {
                    super::super::inspection::source(
                        sources,
                        index,
                        0,
                        format!("/messages/{wire}"),
                        None,
                    );
                    continue;
                }
                for (part, content) in message.content.iter().enumerate() {
                    let pointer = format!("/messages/{wire}/content/{}", first_block + part);
                    match content {
                        ContentPart::Text(text) => super::super::inspection::source(
                            sources,
                            index,
                            part,
                            format!("{pointer}/text"),
                            Some(0..text.len()),
                        ),
                        _ => super::super::inspection::source(sources, index, part, pointer, None),
                    }
                }
            }
        }
    }
}

/// 规整历史：system 上提、按线上角色合并、记录断点落点。
pub(super) fn normalize(
    cfg: &LlmConfig,
    req: &ChatRequest,
    plan: ReasoningEchoPlan,
) -> Result<Normalized, LlmError> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut messages: Vec<Value> = Vec::with_capacity(req.messages.len());
    let mut placements: Vec<Option<Placement>> = vec![None; req.messages.len()];
    // 正在合并的线上 user 消息下标（`Role::User` 与 `Role::Tool` 共用）。
    let mut open_user: Option<usize> = None;

    for (index, message) in req.messages.iter().enumerate() {
        let temporary;
        let message = if message.content.is_archived() {
            temporary = Message {
                role: message.role,
                content: message.content.detached_archive(),
            };
            &temporary
        } else {
            message
        };
        match message.role {
            Role::System | Role::Developer => {
                // system 被上提到顶层，不出现在 messages 里，因此不打断
                // 两侧 user 消息的合并。
                let first_block = system_parts.len();
                for part in &message.content {
                    match part {
                        ContentPart::Text(text) => system_parts.push(text.clone()),
                        other => return Err(unsupported_part(message.role, other)),
                    }
                }
                // 断点可以落在系统提示上（core 恒定给出下标 0）：记下落点与自身块数，
                // 由 `apply_cache` 折到顶层 system 的对应块上。
                placements[index] = Some(Placement::System {
                    first_block,
                    blocks: system_parts.len() - first_block,
                });
            }
            Role::Assistant => {
                let blocks = encode_assistant_blocks(plan, message)?;
                if blocks.is_empty() {
                    // 思维链被全部剔除且没有正文与工具调用的 assistant 消息
                    // 在线上非法，直接丢弃：此时两侧 user 消息在线上相邻，
                    // 必须继续合并才能满足 Anthropic 的角色交替要求。
                    continue;
                }
                open_user = None;
                let blocks_len = blocks.len();
                messages.push(json!({"role": "assistant", "content": blocks}));
                placements[index] = Some(Placement::Block {
                    message: messages.len() - 1,
                    first_block: 0,
                    blocks: blocks_len,
                });
            }
            Role::User | Role::Tool | Role::Custom => {
                if message.role == Role::Custom {
                    for part in &message.content {
                        if !matches!(part, ContentPart::Text(_)) {
                            return Err(unsupported_part(message.role, part));
                        }
                    }
                }
                let blocks = encode_user_blocks(cfg, message)?;
                if blocks.is_empty() {
                    continue;
                }
                let blocks_len = blocks.len();
                match open_user {
                    Some(position) => {
                        let content = messages[position]
                            .get_mut("content")
                            .and_then(Value::as_array_mut)
                            .expect("线上消息由本模块构造成内容块数组");
                        let first_block = content.len();
                        content.extend(blocks);
                        placements[index] = Some(Placement::Block {
                            message: position,
                            first_block,
                            blocks: blocks_len,
                        });
                    }
                    None => {
                        messages.push(json!({"role": "user", "content": blocks}));
                        open_user = Some(messages.len() - 1);
                        placements[index] = Some(Placement::Block {
                            message: messages.len() - 1,
                            first_block: 0,
                            blocks: blocks_len,
                        });
                    }
                }
            }
        }
    }

    Ok(Normalized {
        system_parts,
        system_override: None,
        messages,
        placements,
    })
}

pub(super) fn encode_assistant_blocks(
    plan: ReasoningEchoPlan,
    message: &Message,
) -> Result<Vec<Value>, LlmError> {
    let mut blocks = Vec::with_capacity(message.content.len());
    for part in &message.content {
        match part {
            ContentPart::ResponsePhase(_) => {}
            ContentPart::Text(text) => blocks.push(json!({"type": "text", "text": text})),
            ContentPart::Reasoning(block) => {
                // `Never` 表示 core 主动裁剪历史，此时思维链块一律剔除；
                // 其余模式下无法表达为 Anthropic 块的类型直接报错，
                // 而不是静默丢内容。
                if plan == ReasoningEchoPlan::Strip {
                    continue;
                }
                match block {
                    // 签名在块末尾的 signature_delta 中给出，缺签名时上游会 400。
                    ReasoningBlock::Text { text, signature } => {
                        if let Some(signature) = signature
                            && !signature.is_empty()
                        {
                            blocks.push(json!({
                                "type": "thinking",
                                "thinking": text,
                                "signature": signature,
                            }));
                        }
                    }
                    ReasoningBlock::Redacted { data } => {
                        blocks.push(json!({"type": "redacted_thinking", "data": data}));
                    }
                    ReasoningBlock::Item(_) => {
                        return Err(LlmError::Encode(
                            t!("llm.err.anthropic_reasoning_item").into_owned(),
                        ));
                    }
                }
            }
            ContentPart::ToolCall(call) => blocks.push(encode_tool_call(call)?),
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    Ok(blocks)
}

pub(super) fn encode_tool_call(call: &ToolCall) -> Result<Value, LlmError> {
    let input = call.arguments_json()?;
    if !input.is_object() {
        return Err(LlmError::Encode(
            t!(
                "llm.err.tool_args_object",
                name = call.name.as_str(),
                value = call.arguments
            )
            .into_owned(),
        ));
    }
    Ok(json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.name,
        "input": input,
    }))
}

pub(super) fn encode_user_blocks(
    _cfg: &LlmConfig,
    message: &Message,
) -> Result<Vec<Value>, LlmError> {
    let mut blocks = Vec::with_capacity(message.content.len());
    for part in &message.content {
        match part {
            ContentPart::Text(text) => blocks.push(json!({"type": "text", "text": text})),
            ContentPart::Image { source, .. } => {
                blocks.push(json!({"type": "image", "source": encode_image_source(source)?}));
            }
            ContentPart::ToolResult(result) => {
                // 无图时保持字符串 content（旧线格式兼容）；带图时 content 升级为
                // 内容块数组：文本块 + 图片块，与 Anthropic 的 tool_result 结构一致。
                let content = if result.images.is_empty() {
                    Value::String(result.content.clone())
                } else {
                    let mut parts = Vec::with_capacity(result.images.len() + 1);
                    if !result.content.is_empty() {
                        parts.push(json!({"type": "text", "text": result.content}));
                    }
                    for source in &result.images {
                        parts
                            .push(json!({"type": "image", "source": encode_image_source(source)?}));
                    }
                    Value::Array(parts)
                };
                blocks.push(json!({
                    "type": "tool_result",
                    "tool_use_id": result.tool_call_id,
                    "content": content,
                    "is_error": result.is_error,
                }));
            }
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    Ok(blocks)
}

pub(super) fn encode_image_source(source: &ImageSource) -> Result<Value, LlmError> {
    match source {
        ImageSource::Base64 { media_type, data } => Ok(json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        })),
        ImageSource::Url(url) => Ok(json!({"type": "url", "url": url})),
    }
}

pub(super) fn encode_tool_def(def: &ToolDef) -> Value {
    let mut object = Map::new();
    object.insert("name".into(), Value::String(def.name.clone()));
    if let Some(description) = &def.description {
        object.insert("description".into(), Value::String(description.clone()));
    }
    object.insert("input_schema".into(), def.parameters.clone());
    if let Some(strict) = def.strict {
        object.insert("strict".into(), Value::Bool(strict));
    }
    Value::Object(object)
}

/// `parallel_tool_calls` 映射为 `tool_choice.disable_parallel_tool_use`。
///
/// `Some(false)` 且未指定 `tool_choice` 时合成 `{"type":"auto",…}`；
/// `Some(true)` 不发送该键（默认即允许并行）。
pub(super) fn encode_tool_choice(
    cfg: &LlmConfig,
    choice: Option<&ToolChoice>,
    parallel: Option<bool>,
) -> Option<Value> {
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::Unsupported {
        return None;
    }
    let disable_parallel = cfg.wire.parallel_tool_calls && matches!(parallel, Some(false));
    if choice.is_none() && !disable_parallel && !cfg.extra_body.contains_key("tool_choice") {
        return None;
    }
    let mut object = match choice {
        None => cfg
            .extra_body
            .get("tool_choice")
            .cloned()
            .unwrap_or_else(|| json!({"type":"auto"})),
        Some(ToolChoice::Auto) => json!({"type": "auto"}),
        Some(ToolChoice::None) => json!({"type": "none"}),
        Some(ToolChoice::Required) => json!({"type": "any"}),
        Some(ToolChoice::Tool(name)) => json!({"type": "tool", "name": name}),
    };
    if let Some(object) = object.as_object_mut() {
        object.entry("type").or_insert_with(|| json!("auto"));
        if object.get("type").and_then(Value::as_str) == Some("none") {
            object.remove("name");
            object.remove("disable_parallel_tool_use");
        } else if !cfg.wire.parallel_tool_calls {
            object.remove("disable_parallel_tool_use");
        } else if disable_parallel {
            object.insert("disable_parallel_tool_use".into(), Value::Bool(true));
        }
    }
    Some(object)
}

/// 写入 `thinking` 对象；思考等级通过 `output_config.effort` 发送。
pub(super) fn encode_thinking(
    _cfg: &LlmConfig,
    reasoning: &ReasoningConfig,
    body: &mut Map<String, Value>,
) {
    let mut thinking = body
        .get("thinking")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if !reasoning.enabled {
        thinking.clear();
        thinking.insert("type".into(), json!("disabled"));
    } else {
        thinking.entry("type").or_insert_with(|| json!("adaptive"));
        match thinking.get("type").and_then(Value::as_str) {
            Some("disabled" | "between_tools") => {
                thinking.remove("budget_tokens");
                thinking.remove("display");
            }
            _ => {
                if thinking.get("type").and_then(Value::as_str) != Some("enabled") {
                    thinking.remove("budget_tokens");
                }
                if let Some(display) = reasoning.summary.and_then(display_value) {
                    thinking.insert("display".into(), Value::String(display.into()));
                }
            }
        }
    }
    body.insert("thinking".into(), Value::Object(thinking));
}

/// `ReasoningSummary` → `display`；`None` 表示不发送该键（由模型默认值决定）。
pub(super) fn display_value(summary: ReasoningSummary) -> Option<&'static str> {
    match summary {
        ReasoningSummary::Omitted => Some("omitted"),
        ReasoningSummary::Summarized
        | ReasoningSummary::Auto
        | ReasoningSummary::Concise
        | ReasoningSummary::Detailed => Some("summarized"),
    }
}

/// 应用缓存配置：校验 TTL、把 `cache_control` 落到正确的内容块上。
pub(super) fn apply_cache(
    req: &ChatRequest,
    normalized: &mut Normalized,
) -> Result<CacheOutcome, LlmError> {
    let Some(cache) = &req.options.cache else {
        return Ok(CacheOutcome {
            needs_extended_ttl_beta: false,
        });
    };
    let ttl = match cache.ttl {
        None => None,
        Some(CacheTtl::FiveMinutes) => Some("5m"),
        Some(CacheTtl::OneHour) => Some("1h"),
        Some(CacheTtl::ThirtyMinutes) => {
            return Err(LlmError::Encode(
                t!("llm.err.anthropic_cache_ttl").into_owned(),
            ));
        }
    };
    let mut cache_control = Map::new();
    cache_control.insert("type".into(), Value::String("ephemeral".into()));
    if let Some(ttl) = ttl {
        cache_control.insert("ttl".into(), Value::String(ttl.into()));
    }
    let cache_control = Value::Object(cache_control);

    if cache.breakpoints.is_empty() && cache.instruction_blocks.is_empty() {
        // 未指定断点时缓存整个前缀：`system` 的最后一块之后即「系统提示 + 工具定义」
        // 的边界，这是 Anthropic 官方推荐的最小缓存单元；没有系统提示时，
        // 断点落在首条消息的最后一块。
        if normalized.system_is_empty() {
            normalized.attach_instruction_block(&cache_control, 0, None)?;
        } else {
            normalized.attach_system_breakpoint(&cache_control, None)?;
        }
    } else {
        // 多个断点可以落在同一条被上提的消息上（core 给出的首尾断点常常如此），
        // 顶层 system 因此只升级一次。
        let mut system_breakpoint = false;
        for &index in &cache.breakpoints {
            match normalized.placements.get(index).copied().flatten() {
                Some(Placement::Block {
                    message,
                    first_block,
                    blocks,
                }) => {
                    normalized.attach_message_block(
                        &cache_control,
                        message,
                        first_block + blocks - 1,
                    )?;
                }
                Some(Placement::System { .. }) => system_breakpoint = true,
                None => {
                    return Err(LlmError::Encode(
                        t!("llm.err.cache_no_block", index = index).into_owned(),
                    ));
                }
            }
        }
        if system_breakpoint {
            normalized.attach_system_breakpoint(&cache_control, None)?;
        }
        // 静态前缀块：与工作目录无关的部分单独落位，跨目录、跨会话复用。
        for point in &cache.instruction_blocks {
            normalized.attach_instruction_block(
                &cache_control,
                point.message,
                Some(point.block),
            )?;
        }
    }
    Ok(CacheOutcome {
        needs_extended_ttl_beta: ttl == Some("1h"),
    })
}

pub(super) fn unsupported_part(role: Role, part: &ContentPart) -> LlmError {
    let kind = match part {
        ContentPart::Text(_) => "Text",
        ContentPart::ResponsePhase(_) => "ResponsePhase",
        ContentPart::Image { .. } => "Image",
        ContentPart::Reasoning(_) => "Reasoning",
        ContentPart::ToolCall(_) => "ToolCall",
        ContentPart::ToolResult(_) => "ToolResult",
    };
    LlmError::Encode(t!("llm.err.role_block_kind", role = role, kind = kind).into_owned())
}

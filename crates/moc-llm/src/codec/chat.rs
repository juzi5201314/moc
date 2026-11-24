//! OpenAI Chat Completions 编解码。
//!
//! 线上字段映射见 crate 计划中的映射表；本模块只发送表中列出的字段，未列出的
//! 一律不发送（厂商扩展走 [`crate::LlmConfig::extra_body`]）。

use std::collections::BTreeSet;

use moc_i18n::t;
use serde_json::{Map, Value, json};

use super::echo::{self, ReasoningEchoPlan};
use super::openai;
use super::sse::SseEvent;
use super::{EncodedRequest, StreamDecoder, common_headers};
use crate::config::{
    CacheTtl, LlmConfig, MaxTokensField, ResponseFormatDialect, ToolChoiceDialect,
};
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::types::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, ImageDetail, ImageSource, Message,
    ReasoningBlock, ReasoningConfig, ResponseFormat, Role, StreamEvent, ToolCall, ToolChoice,
    ToolDef,
};

/// 单次流式响应里允许的 `tool_calls[].index` 上界。
///
/// 线上的 `index` 会被直接当作 `Vec` 下标使用，因此必须由本地常量兜底而不是
/// `debug_assert`：异常网关给出的大值会驱动一次超大分配（`handle_alloc_error`
/// 直接 abort 进程），回绕值会让 `index + 1` 溢出。模型返回的并行调用数是个位数，
/// 1024 与 [`crate::guard`] 的 `MAX_BLOCKS` 同量级。
const MAX_TOOL_CALL_SLOTS: usize = 1024;

/// 编码请求。
pub(crate) fn encode(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
) -> Result<EncodedRequest, LlmError> {
    encode_inspected(cfg, req, stream, None)?.into_wire("Chat")
}

pub(super) fn encode_inspected(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
    sources: Option<&mut Vec<super::RequestSource>>,
) -> Result<EncodedRequest<Value>, LlmError> {
    let plan = echo::plan(cfg, req);
    let breakpoints = cache_breakpoints(req, req.messages.len())?;

    let mut body = cfg.extra_body.clone();
    if !cfg.wire.supports_stop {
        body.remove("stop");
    }
    if !cfg.wire.parallel_tool_calls {
        body.remove("parallel_tool_calls");
    }
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::Unsupported {
        body.remove("tool_choice");
    }
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::AnyForRequired
        && body.get("tool_choice").and_then(Value::as_str) == Some("required")
    {
        body.insert("tool_choice".into(), json!("any"));
    }
    if cfg.wire.response_format_dialect == ResponseFormatDialect::Unsupported {
        body.remove("response_format");
    }
    body.insert("model".into(), Value::String(cfg.model.clone()));
    body.insert(
        "messages".into(),
        Value::Array(encode_messages(cfg, req, plan, &breakpoints, sources)?),
    );
    if !req.tools.is_empty() {
        body.insert(
            "tools".into(),
            Value::Array(req.tools.iter().map(encode_tool_def).collect()),
        );
        if let Some(choice) = encode_tool_choice(cfg, req.tool_choice.as_ref()) {
            body.insert("tool_choice".into(), choice);
        }
    }
    if let Some(max) = req.max_output_tokens {
        let field = match cfg.wire.max_output_tokens_field {
            MaxTokensField::MaxTokens => "max_tokens",
            MaxTokensField::MaxCompletionTokens => "max_completion_tokens",
        };
        body.insert(field.into(), Value::from(max));
    }
    if let Some(temperature) = req.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = req.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if !req.stop.is_empty() && cfg.wire.supports_stop {
        body.insert(
            "stop".into(),
            Value::Array(req.stop.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if let Some(reasoning) = &req.reasoning {
        encode_thinking(cfg, reasoning, &mut body);
    }
    if let Some(format) = &req.response_format
        && let Some(value) = encode_response_format(cfg, format)
    {
        body.insert("response_format".into(), value);
    }
    if let Some(parallel) = req.parallel_tool_calls
        && cfg.wire.parallel_tool_calls
    {
        body.insert("parallel_tool_calls".into(), Value::Bool(parallel));
    }
    if let Some(cache) = &req.options.cache {
        if let Some(key) = &cache.key {
            body.insert("prompt_cache_key".into(), Value::String(key.clone()));
        }
        if !breakpoints.is_empty() {
            let ttl = cache.ttl.unwrap_or(CacheTtl::ThirtyMinutes);
            let mut options = body
                .get("prompt_cache_options")
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            options.insert("mode".into(), json!("explicit"));
            options.insert("ttl".into(), json!(ttl.as_str()));
            body.insert("prompt_cache_options".into(), Value::Object(options));
        }
    }
    if let Some(store) = req.options.store {
        body.insert("store".into(), Value::Bool(store));
    }
    if stream {
        body.insert("stream".into(), Value::Bool(true));
        let mut options = body
            .get("stream_options")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        options.entry("include_usage").or_insert(Value::Bool(true));
        body.insert("stream_options".into(), Value::Object(options));
    }

    Ok(EncodedRequest {
        url: cfg.endpoint_url(),
        headers: common_headers(cfg, req).into_vec(),
        body: Value::Object(body),
    })
}

/// 显式缓存：校验 TTL 与断点下标，返回断点消息下标集合。
///
/// Chat 的显式缓存当前只支持 30 分钟 TTL（`prompt_cache_options.ttl` 的唯一取值），
/// 其他取值属于配置错误，直接报错而不是静默降级。
fn cache_breakpoints(req: &ChatRequest, message_count: usize) -> Result<BTreeSet<usize>, LlmError> {
    let Some(cache) = &req.options.cache else {
        return Ok(BTreeSet::new());
    };
    if let Some(ttl) = cache.ttl
        && ttl != CacheTtl::ThirtyMinutes
    {
        return Err(LlmError::Encode(
            t!("llm.err.chat_cache_ttl", value = ttl.as_str()).into_owned(),
        ));
    }
    let mut breakpoints = BTreeSet::new();
    for &index in &cache.breakpoints {
        if index >= message_count {
            return Err(LlmError::Encode(
                t!(
                    "llm.err.cache_index_range",
                    index = index,
                    message_count = message_count
                )
                .into_owned(),
            ));
        }
        breakpoints.insert(index);
    }
    Ok(breakpoints)
}

fn encode_messages(
    cfg: &LlmConfig,
    req: &ChatRequest,
    plan: ReasoningEchoPlan,
    breakpoints: &BTreeSet<usize>,
    mut sources: Option<&mut Vec<super::RequestSource>>,
) -> Result<Vec<Value>, LlmError> {
    let mut out = Vec::with_capacity(req.messages.len());
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
        let first = out.len();
        let breakpoint = breakpoints.contains(&index);
        match message.role {
            Role::System | Role::Developer => {
                out.push(encode_instruction(cfg, message, breakpoint)?);
            }
            Role::User => out.push(encode_user(cfg, message, breakpoint)?),
            Role::Custom => out.push(encode_custom(cfg, message, breakpoint)?),
            Role::Assistant => out.push(encode_assistant(cfg, plan, message, breakpoint)?),
            // tool 消息携带的图片无法进入 tool 角色，编码为紧随其后的 user 消息。
            Role::Tool => out.extend(encode_tool_result(cfg, message, breakpoint)?),
        }
        if let Some(sources) = sources.as_deref_mut() {
            super::inspection::openai_sources(
                sources,
                index,
                message,
                &out[first..],
                first,
                "messages",
            );
        }
    }
    Ok(out)
}

fn encode_instruction(
    cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Value, LlmError> {
    for part in &message.content {
        if !matches!(part, ContentPart::Text(_)) {
            return Err(unsupported_part(message.role, part));
        }
    }
    // 指令消息拍平成单串：块边界只是本地缓存断点的坐标，不是线上结构。
    let text = message.instruction_text();
    let role = if message.role == Role::Developer && cfg.wire.developer_role {
        "developer"
    } else {
        "system"
    };
    Ok(json!({"role": role, "content": finish_text_content(text, breakpoint)?}))
}

fn encode_user(_cfg: &LlmConfig, message: &Message, breakpoint: bool) -> Result<Value, LlmError> {
    let mut blocks = Vec::with_capacity(message.content.len());
    for part in &message.content {
        match part {
            ContentPart::Text(chunk) => blocks.push(json!({"type": "text", "text": chunk})),
            ContentPart::Image { source, detail } => {
                blocks.push(encode_image(source, *detail)?);
            }
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    Ok(json!({"role": "user", "content": finish_content(blocks, breakpoint)?}))
}

/// 运行时注入消息：留在对话流里，支持 `developer` 的供应商走它，否则退回 `user`。
fn encode_custom(cfg: &LlmConfig, message: &Message, breakpoint: bool) -> Result<Value, LlmError> {
    for part in &message.content {
        if !matches!(part, ContentPart::Text(_)) {
            return Err(unsupported_part(message.role, part));
        }
    }
    let role = if cfg.wire.developer_role {
        "developer"
    } else {
        "user"
    };
    Ok(json!({
        "role": role,
        "content": finish_text_content(message.instruction_text(), breakpoint)?,
    }))
}

fn encode_image(source: &ImageSource, detail: Option<ImageDetail>) -> Result<Value, LlmError> {
    let url = match source {
        ImageSource::Url(url) => url.clone(),
        ImageSource::Base64 { media_type, data } => {
            format!("data:{media_type};base64,{data}")
        }
    };
    let mut image_url = Map::new();
    image_url.insert("url".into(), Value::String(url));
    if let Some(detail) = detail {
        image_url.insert("detail".into(), Value::String(detail.as_str().into()));
    }
    Ok(json!({"type": "image_url", "image_url": Value::Object(image_url)}))
}

fn encode_assistant(
    cfg: &LlmConfig,
    plan: ReasoningEchoPlan,
    message: &Message,
    breakpoint: bool,
) -> Result<Value, LlmError> {
    let mut text = String::new();
    let mut reasoning = String::new();
    let mut has_text = false;
    let mut has_reasoning = false;
    let mut tool_calls = Vec::new();
    for part in &message.content {
        match part {
            ContentPart::ResponsePhase(_) => {}
            ContentPart::Text(chunk) => {
                has_text = true;
                text.push_str(chunk);
            }
            ContentPart::Reasoning(block) => {
                if plan.keeps(block)
                    && let ReasoningBlock::Text { text: chunk, .. } = block
                {
                    has_reasoning = true;
                    reasoning.push_str(chunk);
                }
            }
            ContentPart::ToolCall(call) => tool_calls.push(json!({
                "id": call.id,
                "type": "function",
                "function": {"name": call.name, "arguments": call.arguments},
            })),
            other => return Err(unsupported_part(message.role, other)),
        }
    }

    let mut object = Map::new();
    object.insert("role".into(), Value::String("assistant".into()));
    // 显式空文本与缺失正文不同，兼容层可据此满足上游必填字段。
    object.insert(
        "content".into(),
        finish_text_content(text, breakpoint)?.unwrap_or_else(|| {
            if has_text {
                Value::String(String::new())
            } else {
                Value::Null
            }
        }),
    );
    if !tool_calls.is_empty() {
        object.insert("tool_calls".into(), Value::Array(tool_calls));
    }
    if has_reasoning {
        object.insert(echo::reasoning_field(cfg).into(), Value::String(reasoning));
    }
    Ok(Value::Object(object))
}

fn encode_tool_result(
    _cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Vec<Value>, LlmError> {
    let Some(result) = message.tool_results().next() else {
        return Err(LlmError::Encode(
            t!("llm.err.tool_missing_result").into_owned(),
        ));
    };
    // tool 消息的 `content` 是必填字符串：空结果发空串，而不是 `null`。
    let content = if breakpoint {
        finish_content(vec![json!({"type": "text", "text": result.content})], true)?
    } else {
        Value::String(result.content.clone())
    };
    let mut out = vec![json!({
        "role": "tool",
        "tool_call_id": result.tool_call_id,
        "content": content,
    })];
    if result.images.is_empty() {
        return Ok(out);
    }
    // 图片旁路：tool 角色只收字符串，以独立 user 消息补发。
    let mut blocks = Vec::with_capacity(result.images.len() + 1);
    blocks.push(json!({
        "type": "text",
        "text": format!("[image returned by tool {}]", result.tool_call_id),
    }));
    for source in &result.images {
        blocks.push(encode_image(source, None)?);
    }
    out.push(json!({"role": "user", "content": Value::Array(blocks)}));
    Ok(out)
}

fn encode_tool_def(def: &ToolDef) -> Value {
    let mut function = Map::new();
    function.insert("name".into(), Value::String(def.name.clone()));
    if let Some(description) = &def.description {
        function.insert("description".into(), Value::String(description.clone()));
    }
    function.insert("parameters".into(), def.parameters.clone());
    if let Some(strict) = def.strict {
        function.insert("strict".into(), Value::Bool(strict));
    }
    json!({"type": "function", "function": Value::Object(function)})
}

fn encode_tool_choice(cfg: &LlmConfig, choice: Option<&ToolChoice>) -> Option<Value> {
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::Unsupported {
        return None;
    }
    let choice = choice?;
    Some(match choice {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => match cfg.wire.tool_choice_dialect {
            ToolChoiceDialect::AnyForRequired => json!("any"),
            _ => json!("required"),
        },
        ToolChoice::Tool(name) => json!({"type": "function", "function": {"name": name}}),
    })
}

fn encode_response_format(cfg: &LlmConfig, format: &ResponseFormat) -> Option<Value> {
    if cfg.wire.response_format_dialect == ResponseFormatDialect::Unsupported {
        return None;
    }
    Some(match format {
        ResponseFormat::Text => json!({"type": "text"}),
        ResponseFormat::JsonObject => json!({"type": "json_object"}),
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            let mut inner = Map::new();
            inner.insert("name".into(), Value::String(name.clone()));
            if let Some(description) = description {
                inner.insert("description".into(), Value::String(description.clone()));
            }
            inner.insert("schema".into(), schema.clone());
            if let Some(strict) = strict {
                inner.insert("strict".into(), Value::Bool(*strict));
            }
            json!({"type": "json_schema", "json_schema": Value::Object(inner)})
        }
    })
}

/// 写入 `thinking` 对象与 `reasoning_effort` 顶层字段。
///
/// 标准 Chat 不发送厂商 thinking 对象；显式配置 toggle 的厂商用 toggle 表达关闭。
fn encode_thinking(cfg: &LlmConfig, reasoning: &ReasoningConfig, body: &mut Map<String, Value>) {
    let wire = &cfg.wire.thinking;
    if let Some(field) = &wire.toggle_field {
        let value = if reasoning.enabled {
            &wire.enabled_value
        } else {
            &wire.disabled_value
        };
        let mut object = body
            .get(field)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        object.extend(wire.toggle_extra.clone());
        object.insert("type".into(), Value::String(value.clone()));
        body.insert(field.clone(), Value::Object(object));
    }
    if let Some(field) = &wire.effort_field
        && let Some(value) = cfg.wire_effort(Some(reasoning))
    {
        body.insert(field.clone(), Value::String(value));
    }
}

/// 把一组内容块收尾：附加缓存断点，必要时降级为纯字符串。
fn finish_content(mut blocks: Vec<Value>, breakpoint: bool) -> Result<Value, LlmError> {
    if breakpoint {
        let Some(last) = blocks.last_mut() else {
            return Err(LlmError::Encode(t!("llm.err.cache_empty_msg").into_owned()));
        };
        last.as_object_mut()
            .expect("内容块由本模块构造成 JSON 对象")
            .insert(
                "prompt_cache_breakpoint".into(),
                json!({"mode": "explicit"}),
            );
    }
    if !breakpoint
        && blocks.len() == 1
        && blocks[0].get("type") == Some(&json!("text"))
        && let Some(text) = blocks[0].get("text").and_then(Value::as_str)
    {
        return Ok(Value::String(text.to_owned()));
    }
    Ok(Value::Array(blocks))
}

/// 纯文本内容的收尾：无断点时降级为字符串，有断点时升级为带标记的内容块数组。
fn finish_text_content(text: String, breakpoint: bool) -> Result<Option<Value>, LlmError> {
    if !breakpoint {
        return Ok((!text.is_empty()).then_some(Value::String(text)));
    }
    Ok(Some(finish_content(
        vec![json!({"type": "text", "text": text})],
        true,
    )?))
}

fn unsupported_part(role: Role, part: &ContentPart) -> LlmError {
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

/// 非流式响应解码。
pub(crate) fn decode_response(
    cfg: &LlmConfig,
    _req: &ChatRequest,
    value: &Value,
) -> Result<ChatResponse, LlmError> {
    let choice = value
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| {
            choices
                .iter()
                .find(|choice| choice.get("index").and_then(Value::as_u64).unwrap_or(0) == 0)
        });
    let message = choice.and_then(|choice| choice.get("message"));

    let mut content = Vec::new();
    // 块顺序固定为 [Reasoning, Text, ToolCall…]：Chat 线上没有块顺序信息，
    // 固定顺序才能让非流式与流式的产物一致。
    if let Some(reasoning) = message.and_then(|message| reasoning_text(cfg, message))
        && !reasoning.is_empty()
    {
        content.push(ContentPart::Reasoning(ReasoningBlock::Text {
            text: reasoning.to_owned(),
            signature: None,
        }));
    }
    let mut text = text_of(message.and_then(|message| message.get("content")));
    if let Some(refusal) = message
        .and_then(|message| message.get("refusal"))
        .and_then(Value::as_str)
    {
        text.push_str(refusal);
    }
    if !text.is_empty() {
        content.push(ContentPart::Text(text));
    }
    for call in message
        .and_then(|message| message.get("tool_calls"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        content.push(ContentPart::ToolCall(ToolCall {
            id: string_at(call, "/id"),
            name: string_at(call, "/function/name"),
            arguments: string_at(call, "/function/arguments"),
        }));
    }

    let finish_reason = choice
        .and_then(|choice| choice.get("finish_reason"))
        .and_then(Value::as_str)
        .map_or_default(openai::map_finish_reason);
    Ok(ChatResponse {
        id: value.get("id").and_then(Value::as_str).map(str::to_owned),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reasoning_effort: super::reported_effort(value),
        message: Message {
            role: Role::Assistant,
            content: content.into(),
        },
        finish_reason,
        usage: value.get("usage").map_or_default(openai::decode_usage),
        raw: None,
    })
}

fn string_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 取出文本：字符串直接取，数组取其中 `text` 字段拼接（部分网关返回内容块数组，
/// 数组里的裸字符串同样计入）。
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    out.push_str(text);
                } else if let Some(text) = part.as_str() {
                    out.push_str(text);
                }
            }
            out
        }
        _ => String::new(),
    }
}

/// 按「配置字段名 + 别名列表」的顺序找第一个非空思维链字段。
fn reasoning_text<'a>(cfg: &LlmConfig, object: &'a Value) -> Option<&'a str> {
    std::iter::once(&cfg.wire.reasoning_field)
        .chain(cfg.wire.reasoning_field_aliases.iter())
        .find_map(|field| {
            object
                .get(field)
                .and_then(Value::as_str)
                .filter(|text| !text.is_empty())
        })
}

/// Chat 流式状态机。
pub(crate) struct ChatStreamDecoder {
    /// 首个 chunk 已发出 [`StreamEvent::Start`]。
    started: bool,
    /// 已到终态（`[DONE]`、错误或收尾完成）。
    terminated: bool,
    /// 最近一次见到的 `finish_reason`。
    finish_reason: Option<FinishReason>,
    /// 思维链字段名候选（配置字段名 + 别名）。
    reasoning_fields: Vec<String>,
    /// 工具调用 index → 首个非空 ID。
    ///
    /// 线上 `tool_calls[].index` 在同一次响应内稠密、从 0 开始，上界为模型返回的
    /// 并行调用数（个位数），因此用 `Vec` 直接下标寻址，不做哈希；下标另由
    /// [`MAX_TOOL_CALL_SLOTS`] 兜底，超限即拒绝整条流。
    tool_ids: Vec<String>,
    /// 工具调用 index → 已收到的名称片段拼接。
    ///
    /// `ToolCallStart` 必须携带最终 ID：网关可能把 `id` 拆到后续 chunk
    /// （name 先到）。提前用空 ID 发射会让下游把该调用的参数增量落到空 ID 上，
    /// 与消息累加器里锁存的真实 ID 分叉，参数就此丢失。
    tool_names: Vec<String>,
    /// 工具调用 index → `ToolCallStart` 是否已发射。每个 index 只发一次，
    /// 后续名称分片通过快照修正同一块，不重复创建调用。
    tool_started: Vec<bool>,
}

impl ChatStreamDecoder {
    /// 按配置构造。
    pub fn new(cfg: &LlmConfig) -> Self {
        let mut reasoning_fields = Vec::with_capacity(cfg.wire.reasoning_field_aliases.len() + 1);
        reasoning_fields.push(cfg.wire.reasoning_field.clone());
        reasoning_fields.extend(cfg.wire.reasoning_field_aliases.iter().cloned());
        Self {
            started: false,
            terminated: false,
            finish_reason: None,
            reasoning_fields,
            tool_ids: Vec::new(),
            tool_names: Vec::new(),
            tool_started: Vec::new(),
        }
    }

    fn decode_delta(
        &mut self,
        delta: &Value,
        out: &mut Vec<Result<StreamEvent, LlmError>>,
    ) -> Result<(), LlmError> {
        // 与「非流式块顺序固定为 [Reasoning, Text, ToolCall…]」保持一致。
        let reasoning = self
            .reasoning_fields
            .iter()
            .find_map(|field| {
                delta
                    .get(field)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
            })
            .map(str::to_owned);
        if let Some(reasoning) = reasoning {
            out.push(Ok(StreamEvent::ReasoningDelta {
                index: 0,
                text: reasoning,
            }));
        }
        // 少数网关在流式增量里也返回内容块数组（与非流式路径同样的兼容），
        // 只认字符串形态会让整条响应变成「成功但空回复」。
        let text = text_of(delta.get("content"));
        if !text.is_empty() {
            out.push(Ok(StreamEvent::TextDelta { index: 0, text }));
        }
        // OpenAI 没有独立的拒绝通道：`refusal` 增量并入文本，
        // `finish_reason = content_filter` 已表达语义。
        if let Some(refusal) = delta.get("refusal").and_then(Value::as_str)
            && !refusal.is_empty()
        {
            out.push(Ok(StreamEvent::TextDelta {
                index: 0,
                text: refusal.to_owned(),
            }));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for call in calls {
                self.decode_tool_call(call, out)?;
            }
        }
        Ok(())
    }

    fn decode_tool_call(
        &mut self,
        call: &Value,
        out: &mut Vec<Result<StreamEvent, LlmError>>,
    ) -> Result<(), LlmError> {
        let raw_index = call
            .get("index")
            .and_then(Value::as_u64)
            .ok_or_else(|| LlmError::Stream(t!("llm.err.chat_index_missing").into_owned()))?;
        let index = usize::try_from(raw_index).map_err(|_| {
            LlmError::Stream(t!("llm.err.chat_index_range", raw_index = raw_index).into_owned())
        })?;
        if index >= MAX_TOOL_CALL_SLOTS {
            return Err(LlmError::Stream(
                t!(
                    "llm.err.chat_index_cap",
                    index = index,
                    max = MAX_TOOL_CALL_SLOTS
                )
                .into_owned(),
            ));
        }

        if self.tool_ids.len() <= index {
            self.tool_ids.resize(index + 1, String::new());
            self.tool_names.resize(index + 1, String::new());
            self.tool_started.resize(index + 1, false);
        }
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        if self.tool_ids[index].is_empty() && !id.is_empty() {
            self.tool_ids[index] = id.to_owned();
        }
        // 名称片段累积；少数网关把工具名拆到多个 chunk，此时拼接。
        if let Some(fragment) = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        {
            self.tool_names[index].push_str(fragment);
            if self.tool_started[index] {
                out.push(Ok(StreamEvent::ToolCallSnapshot {
                    index,
                    id: None,
                    name: Some(self.tool_names[index].clone()),
                    arguments: None,
                }));
            }
        }
        // 每个 index 只发一次 `ToolCallStart`，且必须等到 ID 与名称都齐：
        // 提前用空 ID 发射会让参数增量落到空 ID 上，与累加器锁存的真实 ID 分叉。
        if !self.tool_started[index]
            && !self.tool_ids[index].is_empty()
            && !self.tool_names[index].is_empty()
        {
            self.tool_started[index] = true;
            out.push(Ok(StreamEvent::ToolCallStart {
                index,
                id: self.tool_ids[index].clone(),
                name: self.tool_names[index].clone(),
            }));
        }
        let arguments = call
            .pointer("/function/arguments")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !arguments.is_empty() {
            out.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                index,
                json: arguments.to_owned(),
            }));
        }
        Ok(())
    }
}

impl StreamDecoder for ChatStreamDecoder {
    fn decode(&mut self, event: SseEvent, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        let data = event.data.trim();
        if data.is_empty() {
            return;
        }
        if data == "[DONE]" {
            self.terminated = true;
            out.push(Ok(StreamEvent::Done {
                finish_reason: self.finish_reason.take().unwrap_or_default(),
            }));
            return;
        }
        let value: Value = match serde_json::from_str(data) {
            Ok(value) => value,
            Err(err) => {
                self.terminated = true;
                out.push(Err(LlmError::Stream(
                    t!("llm.err.chat_stream_json", err = err).into_owned(),
                )));
                return;
            }
        };
        if value.get("error").is_some_and(|error| !error.is_null())
            || value.get("type").and_then(Value::as_str) == Some("error")
        {
            self.terminated = true;
            let (message, code) = openai::parse_error_value(&value).map_or_else(
                || (t!("llm.err.chat_stream_nomsg").into_owned(), None),
                |fields| (fields.message, fields.code),
            );
            let mut error = ApiError::new(
                None,
                ApiErrorKind::Unknown.refine(code.as_deref(), &message),
                message,
                event.data,
            );
            error.code = code;
            out.push(Err(LlmError::Api(Box::new(error))));
            return;
        }

        if !self.started {
            self.started = true;
            out.push(Ok(StreamEvent::Start {
                id: value.get("id").and_then(Value::as_str).map(str::to_owned),
                model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                effort: super::reported_effort(&value),
            }));
        }
        if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
            out.push(Ok(StreamEvent::Usage(openai::decode_usage(usage))));
        }
        for choice in value
            .get("choices")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            // 公共响应只承载一个候选；与非流式路径一样固定选择 index=0。
            if choice.get("index").and_then(Value::as_u64).unwrap_or(0) != 0 {
                continue;
            }
            if let Some(delta) = choice.get("delta")
                && let Err(err) = self.decode_delta(delta, out)
            {
                self.terminated = true;
                out.push(Err(err));
                return;
            }
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                self.finish_reason = Some(openai::map_finish_reason(reason));
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        out.push(Ok(StreamEvent::Done {
            finish_reason: self.finish_reason.take().unwrap_or_default(),
        }));
    }

    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

#[cfg(test)]
mod tests;

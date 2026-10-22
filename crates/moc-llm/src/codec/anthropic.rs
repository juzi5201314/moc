//! Anthropic Messages 编解码。
//!
//! 两处最容易写错的地方，都在本模块内闭合：
//!
//! 1. **按线上角色合并历史**：`Role::User` 与 `Role::Tool` 线上同为 `user`，
//!    连续的这两类消息合并为一条线上 user 消息（内容块按 IR 顺序拼接），
//!    `Role::Assistant` 从不合并。这既满足 Anthropic 的 user/assistant 交替要求，
//!    也让「工具结果 + 中途插入的用户消息」自然落在同一条消息里。
//! 2. **缓存断点落位**：合并会改变下标，因此规整过程同步产出
//!    「IR 消息下标 → 线上 (消息下标, 内容块下标)」映射，`cache_control` 据此落位。

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::echo::{self, ReasoningEchoPlan};
use super::sse::SseEvent;
use super::{EncodedRequest, ErrorFields, StreamDecoder, common_headers};
use crate::config::{CacheTtl, LlmConfig, ToolChoiceDialect};
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::types::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, ImageSource, Message, ReasoningBlock,
    ReasoningConfig, ReasoningSummary, ResponseFormat, Role, StreamEvent, ToolCall, ToolChoice,
    ToolDef, Usage,
};

/// 默认 API 版本头。
const ANTHROPIC_VERSION: &str = "2023-06-01";
/// 1 小时缓存 TTL 所需的 beta 头。
const EXTENDED_TTL_BETA: &str = "extended-cache-ttl-2025-04-11";
/// 编码请求。
pub(crate) fn encode(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
) -> Result<EncodedRequest, LlmError> {
    let max_tokens = req
        .max_output_tokens
        .ok_or_else(|| LlmError::Encode("Anthropic 协议的 max_output_tokens 是必填项".into()))?;
    let plan = echo::plan(cfg, req);
    let mut normalized = normalize(cfg, req, plan)?;
    let cache = apply_cache(req, &mut normalized)?;

    let mut body = cfg.extra_body.clone();
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
    let output_config = encode_output_config(cfg, req);
    if !output_config.is_empty() {
        body.insert("output_config".into(), Value::Object(output_config));
    }
    if stream {
        body.insert("stream".into(), Value::Bool(true));
    }

    let mut headers = common_headers(cfg, req);
    headers.set_default("anthropic-version", ANTHROPIC_VERSION);
    if cache.needs_extended_ttl_beta {
        headers.set_default("anthropic-beta", EXTENDED_TTL_BETA);
    }

    let body = serde_json::to_vec(&Value::Object(body))
        .map_err(|err| LlmError::Encode(format!("Anthropic 请求体序列化失败: {err}")))?;
    Ok(EncodedRequest {
        url: cfg.endpoint_url(),
        headers: headers.into_vec(),
        body,
    })
}

/// 请求体顶层 `output_config`：推理强度与响应格式。
fn encode_output_config(cfg: &LlmConfig, req: &ChatRequest) -> Map<String, Value> {
    let mut output_config = Map::new();
    if let Some(reasoning) = &req.reasoning
        && let Some(effort) = cfg.wire_effort(Some(reasoning))
    {
        output_config.insert("effort".into(), Value::String(effort));
    }
    if cfg.wire.response_format_dialect != crate::config::ResponseFormatDialect::Unsupported
        && let Some(format) = &req.response_format
    {
        let value = match format {
            ResponseFormat::Text => json!({"type": "text"}),
            ResponseFormat::JsonObject => json!({"type": "json_object"}),
            // Anthropic 的 output_config.format 只有 type + schema，
            // name / description / strict 无对应字段，丢弃。
            ResponseFormat::JsonSchema { schema, .. } => {
                json!({"type": "json_schema", "schema": schema})
            }
        };
        output_config.insert("format".into(), value);
    }
    output_config
}

/// 线上消息与断点落位。
struct Normalized {
    /// 顶层 `system` 的文本块，按出现顺序。
    system_parts: Vec<String>,
    /// 顶层 `system` 的内容块形态：写缓存断点时产生。
    system_override: Option<Value>,
    /// 线上消息。
    messages: Vec<Value>,
    /// IR 消息下标 → 该消息在线上请求中的落点。
    placements: Vec<Option<Placement>>,
}

/// IR 消息在线上请求中的落点。
#[derive(Debug, Clone, Copy)]
enum Placement {
    /// 内容块坐标。
    Block {
        /// 线上消息下标。
        message: usize,
        /// 该 IR 消息首块在线上消息内的内容块下标。
        first_block: usize,
        /// 该 IR 消息自身的内容块数。
        blocks: usize,
    },
    /// 消息被上提到顶层 `system` 字段：断点落在 `system` 边界的最后一块上。
    System {
        /// 该 IR 消息首块在 `system` 块数组中的下标。
        first_block: usize,
        /// 该 IR 消息自身的内容块数。
        blocks: usize,
    },
}

/// 缓存落位结果。
struct CacheOutcome {
    /// 是否因为 1h TTL 需要 beta 头。
    needs_extended_ttl_beta: bool,
}

impl Normalized {
    /// 顶层 `system` 字段：无缓存断点时用字符串，有断点时是内容块数组。
    fn system_field(&self) -> Option<Value> {
        match &self.system_override {
            Some(value) => Some(value.clone()),
            None if self.system_is_empty() => None,
            None => Some(Value::String(self.system_parts.join("\n\n"))),
        }
    }

    /// 系统提示是否为空：全部文本块为空即视为没有系统提示。
    fn system_is_empty(&self) -> bool {
        self.system_parts.iter().all(String::is_empty)
    }

    /// 系统提示的内容块形态：每个文本块一个线上块，顺序不变。
    fn system_blocks(&self) -> Vec<Value> {
        self.system_parts
            .iter()
            .map(|text| json!({"type": "text", "text": text}))
            .collect()
    }

    /// 把 `cache_control` 落到指定线上消息的第 `block` 个内容块上。
    fn attach_message_block(
        &mut self,
        cache_control: &Value,
        message: usize,
        block: usize,
    ) -> Result<(), LlmError> {
        let content = self.messages[message]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .expect("线上消息由本模块构造成内容块数组");
        let len = content.len();
        let Some(target) = content.get_mut(block) else {
            return Err(LlmError::Encode(format!(
                "线上消息只有 {len} 个内容块，缓存断点块下标 {block} 越界"
            )));
        };
        target
            .as_object_mut()
            .expect("内容块由本模块构造成 JSON 对象")
            .insert("cache_control".into(), cache_control.clone());
        Ok(())
    }

    /// 把 `cache_control` 落到 IR 第 `message` 条指令消息的第 `block` 块（`None` 表示最后一块）。
    ///
    /// 指令消息以 system / developer 角色下发时被上提到顶层 `system`，块下标换算到
    /// 该系统数组内；以 user 角色下发时它留在 `messages` 里，块边界与 IR 内容块一一对应。
    fn attach_instruction_block(
        &mut self,
        cache_control: &Value,
        message: usize,
        block: Option<usize>,
    ) -> Result<(), LlmError> {
        match self.placements.get(message).copied().flatten() {
            Some(Placement::System {
                first_block,
                blocks,
            }) => self.attach_system_breakpoint(
                cache_control,
                Some(first_block + block.unwrap_or(blocks - 1)),
            ),
            Some(Placement::Block {
                message: line,
                first_block,
                blocks,
            }) => self.attach_message_block(
                cache_control,
                line,
                first_block + block.unwrap_or(blocks - 1),
            ),
            None => Err(LlmError::Encode(format!(
                "缓存断点下标 {message} 没有对应的指令消息"
            ))),
        }
    }

    /// 把 `cache_control` 落到系统提示的第 `block` 块上；`None` 表示最后一块。
    ///
    /// 最后一块之后即「系统提示 + 工具定义」的边界，是 Anthropic 官方推荐的最小
    /// 缓存单元；更靠前的块用于把与工作目录无关的静态前缀单独缓存。多次调用会累积
    /// 到同一份块数组上，因此可以同时落多个块。
    fn attach_system_breakpoint(
        &mut self,
        cache_control: &Value,
        block: Option<usize>,
    ) -> Result<(), LlmError> {
        let mut blocks = match self.system_override.take() {
            Some(Value::Array(blocks)) => blocks,
            _ => self.system_blocks(),
        };
        if self.system_is_empty() {
            return Err(LlmError::Encode(
                "Anthropic 缓存断点需要非空系统提示才能落位（系统提示为空）".into(),
            ));
        }
        let target = block.unwrap_or(blocks.len() - 1);
        let Some(target_block) = blocks.get_mut(target) else {
            return Err(LlmError::Encode(format!(
                "系统提示只有 {} 个内容块，缓存断点块下标 {target} 越界",
                blocks.len()
            )));
        };
        target_block
            .as_object_mut()
            .expect("system 块由本模块构造成 JSON 对象")
            .insert("cache_control".into(), cache_control.clone());
        self.system_override = Some(Value::Array(blocks));
        Ok(())
    }
}

/// 规整历史：system 上提、按线上角色合并、记录断点落点。
fn normalize(
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
            Role::User | Role::Tool => {
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

fn encode_assistant_blocks(
    plan: ReasoningEchoPlan,
    message: &Message,
) -> Result<Vec<Value>, LlmError> {
    let mut blocks = Vec::with_capacity(message.content.len());
    for part in &message.content {
        match part {
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
                            "Anthropic 协议不支持 Responses 的 reasoning item".into(),
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

fn encode_tool_call(call: &ToolCall) -> Result<Value, LlmError> {
    let input = call.arguments_json()?;
    if !input.is_object() {
        return Err(LlmError::Encode(format!(
            "工具调用 {} 的参数必须是 JSON 对象，收到 {}",
            call.name, call.arguments
        )));
    }
    Ok(json!({
        "type": "tool_use",
        "id": call.id,
        "name": call.name,
        "input": input,
    }))
}

fn encode_user_blocks(_cfg: &LlmConfig, message: &Message) -> Result<Vec<Value>, LlmError> {
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

fn encode_image_source(source: &ImageSource) -> Result<Value, LlmError> {
    match source {
        ImageSource::Base64 { media_type, data } => Ok(json!({
            "type": "base64",
            "media_type": media_type,
            "data": data,
        })),
        ImageSource::Url(url) => Ok(json!({"type": "url", "url": url})),
    }
}

fn encode_tool_def(def: &ToolDef) -> Value {
    let mut object = Map::new();
    object.insert("name".into(), Value::String(def.name.clone()));
    if let Some(description) = &def.description {
        object.insert("description".into(), Value::String(description.clone()));
    }
    object.insert("input_schema".into(), def.parameters.clone());
    Value::Object(object)
}

/// `parallel_tool_calls` 映射为 `tool_choice.disable_parallel_tool_use`。
///
/// `Some(false)` 且未指定 `tool_choice` 时合成 `{"type":"auto",…}`；
/// `Some(true)` 不发送该键（默认即允许并行）。
fn encode_tool_choice(
    cfg: &LlmConfig,
    choice: Option<&ToolChoice>,
    parallel: Option<bool>,
) -> Option<Value> {
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::Unsupported {
        return None;
    }
    let disable_parallel = matches!(parallel, Some(false));
    if choice.is_none() && !disable_parallel {
        return None;
    }
    let mut object = match choice {
        None | Some(ToolChoice::Auto) => json!({"type": "auto"}),
        Some(ToolChoice::None) => json!({"type": "none"}),
        Some(ToolChoice::Required) => json!({"type": "any"}),
        Some(ToolChoice::Tool(name)) => json!({"type": "tool", "name": name}),
    };
    if disable_parallel {
        object
            .as_object_mut()
            .expect("tool_choice 由本模块构造成 JSON 对象")
            .insert("disable_parallel_tool_use".into(), Value::Bool(true));
    }
    Some(object)
}

/// 写入 `thinking` 对象；思考等级通过 `output_config.effort` 发送。
fn encode_thinking(_cfg: &LlmConfig, reasoning: &ReasoningConfig, body: &mut Map<String, Value>) {
    let mut thinking = Map::new();
    let kind = if reasoning.enabled {
        "adaptive"
    } else {
        "disabled"
    };
    thinking.insert("type".into(), Value::String(kind.into()));
    if reasoning.enabled
        && let Some(display) = reasoning.summary.and_then(display_value)
    {
        thinking.insert("display".into(), Value::String(display.into()));
    }
    body.insert("thinking".into(), Value::Object(thinking));
}

/// `ReasoningSummary` → `display`；`None` 表示不发送该键（由模型默认值决定）。
fn display_value(summary: ReasoningSummary) -> Option<&'static str> {
    match summary {
        ReasoningSummary::Omitted => Some("omitted"),
        ReasoningSummary::Summarized
        | ReasoningSummary::Auto
        | ReasoningSummary::Concise
        | ReasoningSummary::Detailed => Some("summarized"),
    }
}

/// 应用缓存配置：校验 TTL、把 `cache_control` 落到正确的内容块上。
fn apply_cache(req: &ChatRequest, normalized: &mut Normalized) -> Result<CacheOutcome, LlmError> {
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
                "Anthropic 的缓存 TTL 只支持 5m 与 1h".into(),
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
        // 的边界，这是 Anthropic 官方推荐的最小缓存单元；指令消息以 user 角色下发
        // 且没有系统提示时，退到首条指令消息的最后一块。
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
                    return Err(LlmError::Encode(format!(
                        "缓存断点下标 {index} 没有对应的线上内容块"
                    )));
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

fn unsupported_part(role: Role, part: &ContentPart) -> LlmError {
    let kind = match part {
        ContentPart::Text(_) => "Text",
        ContentPart::Image { .. } => "Image",
        ContentPart::Reasoning(_) => "Reasoning",
        ContentPart::ToolCall(_) => "ToolCall",
        ContentPart::ToolResult(_) => "ToolResult",
    };
    LlmError::Encode(format!("{role} 消息不允许出现 {kind} 内容块"))
}

/// 非流式响应解码。
pub(crate) fn decode_response(
    _cfg: &LlmConfig,
    _req: &ChatRequest,
    value: &Value,
) -> Result<ChatResponse, LlmError> {
    let mut content = Vec::new();
    for block in value
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match block.get("type").and_then(Value::as_str) {
            Some("text") => content.push(ContentPart::Text(string_at(block, "/text"))),
            Some("thinking") => content.push(ContentPart::Reasoning(ReasoningBlock::Text {
                text: string_at(block, "/thinking"),
                signature: block
                    .get("signature")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })),
            Some("redacted_thinking") => {
                content.push(ContentPart::Reasoning(ReasoningBlock::Redacted {
                    data: string_at(block, "/data"),
                }));
            }
            Some("tool_use") => {
                let input = block.get("input").cloned().unwrap_or(Value::Null);
                content.push(ContentPart::ToolCall(ToolCall {
                    id: string_at(block, "/id"),
                    name: string_at(block, "/name"),
                    arguments: serde_json::to_string(&input).map_err(|err| LlmError::Decode {
                        message: format!("工具调用参数序列化失败: {err}"),
                        body: None,
                    })?,
                }));
            }
            // 服务端工具等本次范围外的块类型：跳过而不是中断整条响应。
            _ => {}
        }
    }

    Ok(ChatResponse {
        id: value.get("id").and_then(Value::as_str).map(str::to_owned),
        model: value
            .get("model")
            .and_then(Value::as_str)
            .map(str::to_owned),
        reasoning_effort: super::reported_effort(value),
        message: Message {
            role: Role::Assistant,
            content,
        },
        finish_reason: value
            .get("stop_reason")
            .and_then(Value::as_str)
            .map_or_default(map_stop_reason),
        usage: decode_usage(value.get("usage")),
        raw: None,
        compaction: None,
    })
}

/// Anthropic 的 `stop_reason` 映射。
///
/// 取值以官方 Messages 文档为准：`end_turn` 是自然结束，`pause_turn` 表示服务端
/// 工具把回合交还给客户端，需要原样重发请求续跑。
fn map_stop_reason(reason: &str) -> FinishReason {
    match reason {
        "end_turn" => FinishReason::Stop,
        "stop_sequence" => FinishReason::StopSequence,
        "max_tokens" | "model_context_window_exceeded" => FinishReason::Length,
        "tool_use" => FinishReason::ToolCalls,
        "pause_turn" => FinishReason::PauseTurn,
        "refusal" => FinishReason::Refusal,
        other => FinishReason::Other(other.to_owned()),
    }
}

/// usage 解码：Anthropic 的 `input_tokens` 不含缓存部分，折入缓存字段归一化为完整输入；
/// `total_tokens` 无对应字段。
fn decode_usage(usage: Option<&Value>) -> Usage {
    let Some(usage) = usage else {
        return Usage::default();
    };
    let number = |key: &str| usage.get(key).and_then(Value::as_u64);
    let cache_read = number("cache_read_input_tokens");
    let cache_creation = number("cache_creation_input_tokens");
    let input = number("input_tokens")
        .unwrap_or(0)
        .saturating_add(cache_read.unwrap_or(0))
        .saturating_add(cache_creation.unwrap_or(0));
    Usage {
        input_tokens: input,
        output_tokens: number("output_tokens").unwrap_or(0),
        total_tokens: None,
        cached_input_tokens: cache_read,
        cache_creation_input_tokens: cache_creation,
        reasoning_tokens: usage
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Value::as_u64),
    }
}

/// 解析 Anthropic 错误体。
pub(crate) fn parse_error_body(body: &[u8]) -> Option<ErrorFields> {
    let value: Value = serde_json::from_slice(body).ok()?;
    parse_error_value(&value)
}

/// 解析 Anthropic 错误体（已解析的 JSON）：`{"error":{"type","message"}}`。
pub(crate) fn parse_error_value(value: &Value) -> Option<ErrorFields> {
    let error = value.get("error")?;
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let code = error.get("type").and_then(Value::as_str);
    if message.is_empty() && code.is_none() {
        return None;
    }
    Some(ErrorFields {
        message: message.to_owned(),
        code: code.map(str::to_owned),
        request_id: value
            .get("request_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

/// Anthropic 错误类型到分类的映射。
///
/// 流中错误事件与 200 响应体里的错误都没有 HTTP 状态码，只能靠错误类型字符串分类。
pub(crate) fn kind_from_error_type(error_type: &str, message: &str) -> ApiErrorKind {
    let base = match error_type {
        "authentication_error" => ApiErrorKind::Auth,
        "permission_error" => ApiErrorKind::Permission,
        "not_found_error" => ApiErrorKind::NotFound,
        "rate_limit_error" => ApiErrorKind::RateLimit,
        "overloaded_error" => ApiErrorKind::Overloaded,
        "api_error" => ApiErrorKind::ServerError,
        "timeout_error" => ApiErrorKind::Timeout,
        "request_too_large" => ApiErrorKind::RequestTooLarge,
        "billing_error" => ApiErrorKind::Billing,
        "invalid_request_error" => ApiErrorKind::InvalidRequest,
        _ => ApiErrorKind::Unknown,
    };
    base.refine(Some(error_type), message)
}

fn string_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Anthropic 流式状态机。
pub(crate) struct AnthropicStreamDecoder {
    /// 首个事件已发出 [`StreamEvent::Start`]。
    started: bool,
    /// 已到终态（`message_stop`、错误或收尾完成）。
    terminated: bool,
    /// `message_delta` 里记录的结束原因。
    stop_reason: Option<FinishReason>,
    /// 服务端工具块（`server_tool_use` / `mcp_tool_use`）的内容块下标。
    ///
    /// 这些块的 `input_json_delta` 由服务端自己执行，不是客户端工具调用，
    /// 必须整段跳过。下标按响应内出现顺序最多几十个，`BTreeSet` 足够。
    server_tool_indices: BTreeSet<usize>,
}

impl AnthropicStreamDecoder {
    /// 构造。
    pub fn new(_cfg: &LlmConfig, _req: &ChatRequest) -> Self {
        Self {
            started: false,
            terminated: false,
            stop_reason: None,
            server_tool_indices: BTreeSet::new(),
        }
    }

    fn decode_payload(&mut self, value: &Value, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "message_start" => {
                if !self.started {
                    self.started = true;
                    out.push(Ok(StreamEvent::Start {
                        id: optional_string_at(value, "/message/id"),
                        model: optional_string_at(value, "/message/model"),
                        effort: super::reported_effort(value.get("message").unwrap_or(value)),
                    }));
                }
                if let Some(usage) = value.pointer("/message/usage").filter(|u| !u.is_null()) {
                    out.push(Ok(StreamEvent::Usage(decode_usage(Some(usage)))));
                }
            }
            "content_block_start" => {
                let index = index_of(value);
                let Some(block) = value.get("content_block") else {
                    return;
                };
                match block.get("type").and_then(Value::as_str) {
                    Some("tool_use") => out.push(Ok(StreamEvent::ToolCallStart {
                        index,
                        id: string_at(block, "/id"),
                        name: string_at(block, "/name"),
                    })),
                    Some("redacted_thinking") => {
                        out.push(Ok(StreamEvent::ReasoningItem {
                            index,
                            item: Box::new(json!({
                                "type": "redacted_thinking",
                                "data": string_at(block, "/data"),
                            })),
                        }));
                    }
                    // 服务端工具由上游自己执行：它的入参不是客户端工具调用，
                    // 记下下标后由 `input_json_delta` 分支整段跳过。
                    Some("server_tool_use" | "mcp_tool_use") => {
                        self.server_tool_indices.insert(index);
                    }
                    // 文本与 thinking 由首个增量隐式开始。
                    _ => {}
                }
            }
            "content_block_delta" => {
                let index = index_of(value);
                let Some(delta) = value.get("delta") else {
                    return;
                };
                match delta.get("type").and_then(Value::as_str) {
                    Some("text_delta") => out.push(Ok(StreamEvent::TextDelta {
                        index,
                        text: string_at(delta, "/text"),
                    })),
                    Some("thinking_delta") => out.push(Ok(StreamEvent::ReasoningDelta {
                        index,
                        text: string_at(delta, "/thinking"),
                    })),
                    Some("signature_delta") => out.push(Ok(StreamEvent::ReasoningSignature {
                        index,
                        signature: string_at(delta, "/signature"),
                    })),
                    Some("input_json_delta") => {
                        // 服务端工具的入参不是客户端工具调用：折进消息会得到
                        // id/name 全空的 ToolCall，下一轮回灌必被上游 400 拒绝。
                        if self.server_tool_indices.contains(&index) {
                            return;
                        }
                        out.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                            index,
                            json: string_at(delta, "/partial_json"),
                        }));
                    }
                    // 引用信息不由本 crate 承载。
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                    self.stop_reason = Some(map_stop_reason(reason));
                }
                if let Some(usage) = value.get("usage").filter(|usage| !usage.is_null()) {
                    out.push(Ok(StreamEvent::Usage(decode_usage(Some(usage)))));
                }
            }
            "message_stop" => {
                self.terminated = true;
                out.push(Ok(StreamEvent::Done {
                    finish_reason: self.stop_reason.take().unwrap_or_default(),
                }));
            }
            "error" => {
                self.terminated = true;
                let error_type = string_at(value, "/error/type");
                let message = string_at(value, "/error/message");
                let mut error = ApiError::new(
                    None,
                    kind_from_error_type(&error_type, &message),
                    message,
                    value.to_string(),
                );
                if !error_type.is_empty() {
                    error.code = Some(error_type);
                }
                out.push(Err(LlmError::Api(error)));
            }
            // `ping`、`content_block_stop` 等一律忽略。
            _ => {}
        }
    }
}

impl StreamDecoder for AnthropicStreamDecoder {
    fn decode(&mut self, event: SseEvent, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        let data = event.data.trim();
        if data.is_empty() {
            return;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(value) => self.decode_payload(&value, out),
            Err(err) => {
                self.terminated = true;
                out.push(Err(LlmError::Stream(format!(
                    "Anthropic 流数据不是合法 JSON: {err}"
                ))));
            }
        }
    }

    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if self.terminated {
            return;
        }
        self.terminated = true;
        out.push(Ok(StreamEvent::Done {
            finish_reason: self.stop_reason.take().unwrap_or_default(),
        }));
    }

    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

/// `content_block_*` 事件的下标。
fn index_of(value: &Value) -> usize {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0)
}

/// 空串表示字段缺失。
fn optional_string_at(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        CacheConfig, InstructionBlock, Protocol, ResponseFormatDialect, WireConfig,
    };
    use crate::types::{Effort, RequestOptions, ToolResult};
    use serde_json::json;

    fn config() -> LlmConfig {
        LlmConfig {
            protocol: Protocol::AnthropicMessages,
            base_url: "https://api.anthropic.com/v1".into(),
            model: "claude-sonnet-4-5".into(),
            auth: crate::config::Auth::Header {
                name: "x-api-key".into(),
                value: "test-key".into(),
            },
            ..Default::default()
        }
    }

    fn base_request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user_text("hi")],
            max_output_tokens: Some(1024),
            ..Default::default()
        }
    }

    fn encode_body(cfg: &LlmConfig, req: &ChatRequest, stream: bool) -> Value {
        let encoded = encode(cfg, req, stream).unwrap();
        assert_eq!(encoded.url, format!("{}/messages", cfg.base_url));
        serde_json::from_slice(&encoded.body).unwrap()
    }

    fn headers_of(cfg: &LlmConfig, req: &ChatRequest) -> Vec<(String, String)> {
        encode(cfg, req, false).unwrap().headers
    }

    fn sse(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
            id: None,
        }
    }

    fn events(
        decoder: &mut AnthropicStreamDecoder,
        frames: &[&str],
    ) -> Vec<Result<StreamEvent, LlmError>> {
        let mut out = Vec::new();
        for frame in frames {
            decoder.decode(sse(frame), &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    fn unwrapped(events: Vec<Result<StreamEvent, LlmError>>) -> Vec<StreamEvent> {
        events.into_iter().map(|event| event.unwrap()).collect()
    }

    fn assistant_with_tools(calls: &[(&str, &str)]) -> Message {
        Message {
            role: Role::Assistant,
            content: calls
                .iter()
                .map(|(id, name)| ContentPart::ToolCall(ToolCall::new(*id, *name, "{}")))
                .collect(),
        }
    }

    #[test]
    fn request_minimal_has_no_optional_fields() {
        let body = encode_body(&config(), &base_request(), false);
        assert_eq!(
            body,
            json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 1024,
                "messages": [{"role": "user", "content": [{"type": "text", "text": "hi"}]}],
            })
        );
    }

    #[test]
    fn request_full_maps_every_field() {
        let cfg = LlmConfig {
            wire: WireConfig {
                supports_stop: true,
                ..Default::default()
            },
            ..config()
        };
        let req = ChatRequest {
            messages: vec![
                Message::system("系统提示"),
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::Text("看图".into()),
                        ContentPart::Image {
                            source: ImageSource::Base64 {
                                media_type: "image/png".into(),
                                data: "AAAA".into(),
                            },
                            detail: None,
                        },
                    ],
                },
            ],
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: Some("查天气".into()),
                parameters: json!({"type": "object"}),
                strict: Some(true),
            }],
            tool_choice: Some(ToolChoice::Auto),
            max_output_tokens: Some(4096),
            temperature: Some(0.4),
            top_p: Some(0.9),
            top_k: Some(20),
            stop: vec!["\n\n".into()],
            reasoning: Some(ReasoningConfig {
                enabled: true,
                effort: Some(crate::types::Effort::High),
                summary: Some(ReasoningSummary::Detailed),
            }),
            response_format: Some(ResponseFormat::JsonSchema {
                name: "weather".into(),
                description: Some("结构".into()),
                schema: json!({"type": "object"}),
                strict: Some(true),
            }),
            options: RequestOptions {
                user_id: Some("user-42".into()),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = encode_body(&cfg, &req, true);
        assert_eq!(
            body,
            json!({
                "model": "claude-sonnet-4-5",
                "max_tokens": 4096,
                "system": "系统提示",
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "看图"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                    ],
                }],
                "tools": [{
                    "name": "get_weather",
                    "description": "查天气",
                    "input_schema": {"type": "object"},
                }],
                "tool_choice": {"type": "auto"},
                "temperature": 0.4,
                "top_p": 0.9,
                "top_k": 20,
                "stop_sequences": ["\n\n"],
                "metadata": {"user_id": "user-42"},
                "thinking": {"type": "adaptive", "display": "summarized"},
                "output_config": {
                    "effort": "high",
                    "format": {"type": "json_schema", "schema": {"type": "object"}},
                },
                "stream": true,
            })
        );
    }

    #[test]
    fn system_and_developer_never_merged_into_messages() {
        let req = ChatRequest {
            messages: vec![
                Message::system("s"),
                Message::developer("d"),
                Message::user_text("u"),
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 1);
        for message in messages {
            assert_ne!(message["role"], json!("system"));
            assert_ne!(message["role"], json!("developer"));
        }
    }

    #[test]
    fn merge_by_wire_role() {
        let req = ChatRequest {
            messages: vec![
                Message::user_text("先查"),
                assistant_with_tools(&[("call_1", "get_weather"), ("call_2", "get_date")]),
                Message::tool_result("call_1", "晴", false),
                Message::tool_result("call_2", "2026-09-24", false),
                Message::user_text("再看这个"),
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "user", "content": [{"type": "text", "text": "先查"}]},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "call_1", "name": "get_weather", "input": {}},
                    {"type": "tool_use", "id": "call_2", "name": "get_date", "input": {}},
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "call_1", "content": "晴", "is_error": false},
                    {"type": "tool_result", "tool_use_id": "call_2", "content": "2026-09-24", "is_error": false},
                    {"type": "text", "text": "再看这个"},
                ]},
            ])
        );
    }

    #[test]
    fn tool_use_arguments_become_object() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    r#"{"city":"北京","days":2}"#,
                ))],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][0]["content"][0],
            json!({
                "type": "tool_use",
                "id": "call_1",
                "name": "get_weather",
                "input": {"city": "北京", "days": 2},
            })
        );

        let broken = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{",
                ))],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &broken, false).unwrap_err();
        assert!(err.to_string().contains("不是合法 JSON"), "{err}");

        let not_object = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "[1,2]",
                ))],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &not_object, false).unwrap_err();
        assert!(err.to_string().contains("必须是 JSON 对象"), "{err}");
    }

    #[test]
    fn image_base64_and_url() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/jpeg".into(),
                            data: "BBBB".into(),
                        },
                        detail: Some(crate::types::ImageDetail::High),
                    },
                    ContentPart::Image {
                        source: ImageSource::Url("https://example.com/a.png".into()),
                        detail: None,
                    },
                ],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
                {"type": "image", "source": {"type": "url", "url": "https://example.com/a.png"}},
            ])
        );
    }

    /// reasoning 配置 → thinking 与 output_config.effort 的映射。
    #[test]
    fn thinking_and_effort_map_to_wire_fields() {
        let cases: [(Option<ReasoningConfig>, Option<Value>, Option<Value>); 4] = [
            (
                Some(ReasoningConfig {
                    enabled: true,
                    effort: Some(Effort::High),
                    summary: Some(ReasoningSummary::Omitted),
                }),
                Some(json!({"type": "adaptive", "display": "omitted"})),
                Some(json!({"effort": "high"})),
            ),
            (
                Some(ReasoningConfig {
                    enabled: true,
                    effort: Some(Effort::Medium),
                    ..Default::default()
                }),
                Some(json!({"type": "adaptive"})),
                Some(json!({"effort": "medium"})),
            ),
            (
                Some(ReasoningConfig {
                    enabled: false,
                    effort: Some(Effort::Medium),
                    ..Default::default()
                }),
                Some(json!({"type": "disabled"})),
                None,
            ),
            (None, None, None),
        ];
        for (reasoning, thinking, effort) in cases {
            let req = ChatRequest {
                reasoning,
                ..base_request()
            };
            let body = encode_body(&config(), &req, false);
            assert_eq!(body.get("thinking"), thinking.as_ref(), "{body}");
            match effort {
                Some(effort) => assert_eq!(body.get("output_config"), Some(&effort), "{body}"),
                None => assert!(body.get("output_config").is_none(), "{body}"),
            }
        }
    }

    #[test]
    fn response_format_unsupported_dialect_skips_format() {
        let req = ChatRequest {
            response_format: Some(ResponseFormat::JsonObject),
            ..base_request()
        };
        let cfg = LlmConfig {
            wire: WireConfig {
                response_format_dialect: ResponseFormatDialect::Unsupported,
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert!(body.get("output_config").is_none());
    }

    #[test]
    fn static_system_block_takes_its_own_cache_breakpoint() {
        let system = Message {
            role: Role::System,
            content: vec![
                ContentPart::Text("模板".into()),
                ContentPart::Text("项目".into()),
            ],
        };
        // 无断点：多块系统提示拍平成空行分隔的字符串，与单块形态逐字一致。
        let plain = ChatRequest {
            messages: vec![system.clone(), Message::user_text("hi")],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &plain, false);
        assert_eq!(body["system"], json!("模板\n\n项目"));

        // 消息断点落在系统末块，静态块断点落在第 0 块，两者可以同时存在。
        let req = ChatRequest {
            messages: vec![system, Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![0],
                    instruction_blocks: vec![InstructionBlock {
                        message: 0,
                        block: 0,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "模板", "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "项目", "cache_control": {"type": "ephemeral"}},
            ])
        );

        // 越界的静态块下标报错，而不是静默丢断点。
        let bad = ChatRequest {
            messages: vec![Message::system("只有一块"), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    instruction_blocks: vec![InstructionBlock {
                        message: 0,
                        block: 3,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("越界"), "{err}");
    }

    #[test]
    fn user_role_instruction_blocks_land_on_their_own_message() {
        // 指令消息以 user 角色下发（上游对 system 遵循度不足时的模型配置）：它留在
        // `messages` 里，块断点按块下标落在该消息上，而不是要求非空系统提示。
        let req = ChatRequest {
            messages: vec![
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::Text("工具政策".into()),
                        ContentPart::Text("仓库规则".into()),
                    ],
                },
                Message::user_text("hi"),
            ],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![0],
                    instruction_blocks: vec![InstructionBlock {
                        message: 0,
                        block: 0,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        // 相邻 user 消息合并：静态前缀断点落在第 0 块，消息断点落在指令末块上。
        assert!(body.get("system").is_none());
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [
                    {"type": "text", "text": "工具政策", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "仓库规则", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "hi"},
                ],
            }])
        );
    }

    #[test]
    fn split_instruction_messages_keep_their_own_breakpoints() {
        // 指令拆成 system 组与 user 组两条消息：系统侧的边界断点落在顶层 `system`，
        // user 组的块断点落在它自己的消息块上，互不干扰。
        let req = ChatRequest {
            messages: vec![
                Message {
                    role: Role::System,
                    content: vec![
                        ContentPart::Text("身份".into()),
                        ContentPart::Text("环境".into()),
                    ],
                },
                Message {
                    role: Role::User,
                    content: vec![
                        ContentPart::Text("工具政策".into()),
                        ContentPart::Text("仓库规则".into()),
                    ],
                },
                Message::user_text("hi"),
            ],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![0, 2],
                    instruction_blocks: vec![InstructionBlock {
                        message: 1,
                        block: 0,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "身份"},
                {"type": "text", "text": "环境", "cache_control": {"type": "ephemeral"}},
            ])
        );
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [
                    {"type": "text", "text": "工具政策", "cache_control": {"type": "ephemeral"}},
                    {"type": "text", "text": "仓库规则"},
                    {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}},
                ],
            }])
        );
    }

    #[test]
    fn cache_control_top_level_and_breakpoints() {
        // 无断点：cache_control 落在顶层 system 的最后一块。
        let req = ChatRequest {
            messages: vec![Message::system("系统提示"), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig::default()),
                ..Default::default()
            },
            ..base_request()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["system"],
            json!([{"type": "text", "text": "系统提示", "cache_control": {"type": "ephemeral"}}])
        );

        // 有断点：落在该 IR 消息的最后一个内容块上（跨合并仍然正确）。
        let req = ChatRequest {
            messages: vec![
                Message::system("系统提示"),
                Message::user_text("先查"),
                assistant_with_tools(&[("call_1", "get_weather")]),
                Message::tool_result("call_1", "晴", false),
                Message::user_text("继续"),
            ],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![4],
                    ttl: Some(CacheTtl::OneHour),
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["system"], json!("系统提示"));
        let content = body["messages"][2]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(
            content[1],
            json!({"type": "text", "text": "继续", "cache_control": {"type": "ephemeral", "ttl": "1h"}})
        );
        assert!(content[0].get("cache_control").is_none());

        // 1h TTL 自动补 beta 头，且不覆盖 core 的显式配置。
        let headers = headers_of(&config(), &req);
        assert!(
            headers.contains(&("anthropic-beta".to_string(), EXTENDED_TTL_BETA.to_string())),
            "{headers:?}"
        );

        // 不支持的 TTL 报错。
        let bad = ChatRequest {
            messages: vec![Message::system("s"), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    ttl: Some(CacheTtl::ThirtyMinutes),
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("只支持 5m 与 1h"), "{err}");

        // 断点指向不存在的线上块时报错。
        let bad = ChatRequest {
            messages: vec![Message::system("s"), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![9],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("没有对应的线上内容块"), "{err}");

        // 无系统提示且无断点：退到首条消息的最后一块，而不是报错。
        let plain = ChatRequest {
            messages: vec![Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig::default()),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &plain, false);
        assert!(body.get("system").is_none());
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "hi",
                    "cache_control": {"type": "ephemeral"},
                }],
            }])
        );

        // 指令块断点指向不存在的指令消息时报错，而不是静默丢弃断点。
        let bad = ChatRequest {
            messages: vec![Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    instruction_blocks: vec![InstructionBlock {
                        message: 5,
                        block: 0,
                    }],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("没有对应的指令消息"), "{err}");
    }

    #[test]
    fn cache_breakpoint_on_hoisted_system_message_lands_on_system_boundary() {
        // core 对 Anthropic 恒定给出 `[0, …]`，而 `messages[0]` 是系统提示：
        // 断点落在被上提的消息上时必须落在 `system` 边界，而不是报「没有对应的
        // 线上内容块」——否则每个请求都在本地编码阶段失败。
        let req = ChatRequest {
            messages: vec![
                Message::system("系统提示"),
                Message::developer("补充规则"),
                Message::user_text("hi"),
            ],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![0, 2],
                    ttl: Some(CacheTtl::FiveMinutes),
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        // 系统与开发者消息各自成块，断点按「系统边界」语义落在最后一块上。
        assert_eq!(
            body["system"],
            json!([
                {"type": "text", "text": "系统提示"},
                {
                    "type": "text",
                    "text": "补充规则",
                    "cache_control": {"type": "ephemeral", "ttl": "5m"},
                }
            ])
        );
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [{
                    "type": "text",
                    "text": "hi",
                    "cache_control": {"type": "ephemeral", "ttl": "5m"},
                }],
            }])
        );

        // 断点落在系统提示上但系统提示为空：没有可落位的块，报错而不是静默丢弃。
        let bad = ChatRequest {
            messages: vec![Message::system(""), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![0],
                    ..Default::default()
                }),
                ..Default::default()
            },
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("系统提示为空"), "{err}");
    }

    #[test]
    fn reasoning_blocks_round_trip_with_signature() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning(ReasoningBlock::Text {
                        text: "先想一想".into(),
                        signature: Some("sig-1".into()),
                    }),
                    ContentPart::Reasoning(ReasoningBlock::Redacted {
                        data: "opaque".into(),
                    }),
                    ContentPart::Text("答案".into()),
                ],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "thinking", "thinking": "先想一想", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "答案"},
            ])
        );

        // echo = Never 时 thinking 与 redacted_thinking 一并剔除，其余块保持顺序。
        let req = ChatRequest {
            options: RequestOptions {
                echo_reasoning: crate::types::ReasoningEcho::Never,
                ..Default::default()
            },
            ..req
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][0]["content"],
            json!([{"type": "text", "text": "答案"}])
        );
    }

    #[test]
    fn unsigned_thinking_block_is_dropped() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Reasoning(ReasoningBlock::text("无签名的思考")),
                    ContentPart::Text("正文".into()),
                ],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][0]["content"],
            json!([{"type": "text", "text": "正文"}])
        );
    }

    #[test]
    fn reasoning_only_assistant_message_is_dropped() {
        let req = ChatRequest {
            messages: vec![
                Message::user_text("问题"),
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::Reasoning(ReasoningBlock::text("只有思考"))],
                },
                Message::user_text("继续"),
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        // 空 assistant 消息在线上非法，直接丢弃；两侧 user 消息因此合并。
        assert_eq!(
            body["messages"],
            json!([{
                "role": "user",
                "content": [
                    {"type": "text", "text": "问题"},
                    {"type": "text", "text": "继续"},
                ],
            }])
        );
    }

    #[test]
    fn responses_reasoning_item_is_rejected() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                    json!({"type": "reasoning", "id": "rs_1"}),
                )))],
            }],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let err = encode(&config(), &req, false).unwrap_err();
        assert!(err.to_string().contains("reasoning item"), "{err}");
    }

    #[test]
    fn response_decode_reads_thinking_text_tools_and_usage() {
        let body = json!({
            "id": "msg_1",
            "type": "message",
            "role": "assistant",
            "model": "claude-sonnet-4-5",
            "content": [
                {"type": "thinking", "thinking": "先算一下", "signature": "sig-1"},
                {"type": "redacted_thinking", "data": "opaque"},
                {"type": "text", "text": "答案是 42"},
                {"type": "tool_use", "id": "toolu_1", "name": "get_weather", "input": {"city": "北京"}},
            ],
            "stop_reason": "tool_use",
            "usage": {
                "input_tokens": 100,
                "output_tokens": 40,
                "cache_read_input_tokens": 64,
                "cache_creation_input_tokens": 16,
                "output_tokens_details": {"thinking_tokens": 12},
            },
        });
        let response = decode_response(&config(), &base_request(), &body).unwrap();
        assert_eq!(response.id.as_deref(), Some("msg_1"));
        assert_eq!(response.model.as_deref(), Some("claude-sonnet-4-5"));
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 180,
                output_tokens: 40,
                total_tokens: None,
                cached_input_tokens: Some(64),
                cache_creation_input_tokens: Some(16),
                reasoning_tokens: Some(12),
            }
        );
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Reasoning(ReasoningBlock::Text {
                    text: "先算一下".into(),
                    signature: Some("sig-1".into()),
                }),
                ContentPart::Reasoning(ReasoningBlock::Redacted {
                    data: "opaque".into(),
                }),
                ContentPart::Text("答案是 42".into()),
                ContentPart::ToolCall(ToolCall::new(
                    "toolu_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
            ]
        );
    }

    #[test]
    fn stream_maps_official_stop_reason_values() {
        // `end_turn` 必须落成自然结束；落成 `Other` 会让 core 把正常结束当异常。
        for (wire, expected) in [
            ("end_turn", FinishReason::Stop),
            ("pause_turn", FinishReason::PauseTurn),
        ] {
            let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
            let events = unwrapped(events(
                &mut decoder,
                &[
                    r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5"}}"#,
                    r#"{"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"写完"}}"#,
                    &format!(
                        r#"{{"type":"message_delta","delta":{{"stop_reason":"{wire}"}},"usage":{{"output_tokens":4}}}}"#
                    ),
                    r#"{"type":"message_stop"}"#,
                ],
            ));
            assert_eq!(
                events.last(),
                Some(&StreamEvent::Done {
                    finish_reason: expected,
                }),
                "{wire}"
            );
        }
    }

    #[test]
    fn error_body_table() {
        let cfg = config();
        let cases = [
            (
                400,
                r#"{"type":"error","error":{"type":"invalid_request_error","message":"prompt is too long: 300000 tokens > 200000 maximum"},"request_id":"req_1"}"#,
                ApiErrorKind::ContextLength,
            ),
            (
                401,
                r#"{"type":"error","error":{"type":"authentication_error","message":"invalid x-api-key"}}"#,
                ApiErrorKind::Auth,
            ),
            (
                403,
                r#"{"type":"error","error":{"type":"permission_error","message":"forbidden"}}"#,
                ApiErrorKind::Permission,
            ),
            (
                413,
                r#"{"type":"error","error":{"type":"request_too_large","message":"too big"}}"#,
                ApiErrorKind::RequestTooLarge,
            ),
            (
                429,
                r#"{"type":"error","error":{"type":"rate_limit_error","message":"slow down"}}"#,
                ApiErrorKind::RateLimit,
            ),
            (
                529,
                r#"{"type":"error","error":{"type":"overloaded_error","message":"overloaded"}}"#,
                ApiErrorKind::Overloaded,
            ),
            (
                500,
                r#"{"type":"error","error":{"type":"api_error","message":"boom"}}"#,
                ApiErrorKind::ServerError,
            ),
            (
                402,
                r#"{"type":"error","error":{"type":"billing_error","message":"payment required"}}"#,
                ApiErrorKind::Billing,
            ),
            (
                409,
                r#"{"type":"error","error":{"type":"conflict_error","message":"conflict"}}"#,
                ApiErrorKind::Conflict,
            ),
        ];
        for (status, body, expected) in cases {
            let error = super::super::decode_error(
                &cfg,
                status,
                &reqwest::header::HeaderMap::new(),
                body.as_bytes(),
            );
            assert_eq!(error.kind, expected, "status {status}: {body}");
        }

        // request_id 来自 body 的顶层字段。
        let error = super::super::decode_error(
            &cfg,
            400,
            &reqwest::header::HeaderMap::new(),
            br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"},"request_id":"req_anthropic"}"#,
        );
        assert_eq!(error.request_id.as_deref(), Some("req_anthropic"));
        assert_eq!(error.code.as_deref(), Some("invalid_request_error"));
    }

    #[test]
    fn stream_event_sequence() {
        let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":1}}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":""}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":"想"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":"sig"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"text","text":""}}"#,
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"答案"}}"#,
                r#"{"type":"content_block_stop","index":1}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{\"city\":"}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"\"北京\"}"}}"#,
                r#"{"type":"content_block_stop","index":2}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"},"usage":{"output_tokens":40}}"#,
                r#"{"type":"message_stop"}"#,
                // 终态之后的帧一律丢弃。
                r#"{"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"多余"}}"#,
            ],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: Some("msg_1".into()),
                    model: Some("claude-sonnet-4-5".into()),
                    effort: None,
                },
                StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 1,
                    ..Default::default()
                }),
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "想".into(),
                },
                StreamEvent::ReasoningSignature {
                    index: 0,
                    signature: "sig".into(),
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "答案".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 2,
                    id: "toolu_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 2,
                    json: "{\"city\":".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 2,
                    json: "\"北京\"}".into(),
                },
                StreamEvent::Usage(Usage {
                    output_tokens: 40,
                    ..Default::default()
                }),
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        );
    }

    #[test]
    fn stream_skips_server_tool_arguments() {
        // 服务端工具（web_search / MCP）由上游自己执行，其 input_json_delta
        // 不是客户端工具调用；产出 id/name 全空的 ToolCall 后下一轮必 400。
        let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"message_start","message":{"id":"msg_1","model":"claude-sonnet-4-5"}}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"server_tool_use","id":"srvtoolu_1","name":"web_search"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"query\":\"moc\"}"}}"#,
                r#"{"type":"content_block_stop","index":0}"#,
                r#"{"type":"content_block_start","index":1,"content_block":{"type":"web_search_tool_result","tool_use_id":"srvtoolu_1","content":[]}}"#,
                r#"{"type":"content_block_start","index":2,"content_block":{"type":"mcp_tool_use","id":"mcptoolu_1","name":"remote"}}"#,
                r#"{"type":"content_block_delta","index":2,"delta":{"type":"input_json_delta","partial_json":"{}"}}"#,
                r#"{"type":"content_block_start","index":3,"content_block":{"type":"tool_use","id":"toolu_1","name":"get_weather"}}"#,
                r#"{"type":"content_block_delta","index":3,"delta":{"type":"input_json_delta","partial_json":"{\"city\":\"北京\"}"}}"#,
                r#"{"type":"message_delta","delta":{"stop_reason":"tool_use"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: Some("msg_1".into()),
                    model: Some("claude-sonnet-4-5".into()),
                    effort: None,
                },
                StreamEvent::ToolCallStart {
                    index: 3,
                    id: "toolu_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 3,
                    json: "{\"city\":\"北京\"}".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        );
    }

    #[test]
    fn stream_redacted_thinking_and_ping() {
        let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"ping"}"#,
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"redacted_thinking","data":"opaque"}}"#,
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"citations_delta","citation":{}}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::ReasoningItem {
                    index: 0,
                    item: Box::new(json!({"type": "redacted_thinking", "data": "opaque"})),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Other("unknown".into()),
                },
            ]
        );
    }

    #[test]
    fn stream_error_event_terminates() {
        let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
        let out = events(
            &mut decoder,
            &[
                r#"{"type":"error","error":{"type":"overloaded_error","message":"Overloaded"}}"#,
                r#"{"type":"message_stop"}"#,
            ],
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Err(LlmError::Api(error)) => {
                assert_eq!(error.kind, ApiErrorKind::Overloaded);
                assert_eq!(error.message, "Overloaded");
                assert_eq!(error.status, None);
                assert!(error.is_retryable());
            }
            other => panic!("期望 Api 错误，实际 {other:?}"),
        }
        assert!(decoder.is_terminated());
    }

    #[test]
    fn stream_without_message_stop_finishes_at_eof() {
        let mut decoder = AnthropicStreamDecoder::new(&config(), &base_request());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"message_delta","delta":{"stop_reason":"max_tokens"},"usage":{"output_tokens":9}}"#,
            ],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::Usage(Usage {
                    output_tokens: 9,
                    ..Default::default()
                }),
                StreamEvent::Done {
                    finish_reason: FinishReason::Length,
                },
            ]
        );
    }

    #[test]
    fn tool_result_error_flag_is_encoded() {
        let req = ChatRequest {
            messages: vec![
                assistant_with_tools(&[("call_1", "get_weather")]),
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult(ToolResult {
                        tool_call_id: "call_1".into(),
                        content: "工具炸了".into(),
                        is_error: true,
                        images: Vec::new(),
                    })],
                },
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": "工具炸了",
                "is_error": true,
            })
        );
    }

    #[test]
    fn tool_result_images_upgrade_content_to_blocks() {
        let req = ChatRequest {
            messages: vec![
                assistant_with_tools(&[("call_1", "screenshot")]),
                Message::tool_result_with_images(
                    "call_1",
                    "截图如下",
                    false,
                    vec![ImageSource::Base64 {
                        media_type: "image/png".into(),
                        data: "AAAA".into(),
                    }],
                ),
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][1]["content"][0],
            json!({
                "type": "tool_result",
                "tool_use_id": "call_1",
                "content": [
                    {"type": "text", "text": "截图如下"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}},
                ],
                "is_error": false,
            })
        );

        // 无图结果保持字符串形态，不引入结构变化。
        let plain = ChatRequest {
            messages: vec![
                assistant_with_tools(&[("call_1", "screenshot")]),
                Message::tool_result("call_1", "纯文本", false),
            ],
            max_output_tokens: Some(1024),
            ..Default::default()
        };
        let body = encode_body(&config(), &plain, false);
        assert_eq!(
            body["messages"][1]["content"][0]["content"],
            json!("纯文本")
        );
    }
}

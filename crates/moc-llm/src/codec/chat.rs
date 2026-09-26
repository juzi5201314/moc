//! OpenAI Chat Completions 编解码。
//!
//! 线上字段映射见 crate 计划中的映射表；本模块只发送表中列出的字段，未列出的
//! 一律不发送（厂商扩展走 [`crate::LlmConfig::extra_body`]）。

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::echo::{self, ReasoningEchoPlan};
use super::openai;
use super::sse::SseEvent;
use super::{EncodedRequest, StreamDecoder, common_headers};
use crate::config::{
    CacheTtl, LlmConfig, MaxTokensField, ResponseFormatDialect, ToolChoiceDialect,
};
use crate::error::LlmError;
use crate::types::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, ImageDetail, ImageSource, Message,
    ReasoningBlock, ReasoningConfig, ResponseFormat, Role, StreamEvent, ToolCall, ToolChoice,
    ToolDef,
};

/// 编码请求。
pub(crate) fn encode(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
) -> Result<EncodedRequest, LlmError> {
    let plan = echo::plan(cfg, req);
    let breakpoints = cache_breakpoints(req, req.messages.len())?;

    let mut body = cfg.extra_body.clone();
    body.insert("model".into(), Value::String(cfg.model.clone()));
    body.insert(
        "messages".into(),
        Value::Array(encode_messages(cfg, req, plan, &breakpoints)?),
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
            body.insert(
                "prompt_cache_options".into(),
                json!({"mode": "explicit", "ttl": ttl.as_str()}),
            );
        }
    }
    if let Some(store) = req.options.store {
        body.insert("store".into(), Value::Bool(store));
    }
    if stream {
        body.insert("stream".into(), Value::Bool(true));
        body.insert("stream_options".into(), json!({"include_usage": true}));
    }

    let body = serde_json::to_vec(&Value::Object(body))
        .map_err(|err| LlmError::Encode(format!("Chat 请求体序列化失败: {err}")))?;
    Ok(EncodedRequest {
        url: cfg.endpoint_url(),
        headers: common_headers(cfg, req).into_vec(),
        body,
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
        return Err(LlmError::Encode(format!(
            "Chat 协议的显式缓存只支持 30m，收到 {}",
            ttl.as_str()
        )));
    }
    let mut breakpoints = BTreeSet::new();
    for &index in &cache.breakpoints {
        if index >= message_count {
            return Err(LlmError::Encode(format!(
                "缓存断点下标 {index} 超出消息范围（共 {message_count} 条）"
            )));
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
) -> Result<Vec<Value>, LlmError> {
    let mut out = Vec::with_capacity(req.messages.len());
    for (index, message) in req.messages.iter().enumerate() {
        let breakpoint = breakpoints.contains(&index);
        match message.role {
            Role::System | Role::Developer => {
                out.push(encode_instruction(cfg, message, breakpoint)?);
            }
            Role::User => out.push(encode_user(cfg, message, breakpoint)?),
            Role::Assistant => out.push(encode_assistant(cfg, plan, message, breakpoint)?),
            // tool 消息携带的图片无法进入 tool 角色，编码为紧随其后的 user 消息。
            Role::Tool => out.extend(encode_tool_result(cfg, message, breakpoint)?),
        }
    }
    Ok(out)
}

fn encode_instruction(
    cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Value, LlmError> {
    let mut text = String::new();
    for part in &message.content {
        match part {
            ContentPart::Text(chunk) => text.push_str(chunk),
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    let role = if message.role == Role::Developer && cfg.wire.developer_role {
        "developer"
    } else {
        "system"
    };
    Ok(json!({"role": role, "content": finish_text_content(text, breakpoint)?}))
}

fn encode_user(cfg: &LlmConfig, message: &Message, breakpoint: bool) -> Result<Value, LlmError> {
    let mut blocks = Vec::with_capacity(message.content.len());
    for part in &message.content {
        match part {
            ContentPart::Text(chunk) => blocks.push(json!({"type": "text", "text": chunk})),
            ContentPart::Image { source, detail } => {
                blocks.push(encode_image(cfg, source, *detail)?);
            }
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    Ok(json!({"role": "user", "content": finish_content(blocks, breakpoint)?}))
}

fn encode_image(
    cfg: &LlmConfig,
    source: &ImageSource,
    detail: Option<ImageDetail>,
) -> Result<Value, LlmError> {
    let url = match source {
        ImageSource::Url(url) => {
            if !cfg.wire.image_url {
                return Err(LlmError::Encode(
                    "该线格式不支持图片 URL 来源，请改传 base64".into(),
                ));
            }
            url.clone()
        }
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
    cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Vec<Value>, LlmError> {
    let Some(result) = message.tool_results().next() else {
        return Err(LlmError::Encode("tool 消息缺少 ToolResult 内容块".into()));
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
        "text": format!("[工具 {} 返回的图片]", result.tool_call_id),
    }));
    for source in &result.images {
        blocks.push(encode_image(cfg, source, None)?);
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
/// `enabled == false` 时只写「关闭」开关，不写强度：同时发送强度与关闭是自相矛盾
/// 的配置。`toggle_field` 为 `None` 的线格式完全不发送思考字段。
fn encode_thinking(cfg: &LlmConfig, reasoning: &ReasoningConfig, body: &mut Map<String, Value>) {
    let wire = &cfg.wire.thinking;
    if let Some(field) = &wire.toggle_field {
        let value = if reasoning.enabled {
            &wire.enabled_value
        } else {
            &wire.disabled_value
        };
        let mut object = wire.toggle_extra.clone();
        object.insert("type".into(), Value::String(value.clone()));
        body.insert(field.clone(), Value::Object(object));
    }
    if !reasoning.enabled {
        return;
    }
    if let (Some(field), Some(effort)) = (&wire.effort_field, reasoning.effort) {
        body.insert(field.clone(), Value::String(wire.effort_value(effort)));
    }
}

/// 把一组内容块收尾：附加缓存断点，必要时降级为纯字符串。
fn finish_content(mut blocks: Vec<Value>, breakpoint: bool) -> Result<Value, LlmError> {
    if breakpoint {
        let Some(last) = blocks.last_mut() else {
            return Err(LlmError::Encode("缓存断点落在空消息上".into()));
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
        ContentPart::Image { .. } => "Image",
        ContentPart::Reasoning(_) => "Reasoning",
        ContentPart::ToolCall(_) => "ToolCall",
        ContentPart::ToolResult(_) => "ToolResult",
    };
    LlmError::Encode(format!("{role} 消息不允许出现 {kind} 内容块"))
}

/// 非流式响应解码。
pub(crate) fn decode_response(
    cfg: &LlmConfig,
    _req: &ChatRequest,
    value: &Value,
) -> Result<ChatResponse, LlmError> {
    let choice = value.pointer("/choices/0");
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
        message: Message {
            role: Role::Assistant,
            content,
        },
        finish_reason,
        usage: value.get("usage").map_or_default(openai::decode_usage),
        raw: None,
        compaction: None,
    })
}

fn string_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 取出文本：字符串直接取，数组取其中 `text` 字段拼接（部分网关返回内容块数组）。
fn text_of(value: Option<&Value>) -> String {
    match value {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => {
            let mut out = String::new();
            for part in parts {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
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
    /// 并行调用数（个位数），因此用 `Vec` 直接下标寻址，不做哈希。
    tool_ids: Vec<String>,
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
        if let Some(text) = delta.get("content").and_then(Value::as_str)
            && !text.is_empty()
        {
            out.push(Ok(StreamEvent::TextDelta {
                index: 0,
                text: text.to_owned(),
            }));
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
            .ok_or_else(|| LlmError::Stream("Chat 工具调用增量缺少 index".into()))?;
        let index = usize::try_from(raw_index)
            .map_err(|_| LlmError::Stream(format!("Chat 工具调用 index 超出范围: {raw_index}")))?;
        debug_assert!(index < 1024, "工具调用 index 应稠密且很小，实际 {index}");

        if self.tool_ids.len() <= index {
            self.tool_ids.resize(index + 1, String::new());
        }
        let id = call.get("id").and_then(Value::as_str).unwrap_or_default();
        if self.tool_ids[index].is_empty() && !id.is_empty() {
            self.tool_ids[index] = id.to_owned();
        }
        let name = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !name.is_empty() {
            out.push(Ok(StreamEvent::ToolCallStart {
                index,
                id: self.tool_ids[index].clone(),
                name: name.to_owned(),
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
                out.push(Err(LlmError::Stream(format!(
                    "Chat 流数据不是合法 JSON: {err}"
                ))));
                return;
            }
        };

        if !self.started {
            self.started = true;
            out.push(Ok(StreamEvent::Start {
                id: value.get("id").and_then(Value::as_str).map(str::to_owned),
                model: value
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
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
mod tests {
    use super::*;
    use crate::config::WireConfig;
    use crate::types::{RequestOptions, Usage};
    use serde_json::json;

    fn config() -> LlmConfig {
        LlmConfig {
            protocol: crate::config::Protocol::OpenAiChat,
            base_url: "https://api.deepseek.com/v1".into(),
            model: "test-model".into(),
            ..Default::default()
        }
    }

    fn encode_body(cfg: &LlmConfig, req: &ChatRequest, stream: bool) -> Value {
        let encoded = encode(cfg, req, stream).unwrap();
        assert_eq!(encoded.url, format!("{}/chat/completions", cfg.base_url));
        serde_json::from_slice(&encoded.body).unwrap()
    }

    fn sse(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
            id: None,
        }
    }

    fn feed(
        decoder: &mut ChatStreamDecoder,
        frames: &[&str],
    ) -> Vec<Result<StreamEvent, LlmError>> {
        let mut out = Vec::new();
        for frame in frames {
            decoder.decode(sse(frame), &mut out);
        }
        decoder.finish(&mut out);
        out
    }

    fn events(decoder: &mut ChatStreamDecoder, frames: &[&str]) -> Vec<StreamEvent> {
        feed(decoder, frames)
            .into_iter()
            .map(|event| event.unwrap())
            .collect()
    }

    #[test]
    fn request_minimal_sends_only_model_and_messages() {
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body,
            json!({
                "model": "test-model",
                "messages": [{"role": "user", "content": "hi"}],
            })
        );
    }

    #[test]
    fn explicit_empty_assistant_blocks_preserve_required_wire_fields() {
        let mut request = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![
                    ContentPart::Text(String::new()),
                    ContentPart::Reasoning(ReasoningBlock::text("")),
                ],
            }],
            options: RequestOptions {
                echo_reasoning: crate::types::ReasoningEcho::Always,
                ..Default::default()
            },
            ..Default::default()
        };
        let body = encode_body(&config(), &request, false);
        assert_eq!(body["messages"][0]["content"], "");
        assert_eq!(body["messages"][0]["reasoning_content"], "");
        request.options.echo_reasoning = crate::types::ReasoningEcho::Never;
        let stripped = encode_body(&config(), &request, false);
        assert!(stripped["messages"][0].get("reasoning_content").is_none());
        assert_eq!(stripped["messages"][0]["content"], "");
    }

    #[test]
    fn request_full_maps_every_field() {
        let cfg = LlmConfig {
            wire: WireConfig {
                developer_role: true,
                ..Default::default()
            },
            ..config()
        };
        let req = ChatRequest {
            messages: vec![
                Message::system("系统提示"),
                Message::developer("开发者提示"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning(ReasoningBlock::text("上一轮思路")),
                        ContentPart::Text("调用工具".into()),
                        ContentPart::ToolCall(ToolCall::new(
                            "call_1",
                            "get_weather",
                            r#"{"city":"北京"}"#,
                        )),
                    ],
                },
                Message::tool_result("call_1", "晴", false),
                Message::user_text("谢谢"),
            ],
            native_input: None,
            tools: vec![ToolDef {
                name: "get_weather".into(),
                description: Some("查天气".into()),
                parameters: json!({"type": "object", "properties": {"city": {"type": "string"}}}),
                strict: Some(true),
            }],
            tool_choice: Some(ToolChoice::Auto),
            max_output_tokens: Some(2048),
            temperature: Some(0.3),
            top_p: Some(0.9),
            // Chat 没有 top_k 标准字段：核心想传时用 extra_body 表达，
            // 本 crate 不猜测厂商是否支持。
            top_k: Some(20),
            stop: vec!["\n\n".into()],
            reasoning: Some(ReasoningConfig {
                enabled: true,
                effort: Some(crate::types::Effort::High),
                ..Default::default()
            }),
            response_format: Some(ResponseFormat::JsonObject),
            parallel_tool_calls: Some(false),
            options: RequestOptions {
                store: Some(false),
                ..Default::default()
            },
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(
            body,
            json!({
                "model": "test-model",
                "messages": [
                    {"role": "system", "content": "系统提示"},
                    {"role": "developer", "content": "开发者提示"},
                    {
                        "role": "assistant",
                        "content": "调用工具",
                        "tool_calls": [{
                            "id": "call_1",
                            "type": "function",
                            "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
                        }],
                        "reasoning_content": "上一轮思路",
                    },
                    {"role": "tool", "tool_call_id": "call_1", "content": "晴"},
                    {"role": "user", "content": "谢谢"},
                ],
                "tools": [{
                    "type": "function",
                    "function": {
                        "name": "get_weather",
                        "description": "查天气",
                        "parameters": {"type": "object", "properties": {"city": {"type": "string"}}},
                        "strict": true,
                    },
                }],
                "tool_choice": "auto",
                "max_tokens": 2048,
                "temperature": 0.3,
                "top_p": 0.9,
                "stop": ["\n\n"],
                "thinking": {"type": "enabled"},
                "reasoning_effort": "high",
                "response_format": {"type": "json_object"},
                "parallel_tool_calls": false,
                "store": false,
            })
        );
        assert!(body.get("top_k").is_none(), "top_k 不属于 Chat 的标准字段");
    }

    #[test]
    fn max_tokens_field_switch() {
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            max_output_tokens: Some(64),
            ..Default::default()
        };
        let cfg = config();
        let body = encode_body(&cfg, &req, false);
        assert_eq!(body["max_tokens"], json!(64));
        assert!(body.get("max_completion_tokens").is_none());

        let cfg = LlmConfig {
            wire: WireConfig {
                max_output_tokens_field: MaxTokensField::MaxCompletionTokens,
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(body["max_completion_tokens"], json!(64));
        assert!(body.get("max_tokens").is_none());
    }

    #[test]
    fn developer_role_downgrade() {
        let req = ChatRequest {
            messages: vec![Message::developer("开发者提示")],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"],
            json!([{"role": "system", "content": "开发者提示"}])
        );
    }

    #[test]
    fn thinking_wire_variants() {
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            reasoning: Some(ReasoningConfig {
                enabled: true,
                effort: Some(crate::types::Effort::Medium),
                ..Default::default()
            }),
            ..Default::default()
        };

        // 默认：toggle + effort 同时发送。
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert_eq!(body["reasoning_effort"], json!("medium"));

        // 只有 toggle（Kimi 风格：无强度字段）。
        let cfg = LlmConfig {
            wire: WireConfig {
                thinking: crate::config::ThinkingWire {
                    effort_field: None,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(body["thinking"], json!({"type": "enabled"}));
        assert!(body.get("reasoning_effort").is_none());

        // 只有 effort（Qwen 风格：无 thinking 对象）。
        let cfg = LlmConfig {
            wire: WireConfig {
                thinking: crate::config::ThinkingWire {
                    toggle_field: None,
                    ..Default::default()
                },
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert!(body.get("thinking").is_none());
        assert_eq!(body["reasoning_effort"], json!("medium"));

        // toggle_extra 并入 thinking 对象。
        let cfg = LlmConfig {
            wire: WireConfig {
                thinking: crate::config::ThinkingWire {
                    toggle_extra: serde_json::Map::from_iter([
                        ("keep".into(), json!("all")),
                        ("clear_thinking".into(), json!(false)),
                    ]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(
            body["thinking"],
            json!({"type": "enabled", "keep": "all", "clear_thinking": false})
        );

        // effort_map 改写强度取值。
        let cfg = LlmConfig {
            wire: WireConfig {
                thinking: crate::config::ThinkingWire {
                    effort_map: std::collections::BTreeMap::from([(
                        crate::types::Effort::Medium,
                        "medium-effort".into(),
                    )]),
                    ..Default::default()
                },
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(body["reasoning_effort"], json!("medium-effort"));
    }

    #[test]
    fn thinking_disabled_sends_only_the_disabled_toggle() {
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            reasoning: Some(ReasoningConfig {
                enabled: false,
                effort: Some(crate::types::Effort::High),
                ..Default::default()
            }),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["thinking"], json!({"type": "disabled"}));
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn tool_choice_dialect() {
        let mut req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            tools: vec![ToolDef::new("get_weather", json!({"type": "object"}))],
            tool_choice: Some(ToolChoice::Required),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["tool_choice"], json!("required"));

        let cfg = LlmConfig {
            wire: WireConfig {
                tool_choice_dialect: ToolChoiceDialect::AnyForRequired,
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(body["tool_choice"], json!("any"));

        let cfg = LlmConfig {
            wire: WireConfig {
                tool_choice_dialect: ToolChoiceDialect::Unsupported,
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert!(body.get("tool_choice").is_none());

        // 具名工具与 none。
        req.tool_choice = Some(ToolChoice::Tool("get_weather".into()));
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["tool_choice"],
            json!({"type": "function", "function": {"name": "get_weather"}})
        );

        req.tool_choice = Some(ToolChoice::None);
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["tool_choice"], json!("none"));

        // 无工具时不发送 tool_choice。
        req.tools.clear();
        let body = encode_body(&config(), &req, false);
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn response_format_json_schema_and_unsupported() {
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            response_format: Some(ResponseFormat::JsonSchema {
                name: "weather".into(),
                description: Some("天气结构".into()),
                schema: json!({"type": "object"}),
                strict: Some(true),
            }),
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["response_format"],
            json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "weather",
                    "description": "天气结构",
                    "schema": {"type": "object"},
                    "strict": true,
                },
            })
        );

        let cfg = LlmConfig {
            wire: WireConfig {
                response_format_dialect: ResponseFormatDialect::Unsupported,
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert!(body.get("response_format").is_none());
    }

    #[test]
    fn image_url_and_base64() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::Text("看图".into()),
                    ContentPart::Image {
                        source: ImageSource::Url("https://example.com/a.png".into()),
                        detail: Some(ImageDetail::High),
                    },
                    ContentPart::Image {
                        source: ImageSource::Base64 {
                            media_type: "image/png".into(),
                            data: "AAAA".into(),
                        },
                        detail: None,
                    },
                ],
            }],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"][0]["content"],
            json!([
                {"type": "text", "text": "看图"},
                {"type": "image_url", "image_url": {"url": "https://example.com/a.png", "detail": "high"}},
                {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
            ])
        );

        let cfg = LlmConfig {
            wire: WireConfig {
                image_url: false,
                ..Default::default()
            },
            ..config()
        };
        let err = encode(&cfg, &req, false).unwrap_err();
        assert!(err.to_string().contains("图片 URL"), "{err}");
    }

    #[test]
    fn cache_key_and_breakpoints() {
        let req = ChatRequest {
            messages: vec![Message::system("系统"), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(crate::config::CacheConfig {
                    key: Some("session-1".into()),
                    breakpoints: vec![0],
                    ttl: Some(CacheTtl::ThirtyMinutes),
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["prompt_cache_key"], json!("session-1"));
        assert_eq!(
            body["prompt_cache_options"],
            json!({"mode": "explicit", "ttl": "30m"})
        );
        assert_eq!(
            body["messages"][0]["content"],
            json!([{"type": "text", "text": "系统", "prompt_cache_breakpoint": {"mode": "explicit"}}])
        );
        assert_eq!(body["messages"][1]["content"], json!("hi"));

        // 不支持的 TTL 直接报错，不静默降级。
        let bad = ChatRequest {
            messages: vec![Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(crate::config::CacheConfig {
                    ttl: Some(CacheTtl::FiveMinutes),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("只支持 30m"), "{err}");

        // 越界断点报错。
        let bad = ChatRequest {
            messages: vec![Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(crate::config::CacheConfig {
                    breakpoints: vec![7],
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = encode(&config(), &bad, false).unwrap_err();
        assert!(err.to_string().contains("超出消息范围"), "{err}");
    }

    #[test]
    fn streaming_flag_adds_stream_options() {
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, true);
        assert_eq!(body["stream"], json!(true));
        assert_eq!(body["stream_options"], json!({"include_usage": true}));
    }

    #[test]
    fn response_decode_reads_text_reasoning_tools_and_usage() {
        let body = json!({
            "id": "chatcmpl-1",
            "object": "chat.completion",
            "model": "deepseek-reasoner",
            "choices": [{
                "index": 0,
                "message": {
                    "role": "assistant",
                    "content": "答案是 42",
                    "reasoning_content": "先算一下",
                    "tool_calls": [
                        {"id": "call_1", "type": "function", "function": {"name": "get_weather", "arguments": "{\"city\":\"北京\"}"}},
                        {"id": "call_2", "type": "function", "function": {"name": "get_date", "arguments": "{}"}},
                    ],
                },
                "finish_reason": "tool_calls",
            }],
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 40,
                "total_tokens": 140,
                "prompt_tokens_details": {"cached_tokens": 64},
                "completion_tokens_details": {"reasoning_tokens": 12},
            },
        });
        let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
        assert_eq!(response.id.as_deref(), Some("chatcmpl-1"));
        assert_eq!(response.model.as_deref(), Some("deepseek-reasoner"));
        assert_eq!(response.finish_reason, FinishReason::ToolCalls);
        assert_eq!(
            response.usage,
            Usage {
                input_tokens: 100,
                output_tokens: 40,
                total_tokens: Some(140),
                cached_input_tokens: Some(64),
                cache_creation_input_tokens: None,
                reasoning_tokens: Some(12),
            }
        );
        assert_eq!(
            response.message.content,
            vec![
                ContentPart::Reasoning(ReasoningBlock::Text {
                    text: "先算一下".into(),
                    signature: None,
                }),
                ContentPart::Text("答案是 42".into()),
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
                ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
            ]
        );
    }

    #[test]
    fn response_reasoning_alias() {
        let body = json!({
            "choices": [{
                "message": {"role": "assistant", "content": "hi", "reasoning": "别名字段的思考"},
                "finish_reason": "stop",
            }],
        });
        let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
        assert_eq!(
            response.message.content[0],
            ContentPart::Reasoning(ReasoningBlock::Text {
                text: "别名字段的思考".into(),
                signature: None,
            })
        );

        // 自定义字段名优先于别名。
        let cfg = LlmConfig {
            wire: WireConfig {
                reasoning_field: "thinking_text".into(),
                reasoning_field_aliases: vec!["reasoning".into()],
                ..Default::default()
            },
            ..config()
        };
        let body = json!({
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hi",
                    "thinking_text": "自定义字段",
                    "reasoning": "别名",
                },
                "finish_reason": "stop",
            }],
        });
        let response = decode_response(&cfg, &ChatRequest::default(), &body).unwrap();
        assert_eq!(
            response.message.content[0],
            ContentPart::Reasoning(ReasoningBlock::Text {
                text: "自定义字段".into(),
                signature: None,
            })
        );
    }

    #[test]
    fn finish_reason_table_via_decode() {
        let cases = [
            ("stop", FinishReason::Stop),
            ("length", FinishReason::Length),
            ("tool_calls", FinishReason::ToolCalls),
            ("function_call", FinishReason::ToolCalls),
            ("content_filter", FinishReason::ContentFilter),
            ("something_new", FinishReason::Other("something_new".into())),
        ];
        for (wire, expected) in cases {
            let body = json!({
                "choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": wire}],
            });
            let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
            assert_eq!(response.finish_reason, expected, "{wire}");
        }

        // finish_reason 为 null（未结束的增量形态）时回落到默认值。
        let body = json!({"choices": [{"message": {"role": "assistant", "content": "x"}, "finish_reason": null}]});
        let response = decode_response(&config(), &ChatRequest::default(), &body).unwrap();
        assert_eq!(
            response.finish_reason,
            FinishReason::Other("unknown".into())
        );
    }

    #[test]
    fn parallel_tool_calls_encoded_as_one_message_and_n_results() {
        let req = ChatRequest {
            messages: vec![
                Message::user_text("并行查"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::ToolCall(ToolCall::new(
                            "call_1",
                            "get_weather",
                            "{\"city\":\"北京\"}",
                        )),
                        ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
                        ContentPart::ToolCall(ToolCall::new("call_3", "get_time", "{}")),
                    ],
                },
                Message::tool_result("call_1", "晴", false),
                Message::tool_result("call_2", "2026-09-24", false),
                Message::tool_result("call_3", "12:00", false),
            ],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 5);
        assert_eq!(messages[1]["content"], json!(null));
        assert_eq!(messages[1]["tool_calls"].as_array().unwrap().len(), 3);
        assert_eq!(messages[1]["tool_calls"][0]["id"], json!("call_1"));
        assert_eq!(messages[1]["tool_calls"][2]["id"], json!("call_3"));
        for (offset, (id, content)) in [
            ("call_1", "晴"),
            ("call_2", "2026-09-24"),
            ("call_3", "12:00"),
        ]
        .iter()
        .enumerate()
        {
            let message = &messages[2 + offset];
            assert_eq!(message["role"], json!("tool"));
            assert_eq!(message["tool_call_id"], json!(id));
            assert_eq!(message["content"], json!(content));
        }
    }

    #[test]
    fn parallel_tool_calls_streaming_interleaved() {
        let mut decoder = ChatStreamDecoder::new(&config());
        let events = events(
            &mut decoder,
            &[
                r#"{"id":"chatcmpl-1","model":"test-model","choices":[{"index":0,"delta":{"role":"assistant","content":""},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"get_weather","arguments":""}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"id":"call_2","type":"function","function":{"name":"get_date","arguments":""}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":1,"function":{"arguments":"{}"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"北京\"}"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5,"total_tokens":15}}"#,
                "[DONE]",
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: Some("chatcmpl-1".into()),
                    model: Some("test-model".into()),
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_weather".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 1,
                    id: "call_2".into(),
                    name: "get_date".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{\"city\":".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 1,
                    json: "{}".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "\"北京\"}".into(),
                },
                StreamEvent::Usage(Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: Some(15),
                    ..Default::default()
                }),
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        );
    }

    #[test]
    fn tool_name_fragments_concatenated() {
        let mut decoder = ChatStreamDecoder::new(&config());
        let events = events(
            &mut decoder,
            &[
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"name":"weather"}}]},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{}"}}]},"finish_reason":"tool_calls"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: None,
                    model: None,
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "get_".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call_1".into(),
                    name: "weather".into(),
                },
                StreamEvent::ToolCallArgumentsDelta {
                    index: 0,
                    json: "{}".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::ToolCalls,
                },
            ]
        );
    }

    #[test]
    fn stream_text_and_reasoning_deltas() {
        let mut decoder = ChatStreamDecoder::new(&config());
        let events = events(
            &mut decoder,
            &[
                r#"{"id":"chatcmpl-9","model":"m","choices":[{"index":0,"delta":{"role":"assistant","reasoning_content":"想","content":""},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"reasoning_content":"一下","content":"你"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{"content":"好"},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: Some("chatcmpl-9".into()),
                    model: Some("m".into()),
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "想".into(),
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "一下".into(),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "你".into(),
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "好".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Stop,
                },
            ]
        );
    }

    #[test]
    fn stream_without_done_sentinel_finishes_at_eof() {
        let mut decoder = ChatStreamDecoder::new(&config());
        let events = events(
            &mut decoder,
            &[r#"{"choices":[{"index":0,"delta":{"content":"完"},"finish_reason":"length"}]}"#],
        );
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: None,
                    model: None,
                },
                StreamEvent::TextDelta {
                    index: 0,
                    text: "完".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Length,
                },
            ]
        );
        assert!(decoder.is_terminated());
    }

    #[test]
    fn stream_malformed_json_terminates_with_error() {
        let mut decoder = ChatStreamDecoder::new(&config());
        let out = feed(&mut decoder, &["{oops", r#"{"choices":[]}"#]);
        assert!(matches!(out[0], Err(LlmError::Stream(_))), "{:?}", out[0]);
        assert_eq!(out.len(), 1, "错误后不再产出事件");
        assert!(decoder.is_terminated());
    }

    #[test]
    fn stream_refusal_is_emitted_as_text() {
        let mut decoder = ChatStreamDecoder::new(&config());
        let events = events(
            &mut decoder,
            &[
                r#"{"choices":[{"index":0,"delta":{"refusal":"不能回答"},"finish_reason":null}]}"#,
                r#"{"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}"#,
                "[DONE]",
            ],
        );
        assert_eq!(
            events[1],
            StreamEvent::TextDelta {
                index: 0,
                text: "不能回答".into(),
            }
        );
    }

    #[test]
    fn error_body_table() {
        let cfg = config();
        let cases = [
            (
                400,
                r#"{"error":{"message":"bad temperature","type":"invalid_request_error","param":"temperature","code":null}}"#,
                crate::error::ApiErrorKind::InvalidRequest,
            ),
            (
                401,
                r#"{"error":{"message":"invalid key","type":"authentication_error","code":"invalid_api_key"}}"#,
                crate::error::ApiErrorKind::Auth,
            ),
            (
                404,
                r#"{"error":{"message":"model not found","type":"invalid_request_error","code":"model_not_found"}}"#,
                crate::error::ApiErrorKind::NotFound,
            ),
            (
                429,
                r#"{"error":{"message":"rate limited","type":"rate_limit_error","code":"rate_limit_exceeded"}}"#,
                crate::error::ApiErrorKind::RateLimit,
            ),
            (
                429,
                r#"{"error":{"message":"quota","type":"insufficient_quota","code":"insufficient_quota"}}"#,
                crate::error::ApiErrorKind::QuotaExceeded,
            ),
            (
                500,
                r#"{"error":{"message":"server exploded","type":"server_error","code":null}}"#,
                crate::error::ApiErrorKind::ServerError,
            ),
            (
                400,
                r#"{"error":{"message":"This model's maximum context length is 65536 tokens","type":"invalid_request_error","code":"context_length_exceeded"}}"#,
                crate::error::ApiErrorKind::ContextLength,
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
    }

    #[test]
    fn extra_body_is_overridden_by_standard_fields() {
        let cfg = LlmConfig {
            extra_body: serde_json::Map::from_iter([
                ("temperature".into(), json!(9.9)),
                ("thinking_budget".into(), json!(16384)),
            ]),
            ..config()
        };
        let req = ChatRequest {
            messages: vec![Message::user_text("hi")],
            temperature: Some(0.2),
            ..Default::default()
        };
        let body = encode_body(&cfg, &req, false);
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["thinking_budget"], json!(16384));
    }

    #[test]
    fn empty_tool_result_encodes_as_empty_string() {
        let req = ChatRequest {
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall(ToolCall::new("call_1", "noop", "{}"))],
                },
                Message::tool_result("call_1", "", false),
            ],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["messages"][1]["content"], json!(""));
    }

    #[test]
    fn tool_result_images_are_bypassed_as_user_message() {
        let req = ChatRequest {
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![ContentPart::ToolCall(ToolCall::new(
                        "call_1",
                        "screenshot",
                        "{}",
                    ))],
                },
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
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["messages"],
            json!([
                {"role": "assistant", "content": null, "tool_calls": [
                    {"id": "call_1", "type": "function", "function": {"name": "screenshot", "arguments": "{}"}},
                ]},
                {"role": "tool", "tool_call_id": "call_1", "content": "截图如下"},
                {"role": "user", "content": [
                    {"type": "text", "text": "[工具 call_1 返回的图片]"},
                    {"type": "image_url", "image_url": {"url": "data:image/png;base64,AAAA"}},
                ]},
            ])
        );
    }

    #[test]
    fn tool_history_validation_applies_to_chat() {
        let req = ChatRequest {
            messages: vec![
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                        ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
                    ],
                },
                Message::tool_result("call_1", "晴", false),
            ],
            ..Default::default()
        };
        let err = super::super::encode(&config(), &req, false).unwrap_err();
        assert_eq!(
            err.to_string(),
            "构建请求失败: assistant 消息中的工具调用 [get_weather, get_date] 缺少对应的工具结果消息（缺失: get_date）"
        );
    }
}

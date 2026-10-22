//! OpenAI Responses 编解码。
//!
//! Responses 的 `input` 是一串 item（`message` / `function_call` /
//! `function_call_output` / `reasoning`），而不是角色化的消息数组；assistant
//! 历史因此按内容块拆成多个 item，`ReasoningBlock::Item` 原样往返。

use std::collections::BTreeSet;

use serde_json::{Map, Value, json};

use super::echo::{self, ReasoningEchoPlan};
use super::openai;
use super::sse::SseEvent;
use super::{EncodedRequest, StreamDecoder, common_headers};
use crate::config::{CacheTtl, LlmConfig, ResponseFormatDialect, ToolChoiceDialect};
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::types::{
    ChatRequest, ChatResponse, ContentPart, FinishReason, ImageDetail, ImageSource, Message,
    ReasoningBlock, ReasoningConfig, ReasoningSummary, ResponseFormat, Role, StreamEvent, ToolCall,
    ToolChoice, ToolDef,
};

/// 加密思维链条目的 include 值；`store:false` / ZDR 下回传必需。
const ENCRYPTED_REASONING_INCLUDE: &str = "reasoning.encrypted_content";

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
    let mut input =
        Vec::with_capacity(req.native_input.as_ref().map_or(0, Vec::len) + req.messages.len());
    if let Some(native_input) = &req.native_input {
        input.extend(native_input.iter().cloned());
    }
    input.extend(encode_input(cfg, plan, req, &breakpoints)?);
    body.insert("input".into(), Value::Array(input));
    if let Some(threshold) = cfg.server_compaction_threshold {
        body.insert(
            "context_management".into(),
            json!([{ "type": "compaction", "compact_threshold": threshold }]),
        );
    }
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
        body.insert("max_output_tokens".into(), Value::from(max));
    }
    if let Some(temperature) = req.temperature {
        body.insert("temperature".into(), json!(temperature));
    }
    if let Some(top_p) = req.top_p {
        body.insert("top_p".into(), json!(top_p));
    }
    if let Some(reasoning) = &req.reasoning
        && let Some(value) = encode_reasoning(cfg, reasoning)
    {
        body.insert("reasoning".into(), value);
    }
    if cfg.wire.response_format_dialect != ResponseFormatDialect::Unsupported
        && let Some(format) = &req.response_format
    {
        body.insert("text".into(), json!({"format": encode_format(format)}));
    }
    if let Some(parallel) = req.parallel_tool_calls
        && cfg.wire.parallel_tool_calls
    {
        body.insert("parallel_tool_calls".into(), Value::Bool(parallel));
    }
    if let Some(cache) = &req.options.cache
        && let Some(key) = &cache.key
    {
        body.insert("prompt_cache_key".into(), Value::String(key.clone()));
    }
    if !breakpoints.is_empty() {
        let ttl = req
            .options
            .cache
            .as_ref()
            .and_then(|cache| cache.ttl)
            .unwrap_or(CacheTtl::ThirtyMinutes);
        body.insert(
            "prompt_cache_options".into(),
            json!({"mode": "explicit", "ttl": ttl.as_str()}),
        );
    }
    if let Some(store) = req.options.store {
        body.insert("store".into(), Value::Bool(store));
    }
    if req.options.echo_reasoning != crate::types::ReasoningEcho::Never {
        body.insert(
            "include".into(),
            Value::Array(merge_include(body.get("include"))),
        );
    }
    if stream {
        body.insert("stream".into(), Value::Bool(true));
    }

    let body = serde_json::to_vec(&Value::Object(body))
        .map_err(|err| LlmError::Encode(format!("Responses 请求体序列化失败: {err}")))?;
    Ok(EncodedRequest {
        url: cfg.endpoint_url(),
        headers: common_headers(cfg, req).into_vec(),
        body,
    })
}

/// 合并 `include` 列表：保留 `extra_body` 里已有的条目，追加加密思维链。
fn merge_include(existing: Option<&Value>) -> Vec<Value> {
    let mut out: Vec<Value> = existing
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !out.iter().any(|item| item == ENCRYPTED_REASONING_INCLUDE) {
        out.push(Value::String(ENCRYPTED_REASONING_INCLUDE.into()));
    }
    out
}

/// 显式缓存：校验 TTL 与断点下标。
fn cache_breakpoints(req: &ChatRequest, message_count: usize) -> Result<BTreeSet<usize>, LlmError> {
    let Some(cache) = &req.options.cache else {
        return Ok(BTreeSet::new());
    };
    if let Some(ttl) = cache.ttl
        && ttl != CacheTtl::ThirtyMinutes
    {
        return Err(LlmError::Encode(format!(
            "Responses 协议的显式缓存只支持 30m，收到 {}",
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

fn encode_reasoning(cfg: &LlmConfig, reasoning: &ReasoningConfig) -> Option<Value> {
    let mut object = Map::new();
    if !reasoning.enabled {
        // `none` 是 ReasoningEffort 的合法取值。
        object.insert("effort".into(), Value::String("none".into()));
        return Some(Value::Object(object));
    }
    if let Some(effort) = cfg.wire_effort(Some(reasoning)) {
        object.insert("effort".into(), Value::String(effort));
    }
    if let Some(summary) = reasoning.summary
        && let Some(value) = summary_value(summary)
    {
        object.insert("summary".into(), Value::String(value.into()));
    }
    (!object.is_empty()).then_some(Value::Object(object))
}

/// Responses 没有「关闭摘要」的取值：`Omitted` / `None` 一律不发送 `summary`。
fn summary_value(summary: ReasoningSummary) -> Option<&'static str> {
    match summary {
        ReasoningSummary::Auto => Some("auto"),
        ReasoningSummary::Concise => Some("concise"),
        ReasoningSummary::Detailed => Some("detailed"),
        ReasoningSummary::Summarized => Some("auto"),
        ReasoningSummary::Omitted => None,
    }
}

fn encode_format(format: &ResponseFormat) -> Value {
    match format {
        ResponseFormat::Text => json!({"type": "text"}),
        ResponseFormat::JsonObject => json!({"type": "json_object"}),
        ResponseFormat::JsonSchema {
            name,
            description,
            schema,
            strict,
        } => {
            let mut object = Map::new();
            object.insert("type".into(), Value::String("json_schema".into()));
            object.insert("name".into(), Value::String(name.clone()));
            if let Some(description) = description {
                object.insert("description".into(), Value::String(description.clone()));
            }
            object.insert("schema".into(), schema.clone());
            if let Some(strict) = strict {
                object.insert("strict".into(), Value::Bool(*strict));
            }
            Value::Object(object)
        }
    }
}

fn encode_tool_def(def: &ToolDef) -> Value {
    let mut object = Map::new();
    object.insert("type".into(), Value::String("function".into()));
    object.insert("name".into(), Value::String(def.name.clone()));
    if let Some(description) = &def.description {
        object.insert("description".into(), Value::String(description.clone()));
    }
    object.insert("parameters".into(), def.parameters.clone());
    if let Some(strict) = def.strict {
        object.insert("strict".into(), Value::Bool(strict));
    }
    Value::Object(object)
}

fn encode_tool_choice(cfg: &LlmConfig, choice: Option<&ToolChoice>) -> Option<Value> {
    if cfg.wire.tool_choice_dialect == ToolChoiceDialect::Unsupported {
        return None;
    }
    Some(match choice? {
        ToolChoice::Auto => json!("auto"),
        ToolChoice::None => json!("none"),
        ToolChoice::Required => json!("required"),
        ToolChoice::Tool(name) => json!({"type": "function", "name": name}),
    })
}

/// 编码 `input` 数组。
fn encode_input(
    cfg: &LlmConfig,
    plan: ReasoningEchoPlan,
    req: &ChatRequest,
    breakpoints: &BTreeSet<usize>,
) -> Result<Vec<Value>, LlmError> {
    let mut items = Vec::with_capacity(req.messages.len());
    for (index, message) in req.messages.iter().enumerate() {
        let breakpoint = breakpoints.contains(&index);
        match message.role {
            Role::System | Role::Developer | Role::User => {
                items.push(encode_message_item(cfg, message, breakpoint)?);
            }
            Role::Assistant => {
                encode_assistant_items(plan, message, breakpoint, &mut items)?;
            }
            // tool 结果携带的图片无法进入 function_call_output，编码为紧随的 user 消息。
            Role::Tool => items.extend(encode_tool_result(cfg, message, breakpoint)?),
        }
    }
    Ok(items)
}

/// 编码请求消息到 Responses `input` item，不附加 `ChatRequest::native_input`。
pub(crate) fn encode_input_items(
    cfg: &LlmConfig,
    req: &ChatRequest,
) -> Result<Vec<Value>, LlmError> {
    let plan = echo::plan(cfg, req);
    let breakpoints = cache_breakpoints(req, req.messages.len())?;
    encode_input(cfg, plan, req, &breakpoints)
}

/// 系统 / 开发者 / 用户消息：都是 `input` 里的 `message` item。
fn encode_message_item(
    _cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Value, LlmError> {
    let (role, text_type) = match message.role {
        Role::System => ("system", "input_text"),
        Role::Developer => ("developer", "input_text"),
        Role::User => ("user", "input_text"),
        other => {
            return Err(LlmError::Encode(format!(
                "{other} 消息不能编码为 Responses 的 message item"
            )));
        }
    };
    let mut blocks = Vec::with_capacity(message.content.len());
    if matches!(message.role, Role::System | Role::Developer) {
        // 指令消息拍平成单块：块边界只是本地缓存断点的坐标，不是线上结构。
        for part in &message.content {
            if !matches!(part, ContentPart::Text(_)) {
                return Err(unsupported_part(message.role, part));
            }
        }
        if !message.content.is_empty() {
            blocks.push(json!({"type": text_type, "text": message.instruction_text()}));
        }
    } else {
        for part in &message.content {
            match part {
                ContentPart::Text(text) => blocks.push(json!({"type": text_type, "text": text})),
                ContentPart::Image { source, detail } => {
                    blocks.push(encode_image(source, *detail)?);
                }
                other => return Err(unsupported_part(message.role, other)),
            }
        }
    }
    if breakpoint {
        attach_breakpoint(&mut blocks, "input_text")?;
    }
    Ok(json!({"type": "message", "role": role, "content": blocks}))
}

fn encode_image(source: &ImageSource, detail: Option<ImageDetail>) -> Result<Value, LlmError> {
    let url = match source {
        ImageSource::Url(url) => url.clone(),
        ImageSource::Base64 { media_type, data } => format!("data:{media_type};base64,{data}"),
    };
    let mut block = Map::new();
    block.insert("type".into(), Value::String("input_image".into()));
    block.insert("image_url".into(), Value::String(url));
    if let Some(detail) = detail {
        block.insert("detail".into(), Value::String(detail.as_str().into()));
    }
    Ok(Value::Object(block))
}

/// assistant 历史：按内容块拆成 reasoning / message / function_call 三类 item。
///
/// `ReasoningBlock::Item` 原样输出，位置保持在它配对的 `function_call` 之前。
fn encode_assistant_items(
    plan: ReasoningEchoPlan,
    message: &Message,
    breakpoint: bool,
    items: &mut Vec<Value>,
) -> Result<(), LlmError> {
    let mut pending_text = String::new();
    let mut text_block_index: Option<usize> = None;

    for part in &message.content {
        match part {
            ContentPart::Reasoning(block) => {
                if plan == ReasoningEchoPlan::Strip {
                    continue;
                }
                match block {
                    ReasoningBlock::Item(item) => {
                        flush_text(&mut pending_text, &mut text_block_index, items);
                        items.push((**item).clone());
                    }
                    // 文本与不透明块无法表达为 Responses 的 reasoning item：
                    // 静默丢内容会让历史语义变样，直接报错。
                    ReasoningBlock::Text { .. } | ReasoningBlock::Redacted { .. } => {
                        return Err(LlmError::Encode(
                            "Responses 协议只支持原样回传 reasoning item".into(),
                        ));
                    }
                }
            }
            ContentPart::Text(text) => {
                if text_block_index.is_none() {
                    let position = items.len();
                    items.push(json!({"type": "message", "role": "assistant", "content": []}));
                    text_block_index = Some(position);
                }
                pending_text.push_str(text);
            }
            ContentPart::ToolCall(call) => {
                flush_text(&mut pending_text, &mut text_block_index, items);
                items.push(json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": call.arguments,
                }));
            }
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    flush_text(&mut pending_text, &mut text_block_index, items);

    if breakpoint {
        let position = text_block_index.ok_or_else(|| {
            LlmError::Encode("Responses 的缓存断点只能落在 assistant 消息的文本内容块上".into())
        })?;
        let blocks = items[position]
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .expect("assistant message item 由本模块构造成内容块数组");
        attach_breakpoint(blocks, "output_text")?;
    }
    Ok(())
}

/// 把累积的文本写成一个 assistant message item。
fn flush_text(pending: &mut String, block_index: &mut Option<usize>, items: &mut Vec<Value>) {
    if pending.is_empty() {
        return;
    }
    let text = std::mem::take(pending);
    match *block_index {
        Some(position) => {
            let content = items[position]
                .get_mut("content")
                .and_then(Value::as_array_mut)
                .expect("assistant message item 由本模块构造成内容块数组");
            content.push(json!({"type": "output_text", "text": text}));
        }
        None => {
            items.push(json!({
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text}],
            }));
        }
    }
}

fn encode_tool_result(
    _cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Vec<Value>, LlmError> {
    let Some(result) = message.tool_results().next() else {
        return Err(LlmError::Encode("tool 消息缺少 ToolResult 内容块".into()));
    };
    if breakpoint {
        return Err(LlmError::Encode(
            "Responses 的缓存断点只能落在文本内容块上".into(),
        ));
    }
    let mut items = vec![json!({
        "type": "function_call_output",
        "call_id": result.tool_call_id,
        "output": result.content,
    })];
    if result.images.is_empty() {
        return Ok(items);
    }
    // 图片旁路：function_call_output 只收文本，以独立 user message item 补发。
    let mut blocks = Vec::with_capacity(result.images.len() + 1);
    blocks.push(json!({
        "type": "input_text",
        "text": format!("[image returned by tool {}]", result.tool_call_id),
    }));
    for source in &result.images {
        blocks.push(encode_image(source, None)?);
    }
    items.push(json!({
        "type": "message",
        "role": "user",
        "content": blocks,
    }));
    Ok(items)
}

/// 给最后一个文本内容块打上显式缓存断点。
fn attach_breakpoint(blocks: &mut [Value], text_type: &str) -> Result<(), LlmError> {
    let last = blocks
        .last_mut()
        .ok_or_else(|| LlmError::Encode("缓存断点落在空消息上".into()))?;
    let object = last
        .as_object_mut()
        .expect("内容块由本模块构造成 JSON 对象");
    if object.get("type").and_then(Value::as_str) != Some(text_type) {
        return Err(LlmError::Encode(
            "Responses 的缓存断点只能落在文本内容块上".into(),
        ));
    }
    object.insert(
        "prompt_cache_breakpoint".into(),
        json!({"mode": "explicit"}),
    );
    Ok(())
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
    if value.get("status").and_then(Value::as_str) == Some("failed") {
        return Err(failed_error(value));
    }
    let compaction = value
        .get("output")
        .and_then(Value::as_array)
        .and_then(|items| {
            items
                .iter()
                .find(|item| item.get("type").and_then(Value::as_str) == Some("compaction"))
        })
        .cloned()
        .map(Box::new);

    let mut content = Vec::new();
    for item in value
        .get("output")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        match item.get("type").and_then(Value::as_str) {
            Some("reasoning") => {
                content.push(ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                    item.clone(),
                ))));
            }
            Some("message") => {
                for block in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    match block.get("type").and_then(Value::as_str) {
                        Some("output_text") => {
                            content.push(ContentPart::Text(string_at(block, "/text")));
                        }
                        // 拒绝与正文一样是模型输出，直接计入文本。
                        Some("refusal") => {
                            content.push(ContentPart::Text(string_at(block, "/refusal")));
                        }
                        _ => {}
                    }
                }
            }
            Some("function_call") => content.push(ContentPart::ToolCall(ToolCall {
                id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .or_else(|| item.get("id").and_then(Value::as_str))
                    .unwrap_or_default()
                    .to_owned(),
                name: string_at(item, "/name"),
                arguments: string_at(item, "/arguments"),
            })),
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
        finish_reason: finish_reason(value),
        usage: value.get("usage").map_or_default(openai::decode_usage),
        raw: None,
        compaction,
    })
}

/// `status` / `incomplete_details.reason` → [`FinishReason`]。
fn finish_reason(response: &Value) -> FinishReason {
    match response.get("status").and_then(Value::as_str) {
        Some("completed") => {
            let has_tool_call = response
                .get("output")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .any(|item| item.get("type").and_then(Value::as_str) == Some("function_call"));
            if has_tool_call {
                FinishReason::ToolCalls
            } else {
                FinishReason::Stop
            }
        }
        Some("incomplete") => match response
            .pointer("/incomplete_details/reason")
            .and_then(Value::as_str)
        {
            Some("max_output_tokens") => FinishReason::Length,
            Some("content_filter") => FinishReason::ContentFilter,
            Some(other) => FinishReason::Other(other.to_owned()),
            None => FinishReason::Other("incomplete".into()),
        },
        Some("cancelled") => FinishReason::Cancelled,
        Some(other) => FinishReason::Other(other.to_owned()),
        None => FinishReason::Other("unknown".into()),
    }
}

/// `failed` 状态：错误信息取 `response.error`。
fn failed_error(response: &Value) -> LlmError {
    let error = response.get("error");
    let code = error
        .and_then(|error| error.get("code"))
        .and_then(Value::as_str);
    let message = error
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
        .unwrap_or("Responses 请求失败");
    let mut api_error = ApiError::new(
        None,
        ApiErrorKind::Unknown.refine(code, message),
        message,
        error.map_or_else(String::new, Value::to_string),
    );
    api_error.code = code.map(str::to_owned);
    LlmError::Api(api_error)
}

fn string_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// Responses 流式状态机。
pub(crate) struct ResponsesStreamDecoder {
    /// 首个事件已发出 [`StreamEvent::Start`]。
    started: bool,
    /// 已到终态（`response.completed` / `failed` / `error` 或收尾完成）。
    terminated: bool,
}

impl ResponsesStreamDecoder {
    /// 构造。
    pub fn new(_cfg: &LlmConfig) -> Self {
        Self {
            started: false,
            terminated: false,
        }
    }

    fn decode_payload(&mut self, value: &Value, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response.created" => {
                if !self.started {
                    self.started = true;
                    out.push(Ok(StreamEvent::Start {
                        id: optional_string_at(value, "/response/id"),
                        model: optional_string_at(value, "/response/model"),
                        effort: super::reported_effort(value.get("response").unwrap_or(value)),
                    }));
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                out.push(Ok(StreamEvent::TextDelta {
                    index: index_of(value),
                    text: delta_text(value),
                }));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                out.push(Ok(StreamEvent::ReasoningDelta {
                    index: index_of(value),
                    text: delta_text(value),
                }));
            }
            "response.function_call_arguments.delta" => {
                out.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                    index: index_of(value),
                    json: string_at(value, "/delta"),
                }));
            }
            "response.output_item.added" => {
                let Some(item) = value.get("item") else {
                    return;
                };
                if item.get("type").and_then(Value::as_str) == Some("function_call") {
                    out.push(Ok(StreamEvent::ToolCallStart {
                        index: index_of(value),
                        id: string_at(item, "/call_id"),
                        name: string_at(item, "/name"),
                    }));
                }
            }
            "response.output_item.done" => {
                // 必须用 done 事件的完整 item：added 里的 encrypted_content
                // 可能不完整。
                let Some(item) = value.get("item") else {
                    return;
                };
                match item.get("type").and_then(Value::as_str) {
                    Some("reasoning") => out.push(Ok(StreamEvent::ReasoningItem {
                        index: index_of(value),
                        item: Box::new(item.clone()),
                    })),
                    Some("compaction") => out.push(Ok(StreamEvent::CompactionItem {
                        item: Box::new(item.clone()),
                    })),
                    _ => {}
                }
            }
            "response.completed" | "response.incomplete" => {
                self.terminated = true;
                let response = value.get("response").cloned().unwrap_or(Value::Null);
                if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
                    out.push(Ok(StreamEvent::Usage(openai::decode_usage(usage))));
                }
                out.push(Ok(StreamEvent::Done {
                    finish_reason: finish_reason(&response),
                }));
            }
            "response.failed" => {
                self.terminated = true;
                let response = value.get("response").cloned().unwrap_or(Value::Null);
                out.push(Err(failed_error(&response)));
            }
            "error" => {
                self.terminated = true;
                let code = value.get("code").and_then(Value::as_str);
                let message = value
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("Responses 流错误");
                let mut error = ApiError::new(
                    None,
                    ApiErrorKind::Unknown.refine(code, message),
                    message,
                    value.to_string(),
                );
                error.code = code.map(str::to_owned);
                out.push(Err(LlmError::Api(error)));
            }
            // `*.in_progress` / `*.searching` / `*.done` 等一律忽略。
            _ => {}
        }
    }
}

impl StreamDecoder for ResponsesStreamDecoder {
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
                    "Responses 流数据不是合法 JSON: {err}"
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
            finish_reason: FinishReason::Other("unknown".into()),
        }));
    }

    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

/// 增量文本：`output_text` / `refusal` / reasoning 摘要共用 `delta` 字段。
fn delta_text(value: &Value) -> String {
    value
        .get("delta")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn index_of(value: &Value) -> usize {
    value
        .get("output_index")
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
    use crate::config::{CacheConfig, Protocol, WireConfig};
    use crate::types::{ReasoningEcho, RequestOptions, Usage};
    use serde_json::json;

    fn config() -> LlmConfig {
        LlmConfig {
            protocol: Protocol::OpenAiResponses,
            base_url: "https://api.openai.com/v1".into(),
            model: "gpt-5".into(),
            auth: crate::config::Auth::Bearer("sk-test".into()),
            ..Default::default()
        }
    }

    fn base_request() -> ChatRequest {
        ChatRequest {
            messages: vec![Message::user_text("hi")],
            ..Default::default()
        }
    }

    fn encode_body(cfg: &LlmConfig, req: &ChatRequest, stream: bool) -> Value {
        let encoded = encode(cfg, req, stream).unwrap();
        assert_eq!(encoded.url, format!("{}/responses", cfg.base_url));
        serde_json::from_slice(&encoded.body).unwrap()
    }

    fn sse(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
            id: None,
        }
    }

    fn events(
        decoder: &mut ResponsesStreamDecoder,
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
    fn public_input_encoder_returns_encoded_messages() {
        let items = crate::encode_responses_input_items(&config(), &base_request())
            .expect("Responses 历史可编码");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["role"], "user");
        assert_eq!(items[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn request_minimal_has_no_optional_fields() {
        let body = encode_body(&config(), &base_request(), false);
        // `include` 不是可选项：默认回传思维链时它保证加密 reasoning 可续跑。
        assert_eq!(
            body,
            json!({
                "model": "gpt-5",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "hi"}],
                }],
                "include": ["reasoning.encrypted_content"],
            })
        );
    }

    #[test]
    fn native_input_precedes_encoded_messages_and_threshold_is_opt_in() {
        let native = json!({ "type": "compaction", "id": "cmp_1" });
        let req = ChatRequest {
            native_input: Some(vec![native.clone()]),
            ..base_request()
        };
        let body = encode_body(
            &LlmConfig {
                server_compaction_threshold: Some(128_000),
                ..config()
            },
            &req,
            false,
        );
        assert_eq!(body["input"][0], native);
        assert_eq!(body["input"][1]["role"], "user");
        assert_eq!(
            body["context_management"],
            json!([{ "type": "compaction", "compact_threshold": 128_000 }])
        );
        assert!(
            encode_body(&config(), &base_request(), false)
                .get("context_management")
                .is_none()
        );
    }

    #[test]
    fn request_input_items_order() {
        let req = ChatRequest {
            messages: vec![
                Message::system("系统"),
                Message::user_text("查天气"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning(ReasoningBlock::Item(Box::new(json!({
                            "type": "reasoning",
                            "id": "rs_1",
                            "encrypted_content": "blob",
                            "summary": [],
                        })))),
                        ContentPart::Text("这就去查".into()),
                        ContentPart::ToolCall(ToolCall::new(
                            "call_1",
                            "get_weather",
                            "{\"city\":\"北京\"}",
                        )),
                    ],
                },
                Message::tool_result("call_1", "晴", false),
            ],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(
            body["input"],
            json!([
                {"type": "message", "role": "system", "content": [{"type": "input_text", "text": "系统"}]},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "查天气"}]},
                {
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "blob",
                    "summary": [],
                },
                {"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "这就去查"}]},
                {"type": "function_call", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "晴"},
            ])
        );
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
            body["input"],
            json!([
                {"type": "function_call", "call_id": "call_1", "name": "screenshot", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "截图如下"},
                {"type": "message", "role": "user", "content": [
                    {"type": "input_text", "text": "[image returned by tool call_1]"},
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                ]},
            ])
        );
    }

    #[test]
    fn reasoning_item_echoed_verbatim() {
        let item = json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "opaque-blob",
            "summary": [{"type": "summary_text", "text": "摘要"}],
            "future_field": {"nested": [1, 2, 3]},
        });
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Reasoning(ReasoningBlock::Item(Box::new(
                    item.clone(),
                )))],
            }],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["input"], json!([item]));

        // echo = Never 时被剔除。
        let req = ChatRequest {
            options: RequestOptions {
                echo_reasoning: ReasoningEcho::Never,
                ..Default::default()
            },
            ..req
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["input"], json!([]));
    }

    #[test]
    fn parallel_function_call_outputs_pair_by_call_id() {
        let req = ChatRequest {
            messages: vec![
                Message::user_text("并行查"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning(ReasoningBlock::Item(Box::new(json!({
                            "type": "reasoning",
                            "id": "rs_1",
                        })))),
                        ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                        ContentPart::ToolCall(ToolCall::new("call_2", "get_date", "{}")),
                    ],
                },
                Message::tool_result("call_1", "晴", false),
                Message::tool_result("call_2", "2026-09-24", false),
            ],
            ..Default::default()
        };
        let body = encode_body(&config(), &req, false);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 6);
        // reasoning item 位于全部 function_call 之前。
        assert_eq!(input[1]["type"], json!("reasoning"));
        assert_eq!(input[2]["call_id"], json!("call_1"));
        assert_eq!(input[3]["call_id"], json!("call_2"));
        assert_eq!(input[4]["type"], json!("function_call_output"));
        assert_eq!(input[4]["call_id"], json!("call_1"));
        assert_eq!(input[4]["output"], json!("晴"));
        assert_eq!(input[5]["call_id"], json!("call_2"));
        assert_eq!(input[5]["output"], json!("2026-09-24"));
    }

    #[test]
    fn max_tokens_parallel_and_store() {
        let req = ChatRequest {
            max_output_tokens: Some(2048),
            temperature: Some(0.2),
            top_p: Some(0.8),
            parallel_tool_calls: Some(false),
            options: RequestOptions {
                store: Some(false),
                ..Default::default()
            },
            ..base_request()
        };
        let body = encode_body(&config(), &req, false);
        assert_eq!(body["max_output_tokens"], json!(2048));
        assert_eq!(body["temperature"], json!(0.2));
        assert_eq!(body["top_p"], json!(0.8));
        assert_eq!(body["parallel_tool_calls"], json!(false));
        assert_eq!(body["store"], json!(false));

        // 线格式不支持并行开关时不发送。
        let cfg = LlmConfig {
            wire: WireConfig {
                parallel_tool_calls: false,
                ..Default::default()
            },
            ..config()
        };
        let body = encode_body(&cfg, &req, false);
        assert!(body.get("parallel_tool_calls").is_none());

        // stop 无对应字段，忽略。
        let with_stop = ChatRequest {
            stop: vec!["STOP".into()],
            ..base_request()
        };
        let body = encode_body(&config(), &with_stop, false);
        assert!(body.get("stop").is_none());
        assert!(body.get("stop_sequences").is_none());
    }

    #[test]
    fn reasoning_config_mapping() {
        let enabled = ChatRequest {
            reasoning: Some(ReasoningConfig {
                enabled: true,
                effort: Some(crate::types::Effort::Medium),
                summary: Some(ReasoningSummary::Detailed),
            }),
            ..base_request()
        };
        let body = encode_body(&config(), &enabled, false);
        assert_eq!(
            body["reasoning"],
            json!({"effort": "medium", "summary": "detailed"})
        );

        // 关闭：发送 effort=none。
        let disabled = ChatRequest {
            reasoning: Some(ReasoningConfig {
                enabled: false,
                effort: Some(crate::types::Effort::High),
                ..Default::default()
            }),
            ..base_request()
        };
        let body = encode_body(&config(), &disabled, false);
        assert_eq!(body["reasoning"], json!({"effort": "none"}));

        // Omitted 不发送 summary（Responses 没有关闭摘要的取值）。
        let omitted = ChatRequest {
            reasoning: Some(ReasoningConfig {
                enabled: true,
                summary: Some(ReasoningSummary::Omitted),
                ..Default::default()
            }),
            ..base_request()
        };
        let body = encode_body(&config(), &omitted, false);
        assert!(body.get("reasoning").is_none(), "{body}");

        // 未配置 reasoning 时完全不发送。
        let body = encode_body(&config(), &base_request(), false);
        assert!(body.get("reasoning").is_none());
    }

    #[test]
    fn include_encrypted_reasoning() {
        let body = encode_body(&config(), &base_request(), false);
        assert_eq!(body["include"], json!(["reasoning.encrypted_content"]));

        let never = ChatRequest {
            options: RequestOptions {
                echo_reasoning: ReasoningEcho::Never,
                ..Default::default()
            },
            ..base_request()
        };
        let body = encode_body(&config(), &never, false);
        assert!(body.get("include").is_none());

        // 与 extra_body 里的 include 合并去重。
        let cfg = LlmConfig {
            extra_body: serde_json::Map::from_iter([(
                "include".into(),
                json!([
                    "message.output_text.logprobs",
                    "reasoning.encrypted_content"
                ]),
            )]),
            ..config()
        };
        let body = encode_body(&cfg, &base_request(), false);
        assert_eq!(
            body["include"],
            json!([
                "message.output_text.logprobs",
                "reasoning.encrypted_content"
            ])
        );
    }

    #[test]
    fn image_input_block() {
        let req = ChatRequest {
            messages: vec![Message {
                role: Role::User,
                content: vec![
                    ContentPart::Text("看图".into()),
                    ContentPart::Image {
                        source: ImageSource::Url("https://example.com/a.png".into()),
                        detail: Some(ImageDetail::Low),
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
            body["input"][0]["content"],
            json!([
                {"type": "input_text", "text": "看图"},
                {"type": "input_image", "image_url": "https://example.com/a.png", "detail": "low"},
                {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
            ])
        );
    }

    #[test]
    fn cache_breakpoints_and_ttl_validation() {
        let req = ChatRequest {
            messages: vec![Message::system("系统"), Message::user_text("hi")],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    key: Some("session-1".into()),
                    breakpoints: vec![0],
                    instruction_blocks: Vec::new(),
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
            body["input"][0]["content"],
            json!([{"type": "input_text", "text": "系统", "prompt_cache_breakpoint": {"mode": "explicit"}}])
        );

        let bad_ttl = ChatRequest {
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![0],
                    ttl: Some(CacheTtl::OneHour),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..req
        };
        let err = encode(&config(), &bad_ttl, false).unwrap_err();
        assert!(err.to_string().contains("只支持 30m"), "{err}");

        // 断点落在没有文本内容块的 item 上：报错而不是静默丢弃。
        let bad_target = ChatRequest {
            messages: vec![
                Message::user_text("hi"),
                assistant_with_tools(&[("call_1", "get_weather")]),
                Message::tool_result("call_1", "晴", false),
            ],
            options: RequestOptions {
                cache: Some(CacheConfig {
                    breakpoints: vec![2],
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        };
        let err = encode(&config(), &bad_target, false).unwrap_err();
        assert!(err.to_string().contains("只能落在文本内容块上"), "{err}");
    }

    #[test]
    fn unsupported_reasoning_blocks_are_rejected() {
        let text_block = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Reasoning(ReasoningBlock::text("思考"))],
            }],
            ..Default::default()
        };
        let err = encode(&config(), &text_block, false).unwrap_err();
        assert!(err.to_string().contains("reasoning item"), "{err}");

        let redacted = ChatRequest {
            messages: vec![Message {
                role: Role::Assistant,
                content: vec![ContentPart::Reasoning(ReasoningBlock::Redacted {
                    data: "opaque".into(),
                })],
            }],
            ..Default::default()
        };
        let err = encode(&config(), &redacted, false).unwrap_err();
        assert!(err.to_string().contains("reasoning item"), "{err}");

        // echo = Never：core 主动裁剪历史，直接剔除而不是报错。
        let never = ChatRequest {
            options: RequestOptions {
                echo_reasoning: ReasoningEcho::Never,
                ..Default::default()
            },
            ..text_block
        };
        let body = encode_body(&config(), &never, false);
        assert_eq!(body["input"], json!([]));
    }

    #[test]
    fn response_decode_output_items() {
        let body = json!({
            "id": "resp_1",
            "object": "response",
            "model": "gpt-5",
            "status": "completed",
            "output": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "blob", "summary": []},
                {"type": "message", "role": "assistant", "content": [
                    {"type": "output_text", "text": "答案是 42"},
                    {"type": "refusal", "refusal": "拒绝部分"},
                ]},
                {"type": "function_call", "id": "fc_1", "call_id": "call_1", "name": "get_weather", "arguments": "{\"city\":\"北京\"}"},
            ],
            "usage": {
                "input_tokens": 100,
                "output_tokens": 40,
                "total_tokens": 140,
                "input_tokens_details": {"cached_tokens": 64},
                "output_tokens_details": {"reasoning_tokens": 12},
            },
        });
        let response = decode_response(&config(), &base_request(), &body).unwrap();
        assert_eq!(response.id.as_deref(), Some("resp_1"));
        assert_eq!(response.model.as_deref(), Some("gpt-5"));
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
                ContentPart::Reasoning(ReasoningBlock::Item(Box::new(json!({
                    "type": "reasoning",
                    "id": "rs_1",
                    "encrypted_content": "blob",
                    "summary": [],
                })))),
                ContentPart::Text("答案是 42".into()),
                ContentPart::Text("拒绝部分".into()),
                ContentPart::ToolCall(ToolCall::new(
                    "call_1",
                    "get_weather",
                    "{\"city\":\"北京\"}"
                )),
            ]
        );
    }

    #[test]
    fn decode_response_exposes_compaction_item() {
        let item = json!({ "type": "compaction", "id": "cmp_1", "encrypted_content": "opaque" });
        let response = decode_response(
            &config(),
            &base_request(),
            &json!({ "status": "completed", "output": [item.clone()] }),
        )
        .expect("响应可解码");
        assert_eq!(response.compaction.as_deref(), Some(&item));
        assert!(response.message.content.is_empty());
    }

    #[test]
    fn status_to_finish_reason() {
        let cases = [
            (
                json!({"status": "completed", "output": []}),
                FinishReason::Stop,
            ),
            (
                json!({"status": "completed", "output": [{"type": "function_call"}]}),
                FinishReason::ToolCalls,
            ),
            (
                json!({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}),
                FinishReason::Length,
            ),
            (
                json!({"status": "incomplete", "incomplete_details": {"reason": "content_filter"}}),
                FinishReason::ContentFilter,
            ),
            (
                json!({"status": "incomplete", "incomplete_details": {"reason": "something_new"}}),
                FinishReason::Other("something_new".into()),
            ),
            (json!({"status": "cancelled"}), FinishReason::Cancelled),
            (
                json!({"status": "queued"}),
                FinishReason::Other("queued".into()),
            ),
            (json!({}), FinishReason::Other("unknown".into())),
        ];
        for (body, expected) in cases {
            let response = decode_response(&config(), &base_request(), &body).unwrap();
            assert_eq!(response.finish_reason, expected, "{body}");
        }
    }

    #[test]
    fn failed_status_becomes_api_error() {
        let body = json!({
            "status": "failed",
            "error": {"code": "server_error", "message": "upstream exploded"},
        });
        let err = decode_response(&config(), &base_request(), &body).unwrap_err();
        match err {
            LlmError::Api(error) => {
                assert_eq!(error.message, "upstream exploded");
                assert_eq!(error.code.as_deref(), Some("server_error"));
                assert_eq!(error.status, None);
                assert!(error.raw_body.contains("upstream exploded"));
            }
            other => panic!("期望 Api 错误，实际 {other:?}"),
        }
    }

    #[test]
    fn stream_event_sequence() {
        let mut decoder = ResponsesStreamDecoder::new(&config());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"response.created","response":{"id":"resp_1","model":"gpt-5"}}"#,
                r#"{"type":"response.in_progress","response":{"id":"resp_1"}}"#,
                r#"{"type":"response.output_item.added","output_index":0,"item":{"type":"reasoning","id":"rs_1"}}"#,
                r#"{"type":"response.reasoning_summary_text.delta","output_index":0,"delta":"想一下"}"#,
                r#"{"type":"response.output_item.done","output_index":0,"item":{"type":"reasoning","id":"rs_1","encrypted_content":"full-blob","summary":[{"type":"summary_text","text":"想一下"}]}}"#,
                r#"{"type":"response.output_item.added","output_index":1,"item":{"type":"message","id":"msg_1"}}"#,
                r#"{"type":"response.output_item.done","output_index":3,"item":{"type":"compaction","id":"cmp_1","encrypted_content":"opaque"}}"#,
                r#"{"type":"response.output_text.delta","output_index":1,"delta":"答案"}"#,
                r#"{"type":"response.output_text.done","output_index":1,"text":"答案"}"#,
                r#"{"type":"response.output_item.added","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"get_weather"}}"#,
                r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"{\"city\":"}"#,
                r#"{"type":"response.function_call_arguments.delta","output_index":2,"delta":"\"北京\"}"}"#,
                r#"{"type":"response.output_item.done","output_index":2,"item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"北京\"}"}}"#,
                r#"{"type":"response.completed","response":{"id":"resp_1","model":"gpt-5","status":"completed","output":[{"type":"function_call","call_id":"call_1"}],"usage":{"input_tokens":10,"output_tokens":5,"total_tokens":15}}}"#,
                r#"{"type":"response.output_text.delta","output_index":1,"delta":"多余"}"#,
            ],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::Start {
                    id: Some("resp_1".into()),
                    model: Some("gpt-5".into()),
                    effort: None,
                },
                StreamEvent::ReasoningDelta {
                    index: 0,
                    text: "想一下".into(),
                },
                StreamEvent::ReasoningItem {
                    index: 0,
                    item: Box::new(json!({
                        "type": "reasoning",
                        "id": "rs_1",
                        "encrypted_content": "full-blob",
                        "summary": [{"type": "summary_text", "text": "想一下"}],
                    })),
                },
                StreamEvent::CompactionItem {
                    item: Box::new(
                        json!({ "type": "compaction", "id": "cmp_1", "encrypted_content": "opaque" })
                    ),
                },
                StreamEvent::TextDelta {
                    index: 1,
                    text: "答案".into(),
                },
                StreamEvent::ToolCallStart {
                    index: 2,
                    id: "call_1".into(),
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
    fn stream_refusal_and_incomplete() {
        let mut decoder = ResponsesStreamDecoder::new(&config());
        let events = unwrapped(events(
            &mut decoder,
            &[
                r#"{"type":"response.refusal.delta","output_index":0,"delta":"不能回答"}"#,
                r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"usage":{"input_tokens":3,"output_tokens":1}}}"#,
            ],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    index: 0,
                    text: "不能回答".into(),
                },
                StreamEvent::Usage(Usage {
                    input_tokens: 3,
                    output_tokens: 1,
                    ..Default::default()
                }),
                StreamEvent::Done {
                    finish_reason: FinishReason::Length,
                },
            ]
        );
    }

    #[test]
    fn stream_failed_and_error_terminate() {
        let mut decoder = ResponsesStreamDecoder::new(&config());
        let out = events(
            &mut decoder,
            &[
                r#"{"type":"response.failed","response":{"status":"failed","error":{"code":"server_error","message":"boom"}}}"#,
                r#"{"type":"response.completed","response":{"status":"completed"}}"#,
            ],
        );
        assert_eq!(out.len(), 1);
        match &out[0] {
            Err(LlmError::Api(error)) => {
                assert_eq!(error.message, "boom");
                assert_eq!(error.code.as_deref(), Some("server_error"));
            }
            other => panic!("期望 Api 错误，实际 {other:?}"),
        }
        assert!(decoder.is_terminated());

        let mut decoder = ResponsesStreamDecoder::new(&config());
        let out = events(
            &mut decoder,
            &[r#"{"type":"error","code":"invalid_request","message":"bad request"}"#],
        );
        assert_eq!(out.len(), 1);
        assert!(matches!(out[0], Err(LlmError::Api(_))), "{:?}", out[0]);
    }

    #[test]
    fn stream_without_terminal_event_finishes_at_eof() {
        let mut decoder = ResponsesStreamDecoder::new(&config());
        let events = unwrapped(events(
            &mut decoder,
            &[r#"{"type":"response.output_text.delta","output_index":0,"delta":"完"}"#],
        ));
        assert_eq!(
            events,
            vec![
                StreamEvent::TextDelta {
                    index: 0,
                    text: "完".into(),
                },
                StreamEvent::Done {
                    finish_reason: FinishReason::Other("unknown".into()),
                },
            ]
        );
    }
}

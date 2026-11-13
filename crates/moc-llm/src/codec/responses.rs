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
    encode_inspected(cfg, req, stream, None)?.into_wire("Responses")
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
    body.insert("model".into(), Value::String(cfg.model.clone()));
    let input = encode_input(cfg, plan, req, &breakpoints, sources)?;
    body.insert("input".into(), Value::Array(input));
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
        let mut text = body
            .get("text")
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        text.insert("format".into(), encode_format(format));
        body.insert("text".into(), Value::Object(text));
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

    Ok(EncodedRequest {
        url: cfg.endpoint_url(),
        headers: common_headers(cfg, req).into_vec(),
        body: Value::Object(body),
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
    let mut object = cfg
        .extra_body
        .get("reasoning")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if !reasoning.enabled {
        object.remove("summary");
        object.remove("generate_summary");
        // `none` 是 ReasoningEffort 的合法取值。
        object.insert("effort".into(), Value::String("none".into()));
        return Some(Value::Object(object));
    }
    if let Some(effort) = cfg.wire_effort(Some(reasoning)) {
        object.insert("effort".into(), Value::String(effort));
    }
    if let Some(summary) = reasoning.summary {
        if let Some(value) = summary_value(summary) {
            object.insert("summary".into(), Value::String(value.into()));
        } else {
            object.remove("summary");
            object.remove("generate_summary");
        }
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
    mut sources: Option<&mut Vec<super::RequestSource>>,
) -> Result<Vec<Value>, LlmError> {
    let mut items = Vec::with_capacity(req.messages.len());
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
        let first = items.len();
        let breakpoint = breakpoints.contains(&index);
        match message.role {
            Role::System | Role::Developer | Role::Custom | Role::User => {
                items.push(encode_message_item(cfg, message, breakpoint)?);
            }
            Role::Assistant => {
                encode_assistant_items(plan, message, breakpoint, &mut items)?;
            }
            // tool 结果携带的图片无法进入 function_call_output，编码为紧随的 user 消息。
            Role::Tool => items.extend(encode_tool_result(cfg, message, breakpoint)?),
        }
        if let Some(sources) = sources.as_deref_mut() {
            super::inspection::openai_sources(
                sources,
                index,
                message,
                &items[first..],
                first,
                "input",
            );
        }
    }
    Ok(items)
}

/// 系统 / 开发者 / 用户消息：都是 `input` 里的 `message` item。
fn encode_message_item(
    _cfg: &LlmConfig,
    message: &Message,
    breakpoint: bool,
) -> Result<Value, LlmError> {
    let (role, text_type) = match message.role {
        Role::System => ("system", "input_text"),
        Role::Developer | Role::Custom => ("developer", "input_text"),
        Role::User => ("user", "input_text"),
        other => {
            return Err(LlmError::Encode(format!(
                "{other} 消息不能编码为 Responses 的 message item"
            )));
        }
    };
    let mut blocks = Vec::with_capacity(message.content.len());
    if matches!(message.role, Role::System | Role::Developer | Role::Custom) {
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
    let mut phase = None;
    let mut last_text = None;
    for part in &message.content {
        match part {
            ContentPart::Text(text) => pending_text.push_str(text),
            ContentPart::ResponsePhase(value) => {
                flush_text(&mut pending_text, &mut phase, &mut last_text, items);
                phase = Some(value.as_str());
            }
            ContentPart::Reasoning(block) => {
                if !plan.keeps(block) {
                    continue;
                }
                if let ReasoningBlock::Item(item) = block {
                    flush_text(&mut pending_text, &mut phase, &mut last_text, items);
                    items.push(item.raw.clone());
                }
            }
            ContentPart::ToolCall(call) => {
                flush_text(&mut pending_text, &mut phase, &mut last_text, items);
                items.push(json!({
                    "type": "function_call", "call_id": call.id,
                    "name": call.name, "arguments": call.arguments,
                }));
            }
            other => return Err(unsupported_part(message.role, other)),
        }
    }
    flush_text(&mut pending_text, &mut phase, &mut last_text, items);

    if breakpoint {
        let position = last_text.ok_or_else(|| {
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
fn flush_text(
    pending: &mut String,
    phase: &mut Option<&str>,
    last_text: &mut Option<usize>,
    items: &mut Vec<Value>,
) {
    if pending.is_empty() {
        return;
    }
    let mut item = json!({
        "type": "message", "role": "assistant",
        "content": [{"type": "output_text", "text": std::mem::take(pending)}],
    });
    if let Some(phase) = phase.take() {
        item["phase"] = json!(phase);
    }
    *last_text = Some(items.len());
    items.push(item);
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
        ContentPart::ResponsePhase(_) => "ResponsePhase",
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
                    (item.clone()).into(),
                ))));
            }
            Some("message") => {
                for block in item
                    .get("content")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    if let Some("output_text" | "refusal") =
                        block.get("type").and_then(Value::as_str)
                    {
                        if let Some(phase) = item.get("phase").and_then(Value::as_str) {
                            content.push(ContentPart::ResponsePhase(phase.to_owned()));
                        }
                        let key = if block.get("type").and_then(Value::as_str) == Some("refusal") {
                            "/refusal"
                        } else {
                            "/text"
                        };
                        content.push(ContentPart::Text(string_at(block, key)));
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
            content: content.into(),
        },
        finish_reason: finish_reason(value),
        usage: value.get("usage").map_or_default(openai::decode_usage),
        raw: None,
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
    LlmError::Api(Box::new(api_error))
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
    /// 文本通道最近一次的 `(output_index, content_index)`。
    text_part: Option<(usize, usize)>,
    /// 思维链通道最近一次的 `(output_index, summary_index / content_index)`。
    reasoning_part: Option<(usize, usize)>,
}

impl ResponsesStreamDecoder {
    /// 构造。
    pub fn new(_cfg: &LlmConfig) -> Self {
        Self {
            started: false,
            terminated: false,
            text_part: None,
            reasoning_part: None,
        }
    }

    fn start(&mut self, response: &Value, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        if !self.started {
            self.started = true;
            out.push(Ok(StreamEvent::Start {
                id: optional_string_at(response, "/id"),
                model: optional_string_at(response, "/model"),
                effort: super::reported_effort(response),
            }));
        }
    }

    /// 同一输出项内的子块（`summary_index` / `content_index`）是独立段落，delta
    /// 自身不带边界；同输出项内子块前进时在增量前补段落分隔，否则 `**A**` + `**B**`
    /// 会拼成 `**A****B**` 这类坏 Markdown。跨 `output_index` 对应不同输出项，各自
    /// 从空前缀起算，不能给新输出项补前导空行。
    fn part_delta(last: &mut Option<(usize, usize)>, value: &Value, sub_key: &str) -> String {
        let part = (index_of(value), index_at(value, sub_key));
        let boundary = last
            .replace(part)
            .is_some_and(|(prev_output, prev_sub)| prev_output == part.0 && prev_sub != part.1);
        let delta = delta_text(value);
        if boundary {
            format!("\n\n{delta}")
        } else {
            delta
        }
    }

    /// 完整字段覆盖增量；终态只给出部分字段的兼容网关不得清空已收内容。
    fn output_item(
        index: usize,
        item: &Value,
        complete: bool,
        out: &mut Vec<Result<StreamEvent, LlmError>>,
    ) {
        match item.get("type").and_then(Value::as_str) {
            Some("function_call") => out.push(Ok(StreamEvent::ToolCallSnapshot {
                index,
                id: optional_string_at(item, "/call_id"),
                name: optional_string_at(item, "/name"),
                arguments: item
                    .get("arguments")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            })),
            Some("reasoning") if complete => out.push(Ok(StreamEvent::ReasoningItem {
                index,
                item: Box::new(item.clone()),
            })),
            Some("message") => {
                if let Some(phase) = item.get("phase").and_then(Value::as_str) {
                    out.push(Ok(StreamEvent::ResponsePhase {
                        index,
                        phase: phase.to_owned(),
                    }));
                }
                if let Some(blocks) = item.get("content").and_then(Value::as_array) {
                    let text: String = blocks
                        .iter()
                        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
                            Some("output_text") => block.get("text").and_then(Value::as_str),
                            Some("refusal") => block.get("refusal").and_then(Value::as_str),
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join("\n\n");
                    if !text.is_empty() {
                        out.push(Ok(StreamEvent::TextSnapshot { index, text }));
                    }
                }
            }
            _ => {}
        }
    }

    fn decode_payload(&mut self, value: &Value, out: &mut Vec<Result<StreamEvent, LlmError>>) {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "response.created" => {
                if let Some(response) = value.get("response") {
                    self.start(response, out);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                out.push(Ok(StreamEvent::TextDelta {
                    index: index_of(value),
                    text: Self::part_delta(&mut self.text_part, value, "content_index"),
                }));
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                let sub_key = if kind == "response.reasoning_summary_text.delta" {
                    "summary_index"
                } else {
                    "content_index"
                };
                out.push(Ok(StreamEvent::ReasoningDelta {
                    index: index_of(value),
                    text: Self::part_delta(&mut self.reasoning_part, value, sub_key),
                }));
            }
            "response.function_call_arguments.delta" => {
                out.push(Ok(StreamEvent::ToolCallArgumentsDelta {
                    index: index_of(value),
                    json: string_at(value, "/delta"),
                }));
            }
            "response.function_call_arguments.done" => {
                out.push(Ok(StreamEvent::ToolCallSnapshot {
                    index: index_of(value),
                    id: None,
                    name: None,
                    arguments: value
                        .get("arguments")
                        .and_then(Value::as_str)
                        .map(str::to_owned),
                }));
            }
            "response.output_item.added" | "response.output_item.done" => {
                if let Some(item) = value.get("item") {
                    Self::output_item(
                        index_of(value),
                        item,
                        kind == "response.output_item.done",
                        out,
                    );
                }
            }
            "response.completed" | "response.incomplete" => {
                self.terminated = true;
                let response = value.get("response").unwrap_or(&Value::Null);
                self.start(response, out);
                if let Some(items) = response.get("output").and_then(Value::as_array) {
                    for (index, item) in items.iter().enumerate() {
                        Self::output_item(index, item, true, out);
                    }
                }
                if let Some(usage) = response.get("usage").filter(|usage| !usage.is_null()) {
                    out.push(Ok(StreamEvent::Usage(openai::decode_usage(usage))));
                }
                out.push(Ok(StreamEvent::Done {
                    finish_reason: finish_reason(response),
                }));
            }
            "response.failed" => {
                self.terminated = true;
                let response = value.get("response").unwrap_or(&Value::Null);
                out.push(Err(failed_error(response)));
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
                out.push(Err(LlmError::Api(Box::new(error))));
            }
            // 不承载客户端内容的进度、注解与冗余子块结束事件不产生增量。
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
    index_at(value, "output_index")
}

fn index_at(value: &Value, key: &str) -> usize {
    value
        .get(key)
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
mod tests;

//! 协议编解码层。
//!
//! 三个协议编码器只实现纯函数与状态机，与网络交互全部由 [`crate::Client`] 与
//! [`crate::ChatStream`] 承担，本模块负责分发：

use std::collections::BTreeMap;

use reqwest::header::HeaderMap;
use serde_json::Value;

use crate::config::{Auth, LlmConfig, Protocol};
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::retry::parse_retry_after;
use crate::types::{ChatRequest, ChatResponse, Message, Role, StreamEvent, ToolCall};

use self::sse::SseEvent;

pub(crate) mod anthropic;
pub(crate) mod chat;
pub(crate) mod echo;
pub(crate) mod openai;
pub(crate) mod responses;
pub(crate) mod sse;

/// 编码后的 HTTP 请求。
#[derive(Debug)]
pub(crate) struct EncodedRequest {
    /// 完整 URL（含查询串）。
    pub url: String,
    /// 请求头，已含 `content-type` 与鉴权头。
    pub headers: Vec<(String, String)>,
    /// 请求体；重试时直接克隆字节，不做二次序列化。
    pub body: Vec<u8>,
}

/// 请求头集合。
///
/// 名称统一小写（HTTP 头名大小写不敏感），后写覆盖先写，使优先级规则集中在一处。
/// 头部数量是个位数，`BTreeMap` 的有序迭代还顺带保证了测试断言的确定性。
pub(crate) struct HeaderBag {
    entries: BTreeMap<String, String>,
}

impl HeaderBag {
    /// 空集合。
    pub fn new() -> Self {
        Self {
            entries: BTreeMap::new(),
        }
    }

    /// 写入一个头；同名时覆盖。
    pub fn set(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries
            .insert(name.into().to_ascii_lowercase(), value.into());
    }

    /// 仅在缺少该头时写入：协议默认值不该覆盖 core 的显式配置。
    pub fn set_default(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.entries
            .entry(name.into().to_ascii_lowercase())
            .or_insert_with(|| value.into());
    }

    /// 取出为有序的键值对序列。
    pub fn into_vec(self) -> Vec<(String, String)> {
        self.entries.into_iter().collect()
    }
}

/// 组装三种协议共有的请求头。
///
/// 优先级从低到高：协议默认头（由各协议在调用本函数后追加）<
/// 鉴权头 < [`LlmConfig::headers`] < [`crate::RequestOptions::headers`]。
pub(crate) fn common_headers(cfg: &LlmConfig, req: &ChatRequest) -> HeaderBag {
    let mut bag = HeaderBag::new();
    bag.set("content-type", "application/json");
    match &cfg.auth {
        Auth::None => {}
        Auth::Bearer(token) => bag.set("authorization", format!("Bearer {token}")),
        Auth::Header { name, value } => bag.set(name.clone(), value.clone()),
    }
    for (name, value) in &cfg.headers {
        bag.set(name.clone(), value.clone());
    }
    for (name, value) in &req.options.headers {
        bag.set(name.clone(), value.clone());
    }
    bag
}

/// 上游错误体中抽出的字段。
pub(crate) struct ErrorFields {
    /// 错误消息。
    pub message: String,
    /// `error.code` / `error.type` / `details.error_code` 原文。
    pub code: Option<String>,
    /// 错误体自带的请求 ID（Anthropic 放在 body 顶层，OpenAI 放在响应头）。
    pub request_id: Option<String>,
}

/// 按 [`LlmConfig::protocol`] 编码请求。
pub(crate) fn encode(
    cfg: &LlmConfig,
    req: &ChatRequest,
    stream: bool,
) -> Result<EncodedRequest, LlmError> {
    validate_tool_history(&req.messages)?;
    match cfg.protocol {
        Protocol::OpenAiChat => chat::encode(cfg, req, stream),
        Protocol::OpenAiResponses => responses::encode(cfg, req, stream),
        Protocol::AnthropicMessages => anthropic::encode(cfg, req, stream),
    }
}

/// 非流式响应解码；`capture_raw` 为真时把原始响应体存入 [`ChatResponse::raw`]。
pub(crate) fn decode_response(
    cfg: &LlmConfig,
    req: &ChatRequest,
    body: &[u8],
) -> Result<ChatResponse, LlmError> {
    let value: Value = serde_json::from_slice(body).map_err(|err| LlmError::Decode {
        message: format!("响应体不是合法 JSON: {err}"),
        body: Some(String::from_utf8_lossy(body).into_owned().into_boxed_str()),
    })?;
    if let Some(error) = in_band_error(cfg, &value, body) {
        return Err(LlmError::Api(error));
    }
    let mut response = match cfg.protocol {
        Protocol::OpenAiChat => chat::decode_response(cfg, req, &value)?,
        Protocol::OpenAiResponses => responses::decode_response(cfg, req, &value)?,
        Protocol::AnthropicMessages => anthropic::decode_response(cfg, req, &value)?,
    };
    if req.options.capture_raw {
        response.raw = Some(Box::new(value));
    }
    Ok(response)
}

/// 识别「HTTP 200 但响应体是错误对象」的形态：网关与代理经常这样返回。
///
/// 流式路径三家协议都有等价判断，非流式此前会把它解码成空回复，用户只看到
/// 「成功但没有内容」。错误消息与代号由各协议的错误体解析器给出；解析不出
/// 任何字段（`{"error":null}`、`{"error":{}}`）时按正常响应继续解码。
fn in_band_error(cfg: &LlmConfig, value: &Value, body: &[u8]) -> Option<ApiError> {
    if !value.get("error").is_some_and(|error| !error.is_null()) {
        return None;
    }
    let (kind, fields) = match cfg.protocol {
        // Anthropic 的错误类型有现成映射表，流中错误事件与 200 响应体共用。
        Protocol::AnthropicMessages => {
            let fields = anthropic::parse_error_value(value)?;
            let kind = anthropic::kind_from_error_type(
                fields.code.as_deref().unwrap_or_default(),
                &fields.message,
            );
            (kind, fields)
        }
        // OpenAI 两家没有错误类型表：按关键词细化，未命中即 `Unknown`。
        Protocol::OpenAiChat | Protocol::OpenAiResponses => {
            let fields = openai::parse_error_value(value)?;
            let kind = ApiErrorKind::Unknown.refine(fields.code.as_deref(), &fields.message);
            (kind, fields)
        }
    };
    let mut error = ApiError::new(
        None,
        kind,
        fields.message,
        String::from_utf8_lossy(body).into_owned(),
    );
    error.code = fields.code;
    error.request_id = fields.request_id;
    Some(error)
}

/// 非 2xx 响应解码为 [`ApiError`]。
///
/// 错误体解析失败时降级：分类由状态码兜底，`raw_body` 保留原文。
pub(crate) fn decode_error(
    cfg: &LlmConfig,
    status: u16,
    headers: &HeaderMap,
    body: &[u8],
) -> ApiError {
    let raw = String::from_utf8_lossy(body).into_owned().into_boxed_str();
    let kind = ApiErrorKind::from_status(status);
    let fields = match cfg.protocol {
        Protocol::AnthropicMessages => anthropic::parse_error_body(body),
        Protocol::OpenAiChat | Protocol::OpenAiResponses => openai::parse_error_body(body),
    };
    let mut error = match fields {
        Some(fields) => {
            let kind = kind.refine(fields.code.as_deref(), &fields.message);
            let mut error = ApiError::new(Some(status), kind, fields.message, raw);
            error.code = fields.code;
            error.request_id = fields.request_id;
            error
        }
        None => ApiError::new(Some(status), kind, truncated(&raw, 512), raw),
    };
    // 响应头里的请求 ID 更可靠，优先于错误体自带的那个。
    if let Some(from_headers) = request_id(headers) {
        error.request_id = Some(from_headers);
    }
    error.retry_after = parse_retry_after(
        headers
            .get("retry-after")
            .and_then(|value| value.to_str().ok()),
    );
    error
}

/// 从响应头里取上游请求 ID。
fn request_id(headers: &HeaderMap) -> Option<String> {
    const CANDIDATES: [&str; 3] = ["request-id", "x-request-id", "x-amzn-requestid"];
    CANDIDATES.iter().find_map(|name| {
        headers
            .get(*name)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned)
    })
}

/// 从上游回显（响应体或流事件载荷）里提取实际生效的推理强度原文。
///
/// 各网关位置不一：OpenAI 新版嵌套在 `reasoning.effort`，旧版是顶层
/// `reasoning_effort`，Anthropic 风格放在 `output_config.effort`；按此优先级取
/// 第一个非空字符串。缺失或为空都返回 `None`——「上游没回显」不产生任何比对结论。
///
/// 传进来的 `value` 必须是读 `model` 的同一层对象（Chat 是根、Anthropic 是
/// `/message`、Responses 是 `/response`）。
pub(crate) fn reported_effort(value: &Value) -> Option<String> {
    [
        "/reasoning/effort",
        "/reasoning_effort",
        "/output_config/effort",
    ]
    .into_iter()
    .find_map(|pointer| {
        value
            .pointer(pointer)
            .and_then(Value::as_str)
            .filter(|effort| !effort.trim().is_empty())
            .map(str::to_owned)
    })
}

/// 按字符边界截断，避免把多字节字符切成两半。
pub(crate) fn truncated(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((cut, _)) => {
            let mut out = String::with_capacity(cut + 1);
            out.push_str(&text[..cut]);
            out.push('…');
            out
        }
        None => text.to_owned(),
    }
}

/// 流式状态机。
///
/// 每个协议一个实现，由 [`stream_decoder`] 按 [`LlmConfig::protocol`] 选择。
pub(crate) trait StreamDecoder: Send {
    /// 消费一个 SSE 帧，把产生的事件追加到 `out`。
    ///
    /// 一个帧可能产出 0 个、1 个或多个事件（例如 Chat 的收尾块同时带 `usage` 与
    /// `finish_reason`，Responses 的 `response.completed` 同时带用量与结束原因），
    /// 因此用调用方复用的缓冲区而不是单个事件，避免每帧分配。
    fn decode(&mut self, event: SseEvent, out: &mut Vec<Result<StreamEvent, LlmError>>);

    /// 流结束收尾（补发 `Done` 等）。
    fn finish(&mut self, out: &mut Vec<Result<StreamEvent, LlmError>>);

    /// `[DONE]` 哨兵是否出现、或流是否已到达终态；终态之后的帧直接丢弃。
    fn is_terminated(&self) -> bool;
}

/// 按协议构造流式解码器。
pub(crate) fn stream_decoder(cfg: &LlmConfig, req: &ChatRequest) -> Box<dyn StreamDecoder> {
    match cfg.protocol {
        Protocol::OpenAiChat => Box::new(chat::ChatStreamDecoder::new(cfg)),
        Protocol::OpenAiResponses => Box::new(responses::ResponsesStreamDecoder::new(cfg)),
        Protocol::AnthropicMessages => Box::new(anthropic::AnthropicStreamDecoder::new(cfg, req)),
    }
}

/// 校验工具调用与工具结果的配对关系。
///
/// 每个含 [`crate::ContentPart::ToolCall`] 的 assistant 消息，其后紧跟的连续
/// [`Role::Tool`] 消息必须恰好覆盖其调用 ID 集合：不多、不少、顺序一致。
/// 提前把上游那条难以定位的 400 变成明确的本地错误。
pub(crate) fn validate_tool_history(messages: &[Message]) -> Result<(), LlmError> {
    let mut index = 0;
    while index < messages.len() {
        let message = &messages[index];
        if message.role != Role::Assistant {
            if message.role == Role::Tool {
                return Err(stray_result_error(
                    message
                        .tool_results()
                        .next()
                        .map_or("", |result| result.tool_call_id.as_str()),
                ));
            }
            index += 1;
            continue;
        }

        let calls: Vec<&ToolCall> = message.tool_calls().collect();
        index += 1;
        if calls.is_empty() {
            continue;
        }

        let mut matched = 0;
        while index < messages.len() && messages[index].role == Role::Tool {
            let result = messages[index]
                .tool_results()
                .next()
                .ok_or_else(|| LlmError::Encode("工具结果消息缺少 ToolResult 内容块".into()))?;
            match calls.get(matched) {
                Some(call) if call.id == result.tool_call_id => matched += 1,
                _ => return Err(stray_result_error(&result.tool_call_id)),
            }
            index += 1;
        }

        if matched != calls.len() {
            let names = join_names(&calls);
            let missing = join_names(&calls[matched..]);
            return Err(LlmError::Encode(format!(
                "assistant 消息中的工具调用 [{names}] 缺少对应的工具结果消息（缺失: {missing}）"
            )));
        }
    }
    Ok(())
}

fn stray_result_error(tool_call_id: &str) -> LlmError {
    LlmError::Encode(format!(
        "工具结果消息的 tool_call_id \"{tool_call_id}\" 不在前一条 assistant 消息的工具调用中"
    ))
}

fn join_names(calls: &[&ToolCall]) -> String {
    let mut out = String::new();
    for (position, call) in calls.iter().enumerate() {
        if position > 0 {
            out.push_str(", ");
        }
        out.push_str(&call.name);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ContentPart, ReasoningBlock, ToolCall};
    use serde_json::json;

    fn tool_call(id: &str, name: &str) -> ContentPart {
        ContentPart::ToolCall(ToolCall::new(id, name, "{}"))
    }

    fn assistant_with_calls(calls: &[(&str, &str)]) -> Message {
        Message {
            role: Role::Assistant,
            content: calls.iter().map(|(id, name)| tool_call(id, name)).collect(),
        }
    }

    #[test]
    fn complete_tool_history_passes() {
        let messages = vec![
            Message::user_text("天气"),
            assistant_with_calls(&[("call_1", "get_weather"), ("call_2", "get_date")]),
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_2", "2026-09-24", false),
            Message::user_text("谢谢"),
        ];
        assert!(validate_tool_history(&messages).is_ok());
    }

    #[test]
    fn missing_tool_result_reports_missing_names() {
        let messages = vec![
            Message::user_text("天气"),
            assistant_with_calls(&[("call_1", "get_weather"), ("call_2", "get_date")]),
            Message::tool_result("call_1", "晴", false),
        ];
        let err = validate_tool_history(&messages).unwrap_err();
        assert_eq!(
            err.to_string(),
            "构建请求失败: assistant 消息中的工具调用 [get_weather, get_date] 缺少对应的工具结果消息（缺失: get_date）"
        );

        // 一个结果都没有时，错误里同样点名全部调用。
        let messages = vec![
            assistant_with_calls(&[("call_1", "get_weather")]),
            Message::user_text("算了"),
        ];
        let err = validate_tool_history(&messages).unwrap_err();
        assert!(err.to_string().contains("缺失: get_weather"), "{err}");
    }

    #[test]
    fn stray_tool_result_is_rejected() {
        let messages = vec![
            Message::user_text("天气"),
            assistant_with_calls(&[("call_1", "get_weather")]),
            Message::tool_result("call_1", "晴", false),
            Message::tool_result("call_x", "多余", false),
        ];
        let err = validate_tool_history(&messages).unwrap_err();
        assert_eq!(
            err.to_string(),
            "构建请求失败: 工具结果消息的 tool_call_id \"call_x\" 不在前一条 assistant 消息的工具调用中"
        );

        // 顺序错位同样按「不在前一条 assistant 消息的工具调用中」报错。
        let messages = vec![
            assistant_with_calls(&[("call_1", "get_weather"), ("call_2", "get_date")]),
            Message::tool_result("call_2", "2026-09-24", false),
        ];
        let err = validate_tool_history(&messages).unwrap_err();
        assert!(err.to_string().contains("call_2"), "{err}");
    }

    #[test]
    fn leading_tool_result_is_rejected() {
        let messages = vec![
            Message::tool_result("call_1", "晴", false),
            Message::user_text("继续"),
        ];
        let err = validate_tool_history(&messages).unwrap_err();
        assert!(err.to_string().contains("call_1"), "{err}");
    }

    #[test]
    fn tool_message_without_result_block_is_rejected() {
        let messages = vec![
            assistant_with_calls(&[("call_1", "get_weather")]),
            Message {
                role: Role::Tool,
                content: vec![ContentPart::Text("不是结果".into())],
            },
        ];
        let err = validate_tool_history(&messages).unwrap_err();
        assert!(err.to_string().contains("缺少 ToolResult 内容块"), "{err}");
    }

    #[test]
    fn reasoning_only_assistant_message_is_allowed() {
        let messages = vec![Message {
            role: Role::Assistant,
            content: vec![ContentPart::Reasoning(ReasoningBlock::text("只有思考"))],
        }];
        assert!(validate_tool_history(&messages).is_ok());
    }

    #[test]
    fn encode_validates_history_before_dispatch() {
        let cfg = LlmConfig {
            protocol: Protocol::OpenAiChat,
            model: "test".into(),
            ..Default::default()
        };
        let req = ChatRequest {
            messages: vec![assistant_with_calls(&[("call_1", "get_weather")])],
            ..Default::default()
        };
        let err = encode(&cfg, &req, false).unwrap_err();
        assert!(matches!(err, LlmError::Encode(_)), "{err:?}");
    }

    #[test]
    fn decode_response_rejects_in_band_error_body() {
        // HTTP 200 + 错误体：三家协议都必须报错，而不是解码成「成功但空回复」。
        let cases = [
            (
                // 没有 HTTP 状态码时 OpenAI 风格错误只能按关键词细化，未命中即 Unknown。
                Protocol::OpenAiChat,
                json!({"error": {"message": "bad temperature", "type": "invalid_request_error"}}),
                ApiErrorKind::Unknown,
                "bad temperature",
                Some("invalid_request_error"),
            ),
            (
                Protocol::OpenAiResponses,
                json!({"error": {"message": "upstream exploded", "code": "server_error"}}),
                ApiErrorKind::Unknown,
                "upstream exploded",
                Some("server_error"),
            ),
            (
                // Anthropic 有错误类型表，与流中错误事件走同一映射。
                Protocol::AnthropicMessages,
                json!({
                    "type": "error",
                    "error": {"type": "overloaded_error", "message": "Overloaded"},
                    "request_id": "req_1",
                }),
                ApiErrorKind::Overloaded,
                "Overloaded",
                Some("overloaded_error"),
            ),
        ];
        for (protocol, body, kind, message, code) in cases {
            let cfg = LlmConfig {
                protocol,
                ..Default::default()
            };
            let body = body.to_string();
            let err = decode_response(&cfg, &ChatRequest::default(), body.as_bytes()).unwrap_err();
            match err {
                LlmError::Api(error) => {
                    assert_eq!(error.status, None, "{protocol:?}");
                    assert_eq!(error.kind, kind, "{protocol:?}");
                    assert_eq!(error.message, message, "{protocol:?}");
                    assert_eq!(error.code.as_deref(), code, "{protocol:?}");
                    assert_eq!(error.raw_body.as_ref(), body, "{protocol:?}");
                }
                other => panic!("{protocol:?} 期望 Api 错误，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn decode_response_keeps_null_error_field_as_success() {
        // `null` 与空对象不是错误体：网关在正常响应里也会带上该字段。
        let cases = [
            (
                Protocol::OpenAiChat,
                json!({
                    "error": null,
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                }),
                "hi",
            ),
            (
                Protocol::OpenAiChat,
                json!({
                    "error": {},
                    "choices": [{"index": 0, "message": {"role": "assistant", "content": "hi"}, "finish_reason": "stop"}],
                }),
                "hi",
            ),
            (
                Protocol::OpenAiResponses,
                json!({
                    "error": null,
                    "status": "completed",
                    "output": [{"type": "message", "role": "assistant", "content": [{"type": "output_text", "text": "ok"}]}],
                }),
                "ok",
            ),
            (
                Protocol::AnthropicMessages,
                json!({
                    "error": null,
                    "content": [{"type": "text", "text": "ok"}],
                    "stop_reason": "end_turn",
                }),
                "ok",
            ),
        ];
        for (protocol, body, text) in cases {
            let cfg = LlmConfig {
                protocol,
                ..Default::default()
            };
            let body = body.to_string();
            let response = decode_response(&cfg, &ChatRequest::default(), body.as_bytes()).unwrap();
            assert_eq!(response.message.text(), text, "{protocol:?}");
        }
    }
}

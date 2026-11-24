use super::super::ErrorFields;
use crate::config::LlmConfig;
use crate::error::ApiErrorKind;
use crate::error::LlmError;
use crate::types::ChatRequest;
use crate::types::ChatResponse;
use crate::types::ContentPart;
use crate::types::FinishReason;
use crate::types::Message;
use crate::types::ReasoningBlock;
use crate::types::Role;
use crate::types::ToolCall;
use crate::types::Usage;
use moc_i18n::t;
use serde_json::Value;

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
                        message: t!("llm.err.tool_args_serialize", err = err).into_owned(),
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
        reasoning_effort: super::super::reported_effort(value),
        message: Message {
            role: Role::Assistant,
            content: content.into(),
        },
        finish_reason: value
            .get("stop_reason")
            .and_then(Value::as_str)
            .map_or_default(map_stop_reason),
        usage: decode_usage(value.get("usage")),
        raw: None,
    })
}

/// Anthropic 的 `stop_reason` 映射。
///
/// 取值以官方 Messages 文档为准：`end_turn` 是自然结束，`pause_turn` 表示服务端
/// 工具把回合交还给客户端，需要原样重发请求续跑。
pub(super) fn map_stop_reason(reason: &str) -> FinishReason {
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
pub(super) fn decode_usage(usage: Option<&Value>) -> Usage {
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

pub(super) fn string_at(value: &Value, pointer: &str) -> String {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

/// 空串表示字段缺失。
pub(super) fn optional_string_at(value: &Value, pointer: &str) -> Option<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

/// `content_block_*` 事件的下标。
pub(super) fn index_of(value: &Value) -> usize {
    value
        .get("index")
        .and_then(Value::as_u64)
        .and_then(|index| usize::try_from(index).ok())
        .unwrap_or(0)
}

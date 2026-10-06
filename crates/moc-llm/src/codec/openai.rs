//! 两个 OpenAI 协议（Chat Completions 与 Responses）共用的编解码工具。

use serde_json::Value;

use super::ErrorFields;
use crate::types::{FinishReason, Usage};

/// 解析 usage。
///
/// Chat 用 `prompt_tokens` / `completion_tokens`，Responses 用
/// `input_tokens` / `output_tokens`，两套字段名都识别；缓存与思维链明细同样
/// 兼容两套路径。
pub(crate) fn decode_usage(value: &Value) -> Usage {
    let number = |pointer: &str| value.pointer(pointer).and_then(Value::as_u64);
    Usage {
        input_tokens: number("/prompt_tokens")
            .or_else(|| number("/input_tokens"))
            .unwrap_or(0),
        output_tokens: number("/completion_tokens")
            .or_else(|| number("/output_tokens"))
            .unwrap_or(0),
        total_tokens: number("/total_tokens"),
        cached_input_tokens: number("/prompt_tokens_details/cached_tokens")
            .or_else(|| number("/input_tokens_details/cached_tokens")),
        cache_creation_input_tokens: None,
        reasoning_tokens: number("/completion_tokens_details/reasoning_tokens")
            .or_else(|| number("/output_tokens_details/reasoning_tokens")),
    }
}

/// Chat 的 `finish_reason` 映射；未知取值原样保留为
/// [`FinishReason::Other`]。
pub(crate) fn map_finish_reason(reason: &str) -> FinishReason {
    match reason {
        "stop" => FinishReason::Stop,
        "length" => FinishReason::Length,
        "tool_calls" | "function_call" => FinishReason::ToolCalls,
        "content_filter" => FinishReason::ContentFilter,
        other => FinishReason::Other(other.to_owned()),
    }
}

/// 解析 OpenAI 风格错误体。
///
/// 覆盖的形态：`{"error":{"message","type","code"}}`、
/// `{"error":{"code","details":{"error_code"}}}`（Azure 的支出上限）、
/// `{"error":"字符串"}`、以及直接把错误放在顶层的 `{"message":…}` / `{"detail":…}`。
/// 都解析不出消息时返回 `None`，由调用方按状态码降级。
pub(crate) fn parse_error_body(body: &[u8]) -> Option<ErrorFields> {
    let value: Value = serde_json::from_slice(body).ok()?;
    parse_error_value(&value)
}

pub(crate) fn parse_error_value(value: &Value) -> Option<ErrorFields> {
    let error = value.get("error").unwrap_or(value);
    let message = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| value.get("message").and_then(Value::as_str))
        .or_else(|| value.get("detail").and_then(Value::as_str))
        .or_else(|| error.as_str())
        .unwrap_or_default();
    let code = error
        .pointer("/details/error_code")
        .and_then(Value::as_str)
        .or_else(|| error.get("code").and_then(Value::as_str))
        .or_else(|| error.get("type").and_then(Value::as_str));
    if message.is_empty() && code.is_none() {
        return None;
    }
    Some(ErrorFields {
        message: message.to_owned(),
        code: code.map(str::to_owned),
        request_id: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn usage_decodes_both_dialect_field_names() {
        let cases = [
            (
                json!({
                    "prompt_tokens": 100,
                    "completion_tokens": 40,
                    "total_tokens": 140,
                    "prompt_tokens_details": {"cached_tokens": 64},
                    "completion_tokens_details": {"reasoning_tokens": 12}
                }),
                Usage {
                    input_tokens: 100,
                    output_tokens: 40,
                    total_tokens: Some(140),
                    cached_input_tokens: Some(64),
                    reasoning_tokens: Some(12),
                    ..Default::default()
                },
            ),
            (
                json!({
                    "input_tokens": 200,
                    "output_tokens": 30,
                    "total_tokens": 230,
                    "input_tokens_details": {"cached_tokens": 128},
                    "output_tokens_details": {"reasoning_tokens": 7}
                }),
                Usage {
                    input_tokens: 200,
                    output_tokens: 30,
                    total_tokens: Some(230),
                    cached_input_tokens: Some(128),
                    reasoning_tokens: Some(7),
                    ..Default::default()
                },
            ),
        ];
        for (value, expected) in cases {
            assert_eq!(decode_usage(&value), expected, "{value}");
        }
    }

    #[test]
    fn usage_defaults_missing_fields_to_zero() {
        let usage = decode_usage(&json!({}));
        assert_eq!(usage, Usage::default());
        assert_eq!(usage.total_tokens(), 0);
    }

    #[test]
    fn error_body_variants() {
        let fields = parse_error_body(br#"{"error":{"message":"bad key","type":"authentication_error","param":null,"code":"invalid_api_key"}}"#).unwrap();
        assert_eq!(fields.message, "bad key");
        assert_eq!(fields.code.as_deref(), Some("invalid_api_key"));

        let fields =
            parse_error_body(br#"{"error":{"message":"limit","type":"insufficient_quota"}}"#)
                .unwrap();
        assert_eq!(fields.code.as_deref(), Some("insufficient_quota"));

        let fields = parse_error_body(
            br#"{"error":{"code":"spend_limit","details":{"error_code":"enforced_spend_limit_reached"}}}"#,
        )
        .unwrap();
        assert_eq!(fields.code.as_deref(), Some("enforced_spend_limit_reached"));

        let fields = parse_error_body(br#"{"error":"boom"}"#).unwrap();
        assert_eq!(fields.message, "boom");
        assert_eq!(fields.code, None);

        let fields = parse_error_body(br#"{"message":"top-level"}"#).unwrap();
        assert_eq!(fields.message, "top-level");

        let fields = parse_error_body(br#"{"detail":"not found"}"#).unwrap();
        assert_eq!(fields.message, "not found");

        assert!(parse_error_body(b"<html>gateway error</html>").is_none());
        assert!(parse_error_body(br#"{"unrelated":1}"#).is_none());
    }
}

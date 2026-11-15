//! 面向用户的状态码说明；独立于错误分类和自动重试策略。
//!
//! 标准码以 IANA HTTP Status Code Registry 为准；非标准码注明惯例来源，
//! 不把代理转述的状态码当成已确认的厂商或故障根因。
use std::borrow::Cow;

use rust_i18n::t;

use super::ApiErrorKind;

fn s(slug: &str) -> (Cow<'static, str>, Cow<'static, str>) {
    (
        t!(format!("llm.status.{slug}.title")),
        t!(format!("llm.status.{slug}.hint")),
    )
}

pub(super) fn explanation(
    status: Option<u16>,
    kind: ApiErrorKind,
) -> (Cow<'static, str>, Cow<'static, str>) {
    // 明确的供应商错误类型比同一个 HTTP 状态的笼统描述更有用，例如 429 额度耗尽。
    match kind {
        ApiErrorKind::QuotaExceeded => {
            return s("kind_quota");
        }
        ApiErrorKind::ContextLength => {
            return s("kind_context");
        }
        ApiErrorKind::ThinkingSignatureInvalid => {
            return s("kind_thinking_sig");
        }
        ApiErrorKind::ContentFilter => {
            return s("kind_filter");
        }
        _ => {}
    }
    let Some(status) = status else {
        return match kind {
            ApiErrorKind::Auth => s("kind_auth"),
            ApiErrorKind::Permission => s("kind_permission"),
            ApiErrorKind::Billing => s("kind_billing"),
            ApiErrorKind::RateLimit => s("kind_ratelimit"),
            ApiErrorKind::InvalidRequest => s("kind_invalid"),
            ApiErrorKind::NotFound => s("kind_notfound"),
            ApiErrorKind::RequestTooLarge => s("kind_toolarge"),
            ApiErrorKind::Conflict => s("kind_conflict"),
            ApiErrorKind::Overloaded => s("kind_overloaded"),
            ApiErrorKind::ServerError => s("kind_server"),
            ApiErrorKind::Timeout => s("kind_timeout"),
            _ => s("kind_unknown"),
        };
    };
    match status {
        300..=303 | 305..=308 => s("redirect"),
        304 => s("cached"),
        400 => s("bad_request"),
        401 => s("unauthorized"),
        402 => s("payment"),
        403 => s("forbidden"),
        404 => s("not_found"),
        405 => s("method"),
        406 => s("not_acceptable"),
        407 => s("proxy_auth"),
        408 => s("request_timeout"),
        409 => s("conflict"),
        410 => s("gone"),
        411 => s("length_required"),
        412 => s("precondition_failed"),
        413 => s("payload_too_large"),
        414 => s("uri_too_long"),
        415 => s("unsupported_media"),
        416 => s("range_not_satisfiable"),
        417 => s("expectation_failed"),
        418 => s("teapot"),
        421 => s("misdirected"),
        422 => s("unprocessable"),
        423 => s("locked"),
        424 => s("failed_dependency"),
        425 => s("too_early"),
        426 => s("upgrade_required"),
        428 => s("precondition_required"),
        429 => s("rate_limited"),
        431 => s("header_too_large"),
        451 => s("legal"),
        500 => s("internal"),
        501 => s("not_implemented"),
        502 => s("bad_gateway"),
        503 => s("unavailable"),
        504 => s("gateway_timeout"),
        505 => s("http_version"),
        506 => s("variant_negotiates"),
        507 => s("insufficient_storage"),
        508 => s("loop_detected"),
        510 => s("not_extended"),
        511 => s("network_auth"),
        // 以下含义是厂商惯例，其他代理可以重新定义，提示不宣称识别了真实供应商。
        419 | 440 => s("nonstd_auth"),
        444 => s("nonstd_444"),
        494 => s("nonstd_494"),
        495 | 496 => s("nonstd_cert"),
        497 => s("nonstd_http_https"),
        499 => s("nonstd_499"),
        520 => s("nonstd_520"),
        521 => s("nonstd_521"),
        522 => s("nonstd_522"),
        523 => s("nonstd_523"),
        524 => s("nonstd_524"),
        525 => s("nonstd_525"),
        526 => s("nonstd_526"),
        527 => s("nonstd_527"),
        529 => s("nonstd_529"),
        530 => s("nonstd_530"),
        598 | 599 => s("nonstd_598"),
        // 已知码逐条列在前；未识别的状态按类别兜底，不重复列区间以免吞掉上面的具体含义。
        other => match other / 100 {
            1 => s("other_info"),
            2 => s("other_success"),
            3 => s("other_redirect"),
            4 => s("other_client"),
            5 => s("other_server"),
            _ => s("other_nonstd"),
        },
    }
}

/// 显示层收敛长消息和控制字符；ApiError 的 message/raw_body 始终保留原值。
pub(super) fn message_preview(message: &str) -> String {
    let trimmed = message.trim_start();
    if trimmed
        .get(..15)
        .is_some_and(|s| s.eq_ignore_ascii_case("<!doctype html>"))
        || trimmed
            .get(..5)
            .is_some_and(|s| s.eq_ignore_ascii_case("<html"))
    {
        return t!("llm.status.html_page").into_owned();
    }
    let mut out = String::new();
    for (count, ch) in message.chars().enumerate() {
        if count == 512 {
            out.push('…');
            break;
        }
        out.push(if ch.is_control() { ' ' } else { ch });
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ApiError;

    #[test]
    fn display_bounds_plain_text_and_hides_html_without_mutating_original() {
        rust_i18n::set_locale("zh");
        let text = "中".repeat(600) + "\u{1b}[31m";
        let error = ApiError::new(
            Some(502),
            ApiErrorKind::ServerError,
            text.clone(),
            text.clone(),
        );
        assert!(error.to_string().contains('…'));
        assert!(!error.to_string().contains('\u{1b}'));
        assert_eq!(error.message, text);
        assert_eq!(error.raw_body.as_ref(), text);
        assert_eq!(message_preview("错误\n\u{1b}[31m"), "错误  [31m");
        assert!(
            message_preview("  <!DOCTYPE html><html>bad gateway</html>").contains("HTML 错误页")
        );
        assert_eq!(message_preview("<invalid argument>"), "<invalid argument>");
    }
}

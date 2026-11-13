use super::*;

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
fn responses_reasoning_item_is_rejected() {
    let req = ChatRequest {
        messages: vec![Message {
            role: Role::Assistant,
            content: crate::message_content![ContentPart::Reasoning(ReasoningBlock::Item(
                Box::new(json!({"type": "reasoning", "id": "rs_1"}).into())
            ))],
        }],
        max_output_tokens: Some(1024),
        ..Default::default()
    };
    let err = encode(&config(), &req, false).unwrap_err();
    assert!(err.to_string().contains("reasoning item"), "{err}");
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
        let error = crate::codec::decode_error(
            &cfg,
            status,
            &reqwest::header::HeaderMap::new(),
            body.as_bytes(),
        );
        assert_eq!(error.kind, expected, "status {status}: {body}");
    }

    // request_id 来自 body 的顶层字段。
    let error = crate::codec::decode_error(
        &cfg,
        400,
        &reqwest::header::HeaderMap::new(),
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"bad"},"request_id":"req_anthropic"}"#,
    );
    assert_eq!(error.request_id.as_deref(), Some("req_anthropic"));
    assert_eq!(error.code.as_deref(), Some("invalid_request_error"));
}

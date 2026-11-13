use super::*;
use crate::ApiErrorKind;
use crate::CacheTtl;
use crate::ChatRequest;
use crate::ContentPart;
use crate::ImageSource;
use crate::LlmConfig;
use crate::Message;
use crate::ReasoningBlock;
use crate::ReasoningConfig;
use crate::ReasoningSummary;
use crate::ResponseFormat;
use crate::Role;
use crate::StreamEvent;
use crate::ToolCall;
use crate::ToolChoice;
use crate::ToolDef;
use crate::Usage;
use crate::codec::SseEvent;
use crate::codec::StreamDecoder;
use crate::config::{CacheConfig, InstructionBlock, Protocol, ResponseFormatDialect, WireConfig};
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

mod behavior;
mod cache;
mod requests;
mod responses;
mod streaming;

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

//! moc-llm：LLM 供应商兼容层与处理层。
//!
//! 本 crate 向上给 `moc-core` 提供一套与协议无关的请求 / 响应类型（[`types`]），
//! 向下吞掉三种线上协议的差异：
//!
//! | 协议 | 端点 | 请求编码 / 响应解码 |
//! |---|---|---|
//! | [`Protocol::OpenAiChat`] | `/chat/completions` | `codec::chat` |
//! | [`Protocol::OpenAiResponses`] | `/responses` | `codec::responses` |
//! | [`Protocol::AnthropicMessages`] | `/messages` | `codec::anthropic` |
//!
//! 基本用法：
//!
//! ```no_run
//! use moc_llm::{Auth, ChatRequest, Client, LlmConfig, Message, Protocol, StreamEvent};
//!
//! # async fn run() -> Result<(), moc_llm::LlmError> {
//! let cfg = LlmConfig {
//!     protocol: Protocol::OpenAiChat,
//!     base_url: "https://api.deepseek.com/v1".into(),
//!     model: "deepseek-flash".into(),
//!     auth: Auth::from_env("DEEPSEEK_API_KEY").unwrap(),
//!     ..Default::default()
//! };
//! let req = ChatRequest {
//!     messages: vec![Message::system("你是简洁的助手"), Message::user_text("你好")],
//!     ..Default::default()
//! };
//!
//! let client = Client::new();
//! let resp = client.complete(&cfg, &req).await?;
//! println!("{}", resp.message.text());
//!
//! let mut stream = client.stream(&cfg, &req).await?;
//! while let Some(event) = stream.next_event().await {
//!     if let StreamEvent::TextDelta { text, .. } = event? {
//!         print!("{text}");
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! 设计要点：
//!
//! - **模型与参数组合由 core 决定**：本 crate 不内置厂商/模型知识，只按
//!   [`LlmConfig`] 翻译格式。
//! - **思维链回传是显式开关**：[`RequestOptions::echo_reasoning`] 取
//!   [`ReasoningEcho::Protocol`] 时由各协议编码器按协议规则决定回传范围。
//! - **响应可直接回灌历史**：[`ChatResponse::message`] 含思维链与工具调用，
//!   可以直接 push 进下一轮 [`ChatRequest::messages`]。
//! - **中断可续**：[`ChatStream::interrupt`] 返回修剪过的部分响应，
//!   保留可安全回灌的内容并丢弃半截工具调用。

#![forbid(unsafe_code)]

pub mod client;
pub mod codec;
pub mod config;
pub mod error;
pub mod retry;
pub mod stream;
pub mod types;

pub use client::{Client, ClientBuilder, CompactResponse};
pub use config::{
    Auth, CacheConfig, CacheTtl, EndpointStyle, LlmConfig, MaxTokensField, Protocol,
    ResponseFormatDialect, ThinkingWire, ToolChoiceDialect, WireConfig,
};
pub use error::{ApiError, ApiErrorKind, LlmError, TransportError, TransportKind};
pub use retry::RetryPolicy;
pub use stream::ChatStream;
pub use types::{
    ChatRequest, ChatResponse, ContentPart, Effort, FinishReason, ImageDetail, ImageSource,
    Message, ReasoningBlock, ReasoningConfig, ReasoningEcho, ReasoningSummary, RequestOptions,
    ResponseFormat, Role, StreamEvent, ToolCall, ToolChoice, ToolDef, ToolResult, Usage,
};

/// 将完整对话历史编码为 Responses `input` item，供原生压缩请求复用。
///
/// 返回值不包含 [`ChatRequest::native_input`]；调用方需要保留并前置已有原生载荷。
pub fn encode_responses_input_items(
    cfg: &LlmConfig,
    request: &ChatRequest,
) -> Result<Vec<serde_json::Value>, LlmError> {
    if cfg.protocol != Protocol::OpenAiResponses {
        return Err(LlmError::Encode(
            "Responses 输入编码仅支持 OpenAI Responses 协议".into(),
        ));
    }
    codec::validate_tool_history(&request.messages)?;
    codec::responses::encode_input_items(cfg, request)
}

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
//!
//! 流式协议约定：
//!
//! - [Responses](https://developers.openai.com/api/docs/guides/function-calling#streaming)
//!   的参数 delta 是增量，arguments.done、output_item.done 和最终 output 是快照。
//!   快照覆盖已收字段而不再次拼接；最终正文只补发缺失后缀，已输出正文与快照冲突时报错。
//! - [Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions)
//!   的流式和非流式响应都只消费 choice 0，其他候选不得污染正文、工具调用或结束原因。
//!   兼容网关拆分工具名时，完整名称通过工具快照更新，不重复创建调用。
//! - [Anthropic Messages](https://platform.claude.com/docs/en/build-with-claude/streaming)
//!   的工具 input 在没有 JSON 增量时于块结束交付；有增量时不用初始对象作前缀。
//!   初始正文和签名同样保留。工具支持 strict；结构化输出只接受 JsonSchema，
//!   Text 省略 format，JsonObject 在发送前报错；tool_choice=none 不附加并行开关。
//! - 共用 [SSE 解析器](https://html.spec.whatwg.org/multipage/server-sent-events.html#parsing-an-event-stream)
//!   支持 LF、CRLF、CR、跨网络块的 UTF-8 BOM，以及空 data 行形成的正文换行。
//!
//! 文档契约复核（2026-11-08，范围为文本/图像、客户端函数工具与 reasoning）：
//!
//! - [摘要与加密状态](https://developers.openai.com/api/docs/guides/reasoning#reasoning-summaries)：
//!   core 对有思考能力的模型显式请求 summary=auto；Anthropic 映射为 display=summarized。
//!   加密 item 原样回传，本地 `_moc_display_text` 只用于展示；旧会话 Item 的 JSON 仍可读取。
//!   完整 item 可补发实时摘要，摘要增量不会被空终态或中断抹掉；纯展示文本不伪造回传 item。
//! - [Responses 请求](https://developers.openai.com/api/reference/resources/responses/methods/create)：
//!   assistant 的 phase 与文本/函数调用顺序必须保留。reasoning 的 context/mode、text 的
//!   verbosity 等扩展字段不会因写入 effort/summary/format 而被整块覆盖。
//! - [标准 Chat 请求](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)：
//!   默认仅使用 reasoning_effort，关闭时发送 none；厂商 thinking 对象必须用
//!   compat.thinking.toggle_field 显式配置（如 "thinking"），不默认发给 OpenAI。
//!   stream_options 的扩展字段与 include_usage 合并。多 choice、拒绝、用量和终态有回归覆盖。
//! - [Anthropic thinking](https://platform.claude.com/docs/en/build-with-claude/thinking)：
//!   默认 adaptive；显式 enabled + budget_tokens 配置保留，不强行改成 adaptive，
//!   也不自动添加旧模型未必支持的 effort。thinking/output_config 的其他配置字段保留。
//!   带签名的 thinking 与 redacted_thinking 保留回传；各块签名独立，不跨工具合并。
//! - [工具结果](https://platform.claude.com/docs/en/agents-and-tools/tool-use/handle-tool-calls)：
//!   紧邻调用消息按 call ID 配对，不强制并行结果按调用顺序返回；重复、缺失和陌生 ID 仍报错。
//! - [缓存](https://platform.claude.com/docs/en/build-with-claude/prompt-caching)与
//!   [流式用量](https://platform.claude.com/docs/en/build-with-claude/streaming)：
//!   Anthropic input_tokens 加缓存读写得到总输入；message_delta 用量按累计值覆盖而非累加。
//!   已核对缓存断点、TTL、工具参数结束事件、错误事件与异常 EOF；这些未发现新的确定缺陷。
//!
//! 边界：这里不是三个 API 全部产品功能的实现。音频、服务端内置工具的执行结果、
//! background/polling、跨请求 previous_response_id 和模型专属采样参数约束不在统一内容模型内。
//! 兼容网关的非标准字段由 WireConfig/extra_body 显式配置，不宣称任意厂商模型无需配置即可兼容。

#![forbid(unsafe_code)]

pub mod client;
pub mod codec;
pub mod config;
mod content;
pub mod error;
pub mod guard;
mod images;
pub mod retry;
pub mod stream;
pub mod traffic;
pub mod types;

pub use client::{Client, ClientBuilder};
pub use codec::{RequestInspection, RequestSource, inspect_request};
pub use config::{
    Auth, CacheConfig, CacheTtl, EndpointStyle, InstructionBlock, LlmConfig, MaxTokensField,
    Protocol, ResponseFormatDialect, ThinkingWire, ToolChoiceDialect, WireConfig,
};
pub use error::{ApiError, ApiErrorKind, LlmError, TransportError, TransportKind};
pub use guard::{LoopChannel, LoopViolation, StreamGuardConfig};
pub use images::RequestDiagnostics;
pub use retry::RetryPolicy;
pub use stream::ChatStream;
pub use traffic::{HTTP_RX_BYTES, HTTP_TX_BYTES, record_rx, record_tx};
pub use types::{
    ChatRequest, ChatResponse, ContentPart, Effort, FinishReason, ImageDetail, ImageSource,
    Message, MessageContent, ReasoningBlock, ReasoningConfig, ReasoningEcho, ReasoningItem,
    ReasoningSummary, RequestOptions, ResponseFormat, Role, StreamEvent, ToolCall, ToolChoice,
    ToolDef, ToolResult, Usage,
};

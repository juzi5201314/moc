//! 协议无关的请求 / 响应类型。
//!
//! `moc-core` 只与本模块的类型打交道；三种线上协议（OpenAI Chat Completions、
//! Anthropic Messages、OpenAI Responses）的字段差异全部在 [`crate::codec`] 内消化。
//! 因此这里每个设计决定都必须在三协议下可表达，不偏向任一协议。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::CacheConfig;
use crate::error::LlmError;

/// 消息角色。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Role {
    /// 系统提示。
    System,
    /// 开发者提示（Chat 的 `developer` 角色）。
    Developer,
    /// 用户输入。
    User,
    /// 模型输出。
    Assistant,
    /// 工具执行结果；一条消息恰好承载一个 [`ToolResult`]。
    Tool,
}

impl Role {
    /// 线上角色字符串（Chat / Responses 使用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Role::System => "system",
            Role::Developer => "developer",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一条对话消息。
///
/// 角色与内容块合法组合（编码器在历史完整性校验中强制执行）：
///
/// | 角色 | 允许的内容块 |
/// |---|---|
/// | `System` / `Developer` | `Text` |
/// | `User` | `Text`、`Image` |
/// | `Assistant` | `Text`、`Reasoning`、`ToolCall` |
/// | `Tool` | 恰好一个 `ToolResult` |
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    /// 消息角色。
    pub role: Role,
    /// 内容块，顺序即线上顺序（Anthropic 下与线上块顺序一致）。
    pub content: Vec<ContentPart>,
}

impl Default for Message {
    /// 空 assistant 消息：角色为 [`Role::Assistant`]、内容为空，用作流式累加器
    /// 与 [`ChatResponse`] 的初始状态。未提供 `Role::default()`，避免角色本身出现
    /// 隐含默认值。
    fn default() -> Self {
        Self {
            role: Role::Assistant,
            content: Vec::new(),
        }
    }
}

impl Message {
    /// 系统提示。
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: vec![ContentPart::Text(text.into())],
        }
    }

    /// 开发者提示。
    pub fn developer(text: impl Into<String>) -> Self {
        Self {
            role: Role::Developer,
            content: vec![ContentPart::Text(text.into())],
        }
    }

    /// 用户纯文本消息。
    pub fn user_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: vec![ContentPart::Text(text.into())],
        }
    }

    /// 模型纯文本消息。
    pub fn assistant_text(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![ContentPart::Text(text.into())],
        }
    }

    /// 一条工具结果消息。
    ///
    /// 每个工具调用各配一条消息，即使模型一次并行返回多个调用——合并成一条消息是
    /// Anthropic 编码器的职责，core 侧始终用同一种写法。
    pub fn tool_result(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: vec![ContentPart::ToolResult(ToolResult {
                tool_call_id: tool_call_id.into(),
                content: content.into(),
                is_error,
            })],
        }
    }

    /// 按顺序遍历本消息中的工具调用。
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.content.iter().filter_map(|part| match part {
            ContentPart::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// 按顺序遍历本消息中的工具结果。
    pub fn tool_results(&self) -> impl Iterator<Item = &ToolResult> {
        self.content.iter().filter_map(|part| match part {
            ContentPart::ToolResult(result) => Some(result),
            _ => None,
        })
    }

    /// 拼接全部文本块；思维链、图片与工具调用不参与。
    pub fn text(&self) -> String {
        let mut out = String::new();
        for part in &self.content {
            if let ContentPart::Text(text) = part {
                out.push_str(text);
            }
        }
        out
    }
}

/// 消息内容块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ContentPart {
    /// 文本。
    Text(String),
    /// 图片；只允许出现在 user 消息中。
    Image {
        /// 图片来源。
        source: ImageSource,
        /// 精度提示，仅 Chat / Responses 支持。
        detail: Option<ImageDetail>,
    },
    /// 思维链块；只允许出现在 assistant 消息中。
    Reasoning(ReasoningBlock),
    /// 工具调用；只允许出现在 assistant 消息中，一条消息内可出现多个（并行调用）。
    ToolCall(ToolCall),
    /// 工具结果；只允许出现在 `Role::Tool` 消息中且每条消息恰好一个。
    ToolResult(ToolResult),
}

/// 图片来源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ImageSource {
    /// 远端 URL。
    Url(String),
    /// 内联 base64 数据。
    Base64 {
        /// MIME 类型，如 `image/png`。
        media_type: String,
        /// 不含 `data:` 前缀的 base64 载荷。
        data: String,
    },
}

/// 图片精度提示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ImageDetail {
    /// 由上游自行决定。
    Auto,
    /// 低精度。
    Low,
    /// 高精度。
    High,
}

impl ImageDetail {
    /// 线上字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            ImageDetail::Auto => "auto",
            ImageDetail::Low => "low",
            ImageDetail::High => "high",
        }
    }
}

/// 一次工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    /// 调用 ID；工具结果靠它与调用配对。
    pub id: String,
    /// 工具名。
    pub name: String,
    /// 参数 JSON 字符串，原样保留线上字节（流式增量拼接、回传时逐字节一致）。
    pub arguments: String,
}

impl ToolCall {
    /// 构造一次工具调用。
    pub fn new(
        id: impl Into<String>,
        name: impl Into<String>,
        arguments: impl Into<String>,
    ) -> Self {
        Self {
            id: id.into(),
            name: name.into(),
            arguments: arguments.into(),
        }
    }

    /// 把 `arguments` 解析为 JSON；参数不完整或非法时返回 [`LlmError::Encode`]。
    pub fn arguments_json(&self) -> Result<serde_json::Value, LlmError> {
        serde_json::from_str(&self.arguments).map_err(|err| {
            LlmError::Encode(format!("工具调用 {} 的参数不是合法 JSON: {err}", self.name))
        })
    }
}

/// 一条工具执行结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    /// 对应的 [`ToolCall::id`]。
    pub tool_call_id: String,
    /// 结果文本；多模态结果由 core 转成文本描述后传入。
    pub content: String,
    /// 是否为失败结果（Anthropic 有对应字段，其余协议由 core 拼进文本）。
    pub is_error: bool,
}

/// 思维链块。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ReasoningBlock {
    /// Chat 的 `reasoning_content`；Anthropic 的 `thinking`（`signature` 原样回传）。
    Text {
        /// 思维链文本。
        text: String,
        /// Anthropic 的签名；其余协议为 `None`。
        signature: Option<String>,
    },
    /// Anthropic 的 `redacted_thinking`。
    Redacted {
        /// 上游返回的不透明数据。
        data: String,
    },
    /// Responses 的 reasoning item，原样保存与回传（含 `id` / `encrypted_content` / `summary`）。
    Item(Box<serde_json::Value>),
}

impl ReasoningBlock {
    /// 构造纯文本思维链块（`signature` 为空）。
    pub fn text(text: impl Into<String>) -> Self {
        ReasoningBlock::Text {
            text: text.into(),
            signature: None,
        }
    }

    /// 供 UI 展示的思维链文本；`Item` 取 `summary[].text` 拼接，无摘要时返回 `None`。
    pub fn display_text(&self) -> Option<String> {
        match self {
            ReasoningBlock::Text { text, .. } => Some(text.clone()),
            ReasoningBlock::Redacted { .. } => None,
            ReasoningBlock::Item(item) => {
                let mut out = String::new();
                for entry in item.get("summary")?.as_array()? {
                    if let Some(text) = entry.get("text").and_then(|t| t.as_str()) {
                        out.push_str(text);
                    }
                }
                (!out.is_empty()).then_some(out)
            }
        }
    }
}

/// 工具定义。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDef {
    /// 工具名。
    pub name: String,
    /// 工具用途描述。
    pub description: Option<String>,
    /// 参数的 JSON Schema 对象。
    pub parameters: serde_json::Value,
    /// 是否启用严格模式（Chat / Responses 支持，Anthropic 丢弃）。
    pub strict: Option<bool>,
}

impl ToolDef {
    /// 只带名字与参数 Schema 的工具定义。
    pub fn new(name: impl Into<String>, parameters: serde_json::Value) -> Self {
        Self {
            name: name.into(),
            description: None,
            parameters,
            strict: None,
        }
    }

    /// 设置描述。
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }
}

/// 工具选择策略。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToolChoice {
    /// 由模型决定。
    Auto,
    /// 不调用工具。
    None,
    /// 至少调用一个工具。
    Required,
    /// 必须调用指定工具。
    Tool(String),
}

/// 响应格式约束。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ResponseFormat {
    /// 自由文本。
    Text,
    /// 合法 JSON 对象，
    JsonObject,
    /// 按 JSON Schema 约束输出；Anthropic 只支持 `schema`，
    /// `name` / `description` / `strict` 会被丢弃。
    JsonSchema {
        /// Schema 名称。
        name: String,
        /// Schema 描述。
        description: Option<String>,
        /// Schema 本体。
        schema: serde_json::Value,
        /// 是否启用严格模式。
        strict: Option<bool>,
    },
}

/// 推理强度。
///
/// 线上默认取变体名的小写形式（`xhigh` / `max`），厂商差异由
/// [`crate::config::ThinkingWire::effort_map`] 改写。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Effort {
    /// 关闭推理。
    None,
    /// 最低强度。
    Minimal,
    /// 低强度。
    Low,
    /// 中强度。
    Medium,
    /// 高强度。
    High,
    /// 极高强度。
    XHigh,
    /// 最高强度。
    Max,
}

impl Effort {
    /// 线上默认字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Effort::None => "none",
            Effort::Minimal => "minimal",
            Effort::Low => "low",
            Effort::Medium => "medium",
            Effort::High => "high",
            Effort::XHigh => "xhigh",
            Effort::Max => "max",
        }
    }
}

impl std::fmt::Display for Effort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 思维链摘要粒度。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningSummary {
    /// 由模型决定。
    Auto,
    /// 简洁。
    Concise,
    /// 详细。
    Detailed,
    /// 已摘要。
    Summarized,
    /// 不返回摘要。
    Omitted,
}

/// 思维链请求配置。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ReasoningConfig {
    /// 是否开启思维链；`false` 时按协议发送 `disabled` 或不发送相关字段。
    pub enabled: bool,
    /// 推理强度。
    pub effort: Option<Effort>,
    /// Anthropic manual 模式下的思考预算，必须小于 `max_output_tokens`。
    pub budget_tokens: Option<u32>,
    /// 摘要粒度（Anthropic 的 `display` / Responses 的 `reasoning.summary`）。
    pub summary: Option<ReasoningSummary>,
}

impl ReasoningConfig {
    /// 开启思维链、其余字段留空的配置。
    pub fn enabled() -> Self {
        Self {
            enabled: true,
            ..Default::default()
        }
    }
}

/// 结束原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FinishReason {
    /// 自然结束。
    Stop,
    /// 命中停止序列。
    StopSequence,
    /// 达到输出长度上限。
    Length,
    /// 模型请求调用工具。
    ToolCalls,
    /// 内容被安全策略拦截。
    ContentFilter,
    /// 模型拒绝回答。
    Refusal,
    /// Anthropic 暂停回合，需要原样重发请求续跑。
    PauseTurn,
    /// 上游取消。
    Cancelled,
    /// 仅由 [`crate::ChatStream::interrupt`] 产生，永不来自线上字段。
    Interrupted,
    /// 其余线上取值，原样保留。
    Other(String),
}

impl FinishReason {
    /// 是否以工具调用结束。
    pub fn is_tool_calls(&self) -> bool {
        matches!(self, FinishReason::ToolCalls)
    }
}

impl std::fmt::Display for FinishReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match self {
            FinishReason::Stop => "stop",
            FinishReason::StopSequence => "stop_sequence",
            FinishReason::Length => "length",
            FinishReason::ToolCalls => "tool_calls",
            FinishReason::ContentFilter => "content_filter",
            FinishReason::Refusal => "refusal",
            FinishReason::PauseTurn => "pause_turn",
            FinishReason::Cancelled => "cancelled",
            FinishReason::Interrupted => "interrupted",
            FinishReason::Other(other) => other.as_str(),
        };
        f.write_str(name)
    }
}

/// token 用量。
///
/// 各字段保持上游原始语义，**不做跨协议归一化**：
///
/// | 字段 | Chat 来源 | Anthropic 来源 | Responses 来源 |
/// |---|---|---|---|
/// | `input_tokens` | `prompt_tokens`（含缓存命中部分） | `input_tokens`（不含缓存部分） | `input_tokens`（含缓存命中部分） |
/// | `output_tokens` | `completion_tokens` | `output_tokens` | `output_tokens` |
/// | `total_tokens` | `total_tokens` | 无对应字段 | `total_tokens` |
/// | `cached_input_tokens` | `prompt_tokens_details.cached_tokens` | `cache_read_input_tokens` | `input_tokens_details.cached_tokens` |
/// | `cache_creation_input_tokens` | 无 | `cache_creation_input_tokens` | 无 |
/// | `reasoning_tokens` | `completion_tokens_details.reasoning_tokens` | `output_tokens_details.thinking_tokens` | `output_tokens_details.reasoning_tokens` |
///
/// Anthropic 下的完整输入量由调用方自行加 `cached_input_tokens + cache_creation_input_tokens`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Usage {
    /// 输入 token 数。
    pub input_tokens: u64,
    /// 输出 token 数。
    pub output_tokens: u64,
    /// 上游给出的总量；缺失时为 `None`。
    pub total_tokens: Option<u64>,
    /// 命中提示缓存的输入 token 数。
    pub cached_input_tokens: Option<u64>,
    /// 写入提示缓存的输入 token 数（Anthropic 专有）。
    pub cache_creation_input_tokens: Option<u64>,
    /// 思维链 token 数。
    pub reasoning_tokens: Option<u64>,
}

impl Usage {
    /// 总量；上游未给出时回退为 `input_tokens + output_tokens`。
    pub fn total_tokens(&self) -> u64 {
        self.total_tokens
            .unwrap_or(self.input_tokens + self.output_tokens)
    }

    /// 合并两个用量快照：后到的非零字段覆盖先到的。
    ///
    /// 流式响应里用量可能分多次给出——Anthropic 的 `message_start` 只带输入用量、
    /// `message_delta` 只带输出用量——因此累加器按字段合并，而不是整体替换。
    pub fn merge(&mut self, other: Usage) {
        if other.input_tokens > 0 {
            self.input_tokens = other.input_tokens;
        }
        if other.output_tokens > 0 {
            self.output_tokens = other.output_tokens;
        }
        if other.total_tokens.is_some() {
            self.total_tokens = other.total_tokens;
        }
        if other.cached_input_tokens.is_some() {
            self.cached_input_tokens = other.cached_input_tokens;
        }
        if other.cache_creation_input_tokens.is_some() {
            self.cache_creation_input_tokens = other.cache_creation_input_tokens;
        }
        if other.reasoning_tokens.is_some() {
            self.reasoning_tokens = other.reasoning_tokens;
        }
    }
}

/// 思维链回传策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReasoningEcho {
    /// 按协议默认规则回传（见 [`crate::codec::echo`]）：Chat 仅在携带工具时回传，
    /// Anthropic 与 Responses 总是回传。
    #[default]
    Protocol,
    /// 无论协议规则如何都回传。
    Always,
    /// 一律不回传。
    Never,
}

/// 单次请求的可选行为。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct RequestOptions {
    /// 思维链回传策略。
    pub echo_reasoning: ReasoningEcho,
    /// 是否让上游保存本次会话（Chat / Responses 的 `store`）；`None` 表示不发送。
    pub store: Option<bool>,
    /// 提示缓存配置。
    pub cache: Option<CacheConfig>,
    /// 终端用户标识（Anthropic 的 `metadata.user_id`）。
    pub user_id: Option<String>,
    /// 是否把原始响应体存入 [`ChatResponse::raw`]。
    pub capture_raw: bool,
    /// 覆盖 [`crate::LlmConfig::headers`] 中的同名请求头。
    pub headers: BTreeMap<String, String>,
}

/// 一次对话请求。
///
/// 没有 `stream` 字段：是否流式由 [`crate::Client::stream`] 与
/// [`crate::Client::complete`] 决定。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChatRequest {
    /// 对话历史，按时间顺序。
    pub messages: Vec<Message>,
    /// Responses 服务端压缩返回的原生历史条目，编码时置于普通消息之前。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub native_input: Option<Vec<serde_json::Value>>,
    /// 可用工具；为空表示不启用工具调用。
    pub tools: Vec<ToolDef>,
    /// 工具选择策略。
    pub tool_choice: Option<ToolChoice>,
    /// 输出长度上限。
    pub max_output_tokens: Option<u32>,
    /// 采样温度。
    ///
    /// 用 `f64` 而不是 `f32`：JSON 数字经 `f32` 中转会展开成
    /// `0.20000000298023224` 这类长尾形态，`f64` 才能得到 `0.2` 这样的线上原文。
    pub temperature: Option<f64>,
    /// 核采样。
    pub top_p: Option<f64>,
    /// top-k 采样。
    pub top_k: Option<u32>,
    /// 停止序列。
    pub stop: Vec<String>,
    /// 思维链配置。
    pub reasoning: Option<ReasoningConfig>,
    /// 响应格式约束。
    pub response_format: Option<ResponseFormat>,
    /// 是否允许并行工具调用。
    pub parallel_tool_calls: Option<bool>,
    /// 其余请求选项。
    pub options: RequestOptions,
}

/// 一次对话响应。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct ChatResponse {
    /// 上游响应 ID。
    pub id: Option<String>,
    /// 实际使用的模型名。
    pub model: Option<String>,
    /// assistant 消息，可直接 push 进下一轮请求的 [`ChatRequest::messages`]。
    pub message: Message,
    /// 结束原因。
    pub finish_reason: FinishReason,
    /// token 用量。
    pub usage: Usage,
    /// 原始响应体，仅在 [`RequestOptions::capture_raw`] 为真时填充。
    pub raw: Option<Box<serde_json::Value>>,
    /// Responses 协议返回的原生压缩条目。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub compaction: Option<Box<serde_json::Value>>,
}

impl Default for FinishReason {
    /// 派生不可用（带载荷的变体无法标注 `#[default]`）。取值与协议解码器在
    /// 缺失结束原因时的兜底一致：`Other("unknown")`。
    fn default() -> Self {
        FinishReason::Other("unknown".into())
    }
}

/// 流式增量事件。
///
/// [`StreamEvent::index`] 的语义是「同一响应内该内容块的位置」，取值由各协议
/// 自己的块序号给出（Chat 文本恒为 0、工具调用用 `tool_calls[].index`；
/// Anthropic 用 `content_block` 的 `index`；Responses 用 `output_index`）。
/// 消费方按 `(块种类, index)` 归组，因此取值差异不影响正确性。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum StreamEvent {
    /// 流开始。
    Start {
        /// 上游响应 ID。
        id: Option<String>,
        /// 实际上使用的模型名。
        model: Option<String>,
    },
    /// 文本增量。
    TextDelta {
        /// 内容块序号。
        index: usize,
        /// 增量文本。
        text: String,
    },
    /// 思维链文本增量。
    ReasoningDelta {
        /// 内容块序号。
        index: usize,
        /// 增量文本。
        text: String,
    },
    /// 思维链签名增量（Anthropic）。
    ReasoningSignature {
        /// 内容块序号。
        index: usize,
        /// 签名分片；需要按序拼接。
        signature: String,
    },
    /// 完整思维链条目（Anthropic 的 `redacted_thinking`、Responses 的 reasoning item）。
    ReasoningItem {
        /// 内容块序号。
        index: usize,
        /// 条目原文。
        item: Box<serde_json::Value>,
    },
    /// 工具调用开始。
    ///
    /// 解码器在**每个携带非空 `name` 片段的增量**上发出本事件，`id` 取该 index
    /// 首个非空值。规范形态下每个 index 恰好出现一次；少数兼容网关会把名字拆到
    /// 多个 chunk，此时同一 index 会出现多次，消费方以首次出现为准渲染、
    /// 累加器对该 index 的名字做拼接。
    ToolCallStart {
        /// 内容块序号。
        index: usize,
        /// 调用 ID。
        id: String,
        /// 工具名片段。
        name: String,
    },
    /// 工具调用参数增量（JSON 片段）。
    ToolCallArgumentsDelta {
        /// 内容块序号。
        index: usize,
        /// JSON 片段。
        json: String,
    },
    /// token 用量。
    Usage(Usage),
    /// Responses 服务端压缩历史条目。
    CompactionItem {
        /// 压缩 item 原文。
        item: Box<serde_json::Value>,
    },
    /// 流结束。
    Done {
        /// 结束原因。
        finish_reason: FinishReason,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn message_constructors_produce_expected_role_and_content() {
        let system = Message::system("sys");
        assert_eq!(system.role, Role::System);
        assert_eq!(system.content, vec![ContentPart::Text("sys".into())]);

        let developer = Message::developer("dev");
        assert_eq!(developer.role, Role::Developer);
        assert_eq!(developer.text(), "dev");

        let user = Message::user_text("你好");
        assert_eq!(user.role, Role::User);
        assert_eq!(user.content, vec![ContentPart::Text("你好".into())]);

        let assistant = Message::assistant_text("ok");
        assert_eq!(assistant.role, Role::Assistant);
        assert_eq!(assistant.text(), "ok");

        let tool = Message::tool_result("call_1", "42", true);
        assert_eq!(tool.role, Role::Tool);
        assert_eq!(
            tool.content,
            vec![ContentPart::ToolResult(ToolResult {
                tool_call_id: "call_1".into(),
                content: "42".into(),
                is_error: true,
            })]
        );
        assert_eq!(tool.tool_results().count(), 1);
        assert_eq!(tool.tool_calls().count(), 0);
    }

    #[test]
    fn message_text_concatenates_only_text_parts() {
        let message = Message {
            role: Role::Assistant,
            content: vec![
                ContentPart::Reasoning(ReasoningBlock::text("想一想")),
                ContentPart::Text("前半".into()),
                ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                ContentPart::Text("后半".into()),
            ],
        };
        assert_eq!(message.text(), "前半后半");
        assert_eq!(message.tool_calls().count(), 1);
    }

    #[test]
    fn finish_reason_display_uses_wire_names() {
        let cases = [
            (FinishReason::Stop, "stop"),
            (FinishReason::StopSequence, "stop_sequence"),
            (FinishReason::Length, "length"),
            (FinishReason::ToolCalls, "tool_calls"),
            (FinishReason::ContentFilter, "content_filter"),
            (FinishReason::Refusal, "refusal"),
            (FinishReason::PauseTurn, "pause_turn"),
            (FinishReason::Cancelled, "cancelled"),
            (FinishReason::Interrupted, "interrupted"),
            (
                FinishReason::Other("model_context_window_exceeded".into()),
                "model_context_window_exceeded",
            ),
        ];
        for (reason, expected) in cases {
            assert_eq!(reason.to_string(), expected);
        }
    }

    #[test]
    fn usage_total_tokens_falls_back_to_sum() {
        let explicit = Usage {
            input_tokens: 10,
            output_tokens: 3,
            total_tokens: Some(999),
            ..Default::default()
        };
        assert_eq!(explicit.total_tokens(), 999);

        let derived = Usage {
            input_tokens: 10,
            output_tokens: 3,
            ..Default::default()
        };
        assert_eq!(derived.total_tokens(), 13);
    }

    #[test]
    fn tool_call_arguments_json_parses_or_errors() {
        let call = ToolCall::new("call_1", "get_weather", r#"{"city":"北京"}"#);
        assert_eq!(call.arguments_json().unwrap(), json!({"city": "北京"}));

        let broken = ToolCall::new("call_2", "get_weather", r#"{"city":"#);
        let err = broken.arguments_json().unwrap_err();
        assert!(matches!(err, LlmError::Encode(_)), "{err:?}");
        assert!(err.to_string().contains("get_weather"), "{err}");
    }

    #[test]
    fn reasoning_display_text_reads_summary_entries() {
        let plain = ReasoningBlock::Text {
            text: "思考中".into(),
            signature: Some("sig".into()),
        };
        assert_eq!(plain.display_text().as_deref(), Some("思考中"));

        let redacted = ReasoningBlock::Redacted {
            data: "opaque".into(),
        };
        assert_eq!(redacted.display_text(), None);

        let item = ReasoningBlock::Item(Box::new(json!({
            "type": "reasoning",
            "id": "rs_1",
            "encrypted_content": "blob",
            "summary": [
                {"type": "summary_text", "text": "第一段"},
                {"type": "summary_text", "text": "第二段"},
            ],
        })));
        assert_eq!(item.display_text().as_deref(), Some("第一段第二段"));

        let empty = ReasoningBlock::Item(Box::new(json!({"type": "reasoning", "summary": []})));
        assert_eq!(empty.display_text(), None);
    }

    #[test]
    fn effort_default_strings_are_lowercase_names() {
        assert_eq!(Effort::None.to_string(), "none");
        assert_eq!(Effort::XHigh.to_string(), "xhigh");
        assert_eq!(Effort::Max.to_string(), "max");
        assert_eq!(serde_json::to_value(Effort::XHigh).unwrap(), json!("xhigh"));
    }

    #[test]
    fn request_and_response_round_trip_through_json() {
        let request = ChatRequest {
            messages: vec![
                Message::system("sys"),
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Reasoning(ReasoningBlock::text("思路")),
                        ContentPart::ToolCall(ToolCall::new("call_1", "get_weather", "{}")),
                    ],
                },
                Message::tool_result("call_1", "晴", false),
                Message::user_text("继续"),
            ],
            tools: vec![ToolDef::new("get_weather", json!({"type": "object"}))],
            tool_choice: Some(ToolChoice::Auto),
            max_output_tokens: Some(1024),
            parallel_tool_calls: Some(false),
            options: RequestOptions {
                echo_reasoning: ReasoningEcho::Always,
                store: Some(false),
                capture_raw: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let encoded = serde_json::to_vec(&request).unwrap();
        let decoded: ChatRequest = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, request);
    }
}

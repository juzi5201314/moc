//! 供应商配置体。
//!
//! [`LlmConfig`] 是 core 与本 crate 的唯一契约面：core 决定协议、地址、模型名、
//! 鉴权与线格式开关，本 crate 只负责按配置体把请求类型翻译成线上格式。本 crate 不含
//! 任何以厂商/模型身份为键的内置知识。

use std::collections::BTreeMap;
use std::fmt;

use anyhow::Context;
use serde::{Deserialize, Serialize};

use crate::retry::RetryPolicy;
use crate::types::{Effort, ReasoningConfig};

/// 线上协议。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Protocol {
    /// OpenAI Chat Completions（`/chat/completions`）。
    #[default]
    #[serde(rename = "openai_chat")]
    OpenAiChat,
    /// OpenAI Responses（`/responses`）。
    #[serde(rename = "openai_responses")]
    OpenAiResponses,
    /// Anthropic Messages（`/messages`）。
    #[serde(rename = "anthropic_messages")]
    AnthropicMessages,
}

impl Protocol {
    /// 版本前缀之后的路径片段。
    pub fn path(self) -> &'static str {
        match self {
            Protocol::OpenAiChat => "chat/completions",
            Protocol::OpenAiResponses => "responses",
            Protocol::AnthropicMessages => "messages",
        }
    }
}

/// 端点风格。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EndpointStyle {
    /// `{base_url}/{path}`，`path` 由协议决定。
    #[default]
    Default,
    /// Azure OpenAI：`{base_url}/openai/deployments/{model}/{path}?api-version={api_version}`。
    Azure {
        /// Azure 要求的 `api-version` 查询参数。
        api_version: String,
    },
}

/// 鉴权方式。
#[derive(Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Auth {
    /// 不发送鉴权头。
    #[default]
    None,
    /// `Authorization: Bearer <token>`。
    Bearer(String),
    /// 自定义请求头（Azure 的 `api-key` 等）。
    Header {
        /// 请求头名。
        name: String,
        /// 请求头值。
        value: String,
    },
}

impl Auth {
    /// 从环境变量读取密钥，构造 [`Auth::Bearer`]。
    ///
    /// # Errors
    ///
    /// 环境变量缺失或为空白时返回带上下文的错误。
    pub fn from_env(var: &str) -> anyhow::Result<Auth> {
        let value = std::env::var(var).with_context(|| format!("读取环境变量 {var} 失败"))?;
        if value.trim().is_empty() {
            anyhow::bail!("环境变量 {var} 为空");
        }
        Ok(Auth::Bearer(value))
    }
}

impl fmt::Debug for Auth {
    /// 手写实现：密钥值一律输出 `***`，避免凭据进入日志。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Auth::None => f.write_str("None"),
            Auth::Bearer(_) => f.write_str("Bearer(\"***\")"),
            Auth::Header { name, .. } => f
                .debug_struct("Header")
                .field("name", name)
                .field("value", &"***")
                .finish(),
        }
    }
}

/// 输出长度上限的线上字段名。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaxTokensField {
    /// `max_tokens`。
    #[default]
    MaxTokens,
    /// `max_completion_tokens`。
    MaxCompletionTokens,
}

/// 工具选择方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolChoiceDialect {
    /// `auto` / `none` / `required` / 具名工具。
    #[default]
    Standard,
    /// 把 `required` 写成 `any`（部分兼容网关）。
    AnyForRequired,
    /// 不发送 `tool_choice`。
    Unsupported,
}

/// 响应格式方言。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResponseFormatDialect {
    /// 原样发送 `response_format`。
    #[default]
    Standard,
    /// 不发送 `response_format`。
    Unsupported,
}

/// Chat 协议下思考字段的线上形状。
///
/// `thinking` 对象与 `reasoning_effort` 顶层字段可以单独开关：部分厂商只认后者
/// （Qwen），部分厂商在对象里附带自己的扩展键（Kimi 的 `keep`、GLM 的
/// `clear_thinking`），都通过本结构表达。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ThinkingWire {
    /// `thinking` 对象的开关字段名；`None` 表示不发送该对象。
    pub toggle_field: Option<String>,
    /// 开启时的类型值。
    pub enabled_value: String,
    /// 关闭时的类型值。
    pub disabled_value: String,
    /// 推理强度字段名；`None` 表示不发送。
    pub effort_field: Option<String>,
    /// 强度值改写表；为空时取 [`Effort`] 的小写原名。
    pub effort_map: BTreeMap<Effort, String>,
    /// 追加进 `thinking` 对象的厂商扩展键。
    pub toggle_extra: serde_json::Map<String, serde_json::Value>,
}

impl Default for ThinkingWire {
    fn default() -> Self {
        Self {
            toggle_field: Some("thinking".into()),
            enabled_value: "enabled".into(),
            disabled_value: "disabled".into(),
            effort_field: Some("reasoning_effort".into()),
            effort_map: BTreeMap::new(),
            toggle_extra: serde_json::Map::new(),
        }
    }
}

impl ThinkingWire {
    /// 按 [`Effort`] 取值，未配置改写时用小写原名。
    pub(crate) fn effort_value(&self, effort: Effort) -> String {
        self.effort_map
            .get(&effort)
            .cloned()
            .unwrap_or_else(|| effort.as_str().to_owned())
    }
}

/// 线格式开关。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct WireConfig {
    /// 输出长度上限使用的字段名。
    pub max_output_tokens_field: MaxTokensField,
    /// 是否支持 `stop` / `stop_sequences`。
    pub supports_stop: bool,
    /// 思维链回传时使用的字段名。
    pub reasoning_field: String,
    /// 响应侧识别思维链的字段名候选，按顺序匹配。
    pub reasoning_field_aliases: Vec<String>,
    /// Chat 协议下思考字段的线形状。
    pub thinking: ThinkingWire,
    /// 工具选择方言。
    pub tool_choice_dialect: ToolChoiceDialect,
    /// 是否支持 `parallel_tool_calls`。
    pub parallel_tool_calls: bool,
    /// 是否支持 `developer` 角色；`false` 时降级为 `system`。
    pub developer_role: bool,
    /// 响应格式方言。
    pub response_format_dialect: ResponseFormatDialect,
}

impl Default for WireConfig {
    fn default() -> Self {
        Self {
            max_output_tokens_field: MaxTokensField::MaxTokens,
            supports_stop: true,
            reasoning_field: "reasoning_content".into(),
            reasoning_field_aliases: vec!["reasoning_content".into(), "reasoning".into()],
            thinking: ThinkingWire::default(),
            tool_choice_dialect: ToolChoiceDialect::Standard,
            parallel_tool_calls: true,
            developer_role: false,
            response_format_dialect: ResponseFormatDialect::Standard,
        }
    }
}

/// 提示缓存存活时长。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheTtl {
    /// 5 分钟（Anthropic 默认）。
    FiveMinutes,
    /// 30 分钟（Chat / Responses 的显式缓存模式）。
    ThirtyMinutes,
    /// 1 小时。
    OneHour,
}

impl CacheTtl {
    /// 线上字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            CacheTtl::FiveMinutes => "5m",
            CacheTtl::ThirtyMinutes => "30m",
            CacheTtl::OneHour => "1h",
        }
    }
}

/// 提示缓存配置。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct CacheConfig {
    /// 缓存路由键（Chat / Responses 的 `prompt_cache_key`；Anthropic 无对应字段）。
    pub key: Option<String>,
    /// 消息下标（0 基），在该消息末尾写缓存断点。
    pub breakpoints: Vec<usize>,
    /// 系统提示内的内容块下标（0 基），断点落在这些块上。
    ///
    /// 空表示不按块落位。用于把与工作目录无关的静态前缀单独缓存：断点落在静态
    /// 前缀的最后一块上，同一静态前缀在不同工作目录、不同会话之间共享缓存。
    pub system_blocks: Vec<usize>,
    /// 缓存存活时长。
    pub ttl: Option<CacheTtl>,
}

/// 供应商配置体。
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LlmConfig {
    /// 线上协议。
    pub protocol: Protocol,
    /// 接口根地址，含版本前缀，如 `https://api.openai.com/v1`。
    pub base_url: String,
    /// 模型名；Azure 下即 deployment 名。
    pub model: String,
    /// 鉴权方式。
    pub auth: Auth,
    /// 端点风格。
    pub endpoint: EndpointStyle,
    /// 线格式开关。
    pub wire: WireConfig,
    /// 附加请求头。
    pub headers: BTreeMap<String, String>,
    /// 并入请求体顶层的厂商扩展字段；与标准字段同名时标准字段胜出。
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    /// 超时（秒）：非流式请求是整请求总时长，流式请求是两次字节到达之间的空闲上限。
    pub timeout_secs: Option<f64>,
    /// 连接建立（DNS/TCP/TLS）的时限，单位为秒；不包含等待模型响应头的时间。
    ///
    /// 同一时限共享连接池；未设置时沿用 [`crate::ClientBuilder::connect_timeout`]。
    /// 非流式响应生成与流式首包等待受 `timeout_secs` 约束。
    pub connect_timeout_secs: Option<f64>,
    /// Responses 服务端压缩阈值；配置时请求体带 `context_management`。
    pub server_compaction_threshold: Option<u64>,
    /// 重试策略。
    pub retry: RetryPolicy,
}

impl LlmConfig {
    /// 复制配置体并替换模型名（切换模型时的常规操作）。
    pub fn with_model(&self, model: impl Into<String>) -> LlmConfig {
        let mut next = self.clone();
        next.model = model.into();
        next
    }

    /// 本次请求实际写入请求体的推理强度原文（与三协议编码器同源）。
    ///
    /// `None` 表示请求体不含强度字段：Chat 在 `effort_field` 关闭或思考关闭时不发送，
    /// Anthropic 关闭思考时不发送；Responses 关闭思考时发送 `none`。core 用本方法与
    /// 上游回显值比对，因此这里必须与 [`crate::codec`] 的编码结果逐字一致。
    pub fn wire_effort(&self, reasoning: Option<&ReasoningConfig>) -> Option<String> {
        let reasoning = reasoning?;
        match self.protocol {
            Protocol::OpenAiChat => {
                if !reasoning.enabled {
                    return None;
                }
                self.wire.thinking.effort_field.as_ref()?;
                reasoning
                    .effort
                    .map(|effort| self.wire.thinking.effort_value(effort))
            }
            Protocol::OpenAiResponses => {
                if reasoning.enabled {
                    reasoning.effort.map(|effort| effort.as_str().to_owned())
                } else {
                    Some(Effort::None.as_str().to_owned())
                }
            }
            Protocol::AnthropicMessages => {
                if reasoning.enabled {
                    reasoning.effort.map(|effort| effort.as_str().to_owned())
                } else {
                    None
                }
            }
        }
    }

    /// 模型目录沿用推理端点的版本路径规则，不包含会话模型或 deployment。
    pub fn models_endpoint_url(&self) -> String {
        match &self.endpoint {
            EndpointStyle::Default => format!("{}/models", normalize_base_url(&self.base_url)),
            EndpointStyle::Azure { api_version } => format!(
                "{}/openai/models?api-version={api_version}",
                self.base_url.trim_end_matches('/')
            ),
        }
    }

    /// 本次请求的完整 URL（不含查询串之外的内容）。
    pub(crate) fn endpoint_url(&self) -> String {
        let base = match &self.endpoint {
            EndpointStyle::Default => normalize_base_url(&self.base_url),
            EndpointStyle::Azure { .. } => self.base_url.trim_end_matches('/').to_owned(),
        };
        match &self.endpoint {
            EndpointStyle::Default => format!("{base}/{}", self.protocol.path()),
            EndpointStyle::Azure { api_version } => format!(
                "{base}/openai/deployments/{}/{}?api-version={api_version}",
                self.model,
                self.protocol.path()
            ),
        }
    }
}

fn normalize_base_url(raw: &str) -> String {
    let skip_version = raw.ends_with('#');
    let base = raw.trim_end_matches('#').trim_end_matches('/');
    if skip_version || base.ends_with("/v1") {
        base.to_owned()
    } else {
        format!("{base}/v1")
    }
}

impl LlmConfig {
    /// OpenAI Responses 原生压缩端点 URL。
    pub fn compact_endpoint_url(&self) -> String {
        let base = match &self.endpoint {
            EndpointStyle::Default => normalize_base_url(&self.base_url),
            EndpointStyle::Azure { .. } => self.base_url.trim_end_matches('/').to_owned(),
        };
        match &self.endpoint {
            EndpointStyle::Default => format!("{base}/responses/compact"),
            EndpointStyle::Azure { api_version } => format!(
                "{base}/openai/deployments/{}/responses/compact?api-version={api_version}",
                self.model
            ),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn compact_endpoint_urls_cover_default_and_azure() {
        let default = LlmConfig {
            protocol: Protocol::OpenAiResponses,
            base_url: "https://api.openai.com/v1/".into(),
            ..Default::default()
        };
        assert_eq!(
            default.compact_endpoint_url(),
            "https://api.openai.com/v1/responses/compact"
        );
        let azure = LlmConfig {
            base_url: "https://contoso.openai.azure.com/".into(),
            model: "gpt-deploy".into(),
            endpoint: EndpointStyle::Azure {
                api_version: "2025-04-01".into(),
            },
            ..Default::default()
        };
        assert_eq!(
            azure.compact_endpoint_url(),
            "https://contoso.openai.azure.com/openai/deployments/gpt-deploy/responses/compact?api-version=2025-04-01"
        );
    }
    #[test]
    fn endpoint_urls_normalize_v1_and_honor_escape_marker() {
        for (protocol, base_url, expected) in [
            (
                Protocol::OpenAiChat,
                "https://api.deepseek.com/v1",
                "https://api.deepseek.com/v1/chat/completions",
            ),
            (
                Protocol::OpenAiResponses,
                "https://example.com/",
                "https://example.com/v1/responses",
            ),
            (
                Protocol::OpenAiResponses,
                "https://example.com/v1",
                "https://example.com/v1/responses",
            ),
            (
                Protocol::OpenAiResponses,
                "https://example.com/v1/",
                "https://example.com/v1/responses",
            ),
            (
                Protocol::OpenAiResponses,
                "https://example.com/v1/v1#",
                "https://example.com/v1/v1/responses",
            ),
        ] {
            let config = LlmConfig {
                protocol,
                base_url: base_url.into(),
                ..Default::default()
            };
            assert_eq!(config.endpoint_url(), expected, "{protocol:?} {base_url}");
        }
    }

    fn sample() -> LlmConfig {
        LlmConfig {
            protocol: Protocol::AnthropicMessages,
            base_url: "https://api.anthropic.com/v1".into(),
            model: "claude-sonnet-4-5".into(),
            auth: Auth::Header {
                name: "x-api-key".into(),
                value: "sk-ant-xxx".into(),
            },
            endpoint: EndpointStyle::Azure {
                api_version: "2025-04-01-preview".into(),
            },
            wire: WireConfig {
                max_output_tokens_field: MaxTokensField::MaxCompletionTokens,
                supports_stop: false,
                reasoning_field: "reasoning".into(),
                reasoning_field_aliases: vec!["reasoning".into()],
                thinking: ThinkingWire {
                    toggle_field: Some("thinking".into()),
                    enabled_value: "enabled".into(),
                    disabled_value: "disabled".into(),
                    effort_field: None,
                    effort_map: BTreeMap::from([(Effort::XHigh, "x-high".into())]),
                    toggle_extra: serde_json::Map::from_iter([("keep".into(), json!("all"))]),
                },
                tool_choice_dialect: ToolChoiceDialect::AnyForRequired,
                parallel_tool_calls: false,
                developer_role: true,
                response_format_dialect: ResponseFormatDialect::Unsupported,
            },
            headers: BTreeMap::from([(
                "anthropic-beta".into(),
                "prompt-caching-2024-07-31".into(),
            )]),
            extra_body: serde_json::Map::from_iter([("top_k".into(), json!(20))]),
            timeout_secs: Some(120.0),
            connect_timeout_secs: Some(5.0),
            server_compaction_threshold: None,
            retry: RetryPolicy {
                max_attempts: 5,
                ..Default::default()
            },
        }
    }

    #[test]
    fn config_round_trips_through_json() {
        let config = sample();
        let encoded = serde_json::to_value(&config).unwrap();
        let decoded: LlmConfig = serde_json::from_value(encoded).unwrap();
        assert_eq!(decoded, config);
    }

    #[test]
    fn protocol_serializes_with_explicit_names() {
        assert_eq!(
            serde_json::to_value(Protocol::OpenAiChat).unwrap(),
            json!("openai_chat")
        );
        assert_eq!(
            serde_json::to_value(Protocol::OpenAiResponses).unwrap(),
            json!("openai_responses")
        );
        assert_eq!(
            serde_json::to_value(Protocol::AnthropicMessages).unwrap(),
            json!("anthropic_messages")
        );
    }

    #[test]
    fn missing_fields_fall_back_to_semantic_defaults() {
        let config: LlmConfig = serde_json::from_value(json!({})).unwrap();
        assert_eq!(config.protocol, Protocol::OpenAiChat);
        assert_eq!(config.base_url, "");
        assert_eq!(config.auth, Auth::None);
        assert_eq!(config.endpoint, EndpointStyle::Default);
        assert_eq!(
            config.wire.max_output_tokens_field,
            MaxTokensField::MaxTokens
        );
        assert!(config.wire.supports_stop);
        assert_eq!(config.wire.reasoning_field, "reasoning_content");
        assert_eq!(
            config.wire.reasoning_field_aliases,
            vec!["reasoning_content".to_string(), "reasoning".to_string()]
        );
        assert_eq!(
            config.wire.thinking.toggle_field.as_deref(),
            Some("thinking")
        );
        assert_eq!(config.wire.thinking.enabled_value, "enabled");
        assert_eq!(config.wire.thinking.disabled_value, "disabled");
        assert_eq!(
            config.wire.thinking.effort_field.as_deref(),
            Some("reasoning_effort")
        );
        assert!(config.wire.thinking.effort_map.is_empty());
        assert!(config.wire.thinking.toggle_extra.is_empty());
        assert_eq!(config.wire.tool_choice_dialect, ToolChoiceDialect::Standard);
        assert!(config.wire.parallel_tool_calls);
        assert!(!config.wire.developer_role);
        assert_eq!(
            config.wire.response_format_dialect,
            ResponseFormatDialect::Standard
        );
        assert!(config.headers.is_empty());
        assert!(config.extra_body.is_empty());
        assert_eq!(config.timeout_secs, None);
        assert_eq!(config.connect_timeout_secs, None);
        assert_eq!(config.retry, RetryPolicy::default());
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let err = serde_json::from_value::<LlmConfig>(json!({"base_uri": "x"})).unwrap_err();
        assert!(err.to_string().contains("base_uri"), "{err}");

        let err = serde_json::from_value::<WireConfig>(json!({"max_tokens_field": "max_tokens"}))
            .unwrap_err();
        assert!(err.to_string().contains("max_tokens_field"), "{err}");

        let err = serde_json::from_value::<CacheConfig>(json!({"breakpoint": [1]})).unwrap_err();
        assert!(err.to_string().contains("breakpoint"), "{err}");
    }

    #[test]
    fn auth_debug_never_leaks_secrets() {
        let bearer = Auth::Bearer("sk-secret".into());
        let debug = format!("{bearer:?}");
        assert_eq!(debug, "Bearer(\"***\")");
        assert!(!debug.contains("sk-secret"));

        let header = Auth::Header {
            name: "api-key".into(),
            value: "azure-secret".into(),
        };
        let debug = format!("{header:?}");
        assert!(debug.contains("api-key"), "{debug}");
        assert!(!debug.contains("azure-secret"), "{debug}");

        assert_eq!(format!("{:?}", Auth::None), "None");
    }

    #[test]
    fn auth_from_env_reports_missing_variable_with_context() {
        let err = Auth::from_env("MOC_LLM_TEST_MISSING_KEY_7f3a").unwrap_err();
        let text = format!("{err:#}");
        assert!(text.contains("MOC_LLM_TEST_MISSING_KEY_7f3a"), "{text}");
        assert!(text.contains("读取环境变量"), "{text}");
    }

    #[test]
    fn azure_endpoint_uses_deployment_and_api_version() {
        let config = LlmConfig {
            protocol: Protocol::OpenAiChat,
            base_url: "https://contoso.openai.azure.com".into(),
            model: "gpt-4o-deploy".into(),
            endpoint: EndpointStyle::Azure {
                api_version: "2025-04-01-preview".into(),
            },
            ..Default::default()
        };
        assert_eq!(
            config.endpoint_url(),
            "https://contoso.openai.azure.com/openai/deployments/gpt-4o-deploy/chat/completions?api-version=2025-04-01-preview"
        );
    }

    #[test]
    fn with_model_replaces_only_model() {
        let config = sample();
        let next = config.with_model("claude-haiku-4-5");
        assert_eq!(next.model, "claude-haiku-4-5");
        assert_eq!(next.base_url, config.base_url);
        assert_eq!(next.auth, config.auth);
    }

    #[test]
    fn thinking_effort_value_honours_map() {
        let wire = ThinkingWire {
            effort_map: BTreeMap::from([(Effort::High, "high-effort".into())]),
            ..Default::default()
        };
        assert_eq!(wire.effort_value(Effort::High), "high-effort");
        assert_eq!(wire.effort_value(Effort::Low), "low");
    }

    #[test]
    fn cache_ttl_wire_strings() {
        assert_eq!(CacheTtl::FiveMinutes.as_str(), "5m");
        assert_eq!(CacheTtl::ThirtyMinutes.as_str(), "30m");
        assert_eq!(CacheTtl::OneHour.as_str(), "1h");
        assert_eq!(
            serde_json::to_value(CacheTtl::ThirtyMinutes).unwrap(),
            json!("thirty_minutes")
        );
    }
}

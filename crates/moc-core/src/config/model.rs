//! Provider/model 配置解析与 moc-llm 配置映射。

use std::collections::BTreeMap;
use std::process::Command;

use moc_llm::{
    AnthropicThinkingDialect, Auth, Effort, EndpointStyle, LlmConfig, MaxTokensField, Protocol,
    ReasoningEcho, ResponseFormatDialect, RetryPolicy, ThinkingWire, ToolChoiceDialect, WireConfig,
};
use serde::Deserialize;

use crate::error::CoreError;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProviderApi {
    #[serde(alias = "openai-completions")]
    OpenaiChat,
    OpenaiResponses,
    #[serde(alias = "anthropic-messages")]
    AnthropicMessages,
}

impl ProviderApi {
    fn protocol(&self) -> Protocol {
        match self {
            Self::OpenaiChat => Protocol::OpenAiChat,
            Self::OpenaiResponses => Protocol::OpenAiResponses,
            Self::AnthropicMessages => Protocol::AnthropicMessages,
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ModelCatalog {
    pub roles: ModelRoles,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRoles {
    pub default: String,
    pub smol: String,
    pub slow: String,
    pub plan: Option<String>,
}

impl Default for ModelRoles {
    fn default() -> Self {
        Self {
            default: "ds/deepseek-v4.1-flash:max".into(),
            smol: "ds/deepseek-v4.1-flash:low".into(),
            slow: "cpa-cx/gpt-6-luna:max".into(),
            plan: Some("cpa-cx/ag/gpt-5.6-sol:xhigh".into()),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ProviderDocument {
    pub api: Option<ProviderApi>,
    pub base_url: String,
    pub api_key: String,
    pub headers: BTreeMap<String, String>,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
    pub timeout_secs: Option<f64>,
    pub connect_timeout_secs: Option<f64>,
    #[serde(deserialize_with = "deserialize_provider_retry")]
    pub retry: RetryPolicy,
    pub endpoint: EndpointStyle,
    pub models: BTreeMap<String, ModelDocument>,
}

impl Default for ProviderDocument {
    fn default() -> Self {
        Self {
            api: None,
            base_url: String::new(),
            api_key: String::new(),
            headers: BTreeMap::new(),
            extra_body: serde_json::Map::new(),
            timeout_secs: Some(600.0),
            connect_timeout_secs: Some(20.0),
            retry: provider_retry_defaults(),
            endpoint: EndpointStyle::default(),
            models: BTreeMap::new(),
        }
    }
}

fn provider_retry_defaults() -> RetryPolicy {
    RetryPolicy {
        max_attempts: 10,
        initial_backoff_secs: 0.5,
        max_backoff_secs: 300.0,
        jitter: true,
    }
}

fn deserialize_provider_retry<'de, D>(deserializer: D) -> Result<RetryPolicy, D::Error>
where
    D: serde::Deserializer<'de>,
{
    // 部分表覆写仍继承 core 默认，而不是 LLM 独立客户端的三次重试策略。
    let mut defaults =
        serde_json::to_value(provider_retry_defaults()).map_err(serde::de::Error::custom)?;
    defaults
        .as_object_mut()
        .expect("RetryPolicy 是对象")
        .extend(serde_json::Map::<String, serde_json::Value>::deserialize(
            deserializer,
        )?);
    serde_json::from_value(defaults).map_err(serde::de::Error::custom)
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ModelDocument {
    pub context_window: u64,
    pub max_output_tokens: u32,
    pub reasoning: bool,
    pub thinking_level: ThinkingLevel,
    pub input: Vec<ModelInput>,
    pub compat: ModelCompat,
}

impl Default for ModelDocument {
    fn default() -> Self {
        Self {
            context_window: 200_000,
            max_output_tokens: 32_000,
            reasoning: false,
            thinking_level: ThinkingLevel::Off,
            input: vec![ModelInput::Text],
            compat: ModelCompat::default(),
        }
    }
}

#[derive(Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ModelCompat {
    pub max_output_tokens_field: MaxTokensField,
    pub supports_stop: bool,
    pub supports_prompt_cache_key: bool,
    pub reasoning_field: String,
    pub reasoning_field_aliases: Vec<String>,
    pub anthropic_thinking: AnthropicThinkingDialect,
    pub tool_choice_dialect: ToolChoiceDialect,
    pub parallel_tool_calls: bool,
    pub developer_role: bool,
    pub image_url: bool,
    pub response_format_dialect: ResponseFormatDialect,
    pub echo_reasoning: ReasoningEcho,
    pub requires_reasoning_content_for_tool_calls: bool,
    pub requires_reasoning_content_for_all_assistant_turns: bool,
    pub requires_assistant_content_for_tool_calls: bool,
    pub thinking: ThinkingWire,
    pub extra_body: serde_json::Map<String, serde_json::Value>,
}

impl Default for ModelCompat {
    fn default() -> Self {
        Self {
            max_output_tokens_field: MaxTokensField::default(),
            supports_stop: true,
            supports_prompt_cache_key: true,
            reasoning_field: "reasoning_content".into(),
            reasoning_field_aliases: vec!["reasoning_content".into(), "reasoning".into()],
            anthropic_thinking: AnthropicThinkingDialect::default(),
            tool_choice_dialect: ToolChoiceDialect::default(),
            parallel_tool_calls: true,
            developer_role: false,
            image_url: true,
            response_format_dialect: ResponseFormatDialect::default(),
            echo_reasoning: ReasoningEcho::default(),
            requires_reasoning_content_for_tool_calls: false,
            requires_reasoning_content_for_all_assistant_turns: false,
            requires_assistant_content_for_tool_calls: false,
            thinking: ThinkingWire::default(),
            extra_body: serde_json::Map::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    Auto,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Minimal => "minimal",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Auto => "auto",
        }
    }

    pub fn effort(self) -> Option<Effort> {
        match self {
            Self::Off => Some(Effort::None),
            Self::Minimal => Some(Effort::Minimal),
            Self::Low => Some(Effort::Low),
            Self::Medium => Some(Effort::Medium),
            Self::High => Some(Effort::High),
            Self::Xhigh => Some(Effort::XHigh),
            Self::Max => Some(Effort::Max),
            Self::Auto => None,
        }
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "minimal" => Ok(Self::Minimal),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            "auto" => Ok(Self::Auto),
            _ => Err(CoreError::Config(format!("未知 thinking level `{value}`"))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModelInput {
    Text,
    Image,
}

/// 模型可用于请求前置校验的能力位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelCapabilities {
    pub text: bool,
    pub image: bool,
    pub reasoning: bool,
}

/// 从 provider/model selector 解析得到的可调用模型。
#[derive(Clone)]
pub struct ResolvedModel {
    /// 规范 selector，包含最终 thinking level。
    pub selector: String,
    /// 协议层展示用模型 ID（不包含 thinking level）。
    pub id: String,
    /// 协议层展示名。
    pub display_name: String,
    /// 上游 provider 标识。
    pub provider_id: String,
    /// 上游 model/deployment 标识。
    pub model_id: String,
    /// 模型请求能力。
    pub capabilities: ModelCapabilities,
    /// 上下文 token 上限。
    pub context_window: u64,
    /// 模型输出 token 上限。
    pub max_output_tokens: u32,
    /// 解析后的模型 thinking level。
    pub thinking_level: ThinkingLevel,
    /// 供应商协议配置。
    pub llm: LlmConfig,
    /// 默认思维链回传策略。
    pub echo_reasoning: ReasoningEcho,
    /// Chat/Responses 上游是否接受 prompt_cache_key。
    pub supports_prompt_cache_key: bool,
    /// 工具调用回传 reasoning_content 的上游约束。
    pub requires_reasoning_content_for_tool_calls: bool,
    /// 所有 assistant 历史都必须携带 reasoning_content。
    pub requires_reasoning_content_for_all_assistant_turns: bool,
    /// 工具调用回传 assistant content 的上游约束。
    pub requires_assistant_content_for_tool_calls: bool,
}

#[derive(Clone)]
pub(super) struct ResolvedProvider {
    api: ProviderApi,
    base_url: String,
    auth: Auth,
    headers: BTreeMap<String, String>,
    extra_body: serde_json::Map<String, serde_json::Value>,
    timeout_secs: Option<f64>,
    connect_timeout_secs: Option<f64>,
    retry: RetryPolicy,
    endpoint: EndpointStyle,
    models: BTreeMap<String, ModelDocument>,
}

impl ResolvedProvider {
    pub(super) fn from_document(id: &str, document: ProviderDocument) -> Result<Self, CoreError> {
        let api = document
            .api
            .ok_or_else(|| CoreError::Config(format!("providers.{id}.api 未配置")))?;
        if !(document.base_url.starts_with("https://") || document.base_url.starts_with("http://"))
        {
            return Err(CoreError::Config(format!(
                "providers.{id}.base_url 必须是 http(s) URL"
            )));
        }
        if document.models.is_empty() {
            return Err(CoreError::Config(format!("providers.{id}.models 不能为空")));
        }
        // 上下文窗口为 0 会使压缩阈值饱和为 0，导致每个 step 都触发压缩。
        for (model_id, model) in &document.models {
            if model.context_window == 0 {
                return Err(CoreError::Config(format!(
                    "providers.{id}.models.{model_id}.context_window 必须大于 0"
                )));
            }
        }
        let key = resolve_api_key(id, &document.api_key)?;
        let auth = match (api.protocol(), key) {
            (_, None) => Auth::None,
            (Protocol::AnthropicMessages, Some(value)) => Auth::Header {
                name: "x-api-key".into(),
                value,
            },
            (_, Some(value)) => Auth::Bearer(value),
        };
        Ok(Self {
            api,
            base_url: document.base_url,
            auth,
            headers: document.headers,
            extra_body: document.extra_body,
            timeout_secs: document.timeout_secs,
            connect_timeout_secs: document.connect_timeout_secs,
            retry: document.retry,
            endpoint: document.endpoint,
            models: document.models,
        })
    }

    fn resolve_model(
        &self,
        provider_id: &str,
        model_id: &str,
        model: &ModelDocument,
        level: ThinkingLevel,
    ) -> ResolvedModel {
        let mut extra_body = self.extra_body.clone();
        extra_body.extend(model.compat.extra_body.clone());
        let llm = LlmConfig {
            protocol: self.api.protocol(),
            base_url: self.base_url.clone(),
            model: model_id.to_owned(),
            auth: self.auth.clone(),
            endpoint: self.endpoint.clone(),
            wire: WireConfig {
                max_output_tokens_field: model.compat.max_output_tokens_field,
                supports_stop: model.compat.supports_stop,
                reasoning_field: model.compat.reasoning_field.clone(),
                reasoning_field_aliases: model.compat.reasoning_field_aliases.clone(),
                thinking: model.compat.thinking.clone(),
                anthropic_thinking: model.compat.anthropic_thinking,
                tool_choice_dialect: model.compat.tool_choice_dialect,
                parallel_tool_calls: model.compat.parallel_tool_calls,
                developer_role: model.compat.developer_role,
                image_url: model.compat.image_url,
                response_format_dialect: model.compat.response_format_dialect,
            },
            headers: self.headers.clone(),
            extra_body,
            timeout_secs: self.timeout_secs,
            connect_timeout_secs: self.connect_timeout_secs,
            server_compaction_threshold: None,
            retry: self.retry.clone(),
        };
        let id = format!("{provider_id}/{model_id}");
        ResolvedModel {
            selector: format!("{id}:{}", level.as_str()),
            display_name: format!("{id} @{}", level.as_str()),
            id,
            provider_id: provider_id.to_owned(),
            model_id: model_id.to_owned(),
            capabilities: ModelCapabilities {
                text: model.input.contains(&ModelInput::Text),
                image: model.input.contains(&ModelInput::Image),
                reasoning: model.reasoning,
            },
            context_window: model.context_window,
            max_output_tokens: model.max_output_tokens,
            thinking_level: level,
            llm,
            echo_reasoning: model.compat.echo_reasoning,
            supports_prompt_cache_key: model.compat.supports_prompt_cache_key,
            requires_reasoning_content_for_tool_calls: model
                .compat
                .requires_reasoning_content_for_tool_calls,
            requires_reasoning_content_for_all_assistant_turns: model
                .compat
                .requires_reasoning_content_for_all_assistant_turns,
            requires_assistant_content_for_tool_calls: model
                .compat
                .requires_assistant_content_for_tool_calls,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(super) struct ModelSelector<'a> {
    pub provider_id: &'a str,
    pub model_id: &'a str,
    pub level: Option<ThinkingLevel>,
}

impl<'a> ModelSelector<'a> {
    pub fn parse(value: &'a str) -> Result<Self, CoreError> {
        let (path, level) = if let Some((path, raw_level)) = value.rsplit_once(':') {
            (path, Some(raw_level.parse::<ThinkingLevel>()?))
        } else {
            (value, None)
        };
        let (provider_id, model_id) = path.split_once('/').ok_or_else(|| {
            CoreError::Config(format!(
                "模型 selector `{value}` 必须是 provider/model[:level]"
            ))
        })?;
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(CoreError::Config(format!("模型 selector `{value}` 不完整")));
        }
        Ok(Self {
            provider_id,
            model_id,
            level,
        })
    }
}

pub(super) fn resolve_catalog(
    providers: &BTreeMap<String, ProviderDocument>,
) -> Result<BTreeMap<String, ResolvedProvider>, CoreError> {
    providers
        .iter()
        .map(|(id, document)| {
            Ok((
                id.clone(),
                ResolvedProvider::from_document(id, document.clone())?,
            ))
        })
        .collect()
}

pub(super) fn resolve_one(
    providers: &BTreeMap<String, ResolvedProvider>,
    selector: &str,
    fallback_level: Option<ThinkingLevel>,
) -> Result<ResolvedModel, CoreError> {
    let parsed = ModelSelector::parse(selector)?;
    let provider = providers.get(parsed.provider_id).ok_or_else(|| {
        CoreError::Config(format!(
            "模型 selector `{selector}` 引用了不存在的 provider `{}`",
            parsed.provider_id
        ))
    })?;
    let model = provider.models.get(parsed.model_id).ok_or_else(|| {
        CoreError::Config(format!(
            "模型 selector `{selector}` 引用了不存在的模型 `{}`",
            parsed.model_id
        ))
    })?;
    let level = parsed
        .level
        .or(fallback_level)
        .unwrap_or(model.thinking_level);
    Ok(provider.resolve_model(parsed.provider_id, parsed.model_id, model, level))
}

pub(super) fn all_models(providers: &BTreeMap<String, ResolvedProvider>) -> Vec<ResolvedModel> {
    let mut result = Vec::new();
    for (provider_id, provider) in providers {
        for (model_id, model) in &provider.models {
            result.push(provider.resolve_model(provider_id, model_id, model, model.thinking_level));
        }
    }
    result
}

fn shell_command(command: &str) -> Command {
    #[cfg(unix)]
    {
        let mut shell = Command::new(std::env::var_os("SHELL").unwrap_or_else(|| "sh".into()));
        shell.args(["-lc", command]);
        shell
    }
    #[cfg(windows)]
    {
        let mut shell =
            Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        shell.args(["/D", "/S", "/C"]);
        shell.arg(format!("chcp 65001>nul & ({command})"));
        shell
    }
}

fn resolve_api_key(provider: &str, spec: &str) -> Result<Option<String>, CoreError> {
    if spec.is_empty() {
        return Ok(None);
    }
    let value = if let Some(name) = spec.strip_prefix("env:") {
        std::env::var(name).map_err(|_| {
            CoreError::Config(format!(
                "providers.{provider}.api_key 环境变量 {name} 不存在"
            ))
        })?
    } else if let Some(command) = spec.strip_prefix('!') {
        let output = shell_command(command).output().map_err(|error| {
            CoreError::Config(format!(
                "providers.{provider}.api_key 命令无法启动: {error}"
            ))
        })?;
        if !output.status.success() {
            return Err(CoreError::Config(format!(
                "providers.{provider}.api_key 命令以状态 {} 失败",
                output.status
            )));
        }
        String::from_utf8(output.stdout).map_err(|error| {
            CoreError::Config(format!(
                "providers.{provider}.api_key 命令输出不是 UTF-8: {error}"
            ))
        })?
    } else {
        spec.to_owned()
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(CoreError::Config(format!(
            "providers.{provider}.api_key 解析为空"
        )));
    }
    Ok(Some(value))
}

pub(super) fn validate_roles(
    roles: &ModelRoles,
    providers: &BTreeMap<String, ResolvedProvider>,
) -> Result<(), CoreError> {
    for (role, selector) in [
        ("default", roles.default.as_str()),
        ("smol", roles.smol.as_str()),
        ("slow", roles.slow.as_str()),
    ] {
        resolve_one(providers, selector, None)
            .map_err(|error| CoreError::Config(format!("models.roles.{role} 无法解析：{error}")))?;
    }
    if let Some(selector) = roles
        .plan
        .as_deref()
        .filter(|selector| !selector.is_empty())
    {
        resolve_one(providers, selector, None)
            .map_err(|error| CoreError::Config(format!("models.roles.plan 无法解析：{error}")))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_retry_overrides_keep_core_defaults_and_reject_unknown_keys() {
        for source in ["", "retry = { jitter = false }"] {
            let provider: ProviderDocument = toml::from_str(source).unwrap();
            assert_eq!(provider.retry.max_attempts, 10);
            assert_eq!(provider.retry.initial_backoff_secs, 0.5);
            assert_eq!(provider.retry.max_backoff_secs, 300.0);
        }
        let provider: ProviderDocument = toml::from_str("retry = { max_attempts = 2 }").unwrap();
        assert!(provider.retry.delay(1, None).is_some());
        assert!(provider.retry.delay(2, None).is_none());
        assert!(toml::from_str::<ProviderDocument>("retry = { max_attempt = 2 }").is_err());
    }

    #[test]
    fn selector_preserves_slashes_inside_model_ids_and_parses_level() {
        let selector = ModelSelector::parse("cpa-cx/ag/gpt-5.6-sol:xhigh").expect("selector 合法");
        assert_eq!(selector.provider_id, "cpa-cx");
        assert_eq!(selector.model_id, "ag/gpt-5.6-sol");
        assert_eq!(selector.level, Some(ThinkingLevel::Xhigh));
        assert!(ModelSelector::parse("provider/model:invalid").is_err());
    }
}

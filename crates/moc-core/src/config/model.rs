//! Provider/model 配置解析与 moc-llm 配置映射。

use std::collections::BTreeMap;
use std::process::Command;

use moc_llm::{
    Auth, Effort, EndpointStyle, LlmConfig, MaxTokensField, Protocol, ReasoningEcho,
    ResponseFormatDialect, RetryPolicy, ThinkingWire, ToolChoiceDialect, WireConfig,
};
use moc_i18n::t;
use serde::Deserialize;

use crate::error::CoreError;

/// 缓存保留策略；自动沿用协议默认，不进行额外预热请求。
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CachePolicy {
    #[default]
    Auto,
    Short,
    Long,
    Off,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum ProviderApi {
    OpenaiChat,
    OpenaiResponses,
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

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ModelCatalog {
    /// 模型选择器里置顶的最近使用条数；0 表示不记录也不显示。
    pub recent: usize,
    pub roles: ModelRoles,
    pub fusion: FusionDefaults,
}

impl Default for ModelCatalog {
    fn default() -> Self {
        Self {
            recent: 5,
            roles: ModelRoles::default(),
            fusion: FusionDefaults::default(),
        }
    }
}

/// 探索子代理选择器：沿用父会话当前模型。
pub const ROLE_INHERIT: &str = "inherit";

/// Fusion 选择器别名：展开为 `[models.fusion]` 里配置的 lead/sidekick 组合。
pub const FUSION_ALIAS: &str = "fusion";
/// 显式 Fusion 选择器前缀：`fusion:<lead>+<sidekick>`。
///
/// `+` 两侧各是一个普通的 供应商/模型[:等级] selector，用 `split_once('+')` 切开；
/// 因此模型 id 本身带 `+` 的写法不能在 Fusion 中使用。
pub const FUSION_PREFIX: &str = "fusion:";

/// selector 是否是 Fusion 选择器（别名或显式组合）。
///
/// 持久化的 `session.model` 恒为规范形 `fusion:lead+sidekick`，前缀判定即可。
pub fn is_fusion_selector(selector: &str) -> bool {
    selector == FUSION_ALIAS || selector.starts_with(FUSION_PREFIX)
}

/// 已持久化的 selector 字符串 → 真实发请求的 selector：规范 Fusion 形式
/// 返回 lead 段，普通 selector 原样返回。
///
/// 与 [`crate::ResolvedModel::actual_selector`] 对应：后者作用于已解析的
/// `ResolvedModel`，本函数作用于未解析的字符串（如 transcript 里落盘的
/// `model` 字段）。
pub fn request_selector(selector: &str) -> &str {
    match selector.strip_prefix(FUSION_PREFIX) {
        Some(rest) => rest.split_once('+').map_or(rest, |(lead, _)| lead),
        None => selector,
    }
}

/// `[models.fusion]`：别名 `fusion` 展开用的默认组合；两侧都为空表示未配置。
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FusionDefaults {
    pub lead: String,
    pub sidekick: String,
}

/// 一次 Fusion 会话的模型组合（规范 selector）。
///
/// lead 是本会话实际发请求的模型；sidekick 供 `sidekick` 工具创建持久子会话。
#[derive(Debug, Clone)]
pub struct FusionModels {
    pub lead: String,
    pub sidekick: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRoles {
    pub explore: String,
}

impl Default for ModelRoles {
    fn default() -> Self {
        Self {
            explore: "inherit".into(),
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
    pub max_concurrency: usize,
    pub cache: CachePolicy,
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
            timeout_secs: None,
            connect_timeout_secs: Some(20.0),
            max_concurrency: 0,
            cache: CachePolicy::Auto,
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
    /// 请求体字节上限；留空使用协议无关的 50 MiB 默认值。
    pub max_request_bytes: Option<u64>,
    pub reasoning: bool,
    /// `None` 表示使用统一的默认推理等级集合。
    pub thinking_levels: Option<Vec<ThinkingLevel>>,
    pub input: Vec<ModelInput>,
    pub compat: ModelCompat,
    /// 每百万 token 定价（美元）；未配置时前端隐藏费用段。
    pub pricing: Option<moc_protocol::ModelPricing>,
    /// 是否支持 Fast Mode；开启后前端显示开关，会话打开开关时请求带 `service_tier = "fast"`。
    pub fast_mode: bool,
    /// 图片生成模型：仅在 Image Playground 可见，Agent 主界面隐藏。
    pub image_gen: bool,
}
impl Default for ModelDocument {
    fn default() -> Self {
        Self {
            context_window: 200_000,
            max_output_tokens: 32_000,
            max_request_bytes: None,
            reasoning: false,
            thinking_levels: None,
            input: vec![ModelInput::Text],
            compat: ModelCompat::default(),
            pricing: None,
            fast_mode: false,
            image_gen: false,
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
    pub tool_choice_dialect: ToolChoiceDialect,
    pub parallel_tool_calls: bool,
    pub developer_role: bool,
    pub response_format_dialect: ResponseFormatDialect,
    pub echo_reasoning: ReasoningEcho,
    pub requires_reasoning_content_for_tool_calls: bool,
    pub requires_reasoning_content_for_all_assistant_turns: bool,
    pub requires_assistant_content_for_tool_calls: bool,
    pub thinking: ThinkingWire,
    pub headers: BTreeMap<String, String>,
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
            tool_choice_dialect: ToolChoiceDialect::default(),
            parallel_tool_calls: true,
            developer_role: false,
            response_format_dialect: ResponseFormatDialect::default(),
            echo_reasoning: ReasoningEcho::default(),
            requires_reasoning_content_for_tool_calls: false,
            requires_reasoning_content_for_all_assistant_turns: false,
            requires_assistant_content_for_tool_calls: false,
            thinking: ThinkingWire::default(),
            headers: BTreeMap::new(),
            extra_body: serde_json::Map::new(),
        }
    }
}

/// 思考等级。内置等级之外允许自定义名（如 `ultra`），原样作为推理强度发送；
/// 自定义名是否可用由模型的 `thinking_levels` 声明决定。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
    /// 自定义等级名；经 [`str::parse`] 构造时已校验字符集且不与内置名重复。
    Custom(String),
}

impl ThinkingLevel {
    pub fn as_str(&self) -> &str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
            Self::Custom(name) => name,
        }
    }

    pub fn effort(&self) -> Option<Effort> {
        Some(match self {
            Self::Off => Effort::None,
            Self::Low => Effort::Low,
            Self::Medium => Effort::Medium,
            Self::High => Effort::High,
            Self::Xhigh => Effort::XHigh,
            Self::Max => Effort::Max,
            Self::Custom(name) => Effort::from(name.clone()),
        })
    }
}

impl std::str::FromStr for ThinkingLevel {
    type Err = CoreError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "off" => Ok(Self::Off),
            "low" => Ok(Self::Low),
            "medium" => Ok(Self::Medium),
            "high" => Ok(Self::High),
            "xhigh" => Ok(Self::Xhigh),
            "max" => Ok(Self::Max),
            // 自定义名会出现在 `供应商/模型:等级` 写法与请求体里：只收小写 ASCII 标识符，
            // 避免与分隔符冲突，也避免大小写变体被当成不同等级。
            _ if !value.is_empty()
                && value.len() <= 32
                && value.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_.".contains(&byte)
                }) =>
            {
                Ok(Self::Custom(value.to_owned()))
            }
            _ => Err(CoreError::Config(
                t!("core.err.thinking_level_invalid", value = value).into_owned(),
            )),
        }
    }
}

impl TryFrom<String> for ThinkingLevel {
    type Error = CoreError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
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
    /// 模型上下文窗口上限。
    pub context_window: u64,
    /// 请求体字节上限；`None` 时由图片预算模块采用默认值。
    pub max_request_bytes: Option<u64>,
    pub max_output_tokens: u32,
    /// 解析后的模型 thinking level。
    pub thinking_level: ThinkingLevel,
    /// 模型允许选择的 thinking level；`off` 始终可用于显式关闭推理。
    pub thinking_levels: Vec<ThinkingLevel>,
    /// 每百万 token 定价（美元）；`None` 表示未配置。
    pub pricing: Option<moc_protocol::ModelPricing>,
    /// 模型配置了 `fast_mode = true`，即上游接受 `service_tier = "fast"`。
    pub fast_mode: bool,
    /// 是否为图片生成模型（仅 Image Playground 可见）。
    pub image_gen: bool,
    /// 供应商协议配置。
    pub llm: LlmConfig,
    pub max_concurrency: usize,
    pub cache: CachePolicy,
    /// 默认思维链回传策略。
    pub echo_reasoning: ReasoningEcho,
    /// Chat/Responses 上游是否接受提示_cache_key。
    pub supports_prompt_cache_key: bool,
    /// 工具调用回传 reasoning_content 的上游约束。
    pub requires_reasoning_content_for_tool_calls: bool,
    /// 所有 assistant 历史都必须携带 reasoning_content。
    pub requires_reasoning_content_for_all_assistant_turns: bool,
    /// 工具调用回传 assistant content 的上游约束。
    pub requires_assistant_content_for_tool_calls: bool,
    /// Fusion 组合；普通模型为 `None`。此时 `selector` 是规范 Fusion selector，
    /// 其余字段（llm、provider、能力等）都是 lead 模型的。
    pub fusion: Option<FusionModels>,
}

impl ResolvedModel {
    /// 实际发请求的模型 selector：Fusion 取 lead，普通模型取自身。
    ///
    /// 子代理继承、用量记账与定价查询都用它；会话头与系统提示仍用规范
    /// `selector`（Fusion 时是组合形式）。
    pub fn actual_selector(&self) -> &str {
        match &self.fusion {
            Some(fusion) => &fusion.lead,
            None => &self.selector,
        }
    }

    /// 按会话开关启用 Fast Mode：模型支持且会话打开时在请求体顶层写入
    /// `service_tier = "fast"`；三种协议的编码器都会把 `extra_body` 并入请求体。
    pub fn with_fast_mode(mut self, enabled: bool) -> Self {
        if enabled && self.fast_mode {
            self.llm
                .extra_body
                .insert("service_tier".to_owned(), serde_json::json!("fast"));
        }
        self
    }
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
    max_concurrency: usize,
    cache: CachePolicy,
    retry: RetryPolicy,
    endpoint: EndpointStyle,
    models: BTreeMap<String, ModelDocument>,
}

impl ResolvedProvider {
    pub(super) fn catalog_config(&self) -> LlmConfig {
        LlmConfig {
            protocol: self.api.protocol(),
            base_url: self.base_url.clone(),
            auth: self.auth.clone(),
            headers: self.headers.clone(),
            endpoint: self.endpoint.clone(),
            connect_timeout_secs: self.connect_timeout_secs,
            ..Default::default()
        }
    }
    pub(super) fn from_document(id: &str, document: ProviderDocument) -> Result<Self, CoreError> {
        let api = document.api.ok_or_else(|| {
            CoreError::Config(t!("core.err.provider_no_api", id = id).into_owned())
        })?;
        if !(document.base_url.starts_with("https://") || document.base_url.starts_with("http://"))
        {
            return Err(CoreError::Config(
                t!("core.err.provider_base_url", id = id).into_owned(),
            ));
        }
        super::parameters::validate(
            api.protocol(),
            &document.extra_body,
            &format!("providers.{id}.extra_body"),
        )?;
        // 上下文窗口为 0 会使压缩阈值饱和为 0，导致每一轮都触发压缩。
        for (model_id, model) in &document.models {
            if model.image_gen {
                // 图片生成模型不走聊天协议，跳过聊天专属校验，允许 compat.extra_body 携带任意生图参数。
                continue;
            }
            if model.context_window == 0 {
                return Err(CoreError::Config(
                    t!("core.err.model_ctx_positive", provider = id, model = model_id)
                        .into_owned(),
                ));
            }
            if model.max_request_bytes == Some(0) {
                return Err(CoreError::Config(
                    t!(
                        "core.err.model_req_bytes_positive",
                        provider = id,
                        model = model_id
                    )
                    .into_owned(),
                ));
            }
            if let Some(levels) = &model.thinking_levels
                && (levels.is_empty() || levels.contains(&ThinkingLevel::Off))
            {
                return Err(CoreError::Config(
                    t!(
                        "core.err.model_thinking_non_off",
                        provider = id,
                        model = model_id
                    )
                    .into_owned(),
                ));
            }
            let body =
                super::parameters::merge_body(&document.extra_body, &model.compat.extra_body);
            super::parameters::validate(
                api.protocol(),
                &body,
                &format!("providers.{id}.models.{model_id}.compat.extra_body"),
            )?;
            if model.reasoning
                && let Some(level) =
                    super::parameters::default_effort(api.protocol(), &body, &model.compat.thinking)
                && level != ThinkingLevel::Off
                && !effective_thinking_levels(model).contains(&level)
            {
                return Err(CoreError::Config(
                    t!(
                        "core.err.model_default_effort_missing",
                        provider = id,
                        model = model_id,
                        level = level.as_str()
                    )
                    .into_owned(),
                ));
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
            max_concurrency: document.max_concurrency,
            cache: document.cache,
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
        level: Option<ThinkingLevel>,
    ) -> ResolvedModel {
        let mut extra_body =
            super::parameters::merge_body(&self.extra_body, &model.compat.extra_body);
        if self.cache == CachePolicy::Long
            && self.api.protocol() != Protocol::AnthropicMessages
            && model.compat.supports_prompt_cache_key
        {
            // 24h 是 OpenAI 自动缓存协议的长保留值；只在用户明确选择长缓存时发送。
            extra_body
                .entry("prompt_cache_retention".to_owned())
                .or_insert_with(|| serde_json::json!("24h"));
        }
        let thinking_levels = effective_thinking_levels(model);
        if self.cache == CachePolicy::Off || !model.compat.supports_prompt_cache_key {
            for name in [
                "prompt_cache_key",
                "prompt_cache_options",
                "prompt_cache_retention",
            ] {
                extra_body.remove(name);
            }
        }
        if self.cache == CachePolicy::Off {
            extra_body.remove("cache_control");
        }
        let level = level.unwrap_or_else(|| {
            super::parameters::default_effort(
                self.api.protocol(),
                &extra_body,
                &model.compat.thinking,
            )
            .unwrap_or_else(|| {
                thinking_levels
                    .first()
                    .cloned()
                    .unwrap_or(ThinkingLevel::Off)
            })
        });
        if self.api.protocol() == Protocol::AnthropicMessages
            && level != ThinkingLevel::Off
            && let Some(thinking) = extra_body
                .get_mut("thinking")
                .and_then(serde_json::Value::as_object_mut)
            && thinking.get("type").and_then(serde_json::Value::as_str) == Some("disabled")
        {
            thinking.insert("type".into(), serde_json::json!("adaptive"));
        }
        // HTTP 请求头不区分大小写；先供应商后模型，避免大小写变体打乱覆盖顺序。
        let headers = self
            .headers
            .iter()
            .chain(&model.compat.headers)
            .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
            .collect();
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
                tool_choice_dialect: model.compat.tool_choice_dialect,
                parallel_tool_calls: model.compat.parallel_tool_calls,
                developer_role: model.compat.developer_role,
                response_format_dialect: model.compat.response_format_dialect,
            },
            headers,
            extra_body,
            timeout_secs: self.timeout_secs,
            connect_timeout_secs: self.connect_timeout_secs,
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
            max_request_bytes: model.max_request_bytes,
            max_output_tokens: model.max_output_tokens,
            thinking_level: level,
            thinking_levels,
            pricing: model.pricing,
            fast_mode: model.fast_mode,
            image_gen: model.image_gen,
            llm,
            max_concurrency: self.max_concurrency,
            cache: self.cache,
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
            fusion: None,
        }
    }
}
fn default_thinking_levels() -> Vec<ThinkingLevel> {
    vec![
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ]
}

fn effective_thinking_levels(model: &ModelDocument) -> Vec<ThinkingLevel> {
    if !model.reasoning {
        return Vec::new();
    }
    model
        .thinking_levels
        .clone()
        .unwrap_or_else(default_thinking_levels)
}

#[derive(Debug, Clone)]
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
            CoreError::Config(t!("core.err.model_fmt_bad", value = value).into_owned())
        })?;
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(CoreError::Config(
                t!("core.err.model_fmt_incomplete", value = value).into_owned(),
            ));
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
        CoreError::Config(
            t!(
                "core.err.model_unknown_provider",
                selector = selector,
                provider = parsed.provider_id
            )
            .into_owned(),
        )
    })?;
    let model = provider.models.get(parsed.model_id).ok_or_else(|| {
        CoreError::Config(
            t!(
                "core.err.model_unknown_model",
                selector = selector,
                model = parsed.model_id
            )
            .into_owned(),
        )
    })?;
    if model.image_gen {
        return Err(CoreError::Config(
            t!("core.err.model_is_image_gen", selector = selector).into_owned(),
        ));
    }
    let resolved = provider.resolve_model(
        parsed.provider_id,
        parsed.model_id,
        model,
        parsed.level.or(fallback_level),
    );
    if model.reasoning
        && resolved.thinking_level != ThinkingLevel::Off
        && !resolved.thinking_levels.contains(&resolved.thinking_level)
    {
        return Err(CoreError::Config(
            t!(
                "core.err.model_unsupported_level",
                selector = selector,
                level = resolved.thinking_level.as_str()
            )
            .into_owned(),
        ));
    }
    Ok(resolved)
}

/// 解析仅用于 Image Playground 的图片生成模型（格式 `provider/model`）。
pub(super) fn resolve_image_one(
    providers: &BTreeMap<String, ResolvedProvider>,
    selector: &str,
) -> Result<ResolvedModel, CoreError> {
    let (provider_id, model_id) = selector.split_once('/').ok_or_else(|| {
        CoreError::Config(t!("core.err.model_fmt_bad", value = selector).into_owned())
    })?;
    if provider_id.is_empty() || model_id.is_empty() {
        return Err(CoreError::Config(
            t!("core.err.model_fmt_incomplete", value = selector).into_owned(),
        ));
    }
    let provider = providers.get(provider_id).ok_or_else(|| {
        CoreError::Config(
            t!(
                "core.err.image_model_unknown_provider",
                selector = selector,
                provider = provider_id
            )
            .into_owned(),
        )
    })?;
    let model = provider.models.get(model_id).ok_or_else(|| {
        CoreError::Config(
            t!(
                "core.err.image_model_unknown_model",
                selector = selector,
                model = model_id
            )
            .into_owned(),
        )
    })?;
    if !model.image_gen {
        return Err(CoreError::Config(
            t!("core.err.image_model_not_flagged", selector = selector).into_owned(),
        ));
    }
    Ok(provider.resolve_model(provider_id, model_id, model, None))
}

/// 解析一个可能带 Fusion 语法的 selector：普通写法直接走 [`resolve_one`]；
/// `fusion` 别名展开 `[models.fusion]`，`fusion:<lead>+<sidekick>` 显式组合两侧
/// 各自按普通 selector 解析（嵌套 Fusion 会被明确拒绝）。
pub(super) fn resolve_selector(
    providers: &BTreeMap<String, ResolvedProvider>,
    fusion: &FusionDefaults,
    selector: &str,
) -> Result<ResolvedModel, CoreError> {
    if selector == FUSION_ALIAS {
        if fusion.lead.is_empty() || fusion.sidekick.is_empty() {
            return Err(CoreError::Config(
                t!("core.err.fusion_not_configured").into_owned(),
            ));
        }
        return resolve_fusion(providers, &fusion.lead, &fusion.sidekick);
    }
    if let Some(rest) = selector.strip_prefix(FUSION_PREFIX) {
        let Some((lead, sidekick)) = rest.split_once('+') else {
            return Err(CoreError::Config(
                t!("core.err.fusion_fmt_bad", selector = selector).into_owned(),
            ));
        };
        return resolve_fusion(providers, lead, sidekick);
    }
    resolve_one(providers, selector, None)
}

/// 组装 Fusion 组合：返回 lead 的 `ResolvedModel`，`selector` 改写为规范
/// `fusion:lead+sidekick` 形式，`fusion` 字段携带两侧规范 selector。
fn resolve_fusion(
    providers: &BTreeMap<String, ResolvedProvider>,
    lead_selector: &str,
    sidekick_selector: &str,
) -> Result<ResolvedModel, CoreError> {
    for side in [lead_selector, sidekick_selector] {
        if is_fusion_selector(side) {
            return Err(CoreError::Config(
                t!("core.err.fusion_no_nesting", side = side).into_owned(),
            ));
        }
    }
    let mut lead = resolve_one(providers, lead_selector, None)?;
    let sidekick = resolve_one(providers, sidekick_selector, None)?;
    let fusion = FusionModels {
        lead: lead.selector.clone(),
        sidekick: sidekick.selector.clone(),
    };
    lead.selector = format!("{FUSION_PREFIX}{}+{}", fusion.lead, fusion.sidekick);
    lead.display_name = "Fusion".into();
    lead.fusion = Some(fusion);
    Ok(lead)
}

pub(super) fn all_models(providers: &BTreeMap<String, ResolvedProvider>) -> Vec<ResolvedModel> {
    let mut result = Vec::new();
    for (provider_id, provider) in providers {
        for (model_id, model) in &provider.models {
            if !model.image_gen {
                result.push(provider.resolve_model(provider_id, model_id, model, None));
            }
        }
    }
    result
}

pub(super) fn all_image_models(
    providers: &BTreeMap<String, ResolvedProvider>,
) -> Vec<ResolvedModel> {
    let mut result = Vec::new();
    for (provider_id, provider) in providers {
        for (model_id, model) in &provider.models {
            if model.image_gen {
                result.push(provider.resolve_model(provider_id, model_id, model, None));
            }
        }
    }
    result
}

fn shell_command(command: &str) -> Result<Command, CoreError> {
    let shell = crate::shell::detect()
        .ok_or_else(|| CoreError::Config(t!("core.err.no_shell_keys").into_owned()))?;
    let mut process = Command::new(&shell.program);
    crate::shell::apply_arguments(&mut process, shell, command);
    Ok(process)
}

/// 解析密钥描述：空 → None；`env:NAME` → 环境变量；`!command` → 命令输出；否则字面量。
///
/// `label` 用于错误信息（如 `providers.p.api_key`），从不携带密钥值。
pub(crate) fn resolve_secret(label: &str, spec: &str) -> Result<Option<String>, CoreError> {
    if spec.is_empty() {
        return Ok(None);
    }
    let value = if let Some(name) = spec.strip_prefix("env:") {
        std::env::var(name).map_err(|_| {
            CoreError::Config(t!("core.err.env_missing", label = label, name = name).into_owned())
        })?
    } else if let Some(command) = spec.strip_prefix('!') {
        let output = shell_command(command)?.output().map_err(|error| {
            CoreError::Config(t!("core.err.cmd_start", label = label, error = error).into_owned())
        })?;
        if !output.status.success() {
            return Err(CoreError::Config(
                t!("core.err.cmd_status", label = label, status = output.status).into_owned(),
            ));
        }
        String::from_utf8(output.stdout).map_err(|error| {
            CoreError::Config(
                t!("core.err.cmd_not_utf8", label = label, error = error).into_owned(),
            )
        })?
    } else {
        spec.to_owned()
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(CoreError::Config(
            t!("core.err.parse_empty", label = label).into_owned(),
        ));
    }
    Ok(Some(value))
}

fn resolve_api_key(provider: &str, spec: &str) -> Result<Option<String>, CoreError> {
    resolve_secret(&format!("providers.{provider}.api_key"), spec)
}

pub(super) fn validate_models(
    catalog: &ModelCatalog,
    providers: &BTreeMap<String, ResolvedProvider>,
) -> Result<(), CoreError> {
    // `[models.fusion]` 逐字段编辑：/config 先配一侧再配另一侧，所以只配一侧
    // 算「未配置」而不是错误；每个非空侧本身必须可解析。
    for (label, selector) in [
        ("models.fusion.lead", catalog.fusion.lead.as_str()),
        ("models.fusion.sidekick", catalog.fusion.sidekick.as_str()),
    ] {
        if !selector.is_empty() {
            resolve_one(providers, selector, None).map_err(|error| {
                CoreError::Config(
                    t!(
                        "core.err.label_unparsable",
                        label = label,
                        detail = error.detail()
                    )
                    .into_owned(),
                )
            })?;
        }
    }
    if !matches!(catalog.roles.explore.as_str(), ROLE_INHERIT | "none") {
        resolve_selector(providers, &catalog.fusion, &catalog.roles.explore).map_err(|error| {
            CoreError::Config(
                t!("core.err.explore_agent_unparsable", detail = error.detail()).into_owned(),
            )
        })?;
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
        let custom = ModelSelector::parse("provider/model:ultra-2").expect("自定义等级合法");
        assert_eq!(custom.level, Some(ThinkingLevel::Custom("ultra-2".into())));
        assert_eq!(
            custom.level.and_then(|level| level.effort()),
            Some(Effort::Custom("ultra-2".into()))
        );
        assert!(ModelSelector::parse("provider/model:Ultra").is_err());
        assert!(ModelSelector::parse("provider/model:").is_err());
    }

    #[test]
    fn request_selector_maps_canonical_fusion_to_the_lead() {
        // 规范形 → lead 段（含等级后缀），普通 selector 与别名原样。
        assert_eq!(request_selector("fusion:p/a+p/b"), "p/a");
        assert_eq!(request_selector("fusion:p/a:high+p/b"), "p/a:high");
        assert_eq!(request_selector("fusion:p/a"), "p/a");
        assert_eq!(request_selector("p/a"), "p/a");
        assert_eq!(request_selector("fusion"), "fusion");
    }

    #[test]
    fn image_gen_models_are_isolated_from_agent_models() {
        let mut models = BTreeMap::new();
        models.insert("chat-1".to_owned(), ModelDocument::default());
        models.insert(
            "img-1".to_owned(),
            ModelDocument {
                image_gen: true,
                ..Default::default()
            },
        );
        let provider = ProviderDocument {
            api: Some(ProviderApi::OpenaiChat),
            base_url: "https://example.com/v1".into(),
            models,
            ..Default::default()
        };
        let mut map = BTreeMap::new();
        map.insert("p".to_owned(), provider);
        let catalog = resolve_catalog(&map).expect("解析目录");

        let agent_ids: Vec<_> = all_models(&catalog).into_iter().map(|m| m.id).collect();
        let image_ids: Vec<_> = all_image_models(&catalog)
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(agent_ids, vec!["p/chat-1"]);
        assert_eq!(image_ids, vec!["p/img-1"]);
        assert!(resolve_one(&catalog, "p/img-1", None).is_err());
        assert!(resolve_image_one(&catalog, "p/chat-1").is_err());
        assert!(resolve_image_one(&catalog, "p/img-1").is_ok());
    }
}

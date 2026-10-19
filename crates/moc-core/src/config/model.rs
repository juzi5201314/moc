//! Provider/model 配置解析与 moc-llm 配置映射。

use std::collections::BTreeMap;
use std::process::Command;

use moc_llm::{
    Auth, Effort, EndpointStyle, LlmConfig, MaxTokensField, Protocol, ReasoningEcho,
    ResponseFormatDialect, RetryPolicy, Role, ThinkingWire, ToolChoiceDialect, WireConfig,
};
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

/// 指令消息的角色：默认整条指令以 system 下发；`user` 用于对 system 遵循度不足的
/// 上游，把规定性片段（工具政策、仓库规则等）放到 user 消息里。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstructionRole {
    #[default]
    System,
    User,
}

impl InstructionRole {
    /// 指令消息的线上角色。
    pub fn role(self) -> Role {
        match self {
            Self::System => Role::System,
            Self::User => Role::User,
        }
    }
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

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(super) struct ModelCatalog {
    pub roles: ModelRoles,
}

/// 角色选择器：沿用主模型。
pub const ROLE_INHERIT: &str = "inherit";
/// 计划模型选择器：进入计划模式时不切换模型，沿用会话当前模型。
pub const ROLE_NONE: &str = "none";

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelRoles {
    pub default: String,
    pub explore: String,
    pub plan: String,
}

impl Default for ModelRoles {
    fn default() -> Self {
        Self {
            default: String::new(),
            explore: "inherit".into(),
            plan: "inherit".into(),
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
    /// Responses 原生远程压缩；未配置时按协议与模型名自动判断。
    pub remote_compaction: Option<bool>,
    /// 每百万 token 定价（美元）；未配置时前端隐藏费用段。
    pub pricing: Option<moc_protocol::ModelPricing>,
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
            remote_compaction: None,
            pricing: None,
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
    /// 指令消息的角色。
    pub instruction_role: InstructionRole,
    /// 指令角色为 `user` 时仍留在 system 消息里的片段名。
    pub instruction_system: Vec<String>,
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
            tool_choice_dialect: ToolChoiceDialect::default(),
            parallel_tool_calls: true,
            developer_role: false,
            instruction_role: InstructionRole::System,
            instruction_system: default_instruction_system(),
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

/// 指令角色为 `user` 时默认留在 system 的片段：身份、MCP 清单与信任声明、环境。
/// 规定性片段（工具政策、仓库规则等）全部交给 user 消息。
fn default_instruction_system() -> Vec<String> {
    vec![
        crate::system_prompt::PIECE_INTRO.into(),
        crate::system_prompt::PIECE_MCP_SERVERS.into(),
        crate::system_prompt::PIECE_ENVIRONMENT.into(),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ThinkingLevel {
    Off,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::Xhigh => "xhigh",
            Self::Max => "max",
        }
    }

    pub fn effort(self) -> Option<Effort> {
        match self {
            Self::Off => Some(Effort::None),
            Self::Low => Some(Effort::Low),
            Self::Medium => Some(Effort::Medium),
            Self::High => Some(Effort::High),
            Self::Xhigh => Some(Effort::XHigh),
            Self::Max => Some(Effort::Max),
        }
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
            _ => Err(CoreError::Config(format!(
                "未知的思考等级 {value}；可用值：off、low、medium、high、xhigh、max"
            ))),
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
    /// 模型上下文窗口上限。
    pub context_window: u64,
    /// 请求体字节上限；`None` 时由图片预算模块采用默认值。
    pub max_request_bytes: Option<u64>,
    pub max_output_tokens: u32,
    /// 解析后的模型 thinking level。
    pub thinking_level: ThinkingLevel,
    /// 模型允许选择的 thinking level；`off` 始终可用于显式关闭推理。
    pub thinking_levels: Vec<ThinkingLevel>,
    /// 是否启用 Responses 原生远程压缩。
    pub remote_compaction: bool,
    /// 每百万 token 定价（美元）；`None` 表示未配置。
    pub pricing: Option<moc_protocol::ModelPricing>,
    /// 供应商协议配置。
    pub llm: LlmConfig,
    pub max_concurrency: usize,
    pub cache: CachePolicy,
    /// 指令消息的角色。
    pub instruction_role: InstructionRole,
    /// 指令角色为 `user` 时仍留在 system 消息里的片段名（已按规范名单校验）。
    pub instruction_system: Vec<&'static str>,
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
}

impl ResolvedModel {
    /// 该指令片段在此模型下是否留在 system 消息里。
    pub fn piece_keeps_system(&self, name: &str) -> bool {
        self.instruction_role == InstructionRole::System || self.instruction_system.contains(&name)
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
        let api = document
            .api
            .ok_or_else(|| CoreError::Config(format!("供应商 {id} 没有选择 API 格式")))?;
        if !(document.base_url.starts_with("https://") || document.base_url.starts_with("http://"))
        {
            return Err(CoreError::Config(format!(
                "供应商 {id} 的服务地址必须以 http:// 或 https:// 开头"
            )));
        }
        // 上下文窗口为 0 会使压缩阈值饱和为 0，导致每一轮都触发压缩。
        for (model_id, model) in &document.models {
            if model.context_window == 0 {
                return Err(CoreError::Config(format!(
                    "providers.{id}.models.{model_id}.context_window 必须大于 0"
                )));
            }
            if model.max_request_bytes == Some(0) {
                return Err(CoreError::Config(format!(
                    "providers.{id}.models.{model_id}.max_request_bytes 必须大于 0"
                )));
            }
            if let Some(levels) = &model.thinking_levels
                && (levels.is_empty() || levels.contains(&ThinkingLevel::Off))
            {
                return Err(CoreError::Config(format!(
                    "providers.{id}.models.{model_id}.thinking_levels 必须包含至少一个非 off 等级"
                )));
            }
            if model.remote_compaction == Some(true) && api.protocol() != Protocol::OpenAiResponses
            {
                return Err(CoreError::Config(format!(
                    "providers.{id}.models.{model_id}.remote_compaction 仅支持 openai_responses"
                )));
            }
            for name in &model.compat.instruction_system {
                if crate::system_prompt::canonical_piece_name(name).is_none() {
                    return Err(CoreError::Config(format!(
                        "providers.{id}.models.{model_id}.compat.instruction_system 含未知指令片段名 {name:?}；可用：{}",
                        crate::system_prompt::PIECE_NAMES.join("、")
                    )));
                }
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
        level: ThinkingLevel,
    ) -> ResolvedModel {
        let mut extra_body = self.extra_body.clone();
        extra_body.extend(model.compat.extra_body.clone());
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
        let remote_compaction = model.remote_compaction.unwrap_or(
            self.api.protocol() == Protocol::OpenAiResponses
                && model_id
                    .rsplit('/')
                    .next()
                    .is_some_and(|id| id.starts_with("gpt-")),
        );
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
            max_request_bytes: model.max_request_bytes,
            max_output_tokens: model.max_output_tokens,
            thinking_level: level,
            thinking_levels,
            remote_compaction,
            pricing: model.pricing,
            llm,
            max_concurrency: self.max_concurrency,
            cache: self.cache,
            instruction_role: model.compat.instruction_role,
            instruction_system: model
                .compat
                .instruction_system
                .iter()
                .filter_map(|name| crate::system_prompt::canonical_piece_name(name))
                .collect(),
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
            CoreError::Config(format!("模型写法“{value}”不正确，应为 供应商/模型"))
        })?;
        if provider_id.is_empty() || model_id.is_empty() {
            return Err(CoreError::Config(format!("模型写法“{value}”不完整")));
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
    let thinking_levels = effective_thinking_levels(model);
    let default_level = thinking_levels
        .first()
        .copied()
        .unwrap_or(ThinkingLevel::Off);
    let level = parsed.level.or(fallback_level).unwrap_or(default_level);
    if model.reasoning && level != ThinkingLevel::Off && !thinking_levels.contains(&level) {
        return Err(CoreError::Config(format!(
            "模型 {selector} 不支持思考等级 {}；可在模型的“可选思考等级”中调整",
            level.as_str()
        )));
    }
    Ok(provider.resolve_model(parsed.provider_id, parsed.model_id, model, level))
}

pub(super) fn all_models(providers: &BTreeMap<String, ResolvedProvider>) -> Vec<ResolvedModel> {
    let mut result = Vec::new();
    for (provider_id, provider) in providers {
        for (model_id, model) in &provider.models {
            let level = effective_thinking_levels(model)
                .first()
                .copied()
                .unwrap_or(ThinkingLevel::Off);
            result.push(provider.resolve_model(provider_id, model_id, model, level));
        }
    }
    result
}

fn shell_command(command: &str) -> Result<Command, CoreError> {
    let shell = crate::shell::detect()
        .ok_or_else(|| CoreError::Config("没有可用的 shell，无法解析以 ! 开头的密钥命令".into()))?;
    let (program, args) = crate::shell::command_line(shell, command);
    let mut process = Command::new(program);
    process.args(args);
    Ok(process)
}

/// 解析密钥描述：空 → None；`env:NAME` → 环境变量；`!command` → 命令输出；否则字面量。
///
/// `label` 用于错误信息（如 `providers.p.api_key`、`web_search.api_key`），从不携带密钥值。
pub(crate) fn resolve_secret(label: &str, spec: &str) -> Result<Option<String>, CoreError> {
    if spec.is_empty() {
        return Ok(None);
    }
    let value = if let Some(name) = spec.strip_prefix("env:") {
        std::env::var(name)
            .map_err(|_| CoreError::Config(format!("{label} 环境变量 {name} 不存在")))?
    } else if let Some(command) = spec.strip_prefix('!') {
        let output = shell_command(command)?
            .output()
            .map_err(|error| CoreError::Config(format!("{label} 命令无法启动: {error}")))?;
        if !output.status.success() {
            return Err(CoreError::Config(format!(
                "{label} 命令以状态 {} 失败",
                output.status
            )));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| CoreError::Config(format!("{label} 命令输出不是 UTF-8: {error}")))?
    } else {
        spec.to_owned()
    };
    let value = value.trim().to_owned();
    if value.is_empty() {
        return Err(CoreError::Config(format!("{label} 解析为空")));
    }
    Ok(Some(value))
}

fn resolve_api_key(provider: &str, spec: &str) -> Result<Option<String>, CoreError> {
    resolve_secret(&format!("providers.{provider}.api_key"), spec)
}

pub(super) fn validate_roles(
    roles: &ModelRoles,
    providers: &BTreeMap<String, ResolvedProvider>,
) -> Result<(), CoreError> {
    // 首次启动允许空目录进入设置页；真正创建会话时必须解析出主模型。
    if roles.default.is_empty()
        && providers
            .values()
            .all(|provider| provider.models.is_empty())
    {
        return Ok(());
    }
    for (role, selector, allows_none) in [
        ("主模型", roles.default.as_str(), false),
        ("探索子代理模型", roles.explore.as_str(), false),
        ("计划模型", roles.plan.as_str(), true),
    ] {
        if selector == ROLE_NONE {
            if allows_none {
                continue;
            }
            return Err(CoreError::Config(format!("{role}不能设置为 none")));
        }
        let selector = if selector == ROLE_INHERIT && role != "主模型" {
            roles.default.as_str()
        } else {
            selector
        };
        resolve_one(providers, selector, None)
            .map_err(|error| CoreError::Config(format!("{role}无法解析：{}", error.detail())))?;
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

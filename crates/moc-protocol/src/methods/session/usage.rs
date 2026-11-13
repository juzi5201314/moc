use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// `session/usage` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageParams {
    /// 目标会话。
    pub session_id: String,
}

/// 用量来源：会话自身、Fusion sidekick 子会话，或普通子代理（含其后代）。
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    JsonSchema,
)]
#[serde(rename_all = "snake_case")]
pub enum UsageSource {
    #[default]
    Session,
    Sidekick,
    Subagent,
}

impl UsageSource {
    /// serde `skip_serializing_if` 用的判定：自身用量不写字段保持旧报文形状。
    pub fn is_session(&self) -> bool {
        matches!(self, UsageSource::Session)
    }
}

/// 一个模型在当前会话中的累计用量。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ModelUsageSummary {
    /// 模型选择器。
    pub model: String,
    /// 这行用量属于会话自身还是后代会话；自身用量序列化时省略。
    #[serde(default, skip_serializing_if = "UsageSource::is_session")]
    pub source: UsageSource,
    /// 模型请求次数。
    pub calls: u64,
    /// 输入 token 总数，包含缓存命中与缓存写入部分。
    pub input_tokens: u64,
    /// 输出 token 总数。
    pub output_tokens: u64,
    /// 缓存命中的输入 token；上游不提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cached_input_tokens: Option<u64>,
    /// 缓存写入的输入 token；上游不提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_creation_input_tokens: Option<u64>,
    /// 思考 token；上游不提供时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
    /// 每次模型请求从开始到结束的耗时之和，单位为毫秒。
    pub active_ms: u64,
    /// 首 token 等待（TTFT）总时长，单位为毫秒；只统计上报了首 token 的请求。
    #[serde(default)]
    pub ttft_ms: u64,
    /// 上报了首 token 的请求数；平均 TTFT 用它做分母，失败请求与非流式请求不计入。
    #[serde(default)]
    pub ttft_calls: u64,
    /// 首 token 之后的生成总时长，单位为毫秒；只统计上报了首 token 的请求。
    #[serde(default)]
    pub gen_ms: u64,
}

/// 当前会话的模型用量与全部模型总和
/// （会话自身 + 未被回退孤立的全部后代会话）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionUsageResult {
    /// 按来源与模型选择器排序的明细（会话自身、sidekick、普通子代理）。
    pub models: Vec<ModelUsageSummary>,
    /// 全部模型的总和；`model` 固定为 `total`。
    pub total: ModelUsageSummary,
    /// 工具执行耗时之和，单位为毫秒；手动 bash 与嵌套子调用不计入。
    #[serde(default)]
    pub tool_ms: u64,
}

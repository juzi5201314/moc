//! `config/get`：前端开屏所需的只读配置。
//!
//! 配置本身只读：前端把可选集合拉下来，切换会话模型走
//! [`super::session::SessionSetModel`]。`config/get` 是唯一在 `initialize` 之后、
//! 任何会话之前就允许调用的方法。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// `config/get`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigGet;

impl Method for ConfigGet {
    const NAME: &'static str = "config/get";
    type Params = ConfigGetParams;
    type Result = ConfigGetResult;
}

impl ClientRequest for ConfigGet {}

/// `config/get` 的参数：空对象。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetParams {}

/// `config/get` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetResult {
    /// 可用模型。
    pub models: Vec<ModelInfo>,
    /// 默认模型 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 可用 agent。
    pub agents: Vec<AgentProfile>,
    /// 默认 agent ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// core 的当前工作目录（`session/new` 不带 `cwd` 时用它）。
    pub cwd: String,
    /// `[models.fusion]` 配置的组合；两侧都配置时才有值。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fusion: Option<FusionPair>,
}

/// Fusion 组合的两侧模型，均为规范 `供应商/模型:等级` selector。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FusionPair {
    /// 主导模型：规划、调查、评审与对话。
    pub lead: String,
    /// 执行模型：实现、构建与测试。
    pub sidekick: String,
}

/// 模型接受的输入模态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInput {
    /// 文本消息。
    Text,
    /// 图片输入。
    Image,
}

/// 一个可用模型。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    /// 模型 ID，`session/new` 的 `model` 用它。
    pub id: String,
    /// 展示名。
    pub display_name: String,
    /// 模型允许选择的思考等级；`None` 表示前端使用默认集合。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_levels: Option<Vec<String>>,
    /// 上下文窗口 token 数；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 单次输出上限；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
    /// 每百万 token 定价（美元）；`None` 表示未配置，前端应隐藏费用段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<ModelPricing>,
    /// 模型接受的输入模态；空表示未声明（按纯文本处理）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<ModelInput>,
    /// Responses 原生服务端压缩是否启用。
    #[serde(default)]
    pub remote_compaction: bool,
}

/// 每百万 token 定价（美元）。
///
/// `input` / `output` 必填；缓存两项选填，未配置时前端可按全价估算或跳过。
/// 注意 moc-llm 的 `Usage::input_tokens` 已折入缓存命中部分，费用应按
/// `input - cached` 计非命中输入、`cached` 按 `cache_read` 单价计。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelPricing {
    /// 非缓存输入，每百万 token。
    pub input: f64,
    /// 输出，每百万 token。
    pub output: f64,
    /// 缓存命中输入，每百万 token；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// 缓存写入输入，每百万 token；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

/// 一个可用 agent。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentProfile {
    /// agent ID。
    pub id: String,
    /// 展示名。
    pub display_name: String,
    /// 描述；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes() {
        assert_eq!(<ConfigGet as Method>::NAME, "config/get");
        let result = ConfigGetResult {
            models: vec![ModelInfo {
                id: "deepseek-flash".into(),
                display_name: "DeepSeek Flash".into(),
                thinking_levels: None,
                context_window: Some(128_000),
                max_output_tokens: None,
                pricing: Some(ModelPricing {
                    input: 0.27,
                    output: 1.1,
                    cache_read: Some(0.07),
                    cache_write: None,
                }),
                inputs: vec![ModelInput::Text, ModelInput::Image],
                remote_compaction: true,
            }],
            default_model: Some("deepseek-flash".into()),
            agents: vec![AgentProfile {
                id: "build".into(),
                display_name: "Build".into(),
                description: None,
            }],
            default_agent: Some("build".into()),
            cwd: "/home/soeur/project/moc".into(),
            fusion: Some(FusionPair {
                lead: "deepseek-flash:max".into(),
                sidekick: "deepseek-flash:off".into(),
            }),
        };
        assert_eq!(
            serde_json::to_value(&result).expect("可序列化"),
            json!({
                "models": [{ "id": "deepseek-flash", "display_name": "DeepSeek Flash", "context_window": 128000, "pricing": { "input": 0.27, "output": 1.1, "cache_read": 0.07 }, "inputs": ["text", "image"], "remote_compaction": true }],
                "default_model": "deepseek-flash",
                "agents": [{ "id": "build", "display_name": "Build" }],
                "default_agent": "build",
                "cwd": "/home/soeur/project/moc",
                "fusion": {"lead": "deepseek-flash:max", "sidekick": "deepseek-flash:off"}
            })
        );
        let round_trip: ConfigGetResult =
            serde_json::from_value(serde_json::to_value(&result).expect("可序列化"))
                .expect("可反序列化");
        assert_eq!(round_trip, result);
        assert_eq!(
            serde_json::to_value(ConfigGetParams {}).expect("可序列化"),
            json!({})
        );
    }
}

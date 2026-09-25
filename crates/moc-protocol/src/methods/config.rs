//! `config/get`：前端开屏所需的只读配置。
//!
//! 本轮不含配置写入与模型切换（`session/set_model`）：前端先把可选集合拉下来，
//! 具体切换由 core 自己的命令面决定。`config/get` 是唯一在 `initialize` 之后、
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigGetResult {
    /// 可用模型。
    pub models: Vec<ModelInfo>,
    /// 默认模型 ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
    /// 可用 agent。
    pub agents: Vec<AgentInfo>,
    /// 默认 agent ID。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent: Option<String>,
    /// core 的当前工作目录（`session/new` 不带 `cwd` 时用它）。
    pub cwd: String,
}

/// 一个可用模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelInfo {
    /// 模型 ID，`session/new` 的 `model` 用它。
    pub id: String,
    /// 展示名。
    pub display_name: String,
    /// 上下文窗口 token 数；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_window: Option<u64>,
    /// 单次输出上限；`None` 表示未配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u32>,
}

/// 一个可用 agent。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AgentInfo {
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
                context_window: Some(128_000),
                max_output_tokens: None,
            }],
            default_model: Some("deepseek-flash".into()),
            agents: vec![AgentInfo {
                id: "build".into(),
                display_name: "Build".into(),
                description: None,
            }],
            default_agent: Some("build".into()),
            cwd: "/home/soeur/project/moc".into(),
        };
        assert_eq!(
            serde_json::to_value(&result).expect("可序列化"),
            json!({
                "models": [{ "id": "deepseek-flash", "display_name": "DeepSeek Flash", "context_window": 128000 }],
                "default_model": "deepseek-flash",
                "agents": [{ "id": "build", "display_name": "Build" }],
                "default_agent": "build",
                "cwd": "/home/soeur/project/moc"
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

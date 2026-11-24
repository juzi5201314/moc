use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// `session/context` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextParams {
    /// 目标会话。
    pub session_id: String,
}

/// 一项上下文组成部分。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageSection {
    /// 展示名称，直接显示在上下文占用窗的分块列表里。
    pub name: String,
    /// 估算 token 数。
    pub tokens: u64,
}

/// 当前会话上下文估算结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextUsageResult {
    /// 模型上下文窗口上限。
    pub context_window: u64,
    /// 所有组成部分合计。
    pub total_tokens: u64,
    /// 按来源分类：`system`、`AGENTS.md (p+g)`、`skills`、`tools`、`messages`；
    /// 插件贡献提示词时额外显示 `plugins`。运行时提醒与工具结果归入 `messages`。
    pub sections: Vec<ContextUsageSection>,
}

/// 打开当前上下文；指定 view_id 时只读取已冻结视图的后续目录页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextContentParams {
    pub session_id: String,
    #[serde(default)]
    pub view_id: Option<String>,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// 冻结上下文中的分类或来源条目；目录与隐藏项不携带正文。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextContentSection {
    pub id: String,
    pub name: String,
    /// `category`、`markdown` 或 `json`；不暴露编码协议的字段树。
    pub kind: String,
    pub source: Option<String>,
    pub tokens: u64,
    pub children: u64,
    /// 隐藏原因；有值时不可读取正文或子节点，仍计入上下文用量。
    pub hidden: Option<String>,
    /// 消息条目编码进请求后的角色：`system`、`developer`、`user`、`assistant` 或 `tool`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    /// 消息条目的来源分类，如 `user_input`、`assistant_output`、`tool_result`、`runtime_injection`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextContentResult {
    pub view_id: String,
    pub context_window: u64,
    pub total_tokens: u64,
    pub model: String,
    pub sections: Vec<ContextContentSection>,
    pub next_cursor: Option<u64>,
    /// 当前状态无法编码成模型请求时的原因。
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextReadParams {
    pub session_id: String,
    pub view_id: String,
    pub node_id: String,
    #[serde(default)]
    pub cursor: Option<u64>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ContextReadResult {
    pub view_id: String,
    pub node_id: String,
    pub children: Vec<ContextContentSection>,
    pub text: Option<String>,
    pub next_cursor: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SessionContextCloseParams {
    pub session_id: String,
    pub view_id: String,
}

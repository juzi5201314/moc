use super::super::diff::ToolDiff;
use super::super::job::JobInfo;
use super::GoalRoundMeta;
use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;

/// PTC 中由 JavaScript 发起的嵌套工具调用。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptChildCall {
    pub id: String,
    pub parent_tool_call_id: String,
    pub name: String,
    pub arguments: String,
    pub result: String,
    pub status: ToolStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// edit/write 的结构化 diff；旧会话日志没有此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ToolDiff>,
}

/// 一条运行时提醒里的一条目。
///
/// 只记摘要不记正文：正文就是消息本身，摘要只用来判断「这个条目是否已经在历史里」。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReminderItem {
    /// 稳定条目键，例如 `sandbox`、`mode`、`mcp-index`。
    pub key: String,
    /// 该条目正文的摘要；正文变了才重新注入。
    pub digest: String,
}

/// 挂在一条 `Custom` 消息上的提醒标记。
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReminderStamp {
    /// 本条消息合并了哪些条目。
    pub items: Vec<ReminderItem>,
}

/// 一段 assistant 正文从首个到末个流式增量的墙钟时间窗。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TextSpan {
    /// 首个增量到达的 UNIX 毫秒时间戳。
    pub start_ms: u64,
    /// 末个增量到达的 UNIX 毫秒时间戳。
    pub end_ms: u64,
}

/// 一次模型响应的展示元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ResponseMeta {
    /// 生成该响应的模型选择器（`provider/model[:level]`）。
    pub model: String,
    /// 各正文段的流式时间窗，与 `message` 中的 Text 部分按序一一对应。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub text_spans: Vec<TextSpan>,
    /// 本次响应思考阶段总耗时（毫秒）；无思考内容或旧会话日志为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_duration_ms: Option<u64>,
    /// 本次响应上游报告的思考 token 数（存在思考内容且非零时记录）；无思考或旧会话日志为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_tokens: Option<u64>,
}

/// 一条 transcript 条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TranscriptEntry {
    /// 条目 ID，由 core 生成。
    pub id: String,
    /// 记录时间的 UNIX 毫秒时间戳。
    pub at_ms: u64,
    /// 内容；复用 `moc-llm` 的消息类型。
    pub message: crate::Message,
    /// 该消息对应的工具执行状态；`None` 表示与工具无关。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolExecution>,
    /// 用户手动执行的命令；与 agent 工具调用区分，并控制模型可见性。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bash: Option<super::super::bash::BashCommand>,
    /// 代理间消息的来源元信息；`None` 表示本地输入。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent: Option<super::super::agent::AgentMessageMeta>,
    /// 子代理交付的完整结果；仅供展示，不重复进入模型上下文。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_result: Option<super::super::agent::AgentResultMeta>,
    /// goal 续跑轮的来源元信息；`None` 表示非续跑消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub goal_round: Option<GoalRoundMeta>,
    /// 运行时提醒的来源标记；`None` 表示普通对话消息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reminder: Option<ReminderStamp>,
    /// 已送达模型的任务通知的任务快照；`None` 表示非任务通知消息。
    ///
    /// 消息本身是 `Role::Custom` 的送达原文，参与模型上下文；这个字段只给
    /// 前端把投递渲染成卡片用。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub job_notice: Option<JobInfo>,
    /// 模型响应的展示元信息；其它消息与旧会话日志没有此字段。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response: Option<ResponseMeta>,
}

impl TranscriptEntry {
    pub fn in_context(&self) -> bool {
        self.agent_result.is_none()
            && !self
                .bash
                .as_ref()
                .is_some_and(|bash| bash.exclude_from_context)
    }

    /// 模型视图：`!!` 不产生消息，`!` 带上命令来源，代理消息包上来源标签，
    /// 其余消息保持原样。
    pub fn context_message(&self) -> Option<crate::Message> {
        if !self.in_context() {
            return None;
        }
        if let Some(agent) = &self.agent {
            return Some(crate::Message::user_text(format!(
                "<agent-message from=\"{}\" id=\"{}\">\n{}\n</agent-message>",
                agent.from_title,
                agent.from_id,
                self.message.text()
            )));
        }
        Some(match &self.bash {
            Some(bash) => bash.context_message(&self.message),
            None => self.message.clone(),
        })
    }
}

/// 一次工具执行的终态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ToolExecution {
    /// 对应的 [`crate::ToolCall::id`]。
    pub tool_call_id: String,
    /// 终态。
    pub status: ToolStatus,
    /// 执行耗时毫秒；`None` 表示不计时（拒绝、取消等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    /// 工具产出的结构化 diff；随 transcript 持久化，历史回放据此显示改动。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<ToolDiff>,
}

/// 工具执行终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// 正常结束。
    Completed,
    /// 执行失败（工具报错、超时等）。
    Error,
    /// 未获授权。
    Denied,
    /// 提示被取消。
    Cancelled,
}

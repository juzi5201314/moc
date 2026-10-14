//! 提示方法：`session/prompt` 与 `session/cancel`。
//!
//! # 概念
//!
//! | 单位 | 边界 | 业内其他名字 |
//! | --- | --- | --- |
//! | **提示**（prompt，即一次用户消息） | 一次用户消息 → 代理主动停止，即一次 `session/prompt` 调用 | OpenAI Agents SDK `Task`/`Runner.run`；Assistants `Run`；Codex v1 规范文档 `Task` |
//! | **轮**（turn） | 一次提示内的一次模型请求 + 该响应引发的工具调用批次 | Claude Agent SDK 明文叫 `turn`（*"Each full cycle is one turn"*、`max_turns` = "Maximum tool-use round trips"）；OpenAI Agents SDK `TurnSpanData`/`max_turns`；Codex v1 规范文档 `Turn`；Assistants `RunStep`；LangChain `iteration` |
//! | **模型请求** | 一次 SSE 流（`moc-llm` 的一次调用，可能因重试/压缩重发） | 上游 API 的一次调用 |
//!
//! 内层选 `turn` 是跟随 Claude Agent SDK 与 OpenAI Agents SDK 的用法。但 `turn` 在业内
//! 是**反义**的，读外部文档时不要直接对号入座：Codex 现网（`TurnStarted`/`TurnComplete`
//! 事件，wire tag 出于兼容串行化成 `task_*`）与「多轮对话」的通用语义把它用在外层，
//! 外层改称 `Task`/`Run`。moc 的外层因此另名 `prompt`——与 `session/prompt` 同词根。
//!
//! 一次提示可包含任意多轮，每一轮都会重新请求模型并把工具结果回灌，直到模型不再发起
//! 工具调用（或控制类工具要求收尾）为止——这就是「代理主动停止」。`length` 截断不结束
//! 提示，而是在同一次提示内自动续写（见 `max_length_continues`）。因此**提示**是模式、
//! 取消、只读约束、计时与上下文压缩的作用域单位：`PromptStarted` / `PromptFinished`
//! 各只发一次，[`PromptResult::finish_reason`] 是提示级终态（可能取 `Other("max_turns")`、
//! `Other("compacted")` 这类非上游取值）。
//!
//! `session/prompt` 是**长请求**：它挂到本次提示结束才返回 [`PromptResult`]，过程中的
//! 全部增量通过 `session/update` 通知先行推送。因此前端渲染策略是
//! 「先按通知流式渲染，收到响应后用它替换成权威版本」——[`PromptResult::message`]
//! 与 `session/update` 里 `prompt_finished.message` 逐字段相同，两者可互换校验。
//!
//! `session/cancel` 由协议层自答（查提示注册表并 trip 取消令牌，不进入 core），
//! 因此即使出站队列拥塞、core 正忙于模型请求，取消也能即时生效。它是幂等的：
//! 没有进行中的提示时返回 `cancelled = false` 且整体成功。

use serde::{Deserialize, Serialize};

use super::session::SessionMode;
use crate::method::{ClientRequest, Method};
use crate::{FinishReason, ImageDetail, ImageSource, Message, Usage};

/// `session/prompt`：跑一次提示（一次用户消息 → 代理停止）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPrompt;

/// `session/cancel`：取消该会话进行中的提示。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCancel;

/// `session/prompt` 的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptParams {
    /// 目标会话。
    pub session_id: String,
    /// 本条用户消息的输入；多模态按顺序排列。
    pub input: Vec<InputPart>,
    /// 仅本次提示生效的工作模式覆盖；`None` 沿用会话当前模式。
    ///
    /// 供 `/ask` 这类「只让这一条消息进入某模式」的临时意图使用：core 以它替换
    /// 会话模式跑完整个 `session/prompt`（含其全部轮：多次模型请求与各组工具
    /// 调用）后不予持久化，也不推送 `ModeChanged`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<SessionMode>,
    /// 本条输入是代理间消息：携带来源元信息，落库为 `Entry::Message.agent`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_message: Option<super::agent::AgentMessageMeta>,
}

/// 一段用户输入。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputPart {
    /// 文本。
    Text {
        /// 正文。
        text: String,
    },
    /// 图片；core 按自己的策略投影成 `moc-llm` 的 [`crate::ContentPart::Image`]。
    Image {
        /// 图片来源。
        source: ImageSource,
        /// 精度提示；`None` 交给上游默认。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<ImageDetail>,
    },
}

/// 一次提示的终态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptResult {
    /// 会话 ID。
    pub session_id: String,
    /// 提示 ID；由 core 生成，对协议层不透明。
    pub prompt_id: String,
    /// 权威 assistant 消息；与 `prompt_finished.message` 相同。
    pub message: Message,
    /// 结束原因。
    pub finish_reason: FinishReason,
    /// token 用量。
    pub usage: Usage,
}

/// `session/cancel` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCancelParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/cancel` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCancelResult {
    /// 是否真的取消了进行中的提示。
    pub cancelled: bool,
}

impl Method for SessionPrompt {
    const NAME: &'static str = "session/prompt";
    type Params = PromptParams;
    type Result = PromptResult;
}

impl ClientRequest for SessionPrompt {}

impl Method for SessionCancel {
    const NAME: &'static str = "session/cancel";
    type Params = SessionCancelParams;
    type Result = SessionCancelResult;
}

impl ClientRequest for SessionCancel {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentPart, Role};
    use serde_json::json;

    #[test]
    fn prompt_params_shape() {
        let params = PromptParams {
            session_id: "ses_7f3a".into(),
            input: vec![InputPart::Text {
                text: "看一下 README".into(),
            }],
            mode: None,
            agent_message: None,
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "input": [{ "type": "text", "text": "看一下 README" }]
            })
        );
        // 本次提示的模式覆盖只在显式给出时上线；缺省时保持旧报文形状。
        let ask = PromptParams {
            mode: Some(SessionMode::Ask),
            ..params
        };
        assert_eq!(
            serde_json::to_value(&ask).expect("可序列化")["mode"],
            json!("ask")
        );
        assert_eq!(
            serde_json::from_value::<PromptParams>(json!({
                "session_id": "ses_7f3a",
                "input": [{ "type": "text", "text": "看一下 README" }]
            }))
            .expect("可反序列化")
            .mode,
            None
        );
        let image = InputPart::Image {
            source: ImageSource::Url("https://example.com/a.png".into()),
            detail: Some(ImageDetail::High),
        };
        assert_eq!(
            serde_json::to_value(&image).expect("可序列化"),
            json!({
                "type": "image",
                "source": { "Url": "https://example.com/a.png" },
                "detail": "high"
            })
        );
        // 未知 type 必须报错（前端忽略未知更新的前提是「不认识就整条丢弃」）
        assert!(serde_json::from_value::<InputPart>(json!({ "type": "video" })).is_err());
        assert!(serde_json::from_value::<PromptParams>(json!({ "session_id": "s" })).is_err());
    }

    #[test]
    fn prompt_result_shape() {
        let mut usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            ..Usage::default()
        };
        usage.total_tokens = None;
        let result = PromptResult {
            session_id: "ses_7f3a".into(),
            prompt_id: "prompt_1".into(),
            message: Message {
                role: Role::Assistant,
                content: vec![ContentPart::Text("好的".into())],
            },
            finish_reason: FinishReason::Stop,
            usage,
        };
        assert_eq!(
            serde_json::to_value(&result).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "prompt_id": "prompt_1",
                "message": { "role": "Assistant", "content": [{ "Text": "好的" }] },
                "finish_reason": "Stop",
                "usage": {
                    "input_tokens": 12,
                    "output_tokens": 3,
                    "total_tokens": null,
                    "cached_input_tokens": null,
                    "cache_creation_input_tokens": null,
                    "reasoning_tokens": null
                }
            })
        );
        let round_trip: PromptResult =
            serde_json::from_value(serde_json::to_value(&result).expect("可序列化"))
                .expect("可反序列化");
        assert_eq!(round_trip, result);
    }
}

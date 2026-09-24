//! 轮次方法：`session/prompt` 与 `session/cancel`。
//!
//! `session/prompt` 是**长请求**：它挂到本轮结束才返回 [`TurnResult`]，过程中的
//! 全部增量通过 `session/update` 通知先行推送。因此前端渲染策略是
//! 「先按通知流式渲染，收到响应后用它替换成权威版本」——[`TurnResult::message`]
//! 与 `session/update` 里 `turn_finished.message` 逐字段相同，两者可互换校验。
//!
//! `session/cancel` 由协议层自答（查轮次注册表并 trip 取消令牌，不进入 core），
//! 因此即使出站队列拥塞、core 正忙于模型请求，取消也能即时生效。它是幂等的：
//! 没有在飞轮次时返回 `cancelled = false` 且整体成功。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};
use crate::{FinishReason, ImageDetail, ImageSource, Message, Usage};

/// `session/prompt`：跑一轮 agent 工作。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionPrompt;

/// `session/cancel`：取消该会话在飞的轮次。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCancel;

/// `session/prompt` 的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PromptParams {
    /// 目标会话。
    pub session_id: String,
    /// 本轮用户输入；多模态按顺序排列。
    pub input: Vec<InputPart>,
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

/// 一轮 agent 工作的终态。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnResult {
    /// 会话 ID。
    pub session_id: String,
    /// 轮次 ID；由 core 生成，对协议层不透明。
    pub turn_id: String,
    /// 权威 assistant 消息；与 `turn_finished.message` 相同。
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
    /// 是否真的取消了在飞轮次。
    pub cancelled: bool,
}

impl Method for SessionPrompt {
    const NAME: &'static str = "session/prompt";
    type Params = PromptParams;
    type Result = TurnResult;
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
    fn method_names() {
        assert_eq!(<SessionPrompt as Method>::NAME, "session/prompt");
        assert_eq!(<SessionCancel as Method>::NAME, "session/cancel");
    }

    #[test]
    fn prompt_params_shape() {
        let params = PromptParams {
            session_id: "ses_7f3a".into(),
            input: vec![InputPart::Text {
                text: "看一下 README".into(),
            }],
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "input": [{ "type": "text", "text": "看一下 README" }]
            })
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
    fn turn_result_shape() {
        let mut usage = Usage {
            input_tokens: 12,
            output_tokens: 3,
            ..Usage::default()
        };
        usage.total_tokens = None;
        let result = TurnResult {
            session_id: "ses_7f3a".into(),
            turn_id: "turn_1".into(),
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
                "turn_id": "turn_1",
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
        let round_trip: TurnResult = serde_json::from_value(serde_json::to_value(&result).expect("可序列化"))
            .expect("可反序列化");
        assert_eq!(round_trip, result);
    }
}

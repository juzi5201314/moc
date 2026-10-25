//! `session/btw`：只读旁路问答。
//!
//! 与 [`super::prompt::SessionPrompt`] 的区别是**作用域**：旁路不建立会话提示、
//! 不抢主提示锁、不写 transcript，也不执行工具。core 只复制当前模型视图，追加
//! 最近的有效旁路问答，发起一次生成。因此：
//!
//! - 主任务运行时仍可提问，两者互不取消、互不排队；
//! - 旁路正文永不进入主上下文、`/usage` 与磁盘历史；
//! - `session/btw_cancel` 按 `request_id` 定向取消，不影响主提示；
//! - 旁路历史由前端持有并以文本问答回传，core 只校验上限，不建立旁路存储。
//!
//! 通知 [`BtwUpdateMethod`] 与主 `session/update` 分开，前端可以独立渲染，
//! 不会把旁路增量喂给主转写。

use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method, ServerNotification};
use crate::{FinishReason, Usage};

/// 旁路最多回放的历史轮数；前端超出后从旧到新淘汰。
pub const MAX_BTW_HISTORY_TURNS: usize = 20;

/// `session/btw`：在只读快照上问一次旁路问题。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionBtw;

/// `session/btw_cancel`：定向取消一条在飞的旁路请求。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionBtwCancel;

/// `session/btw_update`：旁路生成过程增量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BtwUpdateMethod;

/// 一轮已完成的旁路问答，由前端回放。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwExchange {
    /// 用户问题。
    pub question: String,
    /// 当时的完整回答。
    pub answer: String,
}

/// `session/btw` 的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwParams {
    /// 目标会话；旁路读取它的当前已提交上下文。
    pub session_id: String,
    /// 前端生成的请求身份；通知、结果与取消都用它匹配。
    pub request_id: u64,
    /// 本次问题；只接受纯文本，不展开附件或资源 URI。
    pub question: String,
    /// 最近的有效旁路问答，最多 [`MAX_BTW_HISTORY_TURNS`] 轮。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub history: Vec<BtwExchange>,
}

/// `session/btw` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwResult {
    /// 会话 ID。
    pub session_id: String,
    /// 与请求相同的身份。
    pub request_id: u64,
    /// 本次实际使用的模型选择器。
    pub model: String,
    /// 权威回答正文；被取消时是已收到的部分内容。
    pub text: String,
    /// 结束原因；`Cancelled` 表示用户定向取消。
    pub finish_reason: FinishReason,
    /// 本次请求的 token 用量；只供前端旁路面板展示，不写入会话统计。
    pub usage: Usage,
    /// 实际回放的历史轮数；预算不足时会少于请求里的轮数。
    pub history_turns_used: usize,
}

/// `session/btw_cancel` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwCancelParams {
    /// 目标会话。
    pub session_id: String,
    /// 要取消的旁路请求；不匹配时是成功的空操作。
    pub request_id: u64,
}

/// `session/btw_cancel` 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwCancelResult {
    /// 是否真的停下了该请求；请求已结束或身份不匹配时为 `false`。
    pub cancelled: bool,
}

/// 旁路生成的一步。
///
/// 与 [`super::update::SessionUpdate`] 一样是会增长的枚举：前端遇到不认识的
/// `type` 必须整条丢弃，不能报错或断开连接。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum BtwUpdate {
    /// 协议层已登记请求，可以安全地发定向取消。
    Started,
    /// 正在等待供应商并发名额。
    Waiting,
    /// 已拿到流式响应，开始生成。
    Generating,
    /// 正文增量。
    TextDelta { text: String },
    /// 思考增量；只供旁路面板展开查看。
    ReasoningDelta { text: String },
}

/// `session/btw_update` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtwUpdateParams {
    /// 会话 ID。
    pub session_id: String,
    /// 与请求相同的身份；旧请求的迟到更新据此丢弃。
    pub request_id: u64,
    /// 本次增量。
    pub update: BtwUpdate,
}

impl Method for SessionBtw {
    const NAME: &'static str = "session/btw";
    type Params = BtwParams;
    type Result = BtwResult;
}

impl ClientRequest for SessionBtw {}

impl Method for SessionBtwCancel {
    const NAME: &'static str = "session/btw_cancel";
    type Params = BtwCancelParams;
    type Result = BtwCancelResult;
}

impl ClientRequest for SessionBtwCancel {}

impl Method for BtwUpdateMethod {
    const NAME: &'static str = "session/btw_update";
    type Params = BtwUpdateParams;
    type Result = super::Empty;
}

impl ServerNotification for BtwUpdateMethod {}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn btw_params_omit_empty_history_and_reject_unknown_fields() {
        let params = BtwParams {
            session_id: "ses_7f3a".into(),
            request_id: 3,
            question: "为什么用软压缩？".into(),
            history: Vec::new(),
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "request_id": 3,
                "question": "为什么用软压缩？"
            })
        );
        let with_history: BtwParams = serde_json::from_value(json!({
            "session_id": "ses_7f3a",
            "request_id": 3,
            "question": "继续",
            "history": [{ "question": "上一问", "answer": "上一答" }]
        }))
        .expect("可反序列化");
        assert_eq!(with_history.history.len(), 1);
        assert!(
            serde_json::from_value::<BtwParams>(json!({
                "session_id": "ses_7f3a",
                "request_id": 3,
                "question": "问",
                "extra": 1
            }))
            .is_err()
        );
    }

    #[test]
    fn btw_update_is_tagged_and_result_round_trips() {
        assert_eq!(
            serde_json::to_value(BtwUpdate::TextDelta { text: "好".into() }).unwrap(),
            json!({ "type": "text_delta", "text": "好" })
        );
        let result = BtwResult {
            session_id: "ses_7f3a".into(),
            request_id: 3,
            model: "test/model".into(),
            text: "好的".into(),
            finish_reason: FinishReason::Stop,
            usage: Usage::default(),
            history_turns_used: 0,
        };
        let value = serde_json::to_value(&result).unwrap();
        assert_eq!(value["finish_reason"], json!("Stop"));
        assert_eq!(serde_json::from_value::<BtwResult>(value).unwrap(), result);
        assert_eq!(
            serde_json::from_value::<BtwResult>(json!({
                "session_id": "s",
                "request_id": 1,
                "model": "m",
                "text": "",
                "finish_reason": "Cancelled",
                "usage": Usage::default(),
                "history_turns_used": 0
            }))
            .unwrap()
            .text,
            String::new()
        );
    }
}

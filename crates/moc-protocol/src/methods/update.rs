//! `session/update` 通知：core 单向推送轮次过程。
//!
//! 通知**不透传** `moc_llm::StreamEvent`：那是供应商细节（内容块序号、签名、
//! 上游响应 ID），前端既无法渲染也不需要。core 负责把它投影成 UI 友好的
//! [`AssistantDelta`]，其中工具调用增量按 `tool_call_id` 归组，前端可以直接
//! 「按 ID 追加参数文本」而不必维护序号映射。
//!
//! # `StreamEvent` → `AssistantDelta` 投影表
//!
//! | `moc_llm::StreamEvent` | 动作 |
//! |---|---|
//! | `Start { id, model }` | 丢弃（`model` 已由 `turn_started` 给出，上游 `id` 前端不需要） |
//! | `TextDelta { index, text }` | `Text { delta: text }` |
//! | `ReasoningDelta { index, text }` | `Reasoning { delta: text }` |
//! | `ReasoningSignature { index, signature }` | 丢弃（不透明，前端无法渲染；完整内容随 `turn_finished.message` 给出） |
//! | `ReasoningItem { index, item }` | 丢弃（同上） |
//! | `ToolCallStart { index, id, name }` | `ToolCallStarted { tool_call_id: id, name }`，并记录 `index → id` |
//! | `ToolCallArgumentsDelta { index, json }` | `ToolCallArgs { tool_call_id: index 映射到的 id, delta: json }`；查不到映射说明对端违约，丢弃并 `warn` |
//! | `Usage(usage)` | `Usage { usage }` |
//!
//! `index` 是同一响应内内容块的位置，只在一次流内有效，因此映射表随轮次销毁。
//! 若上游先发参数后发名字（无法建立映射），core 应丢弃该次参数增量并 `warn`，
//! 而不是改协议形状——完整参数随 `turn_finished.message` 一并给出。
//!
//! # 兼容规则
//!
//! [`SessionUpdate`] 与 [`AssistantDelta`] 是会增长的枚举：core 可以新增变体
//! （不升 major），前端遇到不认识的 `type` 必须**整条丢弃**而不是报错或断开连接。

use serde::{Deserialize, Serialize};

use super::session::{SessionLockInfo, ToolStatus};
use crate::method::{Method, ServerNotification};
use crate::{FinishReason, Message, ToolResult, Usage};

/// `session/update` 的方法标记。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUpdateMethod;

impl Method for SessionUpdateMethod {
    const NAME: &'static str = "session/update";
    type Params = SessionUpdateParams;
    type Result = crate::methods::Empty;
}

// 通知不返回结果；`Result` 只是 `Method` 的形状要求。
impl ServerNotification for SessionUpdateMethod {}

/// `session/update` 的参数。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUpdateParams {
    /// 会话 ID。
    pub session_id: String,
    /// 本次更新。
    pub update: SessionUpdate,
}

/// 一条会话更新。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionUpdate {
    /// 轮次开始。
    TurnStarted {
        /// 轮次 ID。
        turn_id: String,
        /// 实际使用的模型 ID。
        model: String,
        /// 开始时间的 UNIX 毫秒时间戳。
        at_ms: u64,
    },
    /// assistant 增量。
    AssistantDelta {
        /// 轮次 ID。
        turn_id: String,
        /// 增量内容。
        delta: AssistantDelta,
    },
    /// 一次工具调用开始执行。
    ToolStarted {
        /// 轮次 ID。
        turn_id: String,
        /// 调用 ID，与后续 [`SessionUpdate::ToolFinished`] 配对。
        tool_call_id: String,
        /// 工具名。
        name: String,
    },
    /// 一次工具调用结束。
    ToolFinished {
        /// 轮次 ID。
        turn_id: String,
        /// 调用 ID。
        tool_call_id: String,
        /// 终态。
        status: ToolStatus,
        /// 工具回灌给模型的结果原文。
        result: ToolResult,
        /// 执行耗时毫秒；`None` 表示不计时。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        duration_ms: Option<u64>,
    },
    /// core 正在等前端授权（与 [`SessionUpdate::PermissionResolved`] 成对）。
    ///
    /// 这两条镜像更新由 **core** 负责发：`ask_permission` 只发反向请求，
    /// 不隐式发通知，好让 transcript 里能内联渲染「等待授权」状态。
    PermissionRequested {
        /// 轮次 ID。
        turn_id: String,
        /// 授权请求 ID。
        request_id: String,
        /// 相关调用 ID。
        tool_call_id: String,
    },
    /// 授权询问结束。
    PermissionResolved {
        /// 轮次 ID。
        turn_id: String,
        /// 授权请求 ID。
        request_id: String,
        /// 前端选中的选项；`None` 表示前端没给选择（未注册处理器、选项非法、连接中断），core 按拒绝处理。
        option_id: Option<String>,
    },
    /// 轮次结束；`message` 与 `session/prompt` 响应的 [`super::turn::TurnResult::message`] 相同。
    TurnFinished {
        /// 轮次 ID。
        turn_id: String,
        /// 权威 assistant 消息。
        message: Message,
        /// 结束原因。
        finish_reason: FinishReason,
        /// token 用量。
        usage: Usage,
    },
    /// 标题变化（core 自动命名或 `session/rename` 之后）。
    TitleChanged {
        /// 新标题。
        title: String,
    },
    /// 锁投影变化。
    LockChanged {
        /// 新的锁投影；`None` 表示锁已释放。
        lock: Option<SessionLockInfo>,
    },
}

/// assistant 增量。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AssistantDelta {
    /// 正文增量。
    Text {
        /// 增量文本。
        delta: String,
    },
    /// 思维链增量。
    Reasoning {
        /// 增量文本。
        delta: String,
    },
    /// 一次工具调用开始（名字此时才已知）。
    ToolCallStarted {
        /// 调用 ID；后续 [`AssistantDelta::ToolCallArgs`] 用它归组。
        tool_call_id: String,
        /// 工具名。
        name: String,
    },
    /// 工具调用参数增量（JSON 片段，按序拼接后即完整参数）。
    ToolCallArgs {
        /// 调用 ID。
        tool_call_id: String,
        /// JSON 片段。
        delta: String,
    },
    /// token 用量刷新。
    Usage {
        /// 用量。
        usage: Usage,
    },
}

/// `session/prompt` 的轮次结束通知与响应共用的构造。
impl SessionUpdate {
    /// 轮次开始。
    pub fn turn_started(turn_id: impl Into<String>, model: impl Into<String>, at_ms: u64) -> Self {
        Self::TurnStarted {
            turn_id: turn_id.into(),
            model: model.into(),
            at_ms,
        }
    }

    /// assistant 增量。
    pub fn delta(turn_id: impl Into<String>, delta: AssistantDelta) -> Self {
        Self::AssistantDelta {
            turn_id: turn_id.into(),
            delta,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ContentPart, Role};
    use serde_json::json;

    fn usage() -> Usage {
        Usage {
            input_tokens: 12,
            output_tokens: 3,
            ..Usage::default()
        }
    }

    #[test]
    fn notification_shape() {
        assert_eq!(<SessionUpdateMethod as Method>::NAME, "session/update");
        let params = SessionUpdateParams {
            session_id: "ses_7f3a".into(),
            update: SessionUpdate::delta(
                "turn_1",
                AssistantDelta::Text {
                    delta: "好的".into(),
                },
            ),
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            json!({
                "session_id": "ses_7f3a",
                "update": {
                    "type": "assistant_delta",
                    "turn_id": "turn_1",
                    "delta": { "type": "text", "delta": "好的" }
                }
            })
        );
    }

    #[test]
    fn every_variant_shape() {
        let cases = [
            (
                SessionUpdate::turn_started("turn_1", "deepseek-flash", 1_724_000_000_000),
                json!({ "type": "turn_started", "turn_id": "turn_1", "model": "deepseek-flash", "at_ms": 1_724_000_000_000u64 }),
            ),
            (
                SessionUpdate::delta("turn_1", AssistantDelta::Reasoning { delta: "想".into() }),
                json!({ "type": "assistant_delta", "turn_id": "turn_1", "delta": { "type": "reasoning", "delta": "想" } }),
            ),
            (
                SessionUpdate::delta(
                    "turn_1",
                    AssistantDelta::ToolCallStarted {
                        tool_call_id: "call_9".into(),
                        name: "bash".into(),
                    },
                ),
                json!({ "type": "assistant_delta", "turn_id": "turn_1", "delta": { "type": "tool_call_started", "tool_call_id": "call_9", "name": "bash" } }),
            ),
            (
                SessionUpdate::delta(
                    "turn_1",
                    AssistantDelta::ToolCallArgs {
                        tool_call_id: "call_9".into(),
                        delta: "{\"cmd\":".into(),
                    },
                ),
                json!({ "type": "assistant_delta", "turn_id": "turn_1", "delta": { "type": "tool_call_args", "tool_call_id": "call_9", "delta": "{\"cmd\":" } }),
            ),
            (
                SessionUpdate::delta("turn_1", AssistantDelta::Usage { usage: usage() }),
                json!({ "type": "assistant_delta", "turn_id": "turn_1", "delta": { "type": "usage", "usage": { "input_tokens": 12, "output_tokens": 3, "total_tokens": null, "cached_input_tokens": null, "cache_creation_input_tokens": null, "reasoning_tokens": null } } }),
            ),
            (
                SessionUpdate::ToolStarted {
                    turn_id: "turn_1".into(),
                    tool_call_id: "call_9".into(),
                    name: "bash".into(),
                },
                json!({ "type": "tool_started", "turn_id": "turn_1", "tool_call_id": "call_9", "name": "bash" }),
            ),
            (
                SessionUpdate::ToolFinished {
                    turn_id: "turn_1".into(),
                    tool_call_id: "call_9".into(),
                    status: ToolStatus::Denied,
                    result: ToolResult {
                        tool_call_id: "call_9".into(),
                        content: "用户拒绝".into(),
                        is_error: true,
                    },
                    duration_ms: None,
                },
                json!({ "type": "tool_finished", "turn_id": "turn_1", "tool_call_id": "call_9", "status": "denied", "result": { "tool_call_id": "call_9", "content": "用户拒绝", "is_error": true } }),
            ),
            (
                SessionUpdate::PermissionRequested {
                    turn_id: "turn_1".into(),
                    request_id: "perm_1".into(),
                    tool_call_id: "call_9".into(),
                },
                json!({ "type": "permission_requested", "turn_id": "turn_1", "request_id": "perm_1", "tool_call_id": "call_9" }),
            ),
            (
                SessionUpdate::PermissionResolved {
                    turn_id: "turn_1".into(),
                    request_id: "perm_1".into(),
                    option_id: None,
                },
                json!({ "type": "permission_resolved", "turn_id": "turn_1", "request_id": "perm_1", "option_id": null }),
            ),
            (
                SessionUpdate::TurnFinished {
                    turn_id: "turn_1".into(),
                    message: Message {
                        role: Role::Assistant,
                        content: vec![ContentPart::Text("好的".into())],
                    },
                    finish_reason: FinishReason::Stop,
                    usage: usage(),
                },
                json!({ "type": "turn_finished", "turn_id": "turn_1", "message": { "role": "Assistant", "content": [{ "Text": "好的" }] }, "finish_reason": "Stop", "usage": { "input_tokens": 12, "output_tokens": 3, "total_tokens": null, "cached_input_tokens": null, "cache_creation_input_tokens": null, "reasoning_tokens": null } }),
            ),
            (
                SessionUpdate::TitleChanged { title: "新标题".into() },
                json!({ "type": "title_changed", "title": "新标题" }),
            ),
            (
                SessionUpdate::LockChanged {
                    lock: Some(SessionLockInfo {
                        pid: 7,
                        frontend: "tui".into(),
                        process_name: None,
                        acquired_at_ms: 5,
                    }),
                },
                json!({ "type": "lock_changed", "lock": { "pid": 7, "frontend": "tui", "acquired_at_ms": 5 } }),
            ),
            (
                SessionUpdate::LockChanged { lock: None },
                json!({ "type": "lock_changed", "lock": null }),
            ),
        ];
        for (update, expected) in cases {
            let actual = serde_json::to_value(&update).expect("可序列化");
            assert_eq!(actual, expected);
            let round_trip: SessionUpdate = serde_json::from_value(actual).expect("可反序列化");
            assert_eq!(round_trip, update);
        }
    }

    #[test]
    fn unknown_type_is_rejected() {
        // 前端「忽略未知更新」的落地方式：解析失败即整条丢弃，不让连接受影响
        assert!(serde_json::from_value::<SessionUpdate>(json!({ "type": "future_thing", "x": 1 })).is_err());
        assert!(serde_json::from_value::<AssistantDelta>(json!({ "type": "future_delta" })).is_err());
    }
}

use crate::session::entries::Entry;
use moc_llm::FinishReason;
use moc_llm::Role;
use moc_llm::{ContentPart, ToolCall};

use super::*;
use crate::session::entries::EntryPayload;

fn message_entry(message: Message, finish_reason: Option<FinishReason>, at_ms: u64) -> Entry {
    Entry::from_payload(
        EntryPayload::Message {
            message,
            tool: None,
            finish_reason,
            agent: None,
            goal: None,
        },
        "parent".into(),
        at_ms,
    )
}

fn assistant_call(id: &str) -> Message {
    Message {
        role: Role::Assistant,
        content: moc_llm::message_content![ContentPart::ToolCall(ToolCall {
            id: id.into(),
            name: "bash".into(),
            arguments: "{}".into(),
        })],
    }
}

/// 尾部判定只认正向标记：自然 Stop / EndTurn 工具结果为真，
/// 失败、中断与普通工具批次一律为假。
#[test]
fn turn_completed_only_accepts_positive_markers() {
    // 自然结束：assistant + Stop，无工具调用。
    assert!(turn_completed(&[
        message_entry(Message::user_text("问"), None, 1),
        message_entry(Message::assistant_text("答"), Some(FinishReason::Stop), 2,),
    ]));
    // Stop 后跟记账条目（Usage/ModelChange 等非消息条目）：仍然算正常结束。
    assert!(turn_completed(&[
        message_entry(Message::assistant_text("答"), Some(FinishReason::Stop), 1,),
        Entry::from_payload(
            EntryPayload::ModelChange {
                model: "p/m".into(),
            },
            "parent".into(),
            2,
        ),
    ]));
    // EndTurn 收尾：工具结果带 Stop，同批剩余调用的「未执行」占位跟在后面。
    assert!(turn_completed(&[
        message_entry(assistant_call("call_a"), None, 1),
        message_entry(
            Message::tool_result("call_a", "done", false),
            Some(FinishReason::Stop),
            2,
        ),
        message_entry(Message::tool_result("call_b", "未执行", false), None, 3),
    ]));
    // 工具批次跑完但没有标记：下一次模型请求失败，可重试。
    assert!(!turn_completed(&[
        message_entry(assistant_call("call_a"), None, 1),
        message_entry(Message::tool_result("call_a", "done", false), None, 2),
    ]));
    // 尾部是用户消息：上游请求直接失败，什么都没落库。
    assert!(!turn_completed(&[message_entry(
        Message::user_text("问"),
        None,
        1
    )]));
    // 非正常结束的 assistant：Length、Cancelled 都可重试。
    for reason in [FinishReason::Length, FinishReason::Cancelled] {
        assert!(!turn_completed(&[message_entry(
            Message::assistant_text("半截"),
            Some(reason),
            1,
        )]));
    }
    // assistant 带工具调用（即使标了 Stop 也不认）与空 transcript。
    assert!(!turn_completed(&[message_entry(
        assistant_call("call_a"),
        Some(FinishReason::Stop),
        1,
    )]));
    assert!(!turn_completed(&[]));
}

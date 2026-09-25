//! 将上游流事件投影为前端增量，保留工具调用索引关联。

use moc_llm::StreamEvent;
use moc_protocol::methods::{AssistantDelta, SessionUpdate};
use std::collections::HashMap;
pub(super) async fn forward_event(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    turn_id: &str,
    event: StreamEvent,
    calls: &mut HashMap<usize, String>,
) -> Result<(), moc_protocol::ProtocolError> {
    let delta = match event {
        StreamEvent::TextDelta { text, .. } => Some(AssistantDelta::Text { delta: text }),
        StreamEvent::ReasoningDelta { text, .. } => Some(AssistantDelta::Reasoning { delta: text }),
        StreamEvent::ToolCallStart { index, id, name } => {
            let call_id = calls.entry(index).or_insert(id).clone();
            Some(AssistantDelta::ToolCallStarted {
                tool_call_id: call_id,
                name,
            })
        }
        StreamEvent::ToolCallArgumentsDelta { index, json } => {
            if !calls.contains_key(&index) {
                tracing::warn!(index, "丢弃没有 ToolCallStart 的参数片段");
            }
            calls
                .get(&index)
                .cloned()
                .map(|tool_call_id| AssistantDelta::ToolCallArgs {
                    tool_call_id,
                    delta: json,
                })
        }
        StreamEvent::Usage(usage) => Some(AssistantDelta::Usage { usage }),
        StreamEvent::Start { .. }
        | StreamEvent::Done { .. }
        | StreamEvent::ReasoningSignature { .. }
        | StreamEvent::ReasoningItem { .. }
        | StreamEvent::CompactionItem { .. } => None,
    };
    if let Some(delta) = delta {
        update
            .emit(
                session_id.to_owned(),
                SessionUpdate::AssistantDelta {
                    turn_id: turn_id.to_owned(),
                    delta,
                },
            )
            .await?;
    }
    Ok(())
}

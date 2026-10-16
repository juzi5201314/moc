//! 将上游流事件投影为前端增量：只转发前端能渲染的文本类内容。
//!
//! 工具调用的名字与参数不在这里下发——上游增量需要按内容块序号归组，而
//! 序号只在一次流内有效，任何映射错位都会让前端拿到与执行不符的参数。
//! 参数改由 [`crate::service::prompt`] 在执行时随 `ToolStarted` / `ToolFinished`
//! 一次性给出，前端拿到的一定是实际执行的那一份。

use moc_llm::StreamEvent;
use moc_protocol::methods::{AssistantDelta, SessionUpdate};

pub(super) async fn forward_event(
    update: &moc_protocol::UpdateSink,
    session_id: &str,
    prompt_id: &str,
    event: StreamEvent,
) -> Result<(), moc_protocol::ProtocolError> {
    let delta = match event {
        StreamEvent::TextDelta { text, .. } => Some(AssistantDelta::Text { delta: text }),
        StreamEvent::ReasoningDelta { text, .. } => Some(AssistantDelta::Reasoning { delta: text }),
        StreamEvent::Usage(usage) => Some(AssistantDelta::Usage { usage }),
        StreamEvent::Start { .. }
        | StreamEvent::Done { .. }
        | StreamEvent::ToolCallStart { .. }
        | StreamEvent::ToolCallArgumentsDelta { .. }
        | StreamEvent::ReasoningSignature { .. }
        | StreamEvent::ReasoningItem { .. }
        | StreamEvent::CompactionItem { .. } => None,
    };
    if let Some(delta) = delta {
        update
            .emit(
                session_id.to_owned(),
                SessionUpdate::AssistantDelta {
                    prompt_id: prompt_id.to_owned(),
                    delta,
                },
            )
            .await?;
    }
    Ok(())
}

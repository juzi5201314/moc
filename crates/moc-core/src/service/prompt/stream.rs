//! 把上游流事件转换成前端增量，只转发前端能渲染的文本类内容。
//!
//! 工具调用的名字和参数不在这里下发：上游增量要按内容块序号归组，而序号只在一次
//! 流内有效，对应关系一旦错位，前端拿到的参数就和实际执行的不一样。参数由
//! [`crate::service::prompt`] 在执行时随 `ToolStarted` / `ToolFinished` 一次性给出，
//! 保证前端看到的就是实际执行的参数。

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

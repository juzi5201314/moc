//! 转发正文、思考与用量；工具生成预览由相邻 preview 模块按流内身份投影。
//! 正式执行参数仍只来自 ToolStarted / ToolFinished。

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
        | StreamEvent::ToolCallSnapshot { .. }
        | StreamEvent::TextSnapshot { .. }
        | StreamEvent::ResponsePhase { .. }
        | StreamEvent::ReasoningSignature { .. }
        | StreamEvent::ReasoningItem { .. } => None,
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

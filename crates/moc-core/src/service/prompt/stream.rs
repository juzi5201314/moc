//! 转发正文、思考与用量；工具生成预览由相邻 preview 模块按流内身份投影。
//! 正式执行参数仍只来自 ToolStarted / ToolFinished。

use moc_llm::StreamEvent;
use moc_protocol::TextSpan;
use moc_protocol::methods::{AssistantDelta, SessionUpdate};

/// 本次生成中各正文块的流式时间窗，随响应落库供前端划分工作段。
///
/// 块按首个正文事件（增量或阶段）登记，与累加器建块顺序相同；没有非空增量的块
/// 不会折叠成 Text 部分，也就不产出时间窗，所以结果与响应里的 Text 部分按序一一对应。
/// 一次响应只有寥寥几个正文块，线性查找即可。
#[derive(Default)]
pub(super) struct TextSpans(Vec<(usize, Option<TextSpan>)>);

impl TextSpans {
    pub(super) fn observe(&mut self, event: &StreamEvent, now: u64) {
        let (index, text) = match event {
            StreamEvent::TextDelta { index, text } => (*index, text.as_str()),
            StreamEvent::ResponsePhase { index, .. } => (*index, ""),
            _ => return,
        };
        let slot = match self.0.iter().position(|(seen, _)| *seen == index) {
            Some(slot) => slot,
            None => {
                self.0.push((index, None));
                self.0.len() - 1
            }
        };
        if text.is_empty() {
            return;
        }
        self.0[slot]
            .1
            .get_or_insert(TextSpan {
                start_ms: now,
                end_ms: now,
            })
            .end_ms = now;
    }

    pub(super) fn into_spans(self) -> Vec<TextSpan> {
        self.0.into_iter().filter_map(|(_, span)| span).collect()
    }
}

/// 本次生成中思考阶段的墙钟耗时，随响应落库供前后端历史回放还原。
///
/// 首个非空 `ReasoningDelta` 启动计时，正文、工具调用或响应结束时收口；
/// 若响应内有多段思考，累加各段时长。未收到非空思考增量时返回 `None`。
#[derive(Default)]
pub(super) struct ReasoningTiming {
    started_at: Option<u64>,
    elapsed_ms: u64,
    observed: bool,
}

impl ReasoningTiming {
    pub(super) fn observe(&mut self, event: &StreamEvent, now: u64) {
        match event {
            StreamEvent::ReasoningDelta { text, .. } if !text.is_empty() => {
                self.observed = true;
                if self.started_at.is_none() {
                    self.started_at = Some(now);
                }
            }
            StreamEvent::TextDelta { text, .. } if !text.is_empty() => self.close(now),
            StreamEvent::ToolCallStart { .. }
            | StreamEvent::ToolCallArgumentsDelta { .. }
            | StreamEvent::ToolCallSnapshot { .. }
            | StreamEvent::Done { .. } => self.close(now),
            _ => {}
        }
    }

    fn close(&mut self, now: u64) {
        if let Some(started) = self.started_at.take() {
            self.elapsed_ms = self.elapsed_ms.saturating_add(now.saturating_sub(started));
        }
    }

    pub(super) fn into_duration_ms(mut self, now: u64) -> Option<u64> {
        self.close(now);
        self.observed.then_some(self.elapsed_ms)
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    fn text(index: usize, text: &str) -> StreamEvent {
        StreamEvent::TextDelta {
            index,
            text: text.into(),
        }
    }

    #[test]
    fn text_spans_follow_block_registration_and_skip_empty_blocks() {
        let mut spans = TextSpans::default();
        let phase = StreamEvent::ResponsePhase {
            index: 1,
            phase: "commentary".into(),
        };
        spans.observe(&phase, 1);
        spans.observe(&text(0, "a"), 2);
        spans.observe(&text(2, ""), 3);
        spans.observe(&text(1, "b"), 4);
        spans.observe(&text(0, "c"), 5);
        spans.observe(&text(1, "d"), 6);
        let span = |start_ms, end_ms| TextSpan { start_ms, end_ms };
        assert_eq!(spans.into_spans(), [span(4, 6), span(2, 5)]);
    }

    #[test]
    fn reasoning_timing_tracks_streaming_window_and_skips_empty_responses() {
        let mut empty = ReasoningTiming::default();
        empty.observe(&text(0, "hi"), 10);
        assert_eq!(empty.into_duration_ms(20), None);

        let mut timing = ReasoningTiming::default();
        timing.observe(
            &StreamEvent::ReasoningDelta {
                index: 0,
                text: "step 1".into(),
            },
            100,
        );
        timing.observe(
            &StreamEvent::ToolCallStart {
                index: 1,
                id: "c1".into(),
                name: "bash".into(),
            },
            1_350,
        );
        assert_eq!(timing.into_duration_ms(2_000), Some(1_250));
    }
}

use super::*;
use crate::config::{Auth, EndpointStyle};
use crate::types::ReasoningEcho;

fn config(protocol: Protocol) -> LlmConfig {
    LlmConfig {
        protocol,
        base_url: "https://example.com/v1".into(),
        model: "test".into(),
        auth: Auth::None,
        endpoint: EndpointStyle::Default,
        ..Default::default()
    }
}

fn request() -> ChatRequest {
    ChatRequest {
        messages: vec![Message::user_text("hi")],
        options: crate::types::RequestOptions {
            echo_reasoning: ReasoningEcho::Protocol,
            ..Default::default()
        },
        ..Default::default()
    }
}

fn accumulate(protocol: Protocol, events: &[StreamEvent]) -> Accumulator {
    let mut accumulator = Accumulator::default();
    for event in events {
        accumulator.apply(event);
    }
    let _ = protocol;
    accumulator
}

#[test]
fn preview_identities_keep_sparse_indices_and_final_snapshot_ids() {
    let mut stream = stream_from_chunks(&config(Protocol::OpenAiChat), &request(), vec![]);
    for event in [
        StreamEvent::TextDelta {
            index: 9,
            text: "正文".into(),
        },
        StreamEvent::ToolCallArgumentsDelta {
            index: 9,
            json: "{}".into(),
        },
        StreamEvent::ToolCallStart {
            index: 2,
            id: "second".into(),
            name: "read".into(),
        },
        StreamEvent::ToolCallStart {
            index: 9,
            id: "old".into(),
            name: "bash".into(),
        },
        StreamEvent::ToolCallSnapshot {
            index: 9,
            id: Some("final".into()),
            name: None,
            arguments: None,
        },
    ] {
        stream.accumulator.apply(&event);
    }
    let mut identities = stream.tool_call_identities();
    identities.sort();
    assert_eq!(identities, vec![(2, "second".into()), (9, "final".into())]);
}

mod anthropic;
mod behavior;
mod errors;
mod responses;

/// 测试替身：先吐固定字节，然后永远 `Pending`。
struct StalledStream {
    chunks: VecDeque<Bytes>,
}

impl Stream for StalledStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        match self.chunks.pop_front() {
            Some(chunk) => Poll::Ready(Some(Ok(chunk))),
            None => Poll::Pending,
        }
    }
}

fn stream_from_chunks(cfg: &LlmConfig, req: &ChatRequest, chunks: Vec<&'static str>) -> ChatStream {
    let body = StalledStream {
        chunks: chunks
            .into_iter()
            .map(|chunk| Bytes::from_static(chunk.as_bytes()))
            .collect(),
    };
    ChatStream::new(Box::pin(body), cfg, req)
}

/// 测试替身：吐完固定字节后正常 EOF（上游没给终态就关连接）。
struct EndedStream {
    chunks: VecDeque<Bytes>,
}

impl Stream for EndedStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Poll::Ready(self.chunks.pop_front().map(Ok))
    }
}

fn stream_ending_after(
    cfg: &LlmConfig,
    req: &ChatRequest,
    chunks: Vec<&'static str>,
) -> ChatStream {
    let body = EndedStream {
        chunks: chunks
            .into_iter()
            .map(|chunk| Bytes::from_static(chunk.as_bytes()))
            .collect(),
    };
    ChatStream::new(Box::pin(body), cfg, req)
}

async fn decode_frames(protocol: Protocol, frames: &[serde_json::Value]) -> ChatResponse {
    let body = EndedStream {
        chunks: frames
            .iter()
            .map(|frame| Bytes::from(format!("data: {frame}\n\n")))
            .collect(),
    };
    ChatStream::new(Box::pin(body), &config(protocol), &request())
        .finish()
        .await
        .expect("夹具包含协议终态")
}

use super::*;

#[tokio::test]
async fn error_event_terminates_stream() {
    let cfg = config(Protocol::OpenAiChat);
    let req = request();
    let mut stream = stream_from_chunks(
        &cfg,
        &req,
        vec![
            "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"前半\"},\"finish_reason\":null}]}\n\n",
            "data: {oops\n\n",
            "data: [DONE]\n\n",
        ],
    );
    let mut seen = Vec::new();
    while let Some(event) = stream.next_event().await {
        seen.push(event);
    }
    assert_eq!(seen.len(), 3, "Start、文本、错误，之后不再有事件");
    assert!(
        matches!(seen[0], Ok(StreamEvent::Start { .. })),
        "{:?}",
        seen[0]
    );
    assert!(
        matches!(seen[1], Ok(StreamEvent::TextDelta { .. })),
        "{:?}",
        seen[1]
    );
    assert!(matches!(seen[2], Err(LlmError::Stream(_))), "{:?}", seen[2]);

    let error = stream.finish().await.unwrap_err();
    assert!(matches!(error, LlmError::Stream(_)), "{error:?}");
}

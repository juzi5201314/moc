use super::*;

#[test]
fn thinking_stream_is_bounded_without_splitting_utf8() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::Thinking {
        text: String::new(),
        reasoning_tokens: None,
        duration_ms: None,
    });
    app.append_streaming_limited(
        |block| match block {
            Block::Thinking { text, .. } => Some(text),
            _ => None,
        },
        &"思考".repeat(100),
        32,
        "…",
    );
    let Block::Thinking { text, .. } = &app.blocks[0].block else {
        panic!("期望思考块");
    };
    assert!(text.len() <= 32);
    assert!(text.is_char_boundary(text.len()));
}

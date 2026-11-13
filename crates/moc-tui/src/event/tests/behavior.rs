use super::*;

/// core 剪掉失败回合后会回报 `prompt_rewound`：界面那段预览跟着消失。
#[test]
fn prompt_rewound_drops_the_trimmed_preview() {
    use crate::transcript::{Block, ToolCallFinish, ToolCallStart};
    use moc_protocol::ToolResult;
    use moc_protocol::methods::ToolStatus;

    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(Block::Assistant {
        text: "半截正文".into(),
    });
    app.start_tool_block(ToolCallStart {
        id: "call_1".into(),
        name: "read".into(),
        parent_id: None,
        arguments: "{}".into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: "call_1".into(),
        name: "read".into(),
        parent_id: None,
        arguments: "{}".into(),
        status: ToolStatus::Cancelled,
        result: ToolResult {
            tool_call_id: "call_1".into(),
            content: String::new(),
            is_error: true,
            images: Vec::new(),
        },
        duration_ms: None,
        diff: None,
    });

    apply_update(
        &mut app,
        SessionUpdate::PromptRewound {
            replayed_tools: false,
        },
    );

    assert!(app.blocks.is_empty(), "失败回合的预览与占位工具块都要撤掉");
}

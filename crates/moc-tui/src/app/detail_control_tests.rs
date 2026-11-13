use super::*;
use crate::transcript::ToolCallBlock;
use moc_protocol::methods::ToolStatus;

fn thinking() -> Block {
    Block::Thinking {
        text: "思考".into(),
        reasoning_tokens: None,
        duration_ms: None,
    }
}

fn tool() -> Block {
    Block::Tool(Box::new(ToolCallBlock {
        id: "call_1".into(),
        batch_id: None,
        parent_id: None,
        name: "bash".into(),
        arguments: "{}".into(),
        status: Some(ToolStatus::Completed),
        result: Some("done".into()),
        diff: None,
        duration_ms: None,
        running: false,
        progress: None,
        images: Vec::new(),
        job_id: None,
        live_output: None,
    }))
}

/// Ctrl+T / Ctrl+O 切换全局档位时收回该层的手动展开，另一层不受影响。
#[test]
fn cycling_detail_reclaims_manual_expansion() {
    let (events, _) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.push_block(thinking());
    app.push_block(tool());
    app.blocks[0].toggle_level(app.thinking_detail);
    app.blocks[1].toggle_level(app.tool_detail);
    assert_eq!(app.blocks[0].level_override, Some(DetailLevel::Full));
    assert_eq!(app.blocks[1].level_override, Some(DetailLevel::Full));

    // 转一圈回到简略：手动展开必须被收回，块跟随全局档位。
    for _ in 0..3 {
        app.cycle_thinking_detail();
    }
    assert_eq!(app.thinking_detail, DetailLevel::Brief);
    assert_eq!(app.blocks[0].level_override, None);
    assert_eq!(app.blocks[0].level(app.thinking_detail), DetailLevel::Brief);
    assert_eq!(
        app.blocks[1].level_override,
        Some(DetailLevel::Full),
        "Ctrl+T 不该碰工具块"
    );

    app.blocks[0].toggle_level(app.thinking_detail);
    app.cycle_tool_detail();
    assert_eq!(app.tool_detail, DetailLevel::Hidden);
    assert_eq!(app.blocks[1].level_override, None);
    assert_eq!(
        app.blocks[0].level_override,
        Some(DetailLevel::Full),
        "Ctrl+O 不该碰思考块"
    );
    app.blocks[1].toggle_level(app.tool_detail);
    assert_eq!(app.blocks[1].level(app.tool_detail), DetailLevel::Full);
    app.cycle_tool_detail();
    assert_eq!(app.tool_detail, DetailLevel::Brief);
    assert_eq!(app.blocks[1].level_override, None);
    app.cycle_tool_detail();
    assert_eq!(app.tool_detail, DetailLevel::Hidden);
}

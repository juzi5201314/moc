use super::*;
use crate::transcript::ToolCallFinish;
use moc_protocol::ToolResult;
use moc_protocol::methods::ToolStatus;

fn result(tool_call_id: &str, content: &str) -> ToolResult {
    ToolResult {
        tool_call_id: tool_call_id.to_owned(),
        content: content.to_owned(),
        is_error: false,
        images: Vec::new(),
    }
}

#[test]
fn orphan_tool_finish_recovers_name_arguments_and_batch() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.register_tool_batch("batch_1".to_owned(), ["call_1".to_owned()]);
    app.finish_tool_block(ToolCallFinish {
        id: "call_1".to_owned(),
        name: "bash".to_owned(),
        parent_id: None,
        arguments: r#"{"command":"cargo test"}"#.to_owned(),
        status: ToolStatus::Completed,
        result: result("call_1", "退出码 0"),
        duration_ms: Some(7),
        diff: None,
    });

    let Block::Tool(tool) = &app.blocks[0].block else {
        panic!("缺席调用开始时应当补建工具块");
    };
    assert_eq!(
        tool.name, "bash",
        "补建的块必须用更新里的工具名，而不是调用 ID"
    );
    assert_eq!(tool.arguments, r#"{"command":"cargo test"}"#);
    assert_eq!(tool.batch_id.as_deref(), Some("batch_1"));
    assert_eq!(tool.result.as_deref(), Some("退出码 0"));
    assert_eq!(tool.status, Some(ToolStatus::Completed));
    assert!(!tool.running);
}

#[test]
fn orphan_ptc_child_finish_keeps_parent_and_name() {
    let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let mut app = App::new(crate::theme::Theme::global(), events);
    app.finish_tool_block(ToolCallFinish {
        id: "call_parent:1".to_owned(),
        name: "read".to_owned(),
        parent_id: Some("call_parent".to_owned()),
        arguments: r#"{"path":"artifact://ses_1/a.txt"}"#.to_owned(),
        status: ToolStatus::Error,
        result: result("call_parent:1", "读取失败"),
        duration_ms: Some(3),
        diff: None,
    });

    let Block::Tool(tool) = &app.blocks[0].block else {
        panic!("缺席子调用开始时应当补建工具块");
    };
    assert_eq!(tool.name, "read");
    assert_eq!(
        tool.parent_id.as_deref(),
        Some("call_parent"),
        "子调用保持缩进归属"
    );
    assert_eq!(tool.arguments, r#"{"path":"artifact://ses_1/a.txt"}"#);
    assert_eq!(tool.status, Some(ToolStatus::Error));
}

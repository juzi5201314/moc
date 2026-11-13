use super::*;
use crate::DetailLevel;
use crate::render::TITLE_INDENT;
use crate::render::markdown::render_markdown_with;
use crate::render::scrollback::agent_tools::ask_answer_pairs;
use crate::render::scrollback::layout::may_contain_image;
use crate::render::scrollback::layout::may_contain_mermaid;
use crate::render::scrollback::layout::prose_width;
use crate::render::scrollback::layout::render_entry_single;
use crate::render::scrollback::layout::wrapped_rows;
use crate::render::scrollback::messages::assistant_message;
use crate::render::scrollback::messages::format_clock;
use crate::render::scrollback::messages::thinking_block;
use crate::render::scrollback::messages::user_message;
use crate::render::scrollback::structured_tools::find_file_count;
use crate::render::scrollback::structured_tools::grep_counts;
use crate::render::scrollback::text::plain_text;
use crate::render::scrollback::tool_summary::summary_spans;
use crate::render::str_width;
use crate::theme::ColorLevel;
use crate::transcript::{Entry, ToolCallBlock};
use crate::transcript::{ToolCallFinish, ToolCallStart};
use ratatui::text::Span;
use serde_json::Value;

mod agents;
mod behavior;
mod layout;
mod media;
mod messages;
mod tools;
mod tree;

/// 一行的纯文本。
fn plain(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// 一行首个非空白字符所在的列。
fn lead_col(line: &Line<'_>) -> usize {
    let text = plain(line);
    str_width(&text) - str_width(text.trim_start())
}

/// 开始并结束一次工具调用（正常路径：先 start 再 finish）。
fn run_tool(
    app: &mut App,
    id: &str,
    name: &str,
    arguments: &str,
    status: moc_protocol::methods::ToolStatus,
    content: &str,
) {
    app.start_tool_block(ToolCallStart {
        id: id.into(),
        name: name.into(),
        parent_id: None,
        arguments: arguments.into(),
    });
    app.finish_tool_block(ToolCallFinish {
        id: id.into(),
        name: name.into(),
        parent_id: None,
        arguments: arguments.into(),
        status,
        result: moc_protocol::ToolResult {
            tool_call_id: id.into(),
            content: content.into(),
            is_error: status != moc_protocol::methods::ToolStatus::Completed,
            images: Vec::new(),
        },
        duration_ms: Some(1_200),
        diff: None,
    });
}

/// 整份转写按块顺序拼成的行（批次成员只由组首块产出）。
fn transcript(app: &App, width: u16) -> Vec<Line<'static>> {
    (0..app.blocks.len())
        .flat_map(|index| render_entry(index, &app.blocks[index], app, width).lines)
        .collect()
}

//! 会话恢复：`SessionLoadResult.entries` → 转写块。
//!
//! transcript 里 assistant 消息携带 ToolCall、后随的 Tool 消息携带
//! ToolResult；恢复时按 tool_call_id 把结果回填到调用块上。

use moc_protocol::{ContentPart, Role};
use moc_protocol::methods::{ToolExecution, ToolStatus, TranscriptEntry};

use crate::transcript::{Block, Entry, ToolCallBlock};

/// 把 transcript 条目折叠成渲染块。
pub fn blocks_from_entries(entries: &[TranscriptEntry]) -> Vec<Entry> {
    let mut blocks: Vec<Entry> = Vec::new();
    // tool_call_id → 已创建工具块的下标（结果回填用）
    let mut call_index: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for record in entries {
        let TranscriptEntry {
            message, tool, ..
        } = record;
        match message.role {
            Role::User => {
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    blocks.push(Entry::new(Block::User { text }));
                }
            }
            Role::Assistant => {
                for part in &message.content {
                    match part {
                        ContentPart::Text(text) => {
                            blocks.push(Entry::new(Block::Assistant { text: text.clone() }));
                        }
                        ContentPart::Reasoning(reasoning) => {
                            blocks.push(Entry::new(Block::Thinking {
                                text: reasoning
                                    .display_text()
                                    .unwrap_or_else(|| "（思维链内容不可用）".into()),
                            }));
                        }
                        ContentPart::ToolCall(call) => {
                            let block = ToolCallBlock {
                                id: call.id.clone(),
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                                status: None,
                                result: None,
                                diff: None,
                                duration_ms: None,
                                running: false,
                            };
                            call_index.insert(call.id.clone(), blocks.len());
                            blocks.push(Entry::new(Block::Tool(block)));
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {
                for part in &message.content {
                    let ContentPart::ToolResult(result) = part else {
                        continue;
                    };
                    if let Some(&index) = call_index.get(&result.tool_call_id)
                        && let Some(entry) = blocks.get_mut(index)
                        && let Block::Tool(tool) = &mut entry.block
                    {
                        tool.status = Some(if result.is_error {
                            ToolStatus::Error
                        } else {
                            ToolStatus::Completed
                        });
                        tool.result = Some(result.content.clone());
                        entry.touch();
                    }
                }
            }
            _ => {}
        }
        // 恢复条目的终态（含 denied / cancelled）
        if let Some(ToolExecution {
            tool_call_id,
            status,
            duration_ms,
        }) = tool
            && let Some(&index) = call_index.get(tool_call_id)
            && let Some(entry) = blocks.get_mut(index)
            && let Block::Tool(tool_block) = &mut entry.block
        {
            tool_block.status = Some(*status);
            tool_block.duration_ms = *duration_ms;
            tool_block.running = false;
            entry.touch();
        }
    }
    blocks
}

fn collect_text(parts: &[ContentPart]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use moc_protocol::methods::ToolStatus;
    use moc_protocol::{Message, ToolCall, ToolResult};

    fn entry(message: Message, tool: Option<ToolExecution>) -> TranscriptEntry {
        TranscriptEntry {
            id: "entry".into(),
            at_ms: 0,
            message,
            tool,
        }
    }

    #[test]
    fn pairs_tool_calls_with_results() {
        let call = ToolCall::new("call_1", "bash", "{\"command\":\"ls\"}");
        let entries = [
            entry(
                Message {
                    role: Role::User,
                    content: vec![ContentPart::Text("跑 ls".into())],
                },
                None,
            ),
            entry(
                Message {
                    role: Role::Assistant,
                    content: vec![
                        ContentPart::Text("执行命令".into()),
                        ContentPart::ToolCall(call),
                    ],
                },
                None,
            ),
            entry(
                Message {
                    role: Role::Tool,
                    content: vec![ContentPart::ToolResult(ToolResult {
                        tool_call_id: "call_1".into(),
                        content: "a.rs".into(),
                        is_error: false,
                    })],
                },
                Some(ToolExecution {
                    tool_call_id: "call_1".into(),
                    status: ToolStatus::Completed,
                    duration_ms: Some(12),
                }),
            ),
        ];
        let blocks = blocks_from_entries(&entries);
        assert_eq!(blocks.len(), 3);
        assert!(matches!(blocks[0].block, Block::User { .. }));
        let Block::Tool(tool) = &blocks[2].block else {
            panic!("第三个块应是工具块");
        };
        assert_eq!(tool.status, Some(ToolStatus::Completed));
        assert_eq!(tool.result.as_deref(), Some("a.rs"));
    }
}

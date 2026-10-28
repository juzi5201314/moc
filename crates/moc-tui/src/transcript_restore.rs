//! 会话恢复：`SessionLoadResult.entries` → 转写块。
//!
//! transcript 里 assistant 消息携带 ToolCall、后随的 Tool 消息携带
//! ToolResult；恢复时按 tool_call_id 把结果回填到调用块上。

use moc_protocol::methods::{
    ToolExecution, ToolStatus, TranscriptEntry, session::TranscriptChildCall,
};
use moc_protocol::{ContentPart, Role};

use crate::transcript::{Block, Entry, ToolCallBlock};

/// 把 transcript 条目折叠成渲染块。
pub fn blocks_from_entries(entries: &[TranscriptEntry]) -> Vec<Entry> {
    let mut blocks: Vec<Entry> = Vec::new();
    // tool_call_id → 已创建工具块的下标（结果回填用）
    let mut call_index: std::collections::HashMap<String, usize> = std::collections::HashMap::new();

    for record in entries {
        let TranscriptEntry { message, tool, .. } = record;
        if let Some(command) = &record.bash {
            if let Some(tool) = tool {
                blocks.push(Entry::new(Block::Tool(ToolCallBlock {
                    id: tool.tool_call_id.clone(),
                    batch_id: None,
                    parent_id: None,
                    name: "bash".into(),
                    arguments: serde_json::to_string(command).expect("Bash 参数可序列化"),
                    status: Some(tool.status),
                    result: Some(message.text()),
                    diff: None,
                    duration_ms: tool.duration_ms,
                    running: false,
                    progress: None,
                    images: Vec::new(),
                })));
            }
            continue;
        }
        // 代理消息：带来源元数据的卡片，而不是用户输入。
        if let Some(meta) = &record.agent {
            blocks.push(Entry::new(Block::AgentMessage {
                meta: meta.clone(),
                body: message.text(),
            }));
            continue;
        }
        // 运行时提醒是模型内部状态，不属于对话，不进 scrollback。
        if record.reminder.is_some() {
            continue;
        }
        match message.role {
            Role::Custom => {}
            Role::User => {
                let text = collect_text(&message.content);
                if !text.is_empty() {
                    blocks.push(Entry::new(Block::User { text }));
                }
            }
            Role::Assistant => {
                let batch_id =
                    (message.tool_calls().count() > 1).then(|| format!("history-{}", record.id));
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
                                reasoning_tokens: None,
                                duration_ms: None,
                            }));
                        }
                        ContentPart::ToolCall(call) => {
                            let block = ToolCallBlock {
                                id: call.id.clone(),
                                batch_id: batch_id.clone(),
                                parent_id: None,
                                name: call.name.clone(),
                                arguments: call.arguments.clone(),
                                status: None,
                                result: None,
                                diff: None,
                                duration_ms: None,
                                running: false,
                                progress: None,
                                images: Vec::new(),
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

/// 在模型 transcript 基础上恢复 PTC 子工具，并按父调用插入层级块。
pub fn blocks_from_entries_with_children(
    entries: &[TranscriptEntry],
    child_calls: &[TranscriptChildCall],
) -> Vec<Entry> {
    let mut blocks = blocks_from_entries(entries);
    let mut next_positions = std::collections::HashMap::<String, usize>::new();
    for child in child_calls {
        let Some(parent_index) = blocks.iter().position(|entry| {
            matches!(&entry.block, Block::Tool(tool) if tool.id == child.parent_tool_call_id)
        }) else {
            continue;
        };
        let insert_at = next_positions
            .get(&child.parent_tool_call_id)
            .copied()
            .unwrap_or(parent_index + 1);
        for position in next_positions.values_mut() {
            if *position >= insert_at {
                *position += 1;
            }
        }
        let block = ToolCallBlock {
            id: child.id.clone(),
            batch_id: None,
            parent_id: Some(child.parent_tool_call_id.clone()),
            name: child.name.clone(),
            arguments: child.arguments.clone(),
            status: Some(child.status),
            result: Some(child.result.clone()),
            diff: None,
            duration_ms: child.duration_ms,
            running: false,
            progress: None,
            images: Vec::new(),
        };
        blocks.insert(insert_at, Entry::new(Block::Tool(block)));
        next_positions.insert(child.parent_tool_call_id.clone(), insert_at + 1);
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
            bash: None,
            agent: None,
            reminder: None,
        }
    }

    #[test]
    fn runtime_reminders_do_not_enter_the_scrollback() {
        let mut reminder = entry(
            Message::user_text("<system-reminder>Sandbox mode: full.</system-reminder>"),
            None,
        );
        reminder.reminder = Some(moc_protocol::ReminderStamp {
            items: vec![moc_protocol::ReminderItem {
                key: "sandbox".into(),
                digest: "abc".into(),
            }],
        });
        let entries = [entry(Message::user_text("你好"), None), reminder];
        let blocks = blocks_from_entries(&entries);
        assert_eq!(blocks.len(), 1, "提醒是模型内部状态，不该进 scrollback");
        assert!(matches!(
            &blocks[0].block,
            Block::User { text } if text == "你好"
        ));
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
                        images: Vec::new(),
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

    #[test]
    fn restores_ptc_children_under_parent_tool() {
        let entries = [entry(
            Message {
                role: Role::Assistant,
                content: vec![ContentPart::ToolCall(ToolCall::new(
                    "parent", "run_code", "{}",
                ))],
            },
            None,
        )];
        let children = [TranscriptChildCall {
            id: "parent:child".into(),
            parent_tool_call_id: "parent".into(),
            name: "read".into(),
            arguments: "{}".into(),
            result: "ok".into(),
            status: ToolStatus::Completed,
            duration_ms: Some(3),
        }];
        let blocks = blocks_from_entries_with_children(&entries, &children);
        assert_eq!(blocks.len(), 2);
        let Block::Tool(child) = &blocks[1].block else {
            panic!("第二个块应是 PTC 子工具");
        };
        assert_eq!(child.parent_id.as_deref(), Some("parent"));
        assert_eq!(child.result.as_deref(), Some("ok"));
    }

    #[test]
    fn agent_message_entries_restore_as_cards() {
        let mut record = entry(Message::user_text("恢复后仍要看得到"), None);
        record.agent = Some(moc_protocol::methods::AgentMessageMeta {
            from_id: "ses_child".into(),
            from_title: "探查者".into(),
            delivery: moc_protocol::methods::AgentMessageOutcome::Woken,
        });
        let blocks = blocks_from_entries(&[record]);
        match blocks.as_slice() {
            [
                Entry {
                    block: Block::AgentMessage { meta, body },
                    ..
                },
            ] => {
                assert_eq!(meta.from_title, "探查者");
                assert_eq!(
                    meta.delivery,
                    moc_protocol::methods::AgentMessageOutcome::Woken
                );
                assert_eq!(body, "恢复后仍要看得到");
            }
            other => panic!("代理消息必须恢复成卡片：{other:?}"),
        }
    }
}

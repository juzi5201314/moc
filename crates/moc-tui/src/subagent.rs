//! TUI 子代理视图模型与运行时更新归并。

use std::collections::HashMap;

use moc_protocol::methods::{AgentInfo, AssistantDelta, SessionUpdate, ToolStatus};

use crate::transcript::{Block, Entry, ToolCallBlock};

#[derive(Debug, Clone)]
pub struct SubagentView {
    pub info: AgentInfo,
    pub blocks: Vec<Entry>,
    pub tool_indices: HashMap<String, usize>,
    pub pending_args: HashMap<String, String>,
    pub scroll_offset: usize,
    pub running: bool,
}

impl SubagentView {
    pub fn new(info: AgentInfo) -> Self {
        Self {
            running: matches!(
                info.status,
                moc_protocol::methods::AgentStatus::Queued
                    | moc_protocol::methods::AgentStatus::Running
            ),
            info,
            blocks: Vec::new(),
            tool_indices: HashMap::new(),
            pending_args: HashMap::new(),
            scroll_offset: 0,
        }
    }

    pub fn restore(&mut self, entries: Vec<Entry>) {
        self.blocks = entries;
        self.rebuild_tool_indices();
    }

    pub fn apply(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::PromptStarted { .. } => self.running = true,
            SessionUpdate::AssistantDelta { delta, .. } => self.apply_delta(delta),
            SessionUpdate::ToolStarted {
                tool_call_id, name, ..
            } => {
                let arguments = self.pending_args.remove(&tool_call_id).unwrap_or_default();
                let index = self.blocks.len();
                self.blocks.push(Entry::new(Block::Tool(ToolCallBlock {
                    id: tool_call_id.clone(),
                    batch_id: None,
                    parent_id: None,
                    name,
                    arguments,
                    status: None,
                    result: None,
                    diff: None,
                    duration_ms: None,
                    running: true,
                })));
                self.tool_indices.insert(tool_call_id, index);
            }
            SessionUpdate::ToolFinished {
                tool_call_id,
                status,
                result,
                duration_ms,
                diff,
                ..
            } => {
                if let Some(index) = self.tool_indices.get(&tool_call_id).copied()
                    && let Some(entry) = self.blocks.get_mut(index)
                {
                    entry.finish_tool(status, result, duration_ms, diff);
                }
            }
            SessionUpdate::PromptFinished { message, .. } => {
                self.running = false;
                let text = message.text();
                if !text.is_empty() {
                    self.info.last_message = Some(text);
                }
            }
            SessionUpdate::TodoChanged { phases } => {
                self.blocks.push(Entry::new(Block::Todo { phases }));
            }
            SessionUpdate::ContextCompacted {
                method,
                tokens_before,
                tokens_after,
            } => {
                self.blocks.push(Entry::new(Block::Note {
                    text: format!(
                        "上下文已压缩：{method}，{tokens_before} → {tokens_after} tokens"
                    ),
                    error: false,
                }));
            }
            _ => {}
        }
    }

    fn apply_delta(&mut self, delta: AssistantDelta) {
        match delta {
            AssistantDelta::Text { delta } => {
                if !delta.is_empty()
                    && !matches!(
                        self.blocks.last().map(|entry| &entry.block),
                        Some(Block::Assistant { .. })
                    )
                {
                    self.blocks.push(Entry::new(Block::Assistant {
                        text: String::new(),
                    }));
                }
                if let Some(Entry {
                    block: Block::Assistant { text },
                    ..
                }) = self.blocks.last_mut()
                {
                    text.push_str(&delta);
                }
            }
            AssistantDelta::Reasoning { delta } => {
                if !delta.is_empty()
                    && !matches!(
                        self.blocks.last().map(|entry| &entry.block),
                        Some(Block::Thinking { .. })
                    )
                {
                    self.blocks.push(Entry::new(Block::Thinking {
                        text: String::new(),
                        reasoning_tokens: None,
                        duration_ms: None,
                    }));
                }
                if let Some(Entry {
                    block: Block::Thinking { text, .. },
                    ..
                }) = self.blocks.last_mut()
                {
                    text.push_str(&delta);
                }
            }
            AssistantDelta::ToolCallArgs {
                tool_call_id,
                delta,
            } => {
                if let Some(index) = self.tool_indices.get(&tool_call_id).copied()
                    && let Some(entry) = self.blocks.get_mut(index)
                {
                    entry.push_args(&delta);
                } else {
                    self.pending_args
                        .entry(tool_call_id)
                        .or_default()
                        .push_str(&delta);
                }
            }
            _ => {}
        }
    }

    fn rebuild_tool_indices(&mut self) {
        self.tool_indices.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
    }

    pub fn status_label(&self) -> &'static str {
        use moc_protocol::methods::AgentStatus;
        match self.info.status {
            AgentStatus::Queued => "排队中",
            AgentStatus::Running => "运行中",
            AgentStatus::Completed => "已完成",
            AgentStatus::Stopped => "已停止",
            AgentStatus::Failed => "失败",
            AgentStatus::Idle => "空闲",
        }
    }

    pub fn set_status(
        &mut self,
        status: moc_protocol::methods::AgentStatus,
        last_message: Option<String>,
    ) {
        self.info.status = status;
        self.info.updated_at_ms = crate::transcript::now_ms();
        self.running = matches!(
            status,
            moc_protocol::methods::AgentStatus::Queued
                | moc_protocol::methods::AgentStatus::Running
        );
        if last_message.is_some() {
            self.info.last_message = last_message;
        }
    }

    pub fn finish_tool_for_test(
        &mut self,
        id: &str,
        status: ToolStatus,
        result: moc_protocol::ToolResult,
    ) {
        if let Some(index) = self.tool_indices.get(id).copied()
            && let Some(entry) = self.blocks.get_mut(index)
        {
            entry.finish_tool(status, result, None, None);
        }
    }
}

#[derive(Debug, Clone)]
pub struct AgentOperationNotice {
    pub agent_id: String,
    pub message: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info() -> AgentInfo {
        AgentInfo {
            id: "ses_child".into(),
            parent_id: "ses_parent".into(),
            title: "child".into(),
            model: "test/model".into(),
            cwd: "/tmp".into(),
            status: moc_protocol::methods::AgentStatus::Running,
            created_at_ms: 1,
            updated_at_ms: 1,
            last_message: None,
        }
    }

    #[test]
    fn status_update_changes_running_card() {
        let mut view = SubagentView::new(info());
        view.set_status(
            moc_protocol::methods::AgentStatus::Completed,
            Some("done".into()),
        );
        assert!(!view.running);
        assert_eq!(view.status_label(), "已完成");
        assert_eq!(view.info.last_message.as_deref(), Some("done"));
    }
}

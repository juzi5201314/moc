use rust_i18n::t;

use super::App;
use super::trimmable_tail_block;
use crate::transcript::Block;
use crate::transcript::Entry;
use crate::transcript::ToolCallBlock;
use crate::transcript::ToolCallFinish;
use crate::transcript::ToolCallStart;

impl App {
    /// 找到指定调用 ID 的工具块（只读，O(1) 索引）。
    pub fn tool_block(&self, tool_call_id: &str) -> Option<&ToolCallBlock> {
        let index = *self.tool_indices.get(tool_call_id)?;
        match self.blocks.get(index).map(|entry| &entry.block) {
            Some(Block::Tool(tool)) if tool.id == tool_call_id => Some(tool),
            _ => None,
        }
    }

    /// 找到指定调用 ID 的工具条目。
    pub fn tool_entry_mut(&mut self, tool_call_id: &str) -> Option<&mut Entry> {
        let index = *self.tool_indices.get(tool_call_id)?;
        match self.blocks.get(index).map(|entry| &entry.block) {
            Some(Block::Tool(tool)) if tool.id == tool_call_id => self.blocks.get_mut(index),
            _ => None,
        }
    }

    /// 工具执行开始：把更新里的调用信息落地成转写块。
    ///
    /// 模型直发的调用与 `execute` 沙盒里的嵌套调用共用这里，形状只差 `parent_id`
    /// （嵌套调用据此在滚动区缩进）；批次归属在批次开始时就登记好了。
    pub fn start_tool_block(&mut self, call: ToolCallStart) {
        // 工具调用开始即思考流结束：耗时不能把随后的工具执行算进去。
        self.finish_thinking();
        let ToolCallStart {
            id,
            name,
            parent_id,
            arguments,
        } = call;
        let batch_id = self.tool_batches.remove(&id);
        if let Some(entry) = self.tool_entry_mut(&id) {
            if let Block::Tool(tool) = &mut entry.block {
                tool.name = name;
                tool.parent_id = parent_id;
                tool.arguments = arguments;
                tool.preview = None;
                tool.running = true;
                if batch_id.is_some() {
                    tool.batch_id = batch_id;
                }
                entry.touch();
            }
            return;
        }
        let index = self.blocks.len();
        self.blocks
            .push(Entry::new(Block::Tool(Box::new(ToolCallBlock {
                batch_id,
                id: id.clone(),
                parent_id,
                name,
                arguments,
                status: None,
                result: None,
                diff: None,
                duration_ms: None,
                running: true,
                progress: None,
                job_id: None,
                live_output: None,
                preview: None,
                images: Vec::new(),
            }))));
        self.tool_indices.insert(id, index);
    }

    /// 登记同一模型响应返回的工具批次。
    pub fn register_tool_batch(
        &mut self,
        batch_id: String,
        calls: impl IntoIterator<Item = String>,
    ) {
        for tool_call_id in calls {
            if let Some(entry) = self.tool_entry_mut(&tool_call_id) {
                if let Block::Tool(tool) = &mut entry.block {
                    tool.batch_id = Some(batch_id.clone());
                }
                entry.touch();
            } else {
                self.tool_batches.insert(tool_call_id, batch_id.clone());
            }
        }
    }

    /// 工具执行结束：回填终态。
    ///
    /// `name` / `parent_id` / `arguments` 只在调用块缺席时用得上：批次中断或前端中途
    /// 接入时更新里没有配对的 `ToolStarted`，这时用它们（以及本地已登记的批次）补建一个
    /// 与正常路径同形的终态块，而不是把结果丢掉。
    pub fn finish_tool_block(&mut self, finish: ToolCallFinish) {
        let ToolCallFinish {
            id,
            name,
            parent_id,
            arguments,
            status,
            result,
            duration_ms,
            diff,
        } = finish;
        if let Some(entry) = self.tool_entry_mut(&id) {
            if let Block::Tool(tool) = &mut entry.block {
                tool.name = name;
                tool.arguments = arguments;
                tool.parent_id = parent_id;
            }
            entry.finish_tool(status, result, duration_ms, diff);
        } else {
            // 前端中途接入时没有配对的开始更新：补建终态块前同样先定稿思考流。
            self.finish_thinking();
            let index = self.blocks.len();
            self.blocks
                .push(Entry::new(Block::Tool(Box::new(ToolCallBlock {
                    batch_id: self.tool_batches.remove(&id),
                    id: id.clone(),
                    parent_id,
                    name,
                    arguments,
                    status: Some(status),
                    result: Some(result.content),
                    diff,
                    duration_ms,
                    running: false,
                    progress: None,
                    job_id: None,
                    live_output: None,
                    preview: None,
                    // 正常路径在 `Entry::finish_tool` 里填；这条兜底分支不能把图丢掉
                    images: crate::transcript::ToolImage::from_sources(result.images),
                }))));
            self.tool_indices.insert(id, index);
        }
    }

    /// 预览按临时身份更新；正式身份只在确认后替换。
    pub fn preview_tool(
        &mut self,
        id: String,
        name: Option<String>,
        content: moc_protocol::ToolPreviewContent,
    ) {
        if self.tool_block(&id).is_none() {
            self.start_tool_block(ToolCallStart {
                id: id.clone(),
                name: name
                    .clone()
                    .unwrap_or_else(|| t!("tui.tool.fallback_name").into_owned()),
                parent_id: None,
                arguments: String::new(),
            });
        }
        if let Some(entry) = self.tool_entry_mut(&id) {
            if let Block::Tool(tool) = &mut entry.block {
                if let Some(name) = name {
                    tool.name = name;
                }
                tool.running = false;
                tool.preview = Some(crate::transcript::ToolPreview {
                    content,
                    queued: false,
                });
            }
            entry.touch();
        }
    }

    /// 空映射撤销全部未执行预览；执行中的卡片不受影响。
    pub fn resolve_tool_previews(&mut self, calls: Vec<moc_protocol::ToolPreviewResolution>) {
        let mapping: std::collections::HashMap<_, _> = calls
            .into_iter()
            .map(|call| (call.preview_id, call.tool_call_id))
            .collect();
        let mut old_index = 0;
        let mut removed_before_generation = 0;
        let before = self.blocks.len();
        self.blocks.retain_mut(|entry| {
            let keep = if let Block::Tool(tool) = &mut entry.block {
                if let Some(preview) = &mut tool.preview {
                    if let Some(id) = mapping.get(&tool.id) {
                        tool.id.clone_from(id);
                        preview.queued = true;
                        entry.touch();
                        true
                    } else {
                        false
                    }
                } else {
                    true
                }
            } else {
                true
            };
            if !keep && self.generation_start.is_some_and(|start| old_index < start) {
                removed_before_generation += 1;
            }
            old_index += 1;
            keep
        });
        if let Some(start) = &mut self.generation_start {
            *start -= removed_before_generation;
        }
        self.tool_indices.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
        if self.blocks.len() != before {
            self.view_gen = self.view_gen.wrapping_add(1);
        }
    }

    /// 流式尾部是否已有该类块。
    pub fn has_streaming(&self, matches: impl Fn(&Block) -> bool) -> bool {
        self.generation_start
            .is_none_or(|start| self.blocks.len() > start)
            && self
                .blocks
                .last()
                .is_some_and(|entry| matches(&entry.block))
    }

    /// 追加一条系统提示。
    pub fn push_note(&mut self, text: String, tone: crate::transcript::NoteTone) {
        self.blocks.push(Entry::new(Block::Note { text, tone }));
    }

    /// 隐式 todo 提醒：按工具样式插入一条占位块，让用户知道刚刚注入了一次提醒。
    pub fn push_todo_reminder(&mut self) {
        self.push_block(Block::Tool(Box::new(ToolCallBlock {
            id: format!("todo-reminder-{}", self.blocks.len()),
            batch_id: None,
            parent_id: None,
            name: "TodoReminder".into(),
            arguments: String::new(),
            status: Some(moc_protocol::methods::ToolStatus::Completed),
            result: None,
            diff: None,
            duration_ms: None,
            running: false,
            progress: None,
            job_id: None,
            live_output: None,
            preview: None,
            images: Vec::new(),
        })));
    }

    /// 更新运行中工具的进度行（按 `tool_call_id` 定位）。
    pub fn set_tool_progress(
        &mut self,
        tool_call_id: &str,
        rows: Vec<moc_protocol::methods::ToolProgressRow>,
    ) {
        if let Some(entry) = self.tool_entry_mut(tool_call_id) {
            let Block::Tool(tool) = &mut entry.block else {
                return;
            };
            tool.progress = Some(rows);
            entry.touch();
        }
    }

    /// 统计后台运行中的任务数量。
    pub fn background_job_count(&self) -> usize {
        self.blocks
            .iter()
            .filter(|entry| {
                if let Block::Tool(tool) = &entry.block {
                    tool.job_id.is_some()
                } else {
                    false
                }
            })
            .count()
    }

    /// 追加一条用户消息。
    pub fn push_user(&mut self, text: String) {
        self.blocks.push(Entry::timed(Block::User { text }));
    }

    /// 追加一条实时块（助手正文 / 思维链 / 工具调用）：带落地时刻。
    pub fn push_block(&mut self, block: Block) {
        self.blocks.push(Entry::timed(block));
    }

    pub fn start_generation(&mut self) {
        self.mcp_connecting.clear();
        self.resolve_tool_previews(Vec::new());
        // 新一轮生成开始：上一响应若没有正文/工具收尾，思考块在这里定稿。
        self.finish_thinking();
        self.thinking_has_block = false;
        self.generation_start = Some(self.blocks.len());
    }

    pub fn commit_generation(&mut self) {
        self.generation_start = None;
    }

    pub fn discard_generation(&mut self) {
        self.resolve_tool_previews(Vec::new());
        if let Some(start) = self.generation_start.take() {
            // 结果已独立落盘，不属于这次失败的模型生成；只扫描并移除临时尾部。
            self.blocks
                .extract_if(start.., |entry| {
                    !matches!(entry.block, Block::AgentResult { .. })
                })
                .for_each(drop);
            self.tool_indices.retain(|_, index| *index < start);
            self.view_gen = self.view_gen.wrapping_add(1);
        }
        self.thinking_started_at = None;
        self.thinking_has_block = false;
    }

    /// 撤掉上一条提示在转写尾部留下的块：那一轮已经被 `/retry` 从会话里剪掉了。
    ///
    /// 只撤「肯定不在会话里」的块——说明性提示、状态为取消的工具，以及（整轮被丢弃时）
    /// 没跑完的助手正文与思考；碰到已落库的内容（跑完的工具、用户消息……）就停手，
    /// 界面与会话不会因此错位。`keep_reply` 用于重放工具的情况：assistant 消息还留在
    /// 会话里，它的正文不能跟着撤。
    pub fn rewind_failed_turn(&mut self, keep_reply: bool) -> bool {
        let mut removed = false;
        while self
            .blocks
            .last()
            .is_some_and(|entry| trimmable_tail_block(&entry.block, keep_reply))
        {
            self.blocks.pop();
            removed = true;
        }
        if !removed {
            return false;
        }
        self.generation_start = self.generation_start.map(|_| self.blocks.len());
        self.tool_indices
            .retain(|_, index| *index < self.blocks.len());
        self.thinking_started_at = None;
        self.thinking_has_block = false;
        self.view_gen = self.view_gen.wrapping_add(1);
        true
    }

    /// 开始新的思考块：定稿上一块，并登记计时与 usage 回填目标。
    pub fn start_thinking(&mut self) {
        self.finish_thinking();
        self.thinking_has_block = true;
        self.thinking_started_at = Some(std::time::Instant::now());
        self.push_block(Block::Thinking {
            text: String::new(),
            reasoning_tokens: None,
            duration_ms: None,
        });
    }

    /// 结束当前思考块的流式增量并记录实际思考时长。
    ///
    /// 只在思考流真正结束的边界调用：正文/工具开始、下一轮生成开始、提示收尾。
    pub fn finish_thinking(&mut self) {
        let Some(started_at) = self.thinking_started_at.take() else {
            return;
        };
        let elapsed_ms = u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX);
        if let Some(entry) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.block, Block::Thinking { .. }))
            && let Block::Thinking { duration_ms, .. } = &mut entry.block
        {
            *duration_ms = Some(elapsed_ms);
            entry.touch();
        }
    }

    /// 提示收尾：把耗时与结束时刻回填到最后一条助手块（`Worked for` 尾注）。
    pub fn finish_prompt(&mut self) {
        self.resolve_tool_previews(Vec::new());
        self.finish_thinking();
        let elapsed = self.prompt_started_at.map(|at| at.elapsed());
        let was_cancel_requested = self.cancel_requested;
        self.reset_prompt_state();
        self.prompt_running = false;
        if was_cancel_requested {
            self.set_notice(t!("tui.event.stopped").into_owned());
        }
        let Some(elapsed) = elapsed else {
            return;
        };
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if let Some(entry) = self
            .blocks
            .iter_mut()
            .rev()
            .take_while(|entry| !matches!(entry.block, Block::User { .. }))
            .find(|entry| matches!(entry.block, Block::Assistant { .. }))
        {
            entry.stamp_prompt(crate::transcript::now_ms(), elapsed_ms);
        }
    }

    /// 在增量流末尾追加文本：已存在同类流式块则续写，否则由调用方先新建。
    pub fn append_streaming(
        &mut self,
        pick: impl Fn(&mut Block) -> Option<&mut String>,
        delta: &str,
    ) {
        let Some(entry) = self.blocks.last_mut() else {
            return;
        };
        let Some(text) = pick(&mut entry.block) else {
            return;
        };
        text.reserve(delta.len());
        text.push_str(delta);
        // 正文继续增长后表格序号与内容都可能漂移，旧图立刻作废。
        // 旧图的清屏由 graphics::emit 在帧后发现并标记，不在这里猜协议。
        entry.text_appended();
    }

    /// 追加受预算约束的流式文本；用于思考内容，避免单个块吞噬整个会话内存。
    pub fn append_streaming_limited(
        &mut self,
        pick: impl Fn(&mut Block) -> Option<&mut String>,
        delta: &str,
        max_bytes: usize,
        marker: &str,
    ) {
        let Some(entry) = self.blocks.last_mut() else {
            return;
        };
        let Some(text) = pick(&mut entry.block) else {
            return;
        };
        if text.len() >= max_bytes {
            return;
        }
        let remaining = max_bytes - text.len();
        if delta.len() <= remaining {
            text.reserve(delta.len());
            text.push_str(delta);
        } else {
            let mut marker_bytes = marker.len().min(remaining);
            while marker_bytes > 0 && !marker.is_char_boundary(marker_bytes) {
                marker_bytes -= 1;
            }
            let prefix_limit = remaining - marker_bytes;
            let mut prefix = prefix_limit.min(delta.len());
            while prefix > 0 && !delta.is_char_boundary(prefix) {
                prefix -= 1;
            }
            text.reserve(prefix + marker_bytes);
            text.push_str(&delta[..prefix]);
            text.push_str(&marker[..marker_bytes]);
        }
        entry.touch();
    }
}

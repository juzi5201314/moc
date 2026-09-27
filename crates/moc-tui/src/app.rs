//! 应用状态：会话、模型、转写块、折叠开关与界面数据。
//!
//! 状态集中在一个 `App` 结构上，`update` 全部由事件循环驱动；渲染层只读。

use std::collections::HashMap;

use moc_protocol::methods::{TodoPhase, ToolDiff, ToolStatus};
use moc_protocol::{ModelInfo, SessionInfo, ToolResult, Usage};
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;

/// tok/s 滑动窗口长度。
pub const SPEED_WINDOW: std::time::Duration = std::time::Duration::from_secs(10);

use crate::modal::Modal;
use crate::protocol::AppEvent;
use crate::theme::Theme;
use crate::transcript::{Block, Entry, ToolCallBlock};
use crate::widgets::completion::CompletionState;
use crate::widgets::prompt::PromptState;

/// 焦点所在的区域：滚动区浏览或输入框。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Focus {
    #[default]
    Prompt,
    Scrollback,
}

impl Focus {
    pub fn toggle(self) -> Self {
        match self {
            Focus::Prompt => Focus::Scrollback,
            Focus::Scrollback => Focus::Prompt,
        }
    }
}

/// 应用状态。
pub struct App {
    pub theme: &'static Theme,
    /// UI 事件队列的发送端（供异步任务投递结果，如剪贴板图片）。
    pub events: UnboundedSender<AppEvent>,
    /// 当前会话信息（模型、模式、cwd 都从这里读）。
    pub session: Option<SessionInfo>,
    /// 可选模型集合（`config/get` 拉取，/model 菜单与 statusline 用）。
    pub models: Vec<ModelInfo>,
    /// 转写条目（用户消息、回复、工具调用、todo……）。
    pub blocks: Vec<Entry>,
    /// Todo 清单（`TodoChanged` 最新快照）。
    pub todos: Vec<TodoPhase>,
    /// 思考内容全局可见（ctrl+t 切换）。
    pub show_thinking: bool,
    /// 工具调用全局折叠（ctrl+o 切换）。
    pub tools_collapsed: bool,
    /// 视图版本：全局折叠开关等影响全部块渲染的因素变化时自增。
    pub view_gen: u64,
    /// 输入框状态（多行编辑、历史、chip）。
    pub prompt: PromptState,
    /// 斜杠补全菜单；`None` = 关闭。
    pub completion: Option<CompletionState>,
    /// 打开的模态；键盘优先路由给它。
    pub modal: Option<Modal>,
    /// 待审批计划（`PlanSubmitted` 的最新一份）。
    pub pending_plan: Option<(String, String, String)>,
    /// 当前焦点。
    pub focus: Focus,
    /// 最近一次绘制的输入框区域，用于鼠标点击恢复焦点。
    pub prompt_area: Rect,
    /// 滚动偏移（自尾部算起的行数；0 = 跟随尾部）。
    pub scroll_offset: usize,
    /// 是否有在飞轮次。
    pub turn_running: bool,
    /// 当前轮次开始时间（statusline 计时）。
    pub turn_started_at: Option<std::time::Instant>,
    /// 最新一步的累计 token 用量（`AssistantDelta::Usage` 刷新）。
    pub usage: Usage,
    /// 输出 token 采样（tok/s 滑动窗口）。
    pub usage_samples: Vec<(std::time::Instant, u64)>,
    /// 会话累计费用（美元；需要模型 pricing 配置）。
    pub cost_usd: f64,
    /// git 分支（异步刷新）。
    pub git_branch: Option<String>,
    /// statusline 自定义模板（core 配置 `[tui] statusline`）。
    pub statusline_template: Option<String>,
    /// 最近一次绘制的转写区，用于鼠标选区的绝对坐标换算。
    pub scrollback_area: Rect,
    /// 视口命中表（点击折叠用）：`(块下标, 标题行, 标题后行)`。
    pub mouse_hits: Vec<(usize, u16, u16)>,
    /// 视口行文本（`(行, 文本)`，选择复制用）。
    pub paint_lines: Vec<(u16, String)>,
    /// 鼠标拖选区（屏幕坐标 (列, 行) 起点 / 终点）。
    pub selection: Option<((u16, u16), (u16, u16))>,
    /// 按下未抬起的位置（区分点击与拖选）。
    pub pending_click: Option<(u16, u16)>,
    /// ctrl+c / Esc 的二次确认状态。
    pub cancel_pending: bool,
    pub clear_pending: bool,
    /// 一次性提示（操作反馈、启动警告）；带 TTL 自动消失。
    pub notice: Option<String>,
    /// 当前 notice 的设置时刻（toast 超时判定）。
    pub notice_at: Option<std::time::Instant>,
    /// 退出标志。
    pub should_quit: bool,

    /// 工具调用 ID 到转写块下标的索引，工具参数增量走热路径。
    tool_indices: HashMap<String, usize>,
    /// 增量阶段累积的参数：`tool_call_id → JSON 片段拼接`（在工具块创建前到达）。
    pending_args: HashMap<String, String>,
}

impl App {
    pub fn new(theme: &'static Theme, events: UnboundedSender<AppEvent>) -> Self {
        Self {
            theme,
            events,
            session: None,
            models: Vec::new(),
            blocks: Vec::new(),
            todos: Vec::new(),
            show_thinking: true,
            tools_collapsed: false,
            view_gen: 0,
            prompt: PromptState::default(),
            completion: None,
            modal: None,
            pending_plan: None,
            focus: Focus::Prompt,
            prompt_area: Rect::default(),
            scroll_offset: 0,
            turn_running: false,
            turn_started_at: None,
            usage: Usage::default(),
            usage_samples: Vec::new(),
            cost_usd: 0.0,
            git_branch: None,
            statusline_template: None,
            scrollback_area: Rect::default(),
            mouse_hits: Vec::new(),
            paint_lines: Vec::new(),
            selection: None,
            pending_click: None,
            cancel_pending: false,
            clear_pending: false,
            notice: None,
            notice_at: None,
            should_quit: false,
            tool_indices: HashMap::new(),
            pending_args: HashMap::new(),
        }
    }

    /// 设置一次性提示并刷新计时。
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.notice_at = Some(std::time::Instant::now());
    }

    /// notice 是否仍在显示期内。
    pub fn notice_visible(&self) -> bool {
        const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
        match (&self.notice, self.notice_at) {
            (Some(_), Some(at)) => at.elapsed() < NOTICE_TTL,
            _ => false,
        }
    }

    /// 当前模型展示名。
    pub fn model_display(&self) -> &str {
        self.session
            .as_ref()
            .map(|session| session.model.as_str())
            .unwrap_or("未连接")
    }

    /// 找到指定调用 ID 的工具条目。
    pub fn tool_entry_mut(&mut self, tool_call_id: &str) -> Option<&mut Entry> {
        let index = *self.tool_indices.get(tool_call_id)?;
        match self.blocks.get(index).map(|entry| &entry.block) {
            Some(Block::Tool(tool)) if tool.id == tool_call_id => self.blocks.get_mut(index),
            _ => None,
        }
    }

    /// 工具执行开始：把增量阶段累积的信息落地成转写块。
    pub fn start_tool_block(&mut self, tool_call_id: String, name: String) {
        let arguments = self.pending_args.remove(&tool_call_id).unwrap_or_default();
        let index = self.blocks.len();
        self.blocks.push(Entry::new(Block::Tool(ToolCallBlock {
            id: tool_call_id.clone(),
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

    /// 工具执行结束：回填终态。
    pub fn finish_tool_block(
        &mut self,
        tool_call_id: String,
        status: ToolStatus,
        result: ToolResult,
        duration_ms: Option<u64>,
        diff: Option<ToolDiff>,
    ) {
        if let Some(entry) = self.tool_entry_mut(&tool_call_id) {
            entry.finish_tool(status, result, duration_ms, diff);
        } else {
            let index = self.blocks.len();
            self.blocks.push(Entry::new(Block::Tool(ToolCallBlock {
                id: tool_call_id.clone(),
                name: result.tool_call_id.clone(),
                arguments: String::new(),
                status: Some(status),
                result: Some(result.content),
                diff,
                duration_ms,
                running: false,
            })));
            self.tool_indices.insert(tool_call_id, index);
        }
    }

    /// 流式尾部是否已有该类块。
    pub fn has_streaming(&self, matches: impl Fn(&Block) -> bool) -> bool {
        self.blocks
            .last()
            .is_some_and(|entry| matches(&entry.block))
    }

    /// 追加一条系统提示。
    pub fn push_note(&mut self, text: String, error: bool) {
        self.blocks.push(Entry::new(Block::Note { text, error }));
    }

    /// 追加一条用户消息。
    pub fn push_user(&mut self, text: String) {
        self.blocks.push(Entry::timed(Block::User { text }));
    }

    /// 追加一条实时块（助手正文 / 思维链 / 工具调用）：带落地时刻。
    pub fn push_block(&mut self, block: Block) {
        self.blocks.push(Entry::timed(block));
    }

    /// 轮次收尾：把耗时与结束时刻回填到最后一条助手块（`Worked for` 尾注）。
    pub fn finish_turn(&mut self) {
        let elapsed = self.turn_started_at.map(|at| at.elapsed());
        self.turn_running = false;
        self.cancel_pending = false;
        self.turn_started_at = None;
        let Some(elapsed) = elapsed else {
            return;
        };
        let elapsed_ms = u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX);
        if let Some(entry) = self
            .blocks
            .iter_mut()
            .rev()
            .find(|entry| matches!(entry.block, Block::Assistant { .. }))
        {
            entry.stamp_turn(crate::transcript::now_ms(), elapsed_ms);
        }
    }

    /// 切换思考可见性并使视图缓存失效。
    pub fn toggle_thinking(&mut self) {
        self.show_thinking = !self.show_thinking;
        self.view_gen += 1;
    }

    /// 切换工具折叠并使视图缓存失效。
    pub fn toggle_tools(&mut self) {
        self.tools_collapsed = !self.tools_collapsed;
        self.view_gen += 1;
    }

    /// Todo 快照更新：转写里替换最后一个 Todo 块（避免重复树），否则追加。
    pub fn update_todos(&mut self, phases: Vec<TodoPhase>) {
        self.todos = phases.clone();
        let has_todo_block = self
            .blocks
            .iter()
            .any(|entry| matches!(entry.block, Block::Todo { .. }));
        if has_todo_block {
            // 替换最后一个 Todo 块：直接原地改写并推进版本
            if let Some(entry) = self
                .blocks
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.block, Block::Todo { .. }))
            {
                entry.block = Block::Todo { phases };
                entry.touch();
            }
        } else {
            self.blocks.push(Entry::new(Block::Todo { phases }));
        }
    }

    /// 打开模型选择器。
    pub fn open_model_picker(&mut self) {
        if self.models.is_empty() {
            self.set_notice("模型目录为空");
            return;
        }
        let current = self
            .session
            .as_ref()
            .map(|s| s.model.clone())
            .unwrap_or_default();
        let selected = self
            .models
            .iter()
            .position(|model| model.id == current || current.starts_with(&model.id))
            .unwrap_or(0);
        let mut picker = crate::modal::PickerState::default();
        picker.selected = selected;
        picker.offset = selected.saturating_sub(7);
        self.modal = Some(Modal::Model {
            models: self.models.clone(),
            current,
            picker,
        });
    }

    /// 打开会话恢复选择器（列表由调用方异步填充后更新）。
    pub fn open_session_picker(&mut self) {
        self.modal = Some(Modal::Sessions {
            sessions: Vec::new(),
            picker: crate::modal::PickerState {
                loading: true,
                ..Default::default()
            },
        });
    }

    /// 用加载的会话替换当前会话与转写。
    pub fn switch_session(&mut self, info: SessionInfo, blocks: Vec<Entry>) {
        self.session = Some(info);
        self.blocks = blocks;
        self.tool_indices.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
        self.scroll_offset = 0;
        self.view_gen += 1;
    }

    /// 在增量流末尾追加文本：已存在同类流式块则续写，否则由调用方先新建。
    pub fn append_streaming(
        &mut self,
        pick: impl Fn(&mut Block) -> Option<&mut String>,
        delta: &str,
    ) {
        // `pick` 借用 blocks：先定位（借用结束），再取目标下标续写
        let target_index = self
            .blocks
            .iter_mut()
            .rev()
            .position(|entry| pick(&mut entry.block).is_some())
            .map(|offset_from_end| self.blocks.len() - 1 - offset_from_end);
        let Some(target_index) = target_index else {
            return;
        };
        let entry = &mut self.blocks[target_index];
        if let Some(text) = pick(&mut entry.block) {
            text.push_str(delta);
        }
        entry.touch();
    }

    /// 累积「参数增量先于工具执行开始」到达的参数片段。
    pub fn push_pending_args(&mut self, tool_call_id: &str, delta: String) {
        self.pending_args
            .entry(tool_call_id.to_owned())
            .or_default()
            .push_str(&delta);
    }

    // ---------- 输入框编辑（委托给 PromptState） ----------

    /// 插入一个字符。
    pub fn prompt_insert_char(&mut self, ch: char) {
        self.prompt.insert_char(ch);
        self.completion = CompletionState::derive(&self.prompt.text);
    }

    /// 输入框当前文本快照。
    pub fn prompt_text(&self) -> &str {
        &self.prompt.text
    }

    // ---------- statusline 数据 ----------

    /// 当前模型的定价（从模型目录按 selector 匹配）。
    pub fn current_pricing(&self) -> Option<&moc_protocol::methods::ModelPricing> {
        let model = self.session.as_ref()?.model.as_str();
        let base = model.split(':').next()?;
        self.models
            .iter()
            .find(|candidate| candidate.id == base)
            .as_ref()
            .and_then(|info| info.pricing.as_ref())
    }

    /// 当前模型的上下文窗口。
    pub fn context_window(&self) -> Option<u64> {
        let model = self.session.as_ref()?.model.as_str();
        let base = model.split(':').next()?;
        self.models
            .iter()
            .find(|candidate| candidate.id == base)
            .and_then(|info| info.context_window)
    }

    /// 应用一次 usage 刷新：费用累计 + tok/s 窗口采样。
    pub fn apply_usage(&mut self, usage: Usage) {
        let previous = &self.usage;
        let delta_in = usage.input_tokens.saturating_sub(previous.input_tokens);
        let delta_out = usage.output_tokens.saturating_sub(previous.output_tokens);
        let delta_cached = usage
            .cached_input_tokens
            .unwrap_or(0)
            .saturating_sub(previous.cached_input_tokens.unwrap_or(0));
        if delta_in > 0 || delta_out > 0 {
            if let Some(pricing) = self.current_pricing() {
                // input_tokens 已折入缓存命中：非命中输入单独按全价计
                let uncached = delta_in.saturating_sub(delta_cached);
                let cache_price = pricing.cache_read.unwrap_or(pricing.input);
                self.cost_usd += uncached as f64 / 1e6 * pricing.input
                    + delta_out as f64 / 1e6 * pricing.output
                    + delta_cached as f64 / 1e6 * cache_price;
            }
            self.usage_samples
                .push((std::time::Instant::now(), usage.output_tokens));
            let cutoff = std::time::Instant::now() - SPEED_WINDOW;
            self.usage_samples.retain(|(at, _)| *at >= cutoff);
        }
        self.usage = usage;
    }

    /// 滑动窗口内的输出速率（tok/s）。
    pub fn tokens_per_second(&self) -> f64 {
        let Some((oldest, first_tokens)) = self.usage_samples.first() else {
            return 0.0;
        };
        let Some((newest, last_tokens)) = self.usage_samples.last() else {
            return 0.0;
        };
        let elapsed = newest.saturating_duration_since(*oldest).as_secs_f64();
        if elapsed <= 0.0 {
            return 0.0;
        }
        last_tokens.saturating_sub(*first_tokens) as f64 / elapsed
    }

    /// todo 完成进度 `(done, total)`。
    pub fn todo_progress(&self) -> (usize, usize) {
        let total: usize = self.todos.iter().map(|phase| phase.items.len()).sum();
        let done = self
            .todos
            .iter()
            .flat_map(|phase| phase.items.iter())
            .filter(|item| item.status == moc_protocol::methods::TodoStatus::Completed)
            .count();
        (done, total)
    }
}

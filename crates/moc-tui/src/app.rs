//! 应用状态：会话、模型、转写块、折叠开关与界面数据。
//!
//! 状态集中在一个 `App` 结构上，`update` 全部由事件循环驱动；渲染层只读。

use std::collections::HashMap;

use moc_protocol::methods::{TodoPhase, ToolDiff, ToolStatus};
use moc_protocol::{ContextUsageResult, ModelInfo, SessionInfo, SessionMode, ToolResult, Usage};

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

/// 一次性提示（notice）的存活时长。
const NOTICE_TTL: std::time::Duration = std::time::Duration::from_secs(3);
/// 上游配置不一致告警的存活时长：比 notice 长，避免用户错过静默改写。
const WARNING_TTL: std::time::Duration = std::time::Duration::from_secs(8);
/// Ctrl+C 二次确认的动作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CtrlCPending {
    /// 输入框非空时首次按 Ctrl+C，再次按下清空输入框。
    ClearPrompt,
    /// 输入框为空（或中止 Prompt 后）首次按 Ctrl+C，再次按下退出应用。
    Quit,
}

/// Ctrl+C 二次确认有效窗口（3 秒内）。
pub const CTRL_C_TTL: std::time::Duration = std::time::Duration::from_secs(3);

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
    /// 是否仍有未加载的更早 transcript。
    pub history_has_more: bool,
    /// 下一页应使用的游标（当前最早条目的 ID）。
    pub history_before_entry_id: Option<String>,
    /// 避免滚动期间重复发送历史请求。
    pub history_loading: bool,
    /// 滚动到历史边缘后由事件循环发起异步分页。
    pub history_load_requested: bool,
    /// 最近一次渲染计算出的可滚动总偏移。
    pub scroll_max_offset: usize,
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
    /// 输入文本变更代次；异步资源补全只接受最新代次。
    pub prompt_revision: u64,
    /// 当前资源补全请求的 prefix。
    pub completion_prefix: Option<String>,
    /// 打开的模态；键盘优先路由给它。
    pub modal: Option<Modal>,
    /// 待审批计划（`PlanSubmitted` 的最新一份）。
    pub pending_plan: Option<(String, String, String)>,
    /// 当前焦点。
    pub focus: Focus,
    /// 最近一次绘制的输入框区域，用于鼠标点击恢复焦点。
    pub prompt_area: Rect,
    /// 顶部上下文占用区域，用于鼠标点击打开详情。
    pub context_area: Rect,
    /// 输入框下边框右端模式名的屏幕区域（鼠标点击就地弹出模式菜单）。
    pub mode_area: Rect,
    /// 就地模式菜单；`None` = 关闭。
    pub mode_menu: Option<ModeMenu>,
    /// 当前这条用户消息的临时模式覆盖（`/ask` 注入）；整条消息回复完毕才清空。
    ///
    /// 只影响图例与状态栏的展示，会话持久模式仍以 `session.mode` 为准。
    pub transient_mode: Option<SessionMode>,
    /// Core 估算的上下文组成与总量。
    pub context_usage: Option<ContextUsageResult>,
    /// 滚动偏移（自尾部算起的行数；0 = 跟随尾部）。
    pub scroll_offset: usize,
    /// 是否有进行中的提示。
    pub prompt_running: bool,
    /// 是否已经发出取消当前提示的请求。
    pub cancel_requested: bool,
    /// 当前提示开始时间（statusline 计时）。
    pub prompt_started_at: Option<std::time::Instant>,
    /// 当前 thinking block 开始时间（用于计算 duration_ms）。
    pub thinking_started_at: Option<std::time::Instant>,
    pub usage: Usage,
    /// 输出 token 采样（tok/s 滑动窗口）。
    pub usage_samples: Vec<(std::time::Instant, u64)>,
    /// 会话累计费用（美元；需要模型 pricing 配置）。
    pub cost_usd: f64,
    /// git 分支（异步刷新）。
    pub git_branch: Option<String>,
    /// 独立 tui.toml 的 statusline 模板。
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
    /// ctrl+c 二次确认状态及触发时刻（3 秒内有效）。
    pub ctrl_c_pending: Option<(CtrlCPending, std::time::Instant)>,
    /// Esc 二次确认状态。
    pub clear_pending: bool,
    /// 一次性提示（操作反馈、启动警告）；带 TTL 自动消失。
    pub notice: Option<String>,
    /// 当前 notice 的设置时刻（toast 超时判定）。
    pub notice_at: Option<std::time::Instant>,
    /// 上游实际配置与请求不一致的告警；同一时刻最多一条，新告警替换旧告警。
    pub warning: Option<String>,
    /// 当前 warning 的设置时刻（超时判定）。
    pub warning_at: Option<std::time::Instant>,
    /// 退出标志。
    pub terminal_focused: bool,
    pub should_quit: bool,
    pub preferences: crate::config::TuiConfig,
    pub keymap: crate::config::KeyMap,
    pub tui_store: Option<crate::config::TuiStore>,
    pub settings: Option<crate::settings::SettingsPanel>,
    pub next_config_id: u64,
    /// 设置页正在写入的配置范围；`Some` 时新改动先排队。
    pub config_flight: Option<moc_protocol::ConfigScope>,
    /// 排队等待写入的配置改动；写入串行，期间的改动合并到最近一次。
    pub config_queue: Option<ConfigWrite>,
    pub startup_cwd: String,
    pub startup_mode: SessionMode,
    pub startup_pending: Option<String>,
    pub subagents: std::collections::BTreeMap<String, crate::subagent::SubagentView>,
    pub subagent_selected: Option<String>,
    pub subagent_area: Rect,
    pub subagent_hits: Vec<(String, Rect)>,
    pub subagent_generation: u64,
    pub queues: HashMap<String, std::collections::VecDeque<crate::queue::QueuedPrompt>>,
    pub inflight_prompts: std::collections::HashSet<String>,
    pub next_queue_id: u64,
    pub queue_hits: Vec<(u64, Rect, Rect)>,
    pub queue_area: Rect,
    pub queue_offset: usize,
    pub interrupt_pending: std::collections::HashSet<String>,
    pub prompt_stash: Option<PromptState>,

    /// 工具调用 ID 到批次 ID 的暂存映射，批次开始事件先于各个工具开始事件。
    tool_batches: HashMap<String, String>,
    /// 工具调用 ID 到转写块下标的索引，工具参数增量走热路径。
    tool_indices: HashMap<String, usize>,
    /// 增量阶段累积的参数：`tool_call_id → JSON 片段拼接`（在工具块创建前到达）。
    pending_args: HashMap<String, String>,
}

/// 排队等待写入的配置改动。
pub struct ConfigWrite {
    pub scope: moc_protocol::ConfigScope,
    pub cwd: String,
    pub document: serde_json::Value,
    /// 上一次成功写入后的文件版本；写入回执会刷新它。
    pub revision: String,
}

impl App {
    pub fn new(theme: &'static Theme, events: UnboundedSender<AppEvent>) -> Self {
        Self {
            theme,
            events,
            session: None,
            models: Vec::new(),
            blocks: Vec::new(),
            history_has_more: false,
            history_before_entry_id: None,
            history_loading: false,
            history_load_requested: false,
            scroll_max_offset: 0,
            todos: Vec::new(),
            show_thinking: true,
            tools_collapsed: false,
            view_gen: 0,
            prompt: PromptState::default(),
            completion: None,
            prompt_revision: 0,
            completion_prefix: None,
            modal: None,
            pending_plan: None,
            focus: Focus::Prompt,
            prompt_area: Rect::default(),
            context_area: Rect::default(),
            mode_area: Rect::default(),
            mode_menu: None,
            transient_mode: None,
            context_usage: None,
            scroll_offset: 0,
            prompt_running: false,
            cancel_requested: false,
            prompt_started_at: None,
            thinking_started_at: None,
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
            ctrl_c_pending: None,
            clear_pending: false,
            notice: None,
            notice_at: None,
            warning: None,
            warning_at: None,
            terminal_focused: true,
            should_quit: false,
            preferences: crate::config::TuiConfig::default(),
            keymap: crate::config::KeyMap::compile(&crate::config::TuiConfig::default())
                .expect("内置键位合法"),
            tui_store: None,
            settings: None,
            next_config_id: 0,
            config_flight: None,
            config_queue: None,
            startup_cwd: String::new(),
            startup_mode: SessionMode::Normal,
            startup_pending: None,
            subagents: std::collections::BTreeMap::new(),
            subagent_selected: None,
            subagent_area: Rect::default(),
            subagent_hits: Vec::new(),
            subagent_generation: 0,
            queues: HashMap::new(),
            inflight_prompts: std::collections::HashSet::new(),
            next_queue_id: 0,
            queue_hits: Vec::new(),
            queue_area: Rect::default(),
            queue_offset: 0,
            interrupt_pending: std::collections::HashSet::new(),
            prompt_stash: None,
            tool_batches: HashMap::new(),
            tool_indices: HashMap::new(),
            pending_args: HashMap::new(),
        }
    }

    /// 写入配置：有写入在途时先排队，等回执落地后带着新版本接着写。
    pub fn write_config(
        &mut self,
        connection: &crate::protocol::EmbeddedConnection,
        write: ConfigWrite,
    ) {
        if self.config_flight.is_some() {
            self.config_queue = Some(write);
            return;
        }
        self.config_flight = Some(write.scope);
        let client = connection.client.clone();
        let events = self.events.clone();
        let panel = self.settings.as_ref().map_or(0, |panel| panel.id);
        let action = moc_protocol::ConfigAction::Save {
            scope: write.scope,
            cwd: write.cwd,
            document: write.document,
            revision: write.revision,
        };
        tokio::spawn(async move {
            let result = client
                .call::<moc_protocol::ConfigManage>(action)
                .await
                .map_err(anyhow::Error::from);
            let _ = events.send(AppEvent::SettingsResponse {
                panel,
                job: crate::settings::Job::Save,
                result,
            });
        });
    }

    /// 是否有配置写入在途或排队。
    pub fn saving_config(&self) -> bool {
        self.config_flight.is_some() || self.config_queue.is_some()
    }

    /// 设置一次性提示并刷新计时。
    pub fn set_notice(&mut self, text: impl Into<String>) {
        self.notice = Some(text.into());
        self.notice_at = Some(std::time::Instant::now());
    }

    /// 设置上游配置不一致告警并刷新计时。
    ///
    /// 只保留最新一条：同一次提示内多次不一致（多轮各自一次请求）会彼此
    /// 覆盖，绝不堆叠。
    pub fn set_warning(&mut self, text: impl Into<String>) {
        self.warning = Some(text.into());
        self.warning_at = Some(std::time::Instant::now());
    }

    /// notice 的消失时刻；`None` 表示当前没有显示中的 notice。
    ///
    /// 只返回仍在未来的时刻，事件循环才能安全地 `sleep_until`——返回已过去的
    /// 时刻会让 select 立刻再次命中，形成空转。
    pub fn notice_deadline(&self) -> Option<std::time::Instant> {
        let at = self.notice_at.filter(|_| self.notice.is_some())?;
        let deadline = at + NOTICE_TTL;
        (std::time::Instant::now() < deadline).then_some(deadline)
    }

    /// 检查当前是否存在指定类型且在 3 秒 TTL 内有效的 Ctrl+C 待确认状态。
    pub fn has_ctrl_c_pending(&self, expected: CtrlCPending) -> bool {
        match self.ctrl_c_pending {
            Some((kind, at)) => kind == expected && at.elapsed() <= CTRL_C_TTL,
            None => false,
        }
    }

    /// 消费指定类型的 Ctrl+C 待确认状态，若在 3 秒 TTL 内有效则重置为 None 并返回 true。
    pub fn take_ctrl_c_pending(&mut self, expected: CtrlCPending) -> bool {
        if self.has_ctrl_c_pending(expected) {
            self.ctrl_c_pending = None;
            true
        } else {
            self.ctrl_c_pending = None;
            false
        }
    }

    /// notice 是否仍在显示期内。
    pub fn notice_visible(&self) -> bool {
        self.notice_deadline().is_some()
    }

    /// warning 的消失时刻；`None` 表示当前没有显示中的告警。语义同 [`Self::notice_deadline`]。
    pub fn warning_deadline(&self) -> Option<std::time::Instant> {
        let at = self.warning_at.filter(|_| self.warning.is_some())?;
        let deadline = at + WARNING_TTL;
        (std::time::Instant::now() < deadline).then_some(deadline)
    }

    /// warning 是否仍在显示期内。
    pub fn warning_visible(&self) -> bool {
        self.warning_deadline().is_some()
    }

    /// 当前模型展示名；未连接时显示 `idle`。
    pub fn model_display(&self) -> &str {
        self.session
            .as_ref()
            .map_or("idle", |session| model_selector_label(&session.model))
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
        let batch_id = self.tool_batches.remove(&tool_call_id);
        self.start_tool_block_with_parent(tool_call_id, name, None, batch_id);
    }

    /// 登记同一模型响应返回的工具批次。
    pub fn register_tool_batch(
        &mut self,
        batch_id: String,
        calls: impl IntoIterator<Item = String>,
    ) {
        for tool_call_id in calls {
            self.tool_batches.insert(tool_call_id, batch_id.clone());
        }
    }
    /// 普通工具批次中的调用开始。
    pub fn start_batched_tool_block(
        &mut self,
        tool_call_id: String,
        name: String,
        batch_id: String,
    ) {
        self.start_tool_block_with_parent(tool_call_id, name, None, Some(batch_id));
    }

    /// PTC 子工具开始：保留父调用关系供滚动区缩进展示。
    pub fn start_child_tool_block(
        &mut self,
        tool_call_id: String,
        name: String,
        parent_id: String,
    ) {
        self.start_tool_block_with_parent(tool_call_id, name, Some(parent_id), None);
    }

    fn start_tool_block_with_parent(
        &mut self,
        tool_call_id: String,
        name: String,
        parent_id: Option<String>,
        batch_id: Option<String>,
    ) {
        let arguments = self.pending_args.remove(&tool_call_id).unwrap_or_default();
        let index = self.blocks.len();
        self.blocks.push(Entry::new(Block::Tool(ToolCallBlock {
            id: tool_call_id.clone(),
            batch_id,
            parent_id,
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
                batch_id: None,
                parent_id: None,
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
    /// 结束当前思考块并记录实际思考时长。
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
        self.finish_thinking();
        let elapsed = self.prompt_started_at.map(|at| at.elapsed());
        let was_cancel_requested = self.cancel_requested;
        self.prompt_running = false;
        self.cancel_requested = false;
        self.ctrl_c_pending = None;
        self.prompt_started_at = None;
        self.transient_mode = None;
        if was_cancel_requested {
            self.set_notice("Stopped");
        }
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
            entry.stamp_prompt(crate::transcript::now_ms(), elapsed_ms);
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
            self.set_notice("没有可用的模型，请先在设置中添加");
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
        let picker = crate::modal::PickerState {
            selected,
            offset: selected.saturating_sub(7),
            ..Default::default()
        };
        self.modal = Some(Modal::Model {
            models: self.models.clone(),
            current,
            picker,
        });
    }

    /// 打开就地模式菜单：高亮当前模式，并让出可能叠在输入框上方的补全菜单。
    pub fn open_mode_menu(&mut self) {
        if self.session.is_none() {
            self.set_notice("当前没有活动会话");
            return;
        }
        self.completion = None;
        let current = self.display_mode();
        self.mode_menu = Some(ModeMenu {
            selected: current
                .and_then(|mode| MODES.iter().position(|candidate| *candidate == mode))
                .unwrap_or(0),
            area: Rect::default(),
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

    /// 打开消息回退选择器；数据由调用方异步加载。
    pub fn open_rewind(&mut self) {
        self.modal = Some(Modal::Rewind {
            state: crate::modal::RewindState::default(),
        });
    }

    /// 用加载的会话替换当前会话与转写。
    pub fn switch_session(&mut self, info: SessionInfo, blocks: Vec<Entry>) {
        let clm_active = info.mode == SessionMode::ClmExperimental;
        self.session = Some(info);
        self.blocks = blocks;
        self.context_usage = None;
        if clm_active {
            self.set_notice("CLM 实验模式已启用：模型将维护实时上下文");
        }
        self.tool_indices.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
        self.scroll_offset = 0;
        self.history_has_more = false;
        self.history_before_entry_id = None;
        self.history_loading = false;
        self.history_load_requested = false;
        self.scroll_max_offset = 0;
        self.view_gen += 1;
    }

    pub fn set_history_cursor(&mut self, has_more: bool, before_entry_id: Option<String>) {
        self.history_has_more = has_more;
        self.history_before_entry_id = before_entry_id;
        self.history_loading = false;
    }

    pub fn begin_history_load(&mut self) -> bool {
        if self.history_loading || !self.history_has_more {
            return false;
        }
        self.history_loading = true;
        true
    }

    pub fn prepend_history(&mut self, mut blocks: Vec<Entry>) {
        if blocks.is_empty() {
            self.history_loading = false;
            return;
        }
        blocks.append(&mut self.blocks);
        self.blocks = blocks;
        self.tool_batches.clear();
        for (index, entry) in self.blocks.iter().enumerate() {
            if let Block::Tool(tool) = &entry.block {
                self.tool_indices.insert(tool.id.clone(), index);
            }
        }
        self.view_gen = self.view_gen.wrapping_add(1);
        self.history_loading = false;
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
        entry.touch();
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

    /// 累积「参数增量先于工具执行开始」到达的参数片段。
    pub fn push_pending_args(&mut self, tool_call_id: &str, delta: String) {
        self.pending_args
            .entry(tool_call_id.to_owned())
            .or_default()
            .push_str(&delta);
    }

    // ---------- 输入框编辑（委托给 PromptState） ----------

    pub fn prompt_changed(&mut self) {
        self.prompt_revision = self.prompt_revision.wrapping_add(1);
        self.completion = CompletionState::derive(&self.prompt.text);
        self.completion_prefix = None;
    }

    /// 插入一个字符。
    pub fn prompt_insert_char(&mut self, ch: char) {
        self.prompt.insert_char(ch);
        self.prompt_changed();
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

    /// 当前上下文占用：请求实测值优先，未请求时使用 core 估算值。
    pub fn context_used(&self) -> u64 {
        if self.usage.input_tokens > 0 {
            self.usage.input_tokens
        } else {
            self.context_usage
                .as_ref()
                .map_or(0, |usage| usage.total_tokens)
        }
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
        // 将 reasoning_tokens 更新到最近的 Thinking block
        if let Some(reasoning_tokens) = usage.reasoning_tokens
            && let Some(entry) = self
                .blocks
                .iter_mut()
                .rev()
                .find(|entry| matches!(entry.block, Block::Thinking { .. }))
            && let Block::Thinking {
                reasoning_tokens: stored,
                ..
            } = &mut entry.block
            && *stored != Some(reasoning_tokens)
        {
            *stored = Some(reasoning_tokens);
            entry.touch();
        }
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

    /// 图例与状态栏展示的模式：本次提示覆盖优先，否则取会话模式。
    pub fn display_mode(&self) -> Option<SessionMode> {
        self.transient_mode
            .or_else(|| self.session.as_ref().map(|session| session.mode))
    }
}

#[cfg(test)]
mod streaming_budget_tests {
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
}

/// 会话模式的展示名；CLM 以括号标识，保持实验状态始终可见。
pub fn mode_label(mode: SessionMode) -> &'static str {
    match mode {
        SessionMode::Normal => "normal",
        SessionMode::Ask => "ask",
        SessionMode::Plan => "plan",
        SessionMode::Ptc => "ptc",
        SessionMode::ClmExperimental => "(clm)",
    }
}

/// 会话模式顺序：就地菜单条目与键盘循环共用同一份顺序。
pub const MODES: [SessionMode; 4] = [
    SessionMode::Normal,
    SessionMode::Plan,
    SessionMode::Ptc,
    SessionMode::ClmExperimental,
];

/// 点击图例模式名后就地弹出的菜单。
pub struct ModeMenu {
    /// 键盘 / 悬停高亮的条目下标（[`MODES`] 顺序）。
    pub selected: usize,
    /// 上一帧绘制的菜单区域；鼠标命中使用。
    pub area: Rect,
}

impl ModeMenu {
    /// 命中的条目下标；点在边框和菜单外返回 `None`。
    pub fn entry_at(&self, column: u16, row: u16) -> Option<usize> {
        if !self.area.contains((column, row).into()) {
            return None;
        }
        let index = usize::from(row.checked_sub(self.area.y.saturating_add(1))?);
        (index < MODES.len()).then_some(index)
    }
}

/// 模型展示名：选择串即 `供应商/模型:等级`，思考关闭时省掉 `:off` 后缀。
pub fn model_selector_label(selector: &str) -> &str {
    selector.strip_suffix(":off").unwrap_or(selector)
}

#[cfg(test)]
mod tests {
    use super::{ModeMenu, mode_label, model_selector_label};
    use moc_protocol::SessionMode;
    use ratatui::layout::Rect;

    /// 模型与模式在界面上一律用英文标识，且思考关闭时不出现在模型名里。
    #[test]
    fn labels_stay_english() {
        assert_eq!(model_selector_label("test/model:off"), "test/model");
        assert_eq!(model_selector_label("test/model:high"), "test/model:high");
        assert_eq!(model_selector_label("test/model"), "test/model");
        assert_eq!(mode_label(SessionMode::Normal), "normal");
        assert_eq!(mode_label(SessionMode::Ask), "ask");
        assert_eq!(mode_label(SessionMode::Plan), "plan");
        assert_eq!(mode_label(SessionMode::Ptc), "ptc");
        assert_eq!(mode_label(SessionMode::ClmExperimental), "(clm)");
    }

    /// 菜单命中只认边框之间的一行一条目；边框与菜单外都不算条目。
    #[test]
    fn mode_menu_hit_testing_skips_borders() {
        let menu = ModeMenu {
            selected: 1,
            area: Rect::new(10, 5, 24, 6),
        };
        assert_eq!(menu.entry_at(11, 5), None, "上边框不是条目");
        assert_eq!(menu.entry_at(11, 6), Some(0));
        assert_eq!(menu.entry_at(11, 8), Some(2));
        assert_eq!(menu.entry_at(11, 9), Some(3));
        assert_eq!(menu.entry_at(11, 10), None, "下边框不是条目");
        assert_eq!(menu.entry_at(9, 6), None, "左缘之外不命中");
        assert_eq!(menu.entry_at(34, 6), None, "右缘之外不命中");
        assert_eq!(menu.entry_at(11, 4), None, "菜单上方不命中");
    }

    /// 没有活动会话就不开菜单，只提示（此时图例本身显示 `idle`）。
    #[test]
    fn mode_menu_requires_an_active_session() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.open_mode_menu();
        assert!(app.mode_menu.is_none());
        assert_eq!(app.notice.as_deref(), Some("当前没有活动会话"));
    }

    #[test]
    fn finish_turn_reports_stop_after_cancel_request() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);
        app.prompt_running = true;
        app.cancel_requested = true;
        app.finish_prompt();

        assert!(!app.prompt_running);
        assert!(!app.cancel_requested);
        assert_eq!(app.notice.as_deref(), Some("Stopped"));
    }

    /// 告警与 notice 的 TTL：期内可见且给出未来的截止时刻，过期后两者都消失。
    #[test]
    fn warning_and_notice_expire_after_their_ttl() {
        let (events, _receiver) = tokio::sync::mpsc::unbounded_channel();
        let mut app = super::App::new(crate::theme::Theme::global(), events);

        app.set_warning("effort max → high");
        app.set_notice("Stopped");
        assert!(app.warning_visible());
        assert!(app.notice_visible());
        // 截止时刻必须落在未来，事件循环才能安全 sleep_until 而非空转
        assert!(app.warning_deadline().unwrap() > std::time::Instant::now());
        assert!(app.notice_deadline().unwrap() > std::time::Instant::now());

        let stale = std::time::Instant::now() - std::time::Duration::from_secs(600);
        app.warning_at = Some(stale);
        app.notice_at = Some(stale);
        assert!(!app.warning_visible());
        assert!(!app.notice_visible());
        assert!(app.warning_deadline().is_none());
        assert!(app.notice_deadline().is_none());
        // 过期的槽位不再需要重绘，事件循环据此不再安排超时分支
        assert_eq!(app.warning.as_deref(), Some("effort max → high"));
    }
}

use moc_i18n::t;

use super::App;
use super::Focus;
use crate::transcript::Block;
use crate::transcript::Entry;
use moc_protocol::SessionEngine;
use moc_protocol::SessionInfo;
use moc_protocol::Usage;
use ratatui::layout::Rect;

impl App {
    /// 代理面（名册、侧栏）挂靠的会话：聚焦子代理时仍是主会话。
    ///
    /// 人类管理的是「我这一支的子代理」，不是子代理眼里的兄弟；
    /// 模型面的 `list_agents` 在子代理里返回兄弟，那是给模型看的视角。
    pub fn agents_root_id(&self) -> Option<String> {
        match &self.focused {
            Some(focused) => Some(focused.home.id.clone()),
            None => self.session.as_ref().map(|session| session.id.clone()),
        }
    }

    /// 是否聚焦在某个子代理上。
    pub fn focused_agent(&self) -> bool {
        self.focused.is_some()
    }

    /// 取当前会话 ID；没有会话时提示先去配置模型。
    pub fn require_session(&mut self) -> Option<String> {
        let Some(session) = self.session.as_ref() else {
            self.set_notice(t!("tui.event.no_model_hint").into_owned());
            return None;
        };
        Some(session.id.clone())
    }

    /// 用加载的会话替换当前会话与转写。
    ///
    /// 前端同时只呈现一个会话：上一个会话的提示计时、取消标记、临时模式、用量与
    /// todo 都在此收尾，否则队列不再派发、状态栏与停止键都停留在上一会话。
    /// 会话范围的在飞记账（`inflight_prompts`/`inflight_session_ops`/`interrupt_pending`）
    /// 按会话保留：切回仍在运行的会话时，「运行中」的判定与排队派发才正确。
    pub fn switch_session(&mut self, info: SessionInfo, blocks: Vec<Entry>) {
        if let Some(previous) = &self.session
            && previous.id != info.id
        {
            self.btw.leave_session(&previous.id);
            let previous_id = previous.id.clone();
            self.cancel_session_modals(&previous_id);
            if self.focus == Focus::Btw {
                self.focus = Focus::Prompt;
            }
        }
        let clm_active = info.engine == SessionEngine::ClmExperimental;
        let session_id = info.id.clone();
        self.reset_prompt_state();
        // 默认按可写处理；只读加载由 `AppEvent::Loaded` 在处理完本函数后标回。
        self.read_only = false;
        self.compaction_progress = None;
        self.prompt_running = self.inflight_prompts.contains(&session_id);
        self.usage = Usage::default();
        // 用量统计按会话重取；在飞请求都属于上一个会话，水位线一抬就直接作废。
        self.session_usage = None;
        self.usage_applied = self.usage_request;
        self.cost_usd = 0.0;
        self.todos.clear();
        self.todo_hud.reset();
        self.goal = None;
        self.session = Some(info);
        self.blocks = blocks;
        self.context_usage = None;
        self.plan_body_area = Rect::default();
        self.plan_row_map.clear();
        self.plan_line_sources.clear();
        // 切会话丢弃停放的计划审阅：它锚定的是上一个会话的待审计划。
        self.parked_plan = None;
        self.model_picker_anchor = None;
        if clm_active {
            self.set_notice(t!("tui.event.clm_notice").into_owned());
        }
        self.tool_indices.clear();
        // 批次暂存只服务于当前视图的流：转写里已有批次归属的完整形态，
        // 换会话后旧的批次表中的调用既无块可续、也可能被兜底补建误用。
        self.tool_batches.clear();
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
        // 「按项目」范围下，召回历史要跟着会话的 cwd 走。
        self.refresh_input_history();
    }

    /// 收掉与「当前提示」绑定的本地状态；[`App::finish_prompt`] 与会话切换共用。
    ///
    /// `prompt_running` 不在其中：提示收尾要清它，切会话则按会话级在飞记账重算。
    pub(super) fn reset_prompt_state(&mut self) {
        self.resolve_tool_previews(Vec::new());
        // 提示收尾或切会话：在途的重试倒数先定稿，不留悬挂状态。
        self.finish_retry_wait();
        self.mcp_connecting.clear();
        self.thinking_started_at = None;
        self.thinking_has_block = false;
        self.generation_start = None;
        self.prompt_started_at = None;
        self.cancel_requested = false;
        self.ctrl_c_pending = None;
        self.transient_mode = None;
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
        if let Some(start) = &mut self.generation_start {
            *start += blocks.len();
        }
        if let Some(wait) = &mut self.retry_wait {
            wait.note_index += blocks.len();
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

    /// 路径相对化的基准目录：有会话时用会话 cwd，否则用启动目录
    /// （与 [`Self::history_project`] 同一口径）。
    pub(crate) fn cwd(&self) -> &str {
        self.session
            .as_ref()
            .map_or(self.startup_cwd.as_str(), |session| session.cwd.as_str())
    }

    /// 历史归属的项目目录：有会话时用会话 cwd，否则用启动目录。
    pub(super) fn history_project(&self) -> String {
        self.session
            .as_ref()
            .map_or_else(|| self.startup_cwd.clone(), |session| session.cwd.clone())
    }
}

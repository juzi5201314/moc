//! 模态卡片：权限、提问、模型选择、会话恢复、计划审批、帮助。
//!
//! 每种模态自带键盘状态（选中项等）与答复通道（oneshot sender）；键盘
//! 事件优先路由给打开的模态（见 `crate::event`），渲染浮层在 `crate::render`。

use moc_protocol::Role;
use moc_protocol::methods::{
    AskParams, AskResult, ContextUsageResult, ModelInfo, PermissionRequestParams, PermissionResult,
    SessionInfo, TranscriptEntry,
};

/// 可搜索列表模态的状态。
#[derive(Debug, Clone, Default)]
pub struct PickerState {
    /// 当前过滤字符串。
    pub query: String,
    /// 过滤结果中的选中项下标。
    pub selected: usize,
    /// 列表 viewport 的首项下标。
    pub offset: usize,
    /// 是否正在请求后端数据。
    pub loading: bool,
    /// 最近一次请求错误。
    pub error: Option<String>,
}

/// 计划审阅窗口的完整状态。
#[derive(Debug, Clone)]
pub struct PlanReviewState {
    pub plan_id: String,
    pub title: String,
    pub path: String,
    pub markdown: String,
    pub selected: usize,
    pub scroll_offset: usize,
    pub execution_model: String,
    pub submitting: bool,
    pub error: Option<String>,
}

impl PlanReviewState {
    pub fn new(plan: moc_protocol::methods::PlanInfo, execution_model: String) -> Self {
        Self {
            plan_id: plan.plan_id,
            title: plan.title,
            path: plan.path,
            markdown: plan.markdown,
            selected: 0,
            scroll_offset: 0,
            execution_model,
            submitting: false,
            error: None,
        }
    }
}

/// 同一模型选择器可切换会话模型，也可只选择计划执行模型。
pub enum ModelTarget {
    Session,
    PlanReview(PlanReviewState),
}

impl PickerState {
    pub fn reset(&mut self) {
        self.query.clear();
        self.selected = 0;
        self.offset = 0;
        self.error = None;
    }

    pub fn move_selection(&mut self, delta: i32, count: usize, page: usize) {
        if count == 0 {
            self.selected = 0;
            self.offset = 0;
            return;
        }
        let max = count - 1;
        self.selected = if delta < 0 {
            self.selected.saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected.saturating_add(delta as usize).min(max)
        };
        self.keep_visible(page.max(1), count);
    }

    pub fn keep_visible(&mut self, page: usize, count: usize) {
        if count == 0 {
            self.offset = 0;
            self.selected = 0;
            return;
        }
        self.selected = self.selected.min(count - 1);
        if self.selected < self.offset {
            self.offset = self.selected;
        } else if self.selected >= self.offset + page {
            self.offset = self.selected + 1 - page;
        }
        self.offset = self.offset.min(count.saturating_sub(page));
    }
}
/// 消息回退选择器状态；每个用户消息是一列，列内按 transcript 顺序上下移动。
pub struct RewindState {
    pub prompts: Vec<Vec<TranscriptEntry>>,
    pub selected_prompt: usize,
    pub selected_item: usize,
    pub loading: bool,
    pub error: Option<String>,
}

impl Default for RewindState {
    fn default() -> Self {
        Self {
            prompts: Vec::new(),
            selected_prompt: 0,
            selected_item: 0,
            loading: true,
            error: None,
        }
    }
}

impl RewindState {
    pub fn set_entries(&mut self, entries: Vec<TranscriptEntry>) {
        self.prompts.clear();
        for entry in entries {
            if entry.bash.is_some() {
                continue;
            }
            if entry.message.role == Role::User {
                self.prompts.push(vec![entry]);
            } else if let Some(prompt) = self.prompts.last_mut() {
                prompt.push(entry);
            }
        }
        self.selected_prompt = self.prompts.len().saturating_sub(1);
        self.selected_item = self
            .prompts
            .last()
            .map_or(0, |prompt| prompt.len().saturating_sub(1));
        self.loading = false;
        self.error = None;
    }

    pub fn selected(&self) -> Option<&TranscriptEntry> {
        self.prompts
            .get(self.selected_prompt)
            .and_then(|prompt| prompt.get(self.selected_item))
    }

    pub fn move_vertical(&mut self, delta: i32) {
        let Some(prompt) = self.prompts.get(self.selected_prompt) else {
            return;
        };
        let max = prompt.len().saturating_sub(1);
        self.selected_item = if delta < 0 {
            self.selected_item
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_item.saturating_add(delta as usize).min(max)
        };
    }

    pub fn move_horizontal(&mut self, delta: i32) {
        if self.prompts.is_empty() {
            return;
        }
        let max = self.prompts.len().saturating_sub(1);
        self.selected_prompt = if delta < 0 {
            self.selected_prompt
                .saturating_sub(delta.unsigned_abs() as usize)
        } else {
            self.selected_prompt.saturating_add(delta as usize).min(max)
        };
        self.selected_item = self
            .selected_item
            .min(self.prompts[self.selected_prompt].len().saturating_sub(1));
    }
}

/// 打开的模态。
pub enum Modal {
    /// 工具授权：展示 diff 预览（若有）与选项列表。
    Permission {
        params: PermissionRequestParams,
        selected: usize,
        scroll_offset: usize,
        reply: Option<tokio::sync::oneshot::Sender<Result<PermissionResult, RpcErrorOf>>>,
    },
    /// `ask` 工具的用户提问（可多问、可多选、可自定义输入）。
    Ask {
        params: AskParams,
        question: usize,
        /// 每问当前高亮的选项下标（== options.len() 表示自定义输入行）。
        highlight: Vec<usize>,
        /// 每问已选中的 option label 集合。
        selected: Vec<Vec<String>>,
        custom: String,
        custom_cursor: usize,
        editing_custom: bool,
        /// 当前聚焦项预览的滚动行数（切换选项/问题时归零）。
        preview_scroll: usize,
        reply: Option<tokio::sync::oneshot::Sender<Result<AskResult, RpcErrorOf>>>,
    },
    /// 模型选择（`session/set_model`）。
    Model {
        models: Vec<ModelInfo>,
        current: String,
        picker: PickerState,
        target: ModelTarget,
    },
    /// OpenViking 工作区记忆目录。
    Memories {
        root_uri: String,
        entries: Vec<moc_protocol::methods::OpenVikingEntry>,
        picker: PickerState,
    },
    /// OpenViking 正文编辑器。
    MemoryEditor {
        uri: String,
        editor: crate::widgets::prompt::PromptState,
        dirty: bool,
        root_uri: String,
        entries: Vec<moc_protocol::methods::OpenVikingEntry>,
        picker: PickerState,
    },
    /// 消息回退选择器。
    Rewind { state: RewindState },
    /// 会话恢复（`session/list` + `session/load`）。
    Sessions {
        sessions: Vec<SessionInfo>,
        picker: PickerState,
    },
    /// 当前会话的子代理列表。
    Subagents {
        agents: Vec<moc_protocol::methods::AgentInfo>,
        picker: PickerState,
    },
    /// 子代理详情和发送/steer 编辑器。
    SubagentDetail {
        agent_id: String,
        editor: crate::widgets::prompt::PromptState,
        scroll_offset: usize,
    },
    /// 计划审批（`session/plan_approve`）。
    Plan { state: PlanReviewState },
    /// 快捷键帮助。
    Help,
    /// 上下文组成估算详情。
    Context { usage: Option<ContextUsageResult> },
}
/// 回复通道的错误类型别名（协议层 RpcError 的重导出位置）。
type RpcErrorOf = moc_protocol::RpcError;

impl Modal {
    /// 标题行。
    pub fn title(&self) -> String {
        match self {
            Modal::Permission { params, .. } => params.title.clone(),
            Modal::Ask {
                params, question, ..
            } => params
                .questions
                .get(*question)
                .map_or_else(|| "提问".into(), |question| question.question.clone()),
            Modal::Model { .. } => "选择模型".into(),
            Modal::Memories { .. } => "OpenViking 记忆".into(),
            Modal::MemoryEditor { uri, dirty, .. } => {
                format!("{}{}", if *dirty { "* " } else { "" }, uri)
            }
            Modal::Rewind { .. } => "消息回退".into(),
            Modal::Sessions { .. } => "恢复会话".into(),
            Modal::Subagents { .. } => "子代理".into(),
            Modal::SubagentDetail { agent_id, .. } => format!("子代理：{agent_id}"),
            Modal::Plan { state } => format!("计划审批：{}", state.title),
            Modal::Help => "快捷键帮助".into(),
            Modal::Context { .. } => "上下文占用".into(),
        }
    }

    /// 选项行数（键盘循环的范围）。
    pub fn option_count(&self) -> usize {
        match self {
            Modal::Permission { params, .. } => params.options.len(),
            Modal::Ask {
                params, question, ..
            } => params
                .questions
                .get(*question)
                .map(|question| question.options.len() + 1)
                .unwrap_or(0),
            Modal::Model { picker, models, .. } => filtered_models(models, &picker.query).len(),
            Modal::Memories {
                entries, picker, ..
            } => entries
                .iter()
                .filter(|entry| {
                    entry
                        .name
                        .to_lowercase()
                        .contains(&picker.query.to_lowercase())
                })
                .count(),
            Modal::MemoryEditor { .. } => 0,
            Modal::Rewind { .. } => 0,
            Modal::Sessions { picker, sessions } => {
                filtered_sessions(sessions, &picker.query).len()
            }
            Modal::Subagents { picker, agents } => agents
                .iter()
                .filter(|agent| {
                    let query = picker.query.to_lowercase();
                    agent.title.to_lowercase().contains(&query) || agent.id.contains(&query)
                })
                .count(),
            Modal::SubagentDetail { .. } => 0,
            Modal::Plan { .. } => 4,
            Modal::Help | Modal::Context { .. } => 0,
        }
    }

    /// 当前提问是否有选项带预览（为真时卡片放宽，容纳并排的预览栏）。
    pub fn has_option_preview(&self) -> bool {
        match self {
            Modal::Ask {
                params, question, ..
            } => params.questions.get(*question).is_some_and(|question| {
                question.options.iter().any(|option| {
                    option
                        .preview
                        .as_deref()
                        .is_some_and(|text| !text.trim().is_empty())
                })
            }),
            _ => false,
        }
    }

    /// 移动选中项（循环）。
    pub fn move_selection(&mut self, delta: i32) {
        let count = self.option_count();
        if count == 0 {
            return;
        }
        match self {
            Modal::Permission { selected, .. } => {
                *selected = (*selected as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::Plan { state } => {
                state.selected = state
                    .selected
                    .saturating_add_signed(isize::try_from(delta).expect("方向增量可表示为 isize"))
                    .min(count - 1);
            }
            Modal::Ask { .. }
            | Modal::MemoryEditor { .. }
            | Modal::Rewind { .. }
            | Modal::SubagentDetail { .. }
            | Modal::Help
            | Modal::Context { .. } => {}
            Modal::Model { picker, .. }
            | Modal::Memories { picker, .. }
            | Modal::Sessions { picker, .. }
            | Modal::Subagents { picker, .. } => {
                picker.move_selection(delta, count, 8);
            }
        }
    }

    pub fn picker_mut(&mut self) -> Option<&mut PickerState> {
        match self {
            Modal::Model { picker, .. }
            | Modal::Memories { picker, .. }
            | Modal::Sessions { picker, .. } => Some(picker),
            _ => None,
        }
    }

    pub fn set_error(&mut self, message: String) {
        if let Some(picker) = self.picker_mut() {
            picker.loading = false;
            picker.error = Some(message);
        }
    }
}

pub fn filtered_models<'a>(models: &'a [ModelInfo], query: &str) -> Vec<(usize, &'a ModelInfo)> {
    let query = query.to_lowercase();
    models
        .iter()
        .enumerate()
        .filter(|(_, model)| {
            query.is_empty()
                || model.id.to_lowercase().contains(&query)
                || model.display_name.to_lowercase().contains(&query)
        })
        .collect()
}

pub fn filtered_sessions<'a>(
    sessions: &'a [SessionInfo],
    query: &str,
) -> Vec<(usize, &'a SessionInfo)> {
    let query = query.to_lowercase();
    sessions
        .iter()
        .enumerate()
        .filter(|(_, session)| {
            query.is_empty()
                || session.title.to_lowercase().contains(&query)
                || session.cwd.to_lowercase().contains(&query)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Modal, PickerState, RewindState};
    use moc_protocol::methods::{AskOption, AskParams, AskQuestion, TranscriptEntry};
    use moc_protocol::{Message, Role};

    /// 预览判定：只认非空预览，且只作用于提问模态的当前问题。
    #[test]
    fn option_preview_detection_follows_current_question() {
        let question = |id: &str, previews: [Option<&str>; 2]| AskQuestion {
            id: id.into(),
            question: "选哪个？".into(),
            header: None,
            multi: false,
            recommended: None,
            options: previews
                .into_iter()
                .enumerate()
                .map(|(index, preview)| AskOption {
                    label: format!("选项{index}"),
                    description: None,
                    preview: preview.map(str::to_owned),
                })
                .collect(),
        };
        let modal = |questions: Vec<AskQuestion>, question: usize| Modal::Ask {
            params: AskParams {
                session_id: "ses".into(),
                prompt_id: "prompt".into(),
                request_id: "ask".into(),
                questions,
            },
            question,
            highlight: vec![0; 2],
            selected: vec![Vec::new(); 2],
            custom: String::new(),
            custom_cursor: 0,
            editing_custom: false,
            preview_scroll: 0,
            reply: None,
        };

        let questions = vec![
            question("first", [None, Some("   ")]),
            question("second", [None, Some("```\n+--+\n```")]),
        ];
        assert!(!modal(questions.clone(), 0).has_option_preview());
        assert!(modal(questions, 1).has_option_preview());
    }

    #[test]
    fn picker_keeps_selected_item_inside_viewport() {
        let mut picker = PickerState {
            selected: 0,
            offset: 0,
            ..Default::default()
        };
        picker.move_selection(9, 20, 8);
        assert_eq!(picker.selected, 9);
        assert_eq!(picker.offset, 2);
        picker.move_selection(-9, 20, 8);
        assert_eq!(picker.selected, 0);
        assert_eq!(picker.offset, 0);
    }
    #[test]
    fn rewind_navigation_moves_within_turns_and_between_users() {
        let entry = |id: &str, message: Message| TranscriptEntry {
            id: id.into(),
            at_ms: 0,
            message,
            tool: None,
            bash: None,
        };
        let mut state = RewindState::default();
        state.set_entries(vec![
            entry("u1", Message::user_text("第一条")),
            entry("a1", Message::assistant_text("回复")),
            entry("t1", Message::tool_result("call", "结果", false)),
            entry("u2", Message::user_text("第二条")),
        ]);
        assert_eq!(state.prompts.len(), 2);
        assert_eq!(state.prompts[0].len(), 3);
        assert_eq!(state.selected_prompt, 1);
        assert_eq!(state.selected_item, 0);

        state.move_horizontal(-1);
        state.move_vertical(2);
        assert_eq!((state.selected_prompt, state.selected_item), (0, 2));
        state.move_horizontal(1);
        assert_eq!((state.selected_prompt, state.selected_item), (1, 0));
        assert_eq!(state.selected().expect("选中条目").message.role, Role::User);
    }
}

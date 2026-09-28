//! 模态卡片：权限、提问、模型选择、会话恢复、计划审批、帮助。
//!
//! 每种模态自带键盘状态（选中项等）与答复通道（oneshot sender）；键盘
//! 事件优先路由给打开的模态（见 `crate::event`），渲染浮层在 `crate::render`。

use moc_protocol::methods::{
    AskParams, AskResult, ContextUsageResult, ModelInfo, PermissionRequestParams, PermissionResult,
    SessionInfo,
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

/// 打开的模态。
pub enum Modal {
    /// 工具授权：展示 diff 预览（若有）与选项列表。
    Permission {
        params: PermissionRequestParams,
        selected: usize,
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
        editing_custom: bool,
        reply: Option<tokio::sync::oneshot::Sender<Result<AskResult, RpcErrorOf>>>,
    },
    /// 模型选择（`session/set_model`）。
    Model {
        models: Vec<ModelInfo>,
        current: String,
        picker: PickerState,
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
    /// 会话恢复（`session/list` + `session/load`）。
    Sessions {
        sessions: Vec<SessionInfo>,
        picker: PickerState,
    },
    /// 计划审批（`session/plan_approve`）。
    Plan {
        plan_id: String,
        title: String,
        markdown: String,
        selected: usize,
    },
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
            Modal::Sessions { .. } => "恢复会话".into(),
            Modal::Plan { title, .. } => format!("计划审批：{title}"),
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
            Modal::Sessions { picker, sessions } => {
                filtered_sessions(sessions, &picker.query).len()
            }
            Modal::Plan { .. } => 2,
            Modal::Help | Modal::Context { .. } => 0,
        }
    }
    /// 移动选中项（循环）。
    pub fn move_selection(&mut self, delta: i32) {
        let count = self.option_count();
        if count == 0 {
            return;
        }
        match self {
            Modal::Permission { selected, .. } | Modal::Plan { selected, .. } => {
                *selected = (*selected as i32 + delta).clamp(0, count as i32 - 1) as usize;
            }
            Modal::Ask { .. }
            | Modal::MemoryEditor { .. }
            | Modal::Help
            | Modal::Context { .. } => {}
            Modal::Model { picker, .. }
            | Modal::Memories { picker, .. }
            | Modal::Sessions { picker, .. } => {
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
    use super::PickerState;

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
}

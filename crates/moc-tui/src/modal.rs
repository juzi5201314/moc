//! 模态卡片：权限、提问、模型选择、会话恢复、计划审批、帮助。
//!
//! 每种模态自带键盘状态（选中项等）与答复通道（oneshot sender）；键盘
//! 事件优先路由给打开的模态（见 `crate::event`），渲染浮层在 `crate::render`。

use moc_protocol::methods::{
    AskParams, AskResult, ModelInfo, PermissionRequestParams, PermissionResult, SessionInfo,
};

/// 打开的模态。
///
/// `reply` 用 `Option` 包裹：答复时从 `&mut` 里 `take()` 出来消费掉
/// （oneshot 的 `send` 按值取接收端）。
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
        selected: usize,
    },
    /// 会话恢复（`session/list` + `session/load`）。
    Sessions {
        sessions: Vec<SessionInfo>,
        selected: usize,
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
            Modal::Sessions { .. } => "恢复会话".into(),
            Modal::Plan { title, .. } => format!("计划审批：{title}"),
            Modal::Help => "快捷键帮助".into(),
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
                .map(|question| question.options.len() + 1) // + 自定义输入行
                .unwrap_or(0),
            Modal::Model { models, .. } => models.len(),
            Modal::Sessions { sessions, .. } => sessions.len(),
            Modal::Plan { .. } => 2,
            Modal::Help => 0,
        }
    }

    /// 移动选中项（循环）。
    pub fn move_selection(&mut self, delta: i32) {
        let count = self.option_count();
        if count == 0 {
            return;
        }
        let selected = match self {
            Modal::Permission { selected, .. }
            | Modal::Model { selected, .. }
            | Modal::Sessions { selected, .. }
            | Modal::Plan { selected, .. } => selected,
            Modal::Ask { .. } | Modal::Help => return,
        };
        let next = (*selected as i32 + delta).rem_euclid(count as i32) as usize;
        *selected = next;
    }
}

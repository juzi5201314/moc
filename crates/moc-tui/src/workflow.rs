//! Workflow 前端状态：审批窗、run 面板与 phase 分层布局。
//!
//! 布局是纯函数：`phase_layers` 把 plan 的 phase 集合按边（site 级 → phase 级
//! 聚合）做最长路径分层，同层横排；数据边与顺序边分开计数，供渲染层区分。

use std::collections::{BTreeMap, VecDeque};

use moc_protocol::methods::{
    WorkflowApproval, WorkflowPhaseState, WorkflowRoleState, WorkflowRunStatus,
};
use serde_json::Value;

/// 审批窗状态：载荷 + 覆盖表 + 选择/滚动/反馈编辑。
#[derive(Debug)]
pub struct WorkflowApprovalState {
    pub request: Box<WorkflowApproval>,
    /// role_id → 选中模型 selector；初值 = 载荷 defaults。
    pub role_models: BTreeMap<String, String>,
    /// model ref → 选中模型 selector。
    pub model_ref_map: BTreeMap<String, String>,
    /// 焦点行：roles + model_refs 的合并游标（先 roles 后 model_refs）。
    pub focus: usize,
    /// 正文 viewport 的起点与滚动约束；真实可见行数由绘制层回写。
    /// 焦点移动时置 `reveal_selection`，绘制层把焦点行收进窗口。
    pub body_scroll: usize,
    pub list_scroll: usize,
    pub command_scroll: usize,
    pub scroll: crate::modal::ModelScroll,
    /// 保留鼠标坐标，悬停在渲染层按最终命中矩形计算，不直接写 `focus`。
    pub pointer: Option<ratatui::layout::Position>,
    /// 拒绝反馈输入态：`Some` 时键盘进文本行。
    pub feedback: Option<String>,
    /// 折叠（停车）：最小化成状态栏胶囊，事件仍在后台累积。
    pub parked: bool,
}

impl WorkflowApprovalState {
    pub fn new(request: Box<WorkflowApproval>) -> Self {
        Self {
            role_models: request.defaults.role_models.clone(),
            model_ref_map: request.defaults.model_ref_map.clone(),
            request,
            focus: 0,
            body_scroll: 0,
            list_scroll: 0,
            command_scroll: 0,
            scroll: crate::modal::ModelScroll::default(),
            pointer: None,
            feedback: None,
            parked: false,
        }
    }

    /// role 行数量（fan-out 实例也算同一个 role 条目）。
    pub fn role_rows(&self) -> usize {
        self.request
            .defaults
            .role_models
            .keys()
            .count()
            .max(plan_roles(&self.request.plan).len())
    }

    /// 可选行总数 = roles + model_refs。
    pub fn rows(&self) -> usize {
        self.role_rows()
            + self
                .request
                .defaults
                .model_ref_map
                .len()
                .max(plan_model_refs(&self.request.plan).len())
    }

    /// 当前焦点行指向的 role_id 或 model_ref id。
    pub fn focus_target(&self) -> Option<FocusTarget> {
        let roles = self.role_ids();
        if self.focus < roles.len() {
            return roles.get(self.focus).cloned().map(FocusTarget::Role);
        }
        let refs = self.model_ref_ids();
        refs.get(self.focus - roles.len())
            .cloned()
            .map(FocusTarget::ModelRef)
    }
}

/// 焦点目标：role 覆盖项或 model() ref 重映射项。
#[derive(Debug, Clone)]
pub enum FocusTarget {
    Role(String),
    ModelRef(String),
}

/// plan.roles 的 role_id 列表（稳定序）。
fn plan_roles(plan: &Value) -> Vec<Value> {
    plan.get("roles")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

fn plan_model_refs(plan: &Value) -> Vec<Value> {
    plan.get("model_refs")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default()
}

impl WorkflowApprovalState {
    fn role_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = plan_roles(&self.request.plan)
            .iter()
            .filter_map(|r| {
                r.get("role_id")
                    .and_then(|v| v.as_str())
                    .map(str::to_string)
            })
            .collect();
        if ids.is_empty() {
            ids = self.request.defaults.role_models.keys().cloned().collect();
        }
        ids
    }

    fn model_ref_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = plan_model_refs(&self.request.plan)
            .iter()
            .filter_map(|r| r.get("ref_id").and_then(|v| v.as_str()).map(str::to_string))
            .collect();
        if ids.is_empty() {
            ids = self
                .request
                .defaults
                .model_ref_map
                .keys()
                .cloned()
                .collect();
        }
        ids
    }
}

/// 打开审批 modal 后选择模型要还原的状态（ModelTarget::WorkflowRole 的回程票）。
#[derive(Debug)]
pub enum WorkflowRoleBack {
    /// 回审批窗：选择写进覆盖表。
    Approval(Box<WorkflowApprovalState>),
    /// 回 run 面板：调用 session/workflow_set_role_model。
    Run(Box<WorkflowRunState>),
}

/// run 面板状态：快照 + 实时事件投影。
#[derive(Debug)]
pub struct WorkflowRunState {
    pub run_id: String,
    pub title: String,
    pub status: WorkflowRunStatus,
    pub plan: Value,
    pub phases: Vec<WorkflowPhaseState>,
    pub roles: Vec<WorkflowRoleState>,
    pub commands: Vec<String>,
    pub reports: Vec<Value>,
    pub artifacts: Vec<Value>,
    pub holes: Vec<Value>,
    pub usage: Value,
    /// 尾部日志（最多保留 200 行）。
    pub log: Vec<String>,
    /// artifact_id → 选中版本（1 起；缺省最新）。`v` 键循环切换。
    pub artifact_versions: BTreeMap<String, usize>,
    /// `o` 打开的 artifact 全文查看（标题, 正文行）；Esc/o 关闭回到面板。
    pub artifact_view: Option<(String, Vec<Line<'static>>)>,
    /// 焦点行：roles 游标（回车改模型）。
    pub focus: usize,
    /// 正文 viewport 的起点与滚动约束（与审批窗同一语义）。
    pub body_scroll: usize,
    pub list_scroll: usize,
    pub scroll: crate::modal::ModelScroll,
    /// 保留鼠标坐标，悬停在渲染层按最终命中矩形计算，不直接写 `focus`。
    pub pointer: Option<ratatui::layout::Position>,
}

impl WorkflowRunState {
    pub fn log_line(&mut self, line: String) {
        self.log.push(line);
        if self.log.len() > 200 {
            let overflow = self.log.len() - 200;
            self.log.drain(..overflow);
        }
    }

    pub fn active(&self) -> bool {
        matches!(
            self.status,
            WorkflowRunStatus::Running | WorkflowRunStatus::Pending
        )
    }
}

/// 模态键位触发的延后动作（借用结束后统一发 RPC）。
#[derive(Debug)]
pub enum WorkflowAction {
    /// `session/workflow_stop`。
    Stop(String),
    /// `session/workflow_resume`。
    Resume(String),
    /// 打开 run 面板（`session/workflow_run` 取快照）。
    OpenRun(String),
    /// `session/workflow_set_role_model`：(run_id, role_id, model selector)。
    SetRoleModel(String, String, String),
    /// `session/workflow_artifact`：查看 artifact 全文（run_id, artifact_id）。
    ViewArtifact(String, String),
}

/// 模型选择回写：审批态写覆盖表；run 面板产生 SetRoleModel 延后动作。
pub fn apply_workflow_pick(
    mut back: Box<WorkflowRoleBack>,
    key: &FocusTarget,
    selector: String,
) -> (WorkflowRoleBack, Option<WorkflowAction>) {
    match back.as_mut() {
        WorkflowRoleBack::Approval(state) => {
            match key {
                FocusTarget::Role(id) => {
                    state.role_models.insert(id.clone(), selector);
                }
                FocusTarget::ModelRef(id) => {
                    state.model_ref_map.insert(id.clone(), selector);
                }
            }
            (*back, None)
        }
        WorkflowRoleBack::Run(state) => {
            let action = match key {
                FocusTarget::Role(id) => Some(WorkflowAction::SetRoleModel(
                    state.run_id.clone(),
                    id.clone(),
                    selector,
                )),
                FocusTarget::ModelRef(_) => None,
            };
            (*back, action)
        }
    }
}

/// 状态栏胶囊：排队审批（modal 占用时到达的请求，载荷完整保留）+ 活跃 run
/// 数 + 最小化审批的回放票。
#[derive(Debug, Default)]
pub struct WorkflowCapsule {
    /// modal 占用时到达的审批请求按到达序排队；胶囊点击弹出队首。
    pub queued_approvals: VecDeque<Box<WorkflowApproval>>,
    /// 当前活跃 run 的标题（有且仅有一个时显示名字）。
    pub active_runs: BTreeMap<String, String>,
    /// 最小化（Esc）的审批状态：胶囊点击重开。审批仍挂起，不算已决。
    pub parked_approval: Option<Box<WorkflowApprovalState>>,
}

mod artifacts;
mod views;
pub use views::approval_lines;
pub use views::phase_layers;
pub use views::run_lines;
pub use views::status_label;

// —— 渲染：分层图 + role 行 + 命令集（modal_body 用） ——

use ratatui::text::Line;

#[cfg(test)]
mod tests;

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

/// phase 分层：返回每层 phase id（按最长路径；回边/order 边与 data 边都入图，
/// 但只 data 边决定层次——order 边画虚线）。
pub fn phase_layers(plan: &Value) -> Vec<Vec<String>> {
    let phases = plan
        .get("phases")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    // site → phase id。
    let mut site_phase: BTreeMap<String, String> = BTreeMap::new();
    for phase in &phases {
        let id = phase
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        for site in phase
            .get("sites")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default()
        {
            if let Some(site) = site.as_str() {
                site_phase.insert(site.to_string(), id.clone());
            }
        }
    }
    // phase 级 data 边聚合。

    let mut edges: Vec<(String, String)> = Vec::new();
    for edge in plan
        .get("edges")
        .and_then(|e| e.as_array())
        .cloned()
        .unwrap_or_default()
    {
        let (Some(from), Some(to)) = (
            edge.get("from").and_then(|v| v.as_str()),
            edge.get("to").and_then(|v| v.as_str()),
        ) else {
            continue;
        };
        let (Some(from_phase), Some(to_phase)) =
            (site_phase.get(from).cloned(), site_phase.get(to).cloned())
        else {
            continue;
        };
        if from_phase != to_phase {
            edges.push((from_phase, to_phase));
        }
    }
    // 最长路径分层：layer(v) = max(layer(pred)) + 1；无 pred 为 0。
    // edges 无环保证（编译器只对 DAG 建层；有环退化成全零层）。
    let ids: Vec<String> = phases
        .iter()
        .filter_map(|p| p.get("id").and_then(|v| v.as_str()).map(str::to_string))
        .collect();
    let mut layer: BTreeMap<String, usize> = BTreeMap::new();
    // Kahn 序保证 pred 先算。
    let mut done: BTreeMap<String, bool> = BTreeMap::new();
    let mut progress = true;
    while progress {
        progress = false;
        for id in &ids {
            if done.get(id).copied().unwrap_or(false) {
                continue;
            }
            let ready = edges
                .iter()
                .filter(|(_, to)| to == id)
                .all(|(from, _)| done.get(from).copied().unwrap_or(false));
            if ready {
                let level = edges
                    .iter()
                    .filter(|(_, to)| to == id)
                    .map(|(from, _)| layer.get(from).copied().unwrap_or(0) + 1)
                    .max()
                    .unwrap_or(0);
                layer.insert(id.clone(), level);
                done.insert(id.clone(), true);
                progress = true;
            }
        }
        if done.len() == ids.len() {
            break;
        }
        if !progress {
            // 环：剩余全部压到 0 层，渲染仍能出图。
            for id in &ids {
                if !done.get(id).copied().unwrap_or(false) {
                    layer.insert(id.clone(), 0);
                    done.insert(id.clone(), true);
                }
            }
            break;
        }
    }
    let max_layer = layer.values().copied().max().unwrap_or(0);
    let mut layers = vec![Vec::new(); max_layer + 1];
    for (id, level) in layer {
        layers[level].push(id);
    }
    layers
}

/// 终端状态文案（REPL 单行与通知共用）。
pub fn status_label(status: WorkflowRunStatus) -> String {
    match status {
        WorkflowRunStatus::Pending => "pending".into(),
        WorkflowRunStatus::Running => "running".into(),
        WorkflowRunStatus::Completed => "completed".into(),
        WorkflowRunStatus::Errored => "errored".into(),
        WorkflowRunStatus::Stopped { reason } => format!("stopped:{:?}", reason).to_lowercase(),
    }
}

// —— 渲染：分层图 + role 行 + 命令集（modal_body 用） ——

use ratatui::{
    style::Style,
    text::{Line, Span},
};

/// plan.phases 的 id → (name, kind)。
fn phase_index(plan: &Value) -> BTreeMap<String, (String, String)> {
    plan.get("phases")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(|p| {
            Some((
                p.get("id")?.as_str()?.to_string(),
                (
                    p.get("name")?.as_str()?.to_string(),
                    p.get("kind")?.as_str().unwrap_or("phase").to_string(),
                ),
            ))
        })
        .collect()
}

/// phase 状态 → 颜色（run 面板实时着色）。
fn phase_style(status: &str, theme: &crate::theme::Theme) -> Style {
    match status {
        "running" => Style::new().fg(theme.accent_running),
        "done" | "completed" => Style::new().fg(theme.accent_success),
        "failed" | "errored" => Style::new().fg(theme.accent_error),
        "cancelled" | "stopped" => Style::new().fg(theme.gray),
        _ => Style::new().fg(theme.gray_dim),
    }
}

fn role_style(status: &str, theme: &crate::theme::Theme) -> Style {
    phase_style(status, theme)
}

/// 分层图行：层间 `↓`，同层横排盒；宽度不足退化单列。
fn graph_lines(
    plan: &Value,
    status_of: &dyn Fn(&str) -> (String, String),
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let index = phase_index(plan);
    let layers = phase_layers(plan);
    let mut lines = Vec::new();
    for (level, ids) in layers.iter().enumerate() {
        let mut spans = Vec::new();
        for (index_i, id) in ids.iter().enumerate() {
            let (name, kind) = index
                .get(id)
                .cloned()
                .unwrap_or((id.clone(), "phase".into()));
            let (status, _) = status_of(id);
            let marker = if kind == "hole" { "?" } else { "•" };
            let text = format!(" {marker} {name} ");
            let style = phase_style(&status, theme);
            spans.push(Span::styled(text, style));
            if index_i + 1 < ids.len() {
                spans.push(Span::styled("  ", Style::new().fg(theme.gray_dim)));
            }
        }
        let line_width: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        // 窄宽退化：每层一列（多行）。
        if line_width > width && ids.len() > 1 {
            for id in ids {
                let (name, kind) = index
                    .get(id)
                    .cloned()
                    .unwrap_or((id.clone(), "phase".into()));
                let (status, _) = status_of(id);
                let marker = if kind == "hole" { "?" } else { "•" };
                lines.push(Line::from(Span::styled(
                    format!(" {marker} {name} "),
                    phase_style(&status, theme),
                )));
            }
        } else {
            lines.push(Line::from(spans));
        }
        if level + 1 < layers.len() {
            lines.push(Line::from(Span::styled(
                "  ↓",
                Style::new().fg(theme.gray_dim),
            )));
        }
    }
    lines
}

/// 审批窗正文：编排图 + role 模型行 + 命令集 + amend/hole_fill 差异 + 反馈输入。
pub fn approval_lines(
    state: &WorkflowApprovalState,
    width: usize,
    theme: &crate::theme::Theme,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let dim = Style::new().fg(theme.gray_dim);
    let mut lines = Vec::new();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    lines.push(Line::from(Span::styled("编排", dim)));
    lines.extend(graph_lines(
        &state.request.plan,
        &|_| ("pending".into(), String::new()),
        width,
        theme,
    ));
    lines.push(Line::from(String::new()));

    // role 行：role_id + fan-out + 当前选定模型。
    lines.push(Line::from(Span::styled("roles", dim)));
    let roles = state
        .request
        .plan
        .get("roles")
        .and_then(|r| r.as_array())
        .cloned()
        .unwrap_or_default();
    let ref_ids: Vec<String> = state
        .request
        .plan
        .get("model_refs")
        .and_then(|r| r.as_array())
        .map(|refs| {
            refs.iter()
                .filter_map(|r| r.get("ref_id").and_then(|v| v.as_str()).map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let mut row = 0usize;
    for role in &roles {
        let role_id = role
            .get("role_id")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        let fan = role
            .get("fan_out")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let model = state
            .role_models
            .get(&role_id)
            .cloned()
            .unwrap_or_else(|| "inherit".into());
        let name = role
            .get("name_pattern")
            .and_then(|v| v.as_str())
            .unwrap_or(role_id.as_str());
        let fan_text = if fan { " ×N" } else { "" };
        let style = if row == state.focus {
            Style::new().fg(theme.accent_model)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        hits.push((row, lines.len()));
        lines.push(Line::from(Span::styled(
            format!(" {name}{fan_text:<4} → {model}"),
            style,
        )));
        row += 1;
    }
    for ref_id in &ref_ids {
        let model = state
            .model_ref_map
            .get(ref_id)
            .cloned()
            .unwrap_or_else(|| "inherit".into());
        let style = if row == state.focus {
            Style::new().fg(theme.accent_model)
        } else {
            Style::new().fg(theme.text_secondary)
        };
        hits.push((row, lines.len()));
        lines.push(Line::from(Span::styled(
            format!(" model({ref_id}) → {model}"),
            style,
        )));
        row += 1;
    }
    lines.push(Line::from(String::new()));

    // 命令集 / hole_fill 新增命令 / amend 提示。
    let commands: Vec<String> = match state.request.kind {
        moc_protocol::methods::WorkflowApprovalKind::HoleFill => {
            state.request.new_commands.clone().unwrap_or_default()
        }
        _ => state
            .request
            .plan
            .get("commands")
            .and_then(|c| c.as_array())
            .map(|list| {
                list.iter()
                    .filter_map(|c| {
                        c.get("command")
                            .and_then(|v| v.as_str())
                            .map(str::to_string)
                    })
                    .collect()
            })
            .unwrap_or_default(),
    };
    let label = match state.request.kind {
        moc_protocol::methods::WorkflowApprovalKind::HoleFill => "新增命令",
        _ => "world.run 命令集",
    };
    lines.push(Line::from(Span::styled(label, dim)));
    for command in commands {
        lines.push(Line::from(Span::styled(
            format!("  {command}"),
            Style::new().fg(theme.accent_warning),
        )));
    }
    if matches!(
        state.request.kind,
        moc_protocol::methods::WorkflowApprovalKind::Amend
    ) {
        lines.push(Line::from(Span::styled(
            "  （amend：增量修改既有 run）",
            dim,
        )));
    }
    // 脚本全文（syntect 高亮）：hole_fill 时显示的是待审体而非原脚本，跳过。
    if !matches!(
        state.request.kind,
        moc_protocol::methods::WorkflowApprovalKind::HoleFill
    ) && !state.request.script.is_empty()
    {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled("脚本", dim)));
        let start = state.command_scroll;
        for highlighted in crate::render::highlight::highlight("ts", &state.request.script)
            .lines
            .iter()
            .skip(start)
        {
            let mut spans = Vec::with_capacity(highlighted.len() + 1);
            spans.push(Span::raw("  "));
            for part in highlighted {
                spans.push(Span::styled(
                    part.text.clone(),
                    Style::new().fg(part
                        .fg
                        .map(crate::render::highlight::rgb_to_color)
                        .unwrap_or(theme.text_secondary)),
                ));
            }
            lines.push(Line::from(spans));
        }
    }
    if let Some(feedback) = &state.feedback {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled(
            format!("拒绝反馈：{feedback}▌"),
            Style::new().fg(theme.accent_error),
        )));
    }
    (lines, hits)
}

/// run 面板正文：实时着色图 + role 状态行 + 用量 + hole/log。
pub fn run_lines(
    state: &WorkflowRunState,
    width: usize,
    theme: &crate::theme::Theme,
) -> (Vec<Line<'static>>, Vec<(usize, usize)>) {
    let dim = Style::new().fg(theme.gray_dim);
    let mut lines = Vec::new();
    let mut hits: Vec<(usize, usize)> = Vec::new();
    // artifact 全文查看态：整幅替换成内容（markdown 渲染、文本按行）。
    if let Some((title, view)) = &state.artifact_view {
        lines.push(Line::from(Span::styled(
            format!("查看：{title}  （Esc/o 返回）"),
            Style::new().fg(theme.accent_model),
        )));
        lines.extend(view.iter().cloned());
        return (lines, hits);
    }
    lines.push(Line::from(Span::styled(
        format!("状态：{}", status_label(state.status)),
        phase_style(&status_label(state.status), theme),
    )));
    lines.push(Line::from(Span::styled("编排", dim)));
    lines.extend(graph_lines(
        &state.plan,
        &|id| {
            state
                .phases
                .iter()
                .find(|p| p.id == id)
                .map(|p| (p.status.clone(), String::new()))
                .unwrap_or_else(|| ("pending".into(), String::new()))
        },
        width,
        theme,
    ));
    lines.push(Line::from(String::new()));

    lines.push(Line::from(Span::styled("roles", dim)));
    for (index, role) in state.roles.iter().enumerate() {
        hits.push((index, lines.len()));
        let model = role.model.as_deref().unwrap_or("inherit");
        let style = if index == state.focus {
            Style::new().fg(theme.accent_model)
        } else {
            role_style(&role.status, theme)
        };
        lines.push(Line::from(Span::styled(
            format!(
                " {} [{}/{} asks, {} tok] → {}",
                role.name.as_deref().unwrap_or(role.role_id.as_str()),
                role.asks_done,
                role.asks_in_flight + role.asks_done,
                role.spent_tokens,
                model
            ),
            style,
        )));
    }
    if let Some(spent) = state.usage.get("spent_tokens").and_then(|t| t.as_u64())
        && spent > 0
    {
        lines.push(Line::from(Span::styled(
            format!("用量：{spent} tokens{}", cost_label(&state.usage)),
            dim,
        )));
    }
    if !state.reports.is_empty() {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled("reports", dim)));
        for report in &state.reports {
            let text = serde_json::to_string(report).unwrap_or_default();
            lines.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    crate::render::wrap::truncate(&text, width.saturating_sub(2))
                ),
                Style::new().fg(theme.text_secondary),
            )));
        }
    }
    if !state.artifacts.is_empty() {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled("artifacts", dim)));
        // 同一 artifact_id 的多条声明即版本序列；只渲染选中版本（默认最新，v 切换）。
        let mut ids: Vec<String> = Vec::new();
        let mut by_id: BTreeMap<String, Vec<&Value>> = BTreeMap::new();
        for artifact in &state.artifacts {
            let id = artifact
                .get("artifact_id")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string();
            if !by_id.contains_key(&id) {
                ids.push(id.clone());
            }
            by_id.entry(id).or_default().push(artifact);
        }
        for id in &ids {
            let versions = &by_id[id];
            let pick = state
                .artifact_versions
                .get(id)
                .copied()
                .unwrap_or(versions.len())
                .min(versions.len())
                .max(1);
            lines.extend(artifact_lines(
                versions[pick - 1],
                &state.reports,
                pick,
                versions.len(),
                width,
                theme,
            ));
        }
    }
    if !state.holes.is_empty() {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled("holes", dim)));
        for hole in &state.holes {
            let name = hole.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            lines.push(Line::from(Span::styled(
                format!("  ? {name}"),
                Style::new().fg(theme.accent_warning),
            )));
        }
    }
    if !state.log.is_empty() {
        lines.push(Line::from(String::new()));
        lines.push(Line::from(Span::styled("log", dim)));
        for line in state.log.iter().rev().take(8).rev() {
            lines.push(Line::from(Span::styled(format!("  {line}"), dim)));
        }
    }
    (lines, hits)
}

/// report 项按 artifact_id 路由出的数据源（本 artifact 的 item 列表，按到达序）。
fn artifact_items<'a>(artifact: &Value, reports: &'a [Value]) -> Vec<&'a Value> {
    let id = artifact
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    reports
        .iter()
        .filter(|r| r.get("artifact_id").and_then(|v| v.as_str()) == Some(id))
        .filter_map(|r| r.get("item"))
        .collect()
}

/// `ArtifactField` 的 dot path 取值（"timing.after" → item.timing.after）。
fn field_get<'a>(item: &'a Value, field: &str) -> Option<&'a Value> {
    let mut cur = item;
    for part in field.split('.') {
        cur = cur.get(part)?;
    }
    Some(cur)
}

fn field_text(item: &Value, field: &str) -> String {
    field_get(item, field)
        .map(|v| {
            v.as_str()
                .map(str::to_string)
                .unwrap_or_else(|| v.to_string())
        })
        .unwrap_or_else(|| "—".into())
}

fn field_label(field: &Value) -> String {
    field
        .get("label")
        .and_then(|v| v.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| {
            field
                .get("field")
                .and_then(|v| v.as_str())
                .unwrap_or("?")
                .to_string()
        })
        + field
            .get("unit")
            .and_then(|v| v.as_str())
            .map(|u| format!(" {u}"))
            .as_deref()
            .unwrap_or("")
}

/// artifact 按 member 真渲染：数据源 = 该 artifact_id 的 report 项（spec 语义：
/// table 按 key 去重替换、metrics 取最新值、board 按 status 分列且 key 移动、
/// chart 画文本折线/散点；file/markdown 预览内容）。
fn artifact_lines(
    artifact: &Value,
    reports: &[Value],
    version: usize,
    versions: usize,
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme.gray_dim);
    let accent = Style::new().fg(theme.accent_model);
    let member = artifact
        .get("member")
        .and_then(|v| v.as_str())
        .unwrap_or("file");
    let spec = artifact.get("spec").cloned().unwrap_or(Value::Null);
    let id = artifact
        .get("artifact_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let title = spec
        .get("title")
        .and_then(|v| v.as_str())
        .or_else(|| artifact.get("title").and_then(|v| v.as_str()))
        .unwrap_or(id);
    let ver = if versions > 1 {
        format!(" v{version}/{versions}")
    } else {
        String::new()
    };
    let mut out = vec![Line::from(Span::styled(
        format!("  [{member}] {title}{ver}"),
        accent,
    ))];
    let items = artifact_items(artifact, reports);
    match member {
        // file/markdown 条目是 {artifact_id, member, version, file,
        // content_type?, title?, description?, primary?}；内容按 'o' 打开。
        "file" | "markdown" => {
            let file = artifact.get("file").and_then(|v| v.as_str()).unwrap_or("?");
            let mime = artifact
                .get("content_type")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            out.push(Line::from(Span::styled(
                format!("    📄 {file} ({mime})"),
                dim,
            )));
        }
        "chart" => {
            out.extend(chart_artifact_lines(
                &spec,
                &items,
                width.saturating_sub(6),
                theme,
            ));
        }
        "table" => {
            let columns = spec
                .get("columns")
                .and_then(|c| c.as_array())
                .cloned()
                .unwrap_or_default();
            let key = spec.get("key").and_then(|v| v.as_str());
            // key 去重：同 key 的后续项替换（保留最后值、原序）。
            let mut order: Vec<String> = Vec::new();
            let mut rows: BTreeMap<String, &Value> = BTreeMap::new();
            for (i, item) in items.iter().enumerate() {
                let k = key
                    .map(|k| field_text(item, k))
                    .unwrap_or_else(|| i.to_string());
                if !rows.contains_key(&k) {
                    order.push(k.clone());
                }
                rows.insert(k, item);
            }
            let header = columns
                .iter()
                .map(field_label)
                .collect::<Vec<_>>()
                .join("  ");
            out.push(Line::from(Span::styled(format!("    {header}"), accent)));
            for k in &order {
                let cells = columns
                    .iter()
                    .map(|c| {
                        field_text(
                            rows[k],
                            c.get("field").and_then(|v| v.as_str()).unwrap_or(""),
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("  ");
                out.push(Line::from(Span::styled(format!("    {cells}"), dim)));
            }
            if order.is_empty() {
                out.push(Line::from(Span::styled("    （无数据）", dim)));
            }
        }
        "metrics" => {
            let fields = spec
                .get("metrics")
                .and_then(|m| m.as_array())
                .cloned()
                .unwrap_or_default();
            // 每字段取最后一个含该字段的 item 的值。
            for field in &fields {
                let path = field.get("field").and_then(|v| v.as_str()).unwrap_or("");
                let value = items
                    .iter()
                    .rev()
                    .find_map(|item| field_get(item, path))
                    .map(|v| {
                        v.as_str()
                            .map(str::to_string)
                            .unwrap_or_else(|| v.to_string())
                    })
                    .unwrap_or_else(|| "—".into());
                out.push(Line::from(Span::styled(
                    format!("    {} = {}", field_label(field), value),
                    dim,
                )));
            }
        }
        "board" => {
            let key_f = spec.get("key").and_then(|v| v.as_str()).unwrap_or("");
            let status_f = spec.get("status").and_then(|v| v.as_str()).unwrap_or("");
            let columns: Vec<String> = spec
                .get("columns")
                .and_then(|c| c.as_array())
                .map(|c| {
                    c.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let title_f = spec
                .get("card_title")
                .or_else(|| spec.get("cardTitle"))
                .and_then(|v| v.as_str())
                .unwrap_or(key_f);
            // key 去重：后到的同 key 卡移动/更新（位置按最新 status）。
            let mut order: Vec<String> = Vec::new();
            let mut cards: BTreeMap<String, &Value> = BTreeMap::new();
            for item in &items {
                let k = field_text(item, key_f);
                if !cards.contains_key(&k) {
                    order.push(k.clone());
                }
                cards.insert(k, item);
            }
            let mut cols: Vec<(String, Vec<String>)> =
                columns.iter().map(|c| (c.clone(), Vec::new())).collect();
            cols.push(("other".into(), Vec::new()));
            for k in &order {
                let item = cards[k];
                let status = field_text(item, status_f);
                let title = field_text(item, title_f);
                let index = cols
                    .iter()
                    .position(|(name, _)| *name == status)
                    .unwrap_or(cols.len() - 1);
                cols[index].1.push(title);
            }
            for (name, card_titles) in &cols {
                out.push(Line::from(Span::styled(
                    format!("    ▸ {name} ×{}", card_titles.len()),
                    accent,
                )));
                for card in card_titles {
                    out.push(Line::from(Span::styled(format!("      {card}"), dim)));
                }
            }
        }
        _ => {
            out.push(Line::from(Span::styled(
                format!(
                    "    {}",
                    crate::render::wrap::truncate(spec.to_string(), width.saturating_sub(4))
                ),
                dim,
            )));
        }
    }
    out
}

/// chart artifact 的文本渲染：x/y 字段取点，按 type 画散点/柱状行 + 数值表；
/// raster Canvas 路径在 modal 内不可承载，文本图保证数据全真。
fn chart_artifact_lines(
    spec: &Value,
    items: &[&Value],
    width: usize,
    theme: &crate::theme::Theme,
) -> Vec<Line<'static>> {
    let dim = Style::new().fg(theme.gray_dim);
    let accent = Style::new().fg(theme.accent_model);
    let kind = spec.get("type").and_then(|v| v.as_str()).unwrap_or("line");
    let x_f = spec
        .get("x")
        .and_then(|x| x.get("field"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let y_fields: Vec<&Value> = spec
        .get("y")
        .map(|y| {
            if let Some(arr) = y.as_array() {
                arr.iter().collect()
            } else {
                vec![y]
            }
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    // 数值序列。
    let mut series: Vec<(String, Vec<(f64, f64)>)> = Vec::new();
    for y in &y_fields {
        let y_f = y.get("field").and_then(|v| v.as_str()).unwrap_or("");
        let points: Vec<(f64, f64)> = items
            .iter()
            .filter_map(|item| {
                let x = field_get(item, x_f)?.as_f64()?;
                let yv = field_get(item, y_f)?.as_f64()?;
                Some((x, yv))
            })
            .collect();
        series.push((field_label(y), points));
    }
    // 文本 sparkline：每条 series 一行高度标尺。
    for (label, points) in &series {
        if points.is_empty() {
            continue;
        }
        let lo = points.iter().map(|p| p.1).fold(f64::INFINITY, f64::min);
        let hi = points.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max);
        const TICKS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
        let span = (hi - lo).max(f64::EPSILON);
        let glyphs: String = points
            .iter()
            .map(|p| TICKS[(((p.1 - lo) / span) * 7.0).round() as usize])
            .collect();
        let marker = match kind {
            "bar" => "▮",
            "scatter" => "∙",
            _ => "─",
        };
        out.push(Line::from(Span::styled(
            format!("    {marker} {label} [{lo:.2}..{hi:.2}]  {glyphs}"),
            accent,
        )));
        // 末尾值表（x→y 对齐，超宽截断）。
        let table = points
            .iter()
            .map(|(x, y)| format!("{x:.2}→{y:.2}"))
            .collect::<Vec<_>>()
            .join("  ");
        out.push(Line::from(Span::styled(
            format!("      {}", crate::render::wrap::truncate(&table, width)),
            dim,
        )));
    }
    if series.iter().all(|(_, p)| p.is_empty()) {
        out.push(Line::from(Span::styled("    （无数据点）", dim)));
    }
    if let Some(v) = spec
        .get("baseline")
        .and_then(|b| b.get("field"))
        .and_then(|v| v.as_str())
        .and_then(|baseline| {
            items
                .iter()
                .find_map(|item| field_get(item, baseline).and_then(|v| v.as_f64()))
        })
    {
        out.push(Line::from(Span::styled(
            format!("    ── baseline {v:.2}"),
            dim,
        )));
    }
    out
}

fn cost_label(usage: &Value) -> String {
    usage
        .get("cost_usd")
        .and_then(|c| c.as_f64())
        .map(|c| format!(" (${c:.4})"))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn plan() -> Value {
        json!({
            "phases": [
                {"id":"phase#1","name":"plan","kind":"phase","sites":["phase#1","actor#1","ask#1"]},
                {"id":"phase#2","name":"build","kind":"phase","sites":["phase#2","actor#2","ask#2","world#1"]},
                {"id":"phase#3","name":"ship","kind":"phase","sites":["phase#3","world#2"]}
            ],
            "edges": [
                {"from":"ask#1","to":"ask#2","kind":"data"},
                {"from":"phase#1","to":"phase#2","kind":"order"},
                {"from":"world#1","to":"world#2","kind":"data"}
            ]
        })
    }

    #[test]
    fn layers_follow_longest_path() {
        let layers = phase_layers(&plan());
        assert_eq!(
            layers,
            vec![
                vec!["phase#1".to_string()],
                vec!["phase#2".to_string()],
                vec!["phase#3".to_string()]
            ]
        );
    }

    #[test]
    fn sibling_phases_share_a_layer() {
        let plan = json!({
            "phases": [
                {"id":"p1","name":"a","kind":"phase","sites":["s1"]},
                {"id":"p2","name":"b","kind":"phase","sites":["s2"]},
                {"id":"p3","name":"c","kind":"phase","sites":["s3"]}
            ],
            "edges": [
                {"from":"s1","to":"s3","kind":"data"},
                {"from":"s2","to":"s3","kind":"data"}
            ]
        });
        let layers = phase_layers(&plan);
        assert!(layers[0].contains(&"p1".to_string()) && layers[0].contains(&"p2".to_string()));
        assert_eq!(layers[1], vec!["p3".to_string()]);
    }

    #[test]
    fn narrow_fallback_lists_phases_vertically() {
        // 无边：全部在第 0 层，渲染层负责窄宽度退化展示。
        let layers = phase_layers(&json!({
            "phases": [
                {"id":"p1","name":"a","kind":"phase","sites":["s1"]},
                {"id":"p2","name":"b","kind":"phase","sites":["s2"]}
            ],
            "edges": []
        }));
        assert_eq!(layers.len(), 1);
        assert_eq!(layers[0].len(), 2);
    }

    fn approval(
        kind: moc_protocol::methods::WorkflowApprovalKind,
    ) -> Box<moc_protocol::methods::WorkflowApproval> {
        Box::new(moc_protocol::methods::WorkflowApproval {
            request_id: "req-1".into(),
            run_id: "run-1".into(),
            script: "phase(\"plan\"); const a = agent(\"p\"); return 1;".into(),
            plan: json!({
                "phases": [
                    {"id":"phase#1","name":"plan","kind":"phase","sites":["phase#1","actor#1","ask#1"]},
                    {"id":"phase#2","name":"build","kind":"phase","sites":["phase#2","actor#2","ask#2","world#1"]}
                ],
                "edges": [{"from":"ask#1","to":"ask#2","kind":"data"}],
                "roles": [{"role_id":"actor#1","name_pattern":"planner","fan_out":false},{"role_id":"actor#2","name_pattern":"worker","fan_out":true}],
                "model_refs": [{"ref_id":"model#1"}],
                "commands": [{"command":"cargo test"}]
            }),
            defaults: moc_protocol::methods::WorkflowApprovalDefaults {
                role_models: [("actor#1".to_string(), "gpt-x".to_string())]
                    .into_iter()
                    .collect(),
                model_ref_map: [("model#1".to_string(), "gpt-y".to_string())]
                    .into_iter()
                    .collect(),
            },
            args: json!(null),
            max_concurrency: 4,
            kind,
            new_commands: None,
        })
    }

    #[test]
    fn approval_lines_have_role_rows_and_hits() {
        let state = WorkflowApprovalState::new(approval(
            moc_protocol::methods::WorkflowApprovalKind::Launch,
        ));
        let (lines, hits) = approval_lines(&state, 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("planner")));
        assert!(text.iter().any(|l| l.contains("×N")), "fan-out 标记");
        assert!(text.iter().any(|l| l.contains("model(model#1)")));
        assert!(text.iter().any(|l| l.contains("cargo test")));
        assert!(
            text.iter().any(|l| l.contains("phase(\"plan\")")),
            "脚本全文"
        );
        // 2 roles + 1 model_ref = 3 个命中行，行号指向实际行。
        assert_eq!(hits.len(), 3);
        for (row, line_idx) in &hits {
            assert!(*line_idx < lines.len());
            assert!(*row <= 2);
        }
        // hole_fill：只列新增命令、不渲脚本。
        let mut hole = WorkflowApprovalState::new(approval(
            moc_protocol::methods::WorkflowApprovalKind::HoleFill,
        ));
        hole.request.new_commands = Some(vec!["ls".into()]);
        let (lines, _) = approval_lines(&hole, 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("新增命令")));
        assert!(text.iter().any(|l| l.contains("ls")));
        assert!(
            !text.iter().any(|l| l.contains("phase(\"plan\")")),
            "hole_fill 不渲脚本"
        );
    }

    fn run_state() -> WorkflowRunState {
        WorkflowRunState {
            run_id: "run-1".into(),
            title: "run".into(),
            status: WorkflowRunStatus::Running,
            plan: plan(),
            phases: vec![],
            roles: vec![WorkflowRoleState {
                role_id: "actor#1".into(),
                name: Some("planner".into()),
                fan_out: false,
                status: "running".into(),
                model: Some("gpt-x".into()),
                asks_in_flight: 1,
                asks_done: 2,
                spent_tokens: 42,
            }],
            commands: vec![],
            reports: vec![
                json!({"item":{"id":"a","v":1.0,"status":"done"},"artifact_id":"t1"}),
                json!({"item":{"id":"a","v":2.0,"status":"wip"},"artifact_id":"t1"}),
                json!({"item":{"id":"b","v":3.0,"status":"done"},"artifact_id":"t1"}),
            ],
            artifacts: vec![json!({
                "member": "table",
                "artifact_id": "t1",
                "spec": {"title":"rows","key":"id","columns":[{"field":"id"},{"field":"v"},{"field":"status"}]}
            })],
            holes: vec![],
            usage: json!({"spent_tokens": 42}),
            log: vec!["line".into()],
            artifact_versions: BTreeMap::new(),
            artifact_view: None,
            focus: 0,
            body_scroll: 0,
            list_scroll: 0,
            scroll: crate::modal::ModelScroll::default(),
            pointer: None,
        }
    }

    #[test]
    fn run_lines_render_table_with_key_dedup() {
        let (lines, hits) = run_lines(&run_state(), 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        // table：key=id 去重——a 只一行且取最后值 v=2.0/status=wip；b 一行。
        let rows: Vec<&String> = text
            .iter()
            .filter(|l| l.trim_start().starts_with("a  ") || l.trim_start().starts_with("b  "))
            .collect();
        assert_eq!(rows.len(), 2, "key 去重后两行");
        assert!(text.iter().any(|l| l.contains("2.0") && l.contains("wip")));
        assert!(
            text.iter()
                .any(|l| l.trim_start().starts_with("b  ") && l.contains("3.0")),
            "b 行存在"
        );
        // role 行命中 + 用量行。
        assert_eq!(hits.len(), 1);
        assert!(
            text.iter()
                .any(|l| l.contains("[2/3 asks") && l.contains("42 tok"))
        );
        assert!(text.iter().any(|l| l.contains("42 tokens")));
    }

    #[test]
    fn run_lines_metrics_board_chart() {
        let mut state = run_state();
        state.artifacts = vec![
            json!({"member":"metrics","artifact_id":"m1","spec":{"metrics":[{"field":"v","label":"speed"}]}}),
            json!({"member":"board","artifact_id":"b1","spec":{"key":"id","status":"status","columns":["done","wip"]}}),
            json!({"member":"chart","artifact_id":"c1","spec":{"x":{"field":"x"},"y":{"field":"v"},"type":"line"}}),
        ];
        state.reports = vec![
            json!({"item":{"id":"a","v":1.0,"status":"done"},"artifact_id":"m1"}),
            json!({"item":{"id":"a","v":2.0,"status":"wip"},"artifact_id":"m1"}),
            json!({"item":{"id":"a","status":"done"},"artifact_id":"b1"}),
            json!({"item":{"id":"b","status":"wip"},"artifact_id":"b1"}),
            json!({"item":{"id":"a","status":"done"},"artifact_id":"b1"}),
            json!({"item":{"x":0,"v":1.0},"artifact_id":"c1"}),
            json!({"item":{"x":1,"v":4.0},"artifact_id":"c1"}),
            json!({"item":{"x":2,"v":2.0},"artifact_id":"c1"}),
        ];
        let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        // metrics：最新值 v=2.0。
        assert!(
            text.iter()
                .any(|l| l.contains("speed") && l.contains("2.0"))
        );
        // board：a 最后 status=done → done 列；b 在 wip。
        assert!(text.iter().any(|l| l.contains("done ×1")));
        assert!(text.iter().any(|l| l.contains("wip ×1")));
        // chart：有 sparkline 与范围标注。
        assert!(text.iter().any(|l| l.contains("[1.00..4.00]")));
    }

    #[test]
    fn run_lines_version_switch_and_view() {
        let mut state = run_state();
        state.artifacts = vec![
            json!({"member":"markdown","artifact_id":"doc","spec":{"content":"# v1"}}),
            json!({"member":"markdown","artifact_id":"doc","spec":{"content":"# v2"}}),
        ];
        let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("v2/2")), "默认最新版");
        state.artifact_versions.insert("doc".into(), 1);
        let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("v1/2")));
        // 查看态整幅替换。
        state.artifact_view = Some(("doc".into(), vec![Line::raw("hello")]));
        let (lines, _) = run_lines(&state, 60, crate::theme::Theme::global());
        let text: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
        assert!(text.iter().any(|l| l.contains("查看：doc")));
        assert!(text.iter().any(|l| l.contains("hello")));
        assert!(!text.iter().any(|l| l.contains("状态：")));
    }

    #[test]
    fn pick_writes_override_back_to_approval() {
        let back = Box::new(WorkflowRoleBack::Approval(Box::new(
            WorkflowApprovalState::new(approval(
                moc_protocol::methods::WorkflowApprovalKind::Launch,
            )),
        )));
        let (back, action) = apply_workflow_pick(
            back,
            &FocusTarget::Role("actor#2".into()),
            "test/gpt-alt".into(),
        );
        assert!(action.is_none());
        let WorkflowRoleBack::Approval(state) = back else {
            panic!("应回审批窗");
        };
        assert_eq!(
            state.role_models.get("actor#2").map(String::as_str),
            Some("test/gpt-alt"),
            "覆盖写回批准决策用的 role_models"
        );
    }
}

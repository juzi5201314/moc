use moc_i18n::t;

use super::WorkflowApprovalState;
use super::WorkflowRunState;
use super::artifacts::artifact_lines;
use super::artifacts::cost_label;
use moc_protocol::methods::WorkflowRunStatus;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;
use std::collections::BTreeMap;

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

/// plan.phases 的 id → (name, kind)。
pub(super) fn phase_index(plan: &Value) -> BTreeMap<String, (String, String)> {
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
pub(super) fn phase_style(status: &str, theme: &crate::theme::Theme) -> Style {
    match status {
        "running" => Style::new().fg(theme.accent_running),
        "done" | "completed" => Style::new().fg(theme.accent_success),
        "failed" | "errored" => Style::new().fg(theme.accent_error),
        "cancelled" | "stopped" => Style::new().fg(theme.gray),
        _ => Style::new().fg(theme.gray_dim),
    }
}

pub(super) fn role_style(status: &str, theme: &crate::theme::Theme) -> Style {
    phase_style(status, theme)
}

/// 分层图行：层间 `↓`，同层横排盒；宽度不足退化单列。
pub(super) fn graph_lines(
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
    lines.push(Line::from(Span::styled(
        t!("tui.wf.orchestration").into_owned(),
        dim,
    )));
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
        moc_protocol::methods::WorkflowApprovalKind::HoleFill => t!("tui.wf.hole_fill_label"),
        _ => t!("tui.wf.run_cmds_label"),
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
            t!("tui.wf.amend_note").into_owned(),
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
        lines.push(Line::from(Span::styled(
            t!("tui.wf.script").into_owned(),
            dim,
        )));
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
            t!("tui.wf.reject_fmt", feedback = feedback.as_str()).into_owned(),
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
            t!("tui.wf.view_fmt", title = title.as_str()).into_owned(),
            Style::new().fg(theme.accent_model),
        )));
        lines.extend(view.iter().cloned());
        return (lines, hits);
    }
    lines.push(Line::from(Span::styled(
        t!(
            "tui.wf.status_fmt",
            status = status_label(state.status).as_str()
        )
        .into_owned(),
        phase_style(&status_label(state.status), theme),
    )));
    lines.push(Line::from(Span::styled(
        t!("tui.wf.orchestration").into_owned(),
        dim,
    )));
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
            t!(
                "tui.wf.usage_fmt",
                spent = spent,
                cost = cost_label(&state.usage).as_str()
            )
            .into_owned(),
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

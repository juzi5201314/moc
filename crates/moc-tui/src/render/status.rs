use rust_i18n::t;

use super::Activity;
use super::SPINNER_FRAMES;
use super::StatuslineData;
use super::TOKEN_UNITS;
use super::labels;
use super::wrap;
use super::wrap::str_width;
use crate::app::App;
use crate::modal::Modal;
use crate::transcript::Block as TranscriptBlock;
use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use ratatui::widgets::Paragraph;

/// 顶部信息条：左＝品牌 · git 分支 · 工作目录，右＝上下文进度 · 累计费用。
pub(super) fn draw_header(frame: &mut Frame, app: &mut App, area: Rect) {
    app.context_area = Rect::default();
    if area.is_empty() {
        return;
    }
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = vec![Span::styled(
        "moc".to_owned(),
        Style::new()
            .fg(theme.text_primary)
            .add_modifier(Modifier::BOLD),
    )];
    if let Some(branch) = &data.branch {
        spans.push(Span::styled(
            "  ⎇ ".to_owned(),
            Style::new().fg(theme.border),
        ));
        spans.push(Span::styled(
            wrap::truncate(branch.clone(), 24).into_owned(),
            Style::new().fg(theme.accent_success),
        ));
    }
    if !data.workdir.is_empty() {
        spans.push(Span::styled("  ".to_owned(), Style::new()));
        let used = spans
            .iter()
            .map(|span| str_width(&span.content))
            .sum::<usize>();
        let max_workdir = usize::from(area.width).saturating_sub(used + 18).max(8);
        spans.push(Span::styled(
            wrap::truncate(data.workdir.clone(), max_workdir).into_owned(),
            Style::new().fg(theme.gray),
        ));
    }
    // 聚焦子代理：头部常驻标识 + 返回手势，避免忘了自己在谁的会话里。
    if app.focused.is_some() {
        let title = app
            .session
            .as_ref()
            .map_or_else(|| t!("tui.subagent.title").into_owned(), |session| {
                session.title.clone()
            });
        spans.push(Span::styled(
            t!("tui.subagent.header").into_owned(),
            Style::new().fg(theme.accent_tool),
        ));
        spans.push(Span::styled(
            wrap::truncate(title, 24).into_owned(),
            Style::new().fg(theme.text_primary),
        ));
        let back = labels::hints(&[("esc", t!("tui.hint.back"))], usize::MAX);
        // 「Esc 返回」可点：与空输入框按 Esc 同样返回主会话。
        let used: usize = spans
            .iter()
            .map(|span| str_width(&span.content))
            .sum::<usize>()
            + 2;
        let end = (used + str_width(&back)).min(usize::from(area.width));
        if let (Ok(start), Ok(end)) = (u16::try_from(used), u16::try_from(end))
            && start < end
        {
            app.header_back_area = Rect::new(area.x + start, area.y, end - start, 1);
        }
        spans.push(Span::styled(
            format!("  {back}"),
            Style::new().fg(theme.gray_dim),
        ));
    }

    // 右侧：上下文进度条 + 已用/窗口 + 费用
    let mut right: Vec<Span<'static>> = Vec::new();
    if let (Some(percent), Some(window)) = (data.context_percent, data.context_window) {
        let bar_color = context_bar_color(percent, theme);
        right.push(Span::styled(
            context_bar(data.context_used, window, 10),
            Style::new().fg(bar_color),
        ));
        right.push(Span::styled(
            format!(
                " {} / {}",
                format_tokens(data.context_used),
                format_tokens(window)
            ),
            Style::new().fg(theme.gray),
        ));
    }
    if let Some(cost) = data.cost {
        right.push(Span::styled(
            format!("  ${cost:.2}"),
            Style::new().fg(theme.accent_warning),
        ));
    }
    if !right.is_empty() {
        let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
        let right_width: usize = right.iter().map(|span| str_width(&span.content)).sum();
        let width = usize::from(area.width);
        // 挤不下就整段让位：宁可不显示，也不截半截进度条。
        if used + right_width + 2 <= width {
            let right_x = area.x + u16::try_from(width - right_width).unwrap_or(u16::MAX);
            if data.context_percent.is_some() {
                let context_width = right
                    .iter()
                    .take(2)
                    .map(|span| str_width(&span.content))
                    .sum::<usize>();
                app.context_area = Rect {
                    x: right_x,
                    y: area.y,
                    width: u16::try_from(context_width).unwrap_or(u16::MAX),
                    height: area.height,
                };
            }
            spans.push(Span::styled(
                " ".repeat(width - used - right_width),
                Style::new(),
            ));
            spans.extend(right);
        }
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

pub(super) fn draw_statusline(frame: &mut Frame, app: &mut App, area: Rect) {
    if area.is_empty() {
        return;
    }
    // 内置渲染回报右侧统计段宽度：该段可点（与 `/usage` 同义），右对齐贴边。
    // 自定义模板布局未知，不登记命中区。
    let (line, usage_width) = match &app.statusline_template {
        Some(template) => (statusline_from_template(app, template), 0),
        None => statusline_builtin(app, area.width as usize),
    };
    frame.render_widget(Paragraph::new(line), area);
    if usage_width > 0 {
        let width = u16::try_from(usage_width).unwrap_or(area.width);
        app.usage_area = Rect::new(area.x + area.width.saturating_sub(width), area.y, width, 1);
    }
}

pub(super) fn statusline_data(app: &App) -> StatuslineData {
    let status = app.status_label();
    let context_window = app.context_window().filter(|window| *window > 0);
    let context_used = app.context_used();
    let context_percent = context_window.map(|window| context_used as f64 * 100.0 / window as f64);
    let workdir = app
        .session
        .as_ref()
        .map(|session| short_workdir(&session.cwd))
        .unwrap_or_default();
    let prompt_elapsed = if app.prompt_running {
        app.prompt_started_at.map(|at| at.elapsed())
    } else {
        None
    };
    let cost = (app.cost_usd > 0.0).then_some(app.cost_usd);
    // 右下角统计是整个会话的累计值（与 `/usage` 同源），不是单次调用。
    let (tokens_in, tokens_out) = app.session_tokens();
    StatuslineData {
        model: app.model_display().to_owned(),
        status,
        context_percent,
        context_used,
        context_window,
        workdir,
        branch: app.git_branch.clone(),
        todo: {
            let (done, total) = app.todo_progress();
            (total > 0).then_some((done, total))
        },
        tokens_in,
        tokens_out,
        tok_s: app.session_tokens_per_second(),
        prompt_elapsed,
        cost,
    }
}

/// 工作目录缩写：`$HOME` 前缀折叠成 `~`。
pub(super) fn short_workdir(cwd: &str) -> String {
    let Ok(home) = std::env::var("HOME") else {
        return cwd.to_owned();
    };
    if home.is_empty() {
        return cwd.to_owned();
    }
    match cwd.strip_prefix(&home) {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => format!("~{rest}"),
        _ => cwd.to_owned(),
    }
}

/// 工具摘要里的展示路径：`cwd` 内的路径相对化（等于 cwd 显示 `.`），
/// 其余绝对路径交给 [`short_workdir`] 折叠 `$HOME`，相对路径原样。
///
/// 前缀匹配只在 `/` 边界上进行：cwd `/a/b` 不会吃掉 `/a/bc`。
pub(crate) fn display_path<'a>(path: &'a str, cwd: &str) -> std::borrow::Cow<'a, str> {
    if !cwd.is_empty() {
        if path == cwd {
            return ".".into();
        }
        if let Some(rest) = path.strip_prefix(cwd)
            && (rest.starts_with('/') || cwd.ends_with('/'))
        {
            return rest.strip_prefix('/').unwrap_or(rest).into();
        }
    }
    if path.starts_with('/') {
        return short_workdir(path).into();
    }
    path.into()
}

/// 上下文进度条：整数运算算占位（`round(used / window * slots)`），最后一格保留给分母。
///
/// 只用单宽的 `━`/`─`：两端不加全角括号，宽度恒为 `slots`，与右侧数字之间只隔一格空格。
pub(super) fn context_bar(used: u64, window: u64, slots: usize) -> String {
    let slots_u64 = u64::try_from(slots).unwrap_or(1);
    let filled = used
        .saturating_mul(slots_u64)
        .saturating_add(window / 2)
        .checked_div(window)
        .map_or(0, |filled| filled.min(slots_u64));
    let filled = usize::try_from(filled).unwrap_or(slots);
    format!("{}{}", "━".repeat(filled), "─".repeat(slots - filled))
}

/// 上下文进度条按占用率变色。
pub(super) fn context_bar_color(
    percent: f64,
    theme: &crate::theme::Theme,
) -> ratatui::style::Color {
    if percent >= 85.0 {
        theme.accent_error
    } else if percent >= 70.0 {
        theme.accent_warning
    } else {
        theme.accent_success
    }
}

/// token 数按 1k / 1m / 1b 分档显示：`999 → 999`、`1500 → 1.5k`、`1_200_000 → 1.2m`。
///
/// 够得着下一个单位就换单位，绝不写成 `1200k`；不足 1000 直接给原数。进位后
/// 保留两位小数并去掉多余的 0（`1m`、`1.5k`）；舍入把 `999.99k` 抬成 1000 时
/// 一并完成跨档进位（`999_999 → 1m`）。四舍五入在整数上完成，不引入浮点误差。
pub fn format_tokens(value: u64) -> String {
    if value < TOKEN_UNITS[0].0 {
        return value.to_string();
    }
    let index = TOKEN_UNITS
        .iter()
        .rposition(|(scale, _)| value >= *scale)
        .unwrap_or(0);
    let (scale, unit) = TOKEN_UNITS[index];
    // 以 0.01 个当前单位为步长四舍五入。
    let hundredths = (u128::from(value) * 100 + u128::from(scale) / 2) / u128::from(scale);
    let (hundredths, unit) = match TOKEN_UNITS.get(index + 1) {
        // 各档之间正好差 1000 倍，跨档时直接少三位小数。
        Some((_, next)) if hundredths >= 100_000 => (hundredths / 1_000, *next),
        _ => (hundredths, unit),
    };
    let whole = hundredths / 100;
    let frac = hundredths % 100;
    match frac {
        0 => format!("{whole}{unit}"),
        frac if frac.is_multiple_of(10) => format!("{whole}.{}{unit}", frac / 10),
        frac => format!("{whole}.{frac:02}{unit}"),
    }
}

pub(super) fn activity(app: &App) -> Option<Activity> {
    if !app.prompt_running {
        return None;
    }
    if app.cancel_requested {
        return Some(Activity {
            label: t!("tui.status.stopping"),
            color: app.theme.accent_warning,
        });
    }
    if app.skills_compacting {
        return Some(Activity {
            label: t!("tui.status.working"),
            color: app.theme.accent_running,
        });
    }
    if app
        .modal
        .as_ref()
        .is_some_and(|modal| matches!(modal, Modal::Permission { .. } | Modal::Ask { .. }))
    {
        return Some(Activity {
            label: t!("tui.status.waiting"),
            color: app.theme.accent_warning,
        });
    }
    if app
        .blocks
        .iter()
        .rev()
        .any(|entry| matches!(&entry.block, TranscriptBlock::Tool(tool) if tool.running))
    {
        return Some(Activity {
            label: t!("tui.status.running"),
            color: app.theme.accent_running,
        });
    }
    let label = match app.blocks.last().map(|entry| &entry.block) {
        Some(TranscriptBlock::Thinking { .. }) => t!("tui.status.thinking"),
        Some(TranscriptBlock::Assistant { .. }) => t!("tui.status.responding"),
        _ => t!("tui.status.working"),
    };
    Some(Activity {
        label,
        color: app.theme.accent_running,
    })
}

pub(super) fn spinner_frame(app: &App) -> &'static str {
    let elapsed_ms = app
        .prompt_started_at
        .map_or(0, |started| started.elapsed().as_millis());
    let frame_count = u128::try_from(SPINNER_FRAMES.len()).expect("spinner frame count fits u128");
    let frame = (elapsed_ms / 120) % frame_count;
    let index = usize::try_from(frame).expect("spinner frame index fits usize");
    SPINNER_FRAMES[index]
}

/// 窗口标题（OSC 0）：状态图标 + 项目名。
///
/// 等待人工（提问/授权卡片）时给「需人工」提示；运行中复用状态栏
/// 同一个 spinner；空闲只剩 `moc — 项目名`，没有会话时退化为 `moc`。
pub fn terminal_title(app: &App) -> String {
    let cwd = app
        .session
        .as_ref()
        .map_or(app.startup_cwd.as_str(), |session| session.cwd.as_str());
    let project = std::path::Path::new(cwd)
        .file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty());
    let base = match project {
        Some(project) => format!("moc — {project}"),
        None => "moc".to_owned(),
    };
    if app
        .modal
        .as_ref()
        .is_some_and(|modal| matches!(modal, Modal::Permission { .. } | Modal::Ask { .. }))
    {
        t!("tui.status.needs_human_fmt", base = base.as_str()).into_owned()
    } else if app.prompt_running {
        format!("{} {base}", spinner_frame(app))
    } else {
        base
    }
}

/// 内置渲染：左侧运行状态，右侧会话累计 token 统计（与 `/usage` 同源）。
///
/// 返回值附带右侧统计段的显示宽度：统计段可点（等价 `/usage`），由调用方登记命中区；
/// 统计为空或空间不够被裁掉时宽度为 0。
pub(super) fn statusline_builtin(app: &App, width: usize) -> (Line<'static>, usize) {
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    if app.read_only {
        spans.push(Span::styled(
            t!("tui.status.read_only").into_owned(),
            Style::new().fg(theme.accent_warning),
        ));
        spans.push(Span::styled(" · ", Style::new().fg(theme.gray)));
    }
    if let Some(activity) = activity(app) {
        spans.push(Span::styled(
            spinner_frame(app),
            Style::new().fg(activity.color),
        ));
        spans.push(Span::styled(" ", Style::new()));
        spans.push(Span::styled(
            activity.label,
            Style::new().fg(activity.color),
        ));
        if let Some(elapsed) = data.prompt_elapsed {
            spans.push(Span::styled(
                format!(" · {}s", elapsed.as_secs()),
                Style::new().fg(theme.gray),
            ));
        }
    }
    // 右侧统计段：in/out 都是 0 时整段（连同 cost）不显示，空闲不刷一串 0。
    let right = if data.tokens_in == 0 && data.tokens_out == 0 {
        String::new()
    } else {
        format!(
            " ⇅ {} › {} › {:.0} {}",
            format_tokens(data.tokens_in),
            format_tokens(data.tokens_out),
            data.tok_s,
            labels::RATE_UNIT,
        )
    };
    let mut right_cost = if right.is_empty() {
        String::new()
    } else {
        data.cost
            .map(|cost| format!(" › ${cost:.2}"))
            .unwrap_or_default()
    };
    let used: usize = spans.iter().map(|span| str_width(&span.content)).sum();
    let available = width.saturating_sub(used + 2);
    if str_width(&right) + str_width(&right_cost) > available {
        right_cost.clear();
    }
    let right =
        wrap::truncate(right, available.saturating_sub(str_width(&right_cost))).into_owned();
    let right_width = str_width(&right) + str_width(&right_cost);
    let gap = width.saturating_sub(used + right_width).max(1);
    spans.push(Span::styled(" ".repeat(gap), Style::new()));
    spans.push(Span::styled(right, Style::new().fg(theme.gray)));
    spans.push(Span::styled(
        right_cost,
        Style::new().fg(theme.accent_warning),
    ));
    (Line::from(spans), right_width)
}

/// 模板渲染：`{model} · {status} → {workdir}`，占位符按语义着色。
pub(super) fn statusline_from_template(app: &App, template: &str) -> Line<'static> {
    let theme = app.theme;
    let data = statusline_data(app);
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut plain = String::new();
    let mut rest = template;
    while let Some(start) = rest.find('{') {
        let Some(end) = rest[start..].find('}') else {
            break;
        };
        plain.push_str(&rest[..start]);
        let placeholder = &rest[start + 1..start + end];
        let value: Option<(String, ratatui::style::Color)> = match placeholder {
            "model" => Some((data.model.clone(), theme.accent_model)),
            "status" => Some((data.status.clone(), theme.accent_tool)),
            "context" => data
                .context_percent
                .map(|_| (format_tokens(data.context_used), theme.text_secondary)),
            "context_percent" => data
                .context_percent
                .map(|percent| (format_context_percent(percent), theme.text_secondary)),
            "workdir" => Some((data.workdir.clone(), theme.path)),
            "branch" => data
                .branch
                .clone()
                .map(|branch| (format!("⎇ {branch}"), theme.accent_success)),
            "todo" => data
                .todo
                .map(|(done, total)| (format!("☑ {done}/{total}"), theme.accent_success)),
            "tokens_in" => Some((format_tokens(data.tokens_in), theme.text_secondary)),
            "tokens_out" => Some((format_tokens(data.tokens_out), theme.text_secondary)),
            "tok_s" => Some((
                format!("{:.1} {}", data.tok_s, labels::RATE_UNIT),
                theme.accent_running,
            )),
            "prompt_elapsed" => Some((
                data.prompt_elapsed
                    .map(|elapsed| {
                        labels::elapsed(u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
                    })
                    .unwrap_or_else(|| "◷ -".into()),
                theme.gray,
            )),
            "cost" => data
                .cost
                .map(|cost| (format!("${cost:.2}"), theme.accent_warning)),
            other => {
                // 未知占位符按原文保留
                plain.push_str(&format!("{{{other}}}"));
                None
            }
        };
        if let Some((text, color)) = value {
            if !plain.is_empty() {
                spans.push(Span::styled(
                    std::mem::take(&mut plain),
                    Style::new().fg(theme.gray),
                ));
            }
            spans.push(Span::styled(text, Style::new().fg(color)));
        }
        rest = &rest[start + end + 1..];
    }
    plain.push_str(rest);
    if !plain.is_empty() {
        spans.push(Span::styled(plain, Style::new().fg(theme.gray)));
    }
    Line::from(spans)
}

/// 上下文百分比标签：非零但不足 1% 时显示 `<1%`，避免截断成 0%。
pub(super) fn format_context_percent(percent: f64) -> String {
    if percent > 0.0 && percent < 1.0 {
        "<1%".into()
    } else {
        format!("{percent:.0}%")
    }
}

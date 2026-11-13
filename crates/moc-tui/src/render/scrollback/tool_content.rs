use super::super::diff;
use super::super::image::ImagePlacement;
use super::super::image::ImageState;
use super::super::image::layout as image_layout;
use super::super::labels;
use super::super::markdown::render_markdown;
use super::super::wrap::str_width;
use super::super::wrap::wrap_text;
use super::AGENT_MESSAGE_PREVIEW_ROWS;
use super::ASK_TRANSCRIPT_KEY_MAX;
use super::BASH_BRIEF_OUTPUT_ROWS;
use super::BRIEF_BODY_ROWS;
use super::BodyEdge;
use super::FULL_BODY_ROWS;
use super::agent_tools::ask_answer_pairs;
use super::agent_tools::message_agent_rows_count;
use super::agent_tools::message_agent_tool_rows;
use super::agent_tools::sidekick_rows_count;
use super::agent_tools::sidekick_tool_rows;
use super::agent_tools::wait_agent_rows_count;
use super::agent_tools::wait_agent_tool_rows;
use super::structured_tools::execute_rows;
use super::structured_tools::single_line_command;
use super::structured_tools::todo_tree_rows;
use super::text::clamp_rows;
use super::text::head_lines;
use super::text::plain_text;
use crate::app::App;
use crate::theme::Theme;
use crate::transcript::DetailLevel;
use crate::transcript::MAX_TOOL_IMAGES;
use crate::transcript::ToolBody;
use crate::transcript::ToolCallBlock;
use crate::transcript::tool_body_policy;
use ratatui::style::Modifier;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

/// 工具正文行（不含标题与留白，不带缩进，宽度不超过 `width`）。
///
/// 正文先按档位决定是否可见，再按 [`content_limit`] 截断；被略去的内容补一行
/// 省略提示（`bash` 结果取尾部最新输出，其他文本结果取开头）。
/// `command_in_title` 为真时 bash 命令已在标题里，正文不再重复 `$` 行。
pub(super) fn tool_body(
    tool: &ToolCallBlock,
    args: &Value,
    app: &App,
    width: usize,
    level: DetailLevel,
    spawn_images: bool,
    command_in_title: bool,
) -> (Vec<Line<'static>>, Vec<ImagePlacement>) {
    if !shows_tool_content(tool, level) {
        let mut lines = fixed_tool_rows(tool, args, width, app.theme, true);
        if prefer_builtin_redirect(tool) {
            let result = tool.result.as_deref().unwrap_or_default();
            let text = result.strip_prefix("[E_PREFER_BUILTIN] ").unwrap_or(result);
            lines.extend(
                wrap_text(text, width)
                    .into_iter()
                    .map(|row| prefer_builtin_row(row, app.theme)),
            );
        }
        return (lines, Vec::new());
    }
    let theme = app.theme;
    let is_error = tool_is_error(tool);
    let policy = tool_body_policy(&tool.name);

    if !is_error
        && matches!(
            policy,
            ToolBody::None | ToolBody::Summary | ToolBody::AgentPreview
        )
    {
        let mut lines = fixed_tool_rows(tool, args, width, theme, false);
        let placements = tool_images(tool, app, width, spawn_images, &mut lines);
        return (lines, placements);
    }

    let limit = content_limit(level);
    let mut lines = Vec::new();
    let mut dropped = 0usize;
    let text_row = |text: String, color| Line::from(Span::styled(text, Style::new().fg(color)));

    if let Some(diff) = &tool.diff {
        let rendered = diff::rendered_row_count(diff, limit);
        lines = diff::render_diff(diff, width, theme, limit);
        dropped += diff::row_count(diff).saturating_sub(rendered);
    } else if tool.name == "bash" {
        if !command_in_title {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if level == DetailLevel::Full {
                let (shown_command, command_dropped) = head_lines(command, limit);
                for (index, row) in wrap_text(&shown_command, width.saturating_sub(2))
                    .into_iter()
                    .enumerate()
                {
                    let lead = if index == 0 { "$ " } else { "  " };
                    lines.push(Line::from(vec![
                        Span::styled(lead, Style::new().fg(theme.gray_dim)),
                        Span::styled(row, Style::new().fg(theme.command)),
                    ]));
                }
                dropped += command_dropped;
            } else {
                let command = single_line_command(command, width.saturating_sub(2));
                lines.push(Line::from(vec![
                    Span::styled("$ ", Style::new().fg(theme.gray_dim)),
                    Span::styled(command, Style::new().fg(theme.command)),
                ]));
            }
        }
        // 运行中的前台任务：显示实时输出而不是完整结果
        if tool.running && tool.live_output.is_some() {
            if let Some(live_out) = &tool.live_output {
                let budget = BASH_BRIEF_OUTPUT_ROWS;
                let rows = clamp_rows(live_out, width, budget, BodyEdge::Tail);
                let color = theme.gray_bright;
                lines.extend(rows.rows.into_iter().map(|text| text_row(text, color)));
            }
        } else if let Some(result) = &tool.result {
            let budget = if level == DetailLevel::Full {
                limit.saturating_sub(lines.len())
            } else {
                BASH_BRIEF_OUTPUT_ROWS
            };
            if prefer_builtin_redirect(tool) {
                // 策略拒绝按建议展示：去掉错误码前缀、从开头截断（说明句在前），
                // 正文用普通输出色；`- <tool> {…}` 建议行的工具名用工具色标出。
                let text = result.strip_prefix("[E_PREFER_BUILTIN] ").unwrap_or(result);
                let rows = clamp_rows(text, width, budget, BodyEdge::Head);
                dropped += rows.dropped;
                for text in rows.rows {
                    lines.push(prefer_builtin_row(text, theme));
                }
            } else {
                let rows = clamp_rows(result, width, budget, BodyEdge::Tail);
                dropped += rows.dropped;
                let color = if is_error {
                    theme.diff_delete_fg
                } else {
                    theme.gray_bright
                };
                lines.extend(rows.rows.into_iter().map(|text| text_row(text, color)));
            }
        }
    } else if tool.name == "message_agent" {
        let (rows, omitted) = message_agent_tool_rows(tool, args, width, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "wait_agent" {
        let (rows, omitted) = wait_agent_tool_rows(tool, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "sidekick" && !is_error {
        let (rows, omitted) = sidekick_tool_rows(tool, width, theme, limit);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "ask" && !is_error {
        // 每问恒占一行：key（header 兜底 id）降档对齐、答案单行截断，
        // 「未回答」用确认页同款警告标记；行数上限走 content_limit，超出的计 dropped。
        let pairs = ask_answer_pairs(tool, args);
        let key_width = pairs
            .iter()
            .map(|(key, ..)| str_width(key))
            .max()
            .unwrap_or(0)
            .min(ASK_TRANSCRIPT_KEY_MAX);
        let answer_width = width.saturating_sub(key_width + 2);
        for (index, (key, text, unanswered)) in pairs.iter().enumerate() {
            if index >= limit {
                dropped += 1;
                continue;
            }
            let mut spans = Vec::with_capacity(2);
            if !key.is_empty() {
                let key = crate::render::wrap::truncate(key.clone(), key_width).into_owned();
                let pad = " ".repeat(key_width.saturating_sub(str_width(&key)));
                spans.push(Span::styled(
                    format!("{key}{pad}: "),
                    Style::new().fg(theme.text_secondary),
                ));
            }
            spans.push(Span::styled(
                crate::render::wrap::truncate(text.clone(), answer_width).into_owned(),
                if *unanswered {
                    Style::new()
                        .fg(theme.accent_warning)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::new().fg(theme.text_primary)
                },
            ));
            lines.push(Line::from(spans));
        }
    } else if tool.name == "execute" && !is_error {
        let (rows, omitted) = execute_rows(tool, args, width, limit, theme);
        dropped += omitted;
        lines.extend(rows);
    } else if tool.name == "todo" && !is_error {
        let phases = tool
            .result
            .as_deref()
            .map(moc_protocol::methods::parse_todos)
            .unwrap_or_default();
        lines.extend(todo_tree_rows(&phases, width, level, theme));
    } else if let Some(result) = &tool.result {
        // grep 与通用文本结果：取开头；失败输出给错误色
        let rows = clamp_rows(result, width, limit, BodyEdge::Head);
        dropped += rows.dropped;
        let color = if is_error {
            theme.diff_delete_fg
        } else {
            theme.gray_bright
        };
        lines.extend(rows.rows.into_iter().map(|text| text_row(text, color)));
    }

    // 省略提示统一由此处追加；diff 分支由 `render_diff` 自己产出（只有它知道
    // diff 的总行数），因此不叠加第二行。
    if dropped > 0 && tool.diff.is_none() {
        lines.push(text_row(labels::more_lines(dropped), theme.gray_dim));
    }
    if tool.has_notices() {
        // 通知可能落在预览裁掉的中段；只补充尚未显示的内容。
        let shown: String = lines
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_ref())
            .collect();
        for notice in crate::transcript::tool_notices(tool.result.as_deref().unwrap_or_default()) {
            if !shown.contains(notice) {
                lines.extend(
                    wrap_text(notice, width)
                        .into_iter()
                        .map(|row| text_row(row, theme.accent_warning)),
                );
            }
        }
    }
    let placements = tool_images(tool, app, width, spawn_images, &mut lines);
    (lines, placements)
}

/// 工具正文的展示行数：与 [`tool_body`] 同规则（含省略提示行），懒渲染估高用。
pub(super) fn tool_body_rows(
    tool: &ToolCallBlock,
    args: &Value,
    app: &App,
    width: usize,
    level: DetailLevel,
    command_in_title: bool,
) -> usize {
    if !shows_tool_content(tool, level) || tool.has_notices() {
        return tool_body(tool, args, app, width, level, false, command_in_title)
            .0
            .len();
    }
    let is_error = tool_is_error(tool);
    let policy = tool_body_policy(&tool.name);

    if !is_error
        && matches!(
            policy,
            ToolBody::None | ToolBody::Summary | ToolBody::AgentPreview
        )
    {
        let mut rows = fixed_tool_rows(tool, args, width, app.theme, false);
        tool_images(tool, app, width, false, &mut rows);
        return rows.len();
    }

    let limit = content_limit(level);
    let mut rows = 0usize;
    let mut dropped = 0usize;

    if let Some(diff) = &tool.diff {
        let rendered = diff::rendered_row_count(diff, limit);
        rows += rendered;
        dropped += diff::row_count(diff).saturating_sub(rendered);
    } else if tool.name == "bash" {
        if !command_in_title {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if level == DetailLevel::Full {
                let (shown_command, command_dropped) = head_lines(command, limit);
                rows += wrap_text(&shown_command, width.saturating_sub(2)).len();
                dropped += command_dropped;
            } else {
                rows += 1;
            }
        }
        if let Some(result) = &tool.result {
            let budget = if level == DetailLevel::Full {
                limit.saturating_sub(rows)
            } else {
                BASH_BRIEF_OUTPUT_ROWS
            };
            let clamped = clamp_rows(result, width, budget, BodyEdge::Tail);
            rows = rows.saturating_add(clamped.shown);
            dropped = dropped.saturating_add(clamped.dropped);
        }
    } else if tool.name == "message_agent" {
        let (count, omitted) = message_agent_rows_count(tool, args, width, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "wait_agent" {
        let (count, omitted) = wait_agent_rows_count(tool, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "sidekick" && !is_error {
        let (count, omitted) = sidekick_rows_count(tool, width, limit);
        rows += count;
        dropped += omitted;
    } else if tool.name == "ask" && !is_error {
        // 每问恒占一行（与 [`tool_body`] 同规则）：行数上限 = 答案条数。
        let count = ask_answer_pairs(tool, args).len();
        rows += count.min(limit);
        dropped += count.saturating_sub(limit);
    } else if tool.name == "execute" && !is_error {
        // 完整档 execute 直接复用行构造函数取行数，估高与渲染同源。
        let (constructed, omitted) = execute_rows(tool, args, width, limit, app.theme);
        rows += constructed.len();
        dropped += omitted;
    } else if tool.name == "todo" && !is_error {
        let phases = tool
            .result
            .as_deref()
            .map(moc_protocol::methods::parse_todos)
            .unwrap_or_default();
        rows += todo_tree_rows(&phases, width, level, app.theme).len();
    } else if let Some(result) = &tool.result {
        let clamped = clamp_rows(result, width, limit, BodyEdge::Head);
        rows += clamped.shown;
        dropped += clamped.dropped;
    }

    // 图片行与会话状态相关（解码中 / 就绪 / 失败），只能复用渲染判定
    let mut probe = Vec::new();
    tool_images(tool, app, width, false, &mut probe);
    rows += probe.len();
    rows + usize::from(dropped > 0)
}

/// 把工具结果里的图片追加到正文行后，返回它们的落位。
///
/// 行数与 [`tool_body_rows`] 走同一份判定：状态（解码中 / 就绪 / 失败）决定占
/// 几行，两侧必须完全一致，否则懒渲染的块高会与真实渲染错位。
pub(super) fn tool_images(
    tool: &ToolCallBlock,
    app: &App,
    width: usize,
    spawn_images: bool,
    lines: &mut Vec<Line<'static>>,
) -> Vec<ImagePlacement> {
    let theme = app.theme;
    let mut placements = Vec::new();
    if tool.images.is_empty() {
        return placements;
    }
    let max_cols = u16::try_from(width).unwrap_or(u16::MAX);
    let shown = tool.images.len().min(MAX_TOOL_IMAGES);
    let note = |text: String| Line::from(Span::styled(text, Style::new().fg(theme.gray_dim)));
    for image in &tool.images[..shown] {
        let Some(graphics) = app.graphics else {
            lines.push(note(format!("[图片 {}]", image.media_type)));
            continue;
        };
        let state = if spawn_images {
            match image.spec_factory() {
                Some(make) => app.images.state(&image.key, make),
                // 超出显示上限的图在落地时已丢弃载荷：不该走到这里
                None => ImageState::Failed("图片载荷已丢弃".into()),
            }
        } else {
            match app.images.peek(&image.key) {
                Some(state) => state,
                None => ImageState::Pending,
            }
        };
        match state {
            ImageState::Ready(prepared) => {
                let Some(layout) = image_layout(prepared.size(), graphics.cell, max_cols) else {
                    lines.push(note(format!("[图片 {}]", image.media_type)));
                    continue;
                };
                let row = lines.len();
                for _ in 0..layout.rows {
                    lines.push(Line::from(" ".repeat(width)));
                }
                placements.push(ImagePlacement {
                    row,
                    // 正文行不带内部前缀，落位列由调用方随正文一起平移
                    x: 0,
                    cols: layout.cols,
                    rows: layout.rows,
                    px: layout.px,
                    key: image.key.clone(),
                });
            }
            ImageState::Pending => lines.push(note("图片解码中…".to_owned())),
            ImageState::Failed(reason) => lines.push(note(format!("图片无法显示：{reason}"))),
        }
    }
    if tool.images.len() > shown {
        lines.push(note(format!(
            "{} 还有 {} 张图片",
            labels::OMITTED,
            tool.images.len() - shown
        )));
    }
    placements
}

/// 固定摘要只渲染元信息、任务／报告开头和截断警告，不显示原始正文。
pub(super) fn fixed_tool_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    theme: &Theme,
    warnings_only: bool,
) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    let mut preview = |label: &str, text: &str| {
        rows.push(Line::from(Span::styled(
            label.to_owned(),
            Style::new()
                .fg(theme.text_secondary)
                .add_modifier(Modifier::BOLD),
        )));
        let (head, omitted) = head_lines(text, BRIEF_BODY_ROWS);
        let rendered = render_markdown(&head, width.max(8), theme);
        let mut content = rendered
            .into_iter()
            .filter(|line| !plain_text(line).trim().is_empty());
        rows.extend(content.by_ref().take(AGENT_MESSAGE_PREVIEW_ROWS));
        if omitted > 0 || content.next().is_some() {
            rows.push(Line::from(Span::styled(
                "⋯ 仅显示开头",
                Style::new().fg(theme.gray_dim),
            )));
        }
    };
    if tool.name == "spawn_agent" && !warnings_only {
        if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
            for task in tasks {
                preview(
                    task.get("title")
                        .and_then(Value::as_str)
                        .unwrap_or("子代理任务"),
                    task.get("task").and_then(Value::as_str).unwrap_or_default(),
                );
            }
        } else {
            preview(
                "子代理任务",
                args.get("task").and_then(Value::as_str).unwrap_or_default(),
            );
        }
    } else if tool.name == "yield" && !warnings_only {
        preview(
            "交付报告",
            args.get("report")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
    }
    if let Some(result) = tool.result.as_deref() {
        if tool.name == "web_fetch"
            && let Some(json) = result
                .lines()
                .next()
                .and_then(|line| line.strip_prefix("Web fetch summary: "))
            && let Ok(Value::Array(pages)) = serde_json::from_str::<Value>(json)
        {
            for page in pages {
                if warnings_only
                    && page.get("truncated").and_then(Value::as_bool) != Some(true)
                    && page.get("error").is_none()
                {
                    continue;
                }
                let url = page.get("url").and_then(Value::as_str).unwrap_or_default();
                let title = page.get("title").and_then(Value::as_str).unwrap_or(url);
                let text = if let Some(error) = page.get("error").and_then(Value::as_str) {
                    format!("抓取失败：{url} — {error}")
                } else {
                    format!(
                        "{title} · 约 {} tokens{}",
                        page.get("tokens").and_then(Value::as_u64).unwrap_or(0),
                        if page.get("truncated").and_then(Value::as_bool) == Some(true) {
                            " · 警告：内容已截断"
                        } else {
                            ""
                        }
                    )
                };
                let color = if page.get("error").is_some()
                    || page.get("truncated").and_then(Value::as_bool) == Some(true)
                {
                    theme.accent_warning
                } else {
                    theme.gray_bright
                };
                rows.extend(
                    wrap_text(&text, width)
                        .into_iter()
                        .map(|row| Line::from(Span::styled(row, Style::new().fg(color)))),
                );
            }
        }
        for notice in crate::transcript::tool_notices(result) {
            rows.extend(
                wrap_text(notice, width).into_iter().map(|row| {
                    Line::from(Span::styled(row, Style::new().fg(theme.accent_warning)))
                }),
            );
        }
    }
    rows
}

/// 警告不受详细程度限制，其余正文遵循全局预设或单条展开。
pub(super) fn shows_tool_body(tool: &ToolCallBlock, level: DetailLevel) -> bool {
    shows_tool_content(tool, level)
        || tool.has_notices()
        || prefer_builtin_redirect(tool)
        || tool.name == "web_fetch"
}

pub(super) fn shows_tool_content(tool: &ToolCallBlock, level: DetailLevel) -> bool {
    if level == DetailLevel::Hidden || !tool.has_body() {
        return false;
    }
    if level == DetailLevel::Full {
        return true;
    }
    match tool_body_policy(&tool.name) {
        ToolBody::Summary => tool.name == "web_fetch" || tool_is_error(tool),
        ToolBody::FullOnly => tool_is_error(tool),
        _ => true,
    }
}

/// 普通正文的展示行上限；隐藏档为 0（计数型工具不进正文，数量在标题尾列）。
pub(super) fn content_limit(level: DetailLevel) -> usize {
    match level {
        DetailLevel::Hidden => 0,
        DetailLevel::Brief => BRIEF_BODY_ROWS,
        DetailLevel::Full => FULL_BODY_ROWS,
    }
}

/// 工具是否以失败终态结束；运行中（`status: None`）不算失败。
pub(super) fn tool_is_error(tool: &ToolCallBlock) -> bool {
    tool.status
        .is_some_and(|status| status != moc_protocol::methods::ToolStatus::Completed)
}

/// bash 被「内置工具优先」策略拒绝：结果带固定前缀，按建议而非失败展示。
pub(super) fn prefer_builtin_redirect(tool: &ToolCallBlock) -> bool {
    tool_is_error(tool)
        && tool.name == "bash"
        && tool
            .result
            .as_deref()
            .is_some_and(|result| result.starts_with("[E_PREFER_BUILTIN]"))
}

/// 策略拒绝正文的一行：`- <tool> {…}` 建议行的工具名用工具色标出，其余行整行正文色。
pub(super) fn prefer_builtin_row(text: String, theme: &Theme) -> Line<'static> {
    if let Some(rest) = text.strip_prefix("- ")
        && let Some(split) = rest.find(' ')
    {
        let (name, args) = rest.split_at(split);
        return Line::from(vec![
            Span::styled("- ", Style::new().fg(theme.gray_dim)),
            Span::styled(name.to_owned(), Style::new().fg(theme.accent_tool)),
            Span::styled(args.to_owned(), Style::new().fg(theme.gray_bright)),
        ]);
    }
    Line::from(Span::styled(text, Style::new().fg(theme.gray_bright)))
}

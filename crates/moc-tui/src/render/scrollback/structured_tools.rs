use rust_i18n::t;

use super::super::labels;
use super::super::wrap::clip_line;
use super::super::wrap::str_width;
use super::super::wrap::wrap_text;
use super::BodyEdge;
use super::layout::tree_rail_style;
use super::text::clamp_rows;
use super::todo_status_marker;
use crate::theme::Theme;
use crate::transcript::DetailLevel;
use crate::transcript::ToolCallBlock;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

/// grep／find 在隐藏、简略档的计数 span；其它工具返回 `None`。
///
/// 非零计数用 [`Theme::text_secondary`]，零结果退为 [`Theme::gray`]，分隔符用 [`Theme::gray_dim`]。
pub(super) fn tool_count_spans(
    tool: &ToolCallBlock,
    result: &str,
    theme: &Theme,
) -> Option<Vec<Span<'static>>> {
    let count_span = |count: usize, singular: &str, plural: &str| {
        Span::styled(
            format_count(count, singular, plural),
            Style::new().fg(if count > 0 {
                theme.text_secondary
            } else {
                theme.gray
            }),
        )
    };
    match tool.name.as_str() {
        "grep" => {
            let (matches, files) = grep_counts(result);
            Some(vec![
                count_span(matches, t!("tui.tool.match_one").as_ref(), t!("tui.tool.match_many").as_ref()),
                Span::styled(labels::SEP.to_owned(), Style::new().fg(theme.gray_dim)),
                count_span(files, t!("tui.tool.file_one").as_ref(), t!("tui.tool.file_many").as_ref()),
            ])
        }
        "find" => Some(vec![count_span(
            find_file_count(result),
            t!("tui.tool.file_one").as_ref(),
            t!("tui.tool.file_many").as_ref(),
        )]),
        _ => None,
    }
}

/// 数量标签：单数用 `1 file`，其余用 `N files`。
pub(super) fn format_count(count: usize, singular: &str, plural: &str) -> String {
    if count == 1 {
        format!("1 {singular}")
    } else {
        format!("{count} {plural}")
    }
}

/// grep 结果里的匹配行与文件数。
///
/// 行格式由 core 固定，每个文件是两种形态之一：单行文件写成 `路径:行号:文本`
/// （匹配）或 `路径-行号-文本`（上下文）；多行文件写成 `路径` 标题行加若干
/// 「两个空格 + 行号 + 分隔符 + 文本」的行。缩进行只从行首解析，正文里的
/// `12:34:56` 不会参与判断；`[` 开头的提示行跳过。
pub(super) fn grep_counts(result: &str) -> (usize, usize) {
    let mut matches = 0usize;
    let mut files = std::collections::BTreeSet::new();
    let mut current: Option<&str> = None;
    for line in result.lines() {
        if line.starts_with('[') {
            continue;
        }
        if let Some(marker) = grouped_row_marker(line) {
            if marker == b':' {
                matches += 1;
                if let Some(path) = current {
                    files.insert(path);
                }
            }
            continue;
        }
        if let Some((path, marker)) = grep_row_prefix(line) {
            current = Some(path);
            if marker == b':' {
                matches += 1;
                files.insert(path);
            }
            continue;
        }
        current = Some(line);
    }
    (matches, files.len())
}

/// 分组结果行的「行号 + 分隔符」前缀：`  12: 文本` 与 `  12- 文本`。
///
/// 只解析行首，避免把正文里的 `12:34:56` 当成行号前缀。返回分隔符。
pub(super) fn grouped_row_marker(line: &str) -> Option<u8> {
    let rest = line.strip_prefix("  ")?;
    let digits = rest.find(|ch: char| !ch.is_ascii_digit())?;
    if digits == 0 {
        return None;
    }
    let marker = *rest.as_bytes().get(digits)?;
    matches!(marker, b':' | b'-').then_some(marker)
}

/// 找 grep 结果行的「路径 + 分隔符 + 行号 + 分隔符」前缀。
pub(super) fn grep_row_prefix(line: &str) -> Option<(&str, u8)> {
    let bytes = line.as_bytes();
    for (index, byte) in bytes.iter().enumerate() {
        if !matches!(*byte, b':' | b'-') {
            continue;
        }
        let mut end = index + 1;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end > index + 1 && bytes.get(end) == Some(byte) {
            return Some((&line[..index], *byte));
        }
    }
    None
}

/// find 结果里的路径数；跳过空行与 core 追加的提示行。
pub(super) fn find_file_count(result: &str) -> usize {
    result
        .lines()
        .filter(|line| {
            let line = line.trim();
            !line.is_empty()
                && line != "No files found matching pattern"
                && !line.starts_with("[limit ")
                && !line.starts_with("[Scan incomplete")
        })
        .count()
}

/// 把命令折成单行预览：换行与连续空白折叠成一个空格，再按宽度截断。
pub(super) fn single_line_command(command: &str, width: usize) -> String {
    let flattened = command.split_whitespace().collect::<Vec<_>>().join(" ");
    crate::render::wrap::truncate(flattened, width).into_owned()
}

/// todo 调用结果的快照树：结果文本解析出的阶段列表，正文列宽度下的树形行。
///
/// 单阶段省略阶段标题，多阶段保留阶段树。简略档按整份清单保留前三项和后两项，
/// 不因完成状态隐藏任务；省略提示放在第三项后，不截掉末尾任务。
pub(super) fn todo_tree_rows(
    phases: &[moc_protocol::methods::TodoPhase],
    width: usize,
    level: DetailLevel,
    theme: &Theme,
) -> Vec<Line<'static>> {
    use moc_protocol::methods::TodoStatus;

    let rail_style = tree_rail_style(theme);
    let total: usize = phases.iter().map(|phase| phase.items.len()).sum();
    let omitted = if level == DetailLevel::Brief {
        total.saturating_sub(5)
    } else {
        0
    };
    let tail = total.saturating_sub(2);
    let single = phases.len() == 1;
    let mut offset = 0;
    // 任务文本列：连接符/竖线 3 格 + 状态符号 2 格
    let text_width = width.saturating_sub(5).max(1);
    let mut rows: Vec<Line<'static>> = Vec::new();
    for (phase_index, phase) in phases.iter().enumerate() {
        let start = offset;
        offset += phase.items.len();
        if omitted > 0 && start >= 3 && offset <= tail {
            continue;
        }
        let last_phase = phase_index + 1 == phases.len();
        if !single {
            let done = phase
                .items
                .iter()
                .filter(|item| item.status == TodoStatus::Completed)
                .count();
            let total = phase.items.len();
            let count = format!("{}{done}/{total}", labels::SEP);
            let name_budget = width.saturating_sub(3 + str_width(&count));
            rows.push(clip_line(
                Line::from(vec![
                    Span::styled(if last_phase { "└─ " } else { "├─ " }, rail_style),
                    Span::styled(
                        crate::render::wrap::truncate(phase.phase.as_str(), name_budget)
                            .into_owned(),
                        Style::new().fg(theme.text_secondary),
                    ),
                    Span::styled(
                        count,
                        Style::new().fg(if done == total && total > 0 {
                            theme.accent_success
                        } else {
                            theme.gray_dim
                        }),
                    ),
                ]),
                width,
            ));
        }
        if phase.items.is_empty() {
            continue;
        }
        let phase_rail = if last_phase { "   " } else { "│  " };
        for (item_index, item) in phase.items.iter().enumerate() {
            let index = start + item_index;
            if omitted > 0 && (3..tail).contains(&index) {
                continue;
            }
            let last_leaf = item_index + 1 == phase.items.len();
            let (marker, marker_style, text_style) = todo_status_marker(item.status, theme);
            let wrapped = wrap_text(&item.task, text_width);
            let last_row = wrapped.len() - 1;
            for (row_index, row_text) in wrapped.into_iter().enumerate() {
                let mut spans = if single {
                    vec![Span::styled(
                        if row_index == 0 {
                            if last_leaf { "└─ " } else { "├─ " }
                        } else if last_leaf {
                            "   "
                        } else {
                            "│  "
                        },
                        rail_style,
                    )]
                } else {
                    vec![Span::styled(phase_rail, rail_style)]
                };
                spans.push(if row_index == 0 {
                    Span::styled(marker, marker_style)
                } else {
                    Span::raw("  ")
                });
                spans.push(Span::styled(row_text, text_style));
                if row_index == last_row
                    && let Some(reason) = &item.reason
                {
                    spans.push(Span::styled(
                        format!(" — {reason}"),
                        Style::new().fg(theme.gray),
                    ));
                }
                rows.push(clip_line(Line::from(spans), width));
            }
            if omitted > 0 && index == 2 {
                rows.push(Line::from(vec![
                    Span::styled(if single { "├─ " } else { "   " }, rail_style),
                    Span::styled(
                        t!("tui.tool.omitted_fmt", n = omitted).into_owned(),
                        Style::new().fg(theme.gray_dim),
                    ),
                ]));
            }
        }
    }
    rows
}

/// execute 完整档正文：高亮脚本（行号槽 `{n:>w} │ `）+ `result` 段。
///
/// `result` 能解析成 JSON object/array 就画 JSON 树，标量并进标签行
/// （`result: 42`），不是 JSON 按普通文本（Head 截断）；任何档位都不把
/// 原始返回值 JSON 直接倒出来。返回（展示行、被略去的行数）。
pub(super) fn execute_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    limit: usize,
    theme: &Theme,
) -> (Vec<Line<'static>>, usize) {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut dropped = 0usize;
    let push = |lines: &mut Vec<Line<'static>>, line: Line<'static>, dropped: &mut usize| {
        if lines.len() >= limit {
            *dropped += 1;
        } else {
            lines.push(line);
        }
    };
    let code = args.get("code").and_then(Value::as_str).unwrap_or_default();
    let highlighted = super::super::highlight::highlight("javascript", code);
    let digits = highlighted.lines.len().max(1).to_string().len();
    let gutter = digits + 3;
    let text_width = width.saturating_sub(gutter).max(8);
    let rail = tree_rail_style(theme);
    for (index, spans) in highlighted.lines.iter().enumerate() {
        let styled: Vec<Span<'static>> = spans
            .iter()
            .map(|part| {
                Span::styled(
                    part.text.clone(),
                    Style::new().fg(part
                        .fg
                        .map(super::super::highlight::rgb_to_color)
                        .unwrap_or(theme.text_secondary)),
                )
            })
            .collect();
        for (row_index, row) in super::super::wrap::wrap_spans(&styled, text_width)
            .into_iter()
            .enumerate()
        {
            let mut line = Vec::with_capacity(row.spans.len() + 2);
            line.push(Span::styled(
                if row_index == 0 {
                    format!("{:>w$} ", index + 1, w = digits)
                } else {
                    " ".repeat(digits + 1)
                },
                Style::new().fg(theme.gray_dim),
            ));
            line.push(Span::styled("│ ", rail));
            line.extend(row.spans);
            push(&mut lines, Line::from(line), &mut dropped);
        }
    }
    if let Some(result) = tool.result.as_deref() {
        match serde_json::from_str::<Value>(result) {
            Ok(value) if value.is_object() || value.is_array() => {
                push(
                    &mut lines,
                    Line::from(Span::styled(
                        t!("tui.tool.result_label").into_owned(),
                        Style::new().fg(theme.gray),
                    )),
                    &mut dropped,
                );
                for row in json_member_rows(&value, &[], 1, width, theme) {
                    push(&mut lines, row, &mut dropped);
                }
            }
            Ok(value) => {
                let mut spans = vec![Span::styled(
                    t!("tui.tool.result_prefix").into_owned(),
                    Style::new().fg(theme.gray),
                )];
                spans.extend(json_scalar_spans(&value, width.saturating_sub(8), theme));
                push(
                    &mut lines,
                    clip_line(Line::from(spans), width),
                    &mut dropped,
                );
            }
            Err(_) => {
                let budget = limit.saturating_sub(lines.len());
                let clamped = clamp_rows(result, width, budget, BodyEdge::Head);
                dropped += clamped.dropped;
                for row in clamped.rows {
                    lines.push(Line::from(Span::styled(
                        row,
                        Style::new().fg(theme.gray_bright),
                    )));
                }
            }
        }
    }
    (lines, dropped)
}

/// JSON 成员行：`├─ key: 值`；容器成员独占一行、子成员挂在竖线下。
///
/// 深度超过 6 的容器折成 `{N keys}`／`[N items]`；空容器写 `{}`／`[]`。
pub(super) fn json_member_rows(
    value: &Value,
    rails: &[bool],
    depth: usize,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let members: Vec<(String, &Value)> = match value {
        Value::Object(map) => map.iter().map(|(key, v)| (key.clone(), v)).collect(),
        Value::Array(items) => items
            .iter()
            .enumerate()
            .map(|(index, v)| (format!("[{index}]"), v))
            .collect(),
        _ => Vec::new(),
    };
    let mut rows = Vec::new();
    for (index, (key, member)) in members.iter().enumerate() {
        let last = index + 1 == members.len();
        let mut prefix = String::new();
        for rail in rails {
            prefix.push_str(if *rail { "│  " } else { "   " });
        }
        prefix.push_str(if last { "└─ " } else { "├─ " });
        let content_width = width.saturating_sub(str_width(&prefix)).max(8);
        let mut spans = vec![
            Span::styled(prefix, tree_rail_style(theme)),
            Span::styled(key.clone(), Style::new().fg(theme.accent_user)),
        ];
        let children = match member {
            Value::Object(map) if !map.is_empty() && depth < 6 => Some(json_member_rows(
                member,
                &[rails, &[!last]].concat(),
                depth + 1,
                width,
                theme,
            )),
            Value::Array(items) if !items.is_empty() && depth < 6 => Some(json_member_rows(
                member,
                &[rails, &[!last]].concat(),
                depth + 1,
                width,
                theme,
            )),
            _ => None,
        };
        if let Some(children) = children {
            rows.push(clip_line(Line::from(spans), width));
            rows.extend(children);
            continue;
        }
        spans.push(Span::styled(": ".to_owned(), Style::new().fg(theme.gray)));
        match member {
            Value::Object(map) => spans.push(Span::styled(
                if map.is_empty() {
                    "{}".to_owned()
                } else {
                    format!("{{{} keys}}", map.len())
                },
                Style::new().fg(theme.gray),
            )),
            Value::Array(items) => spans.push(Span::styled(
                if items.is_empty() {
                    "[]".to_owned()
                } else {
                    format!("[{} items]", items.len())
                },
                Style::new().fg(theme.gray),
            )),
            // 标量预算要扣掉 `key: ` 的宽度，否则整行超长时后缀会被 clip_line 吃掉。
            scalar => spans.extend(json_scalar_spans(
                scalar,
                content_width.saturating_sub(str_width(key) + 2),
                theme,
            )),
        }
        rows.push(clip_line(Line::from(spans), width));
    }
    rows
}

/// JSON 标量的着色 span：字符串 `"首行…"`（多行追加 ` (N lines)`）、
/// 数字 `accent_running`、bool/null `accent_assistant`。
pub(super) fn json_scalar_spans(value: &Value, budget: usize, theme: &Theme) -> Vec<Span<'static>> {
    match value {
        Value::String(text) => {
            let first = text.lines().next().unwrap_or_default();
            let count = text.lines().count();
            // ` (N lines)` 后缀要计入预算：先扣掉它再给首行截断，
            // 否则后缀会被行尾的 clip_line 吃掉。
            let suffix = if count > 1 {
                format!(" ({count} lines)")
            } else {
                String::new()
            };
            let shown = crate::render::wrap::truncate(
                first.to_owned(),
                budget
                    .saturating_sub(2)
                    .saturating_sub(str_width(&suffix))
                    .max(1),
            );
            let mut spans = vec![Span::styled(
                format!("\"{shown}\""),
                Style::new().fg(theme.accent_success),
            )];
            if !suffix.is_empty() {
                spans.push(Span::styled(suffix, Style::new().fg(theme.gray)));
            }
            spans
        }
        Value::Number(number) => vec![Span::styled(
            number.to_string(),
            Style::new().fg(theme.accent_running),
        )],
        Value::Bool(flag) => vec![Span::styled(
            flag.to_string(),
            Style::new().fg(theme.accent_assistant),
        )],
        Value::Null => vec![Span::styled(
            "null".to_owned(),
            Style::new().fg(theme.accent_assistant),
        )],
        _ => Vec::new(),
    }
}

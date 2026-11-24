use super::super::TITLE_INDENT;
use super::super::diff;
use super::super::labels;
use super::super::wrap::clip_line;
use super::super::wrap::str_width;
use super::agent_tools::message_receipts;
use super::agent_tools::wait_result;
use super::messages::outcome_label;
use super::structured_tools::tool_count_spans;
use super::tool_content::prefer_builtin_redirect;
use super::tool_content::tool_is_error;
use crate::theme::Theme;
use crate::transcript::DetailLevel;
use crate::transcript::ToolCallBlock;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use moc_i18n::t;
use serde_json::Value;
use std::borrow::Cow;

/// 工具标题名：自定义标题（`IRC`、`Sidekick`、`! bash`）统一在此决定。
pub(super) fn tool_title<'a>(tool: &'a ToolCallBlock, args: &Value) -> Cow<'a, str> {
    match tool.name.as_str() {
        "message_agent" => labels::title::IRC.into(),
        "sidekick" => labels::title::SIDEKICK.into(),
        _ => match args.get("exclude_from_context").and_then(Value::as_bool) {
            Some(true) => t!("tui.tool.bash_excluded").into_owned().into(),
            Some(false) => "! bash".into(),
            None => tool.name.as_str().into(),
        },
    }
}

/// 摘要后的尾注：已完成调用的耗时（todo 是完成进度）；运行中没有尾部。
///
/// todo 的 `done/total` 里 done 计「已关闭」（completed + abandoned），
/// 全部关闭时给成功色。
pub(super) fn tool_title_tail(tool: &ToolCallBlock, theme: &Theme) -> Vec<Span<'static>> {
    if let Some(preview) = &tool.preview {
        return vec![Span::styled(
            if preview.queued {
                t!("tui.tool.queued")
            } else {
                t!("tui.tool.generating")
            }
            .into_owned(),
            Style::new().fg(theme.gray_dim),
        )];
    }
    if tool.name == "todo" {
        let Some(result) = tool.result.as_deref() else {
            return Vec::new();
        };
        let phases = moc_protocol::methods::parse_todos(result);
        let mut done = 0usize;
        let mut total = 0usize;
        for item in phases.iter().flat_map(|phase| &phase.items) {
            total += 1;
            done += usize::from(matches!(
                item.status,
                moc_protocol::methods::TodoStatus::Completed
                    | moc_protocol::methods::TodoStatus::Abandoned
            ));
        }
        if total == 0 {
            return Vec::new();
        }
        return vec![Span::styled(
            format!("{done}/{total}"),
            Style::new().fg(if done == total {
                theme.accent_success
            } else {
                theme.text_secondary
            }),
        )];
    }
    let mut tail: Vec<Span<'static>> = Vec::new();
    // grep／find 的计数进尾列：非完整档没有正文，计数不能在转写里消失。
    if !tool_is_error(tool)
        && let Some(result) = tool.result.as_deref()
        && let Some(spans) = tool_count_spans(tool, result, theme)
    {
        tail.extend(spans);
    }
    if let Some(ms) = tool.duration_ms {
        if !tail.is_empty() {
            tail.push(Span::styled(
                labels::SEP.to_owned(),
                Style::new().fg(theme.gray_dim),
            ));
        }
        tail.push(Span::styled(
            labels::duration(ms),
            Style::new().fg(theme.gray_dim),
        ));
    }
    tail
}

/// 单卡标题行留给参数摘要的列宽：让出标记、bullet（子调用无 bullet）、
/// 工具名、一格间距与紧随摘要的尾注。
pub(super) fn tool_summary_budget(
    name: &str,
    tool: &ToolCallBlock,
    theme: &Theme,
    width: usize,
    nested: bool,
) -> usize {
    let tail = if nested {
        batch_member_tail(tool, theme)
    } else {
        tool_title_tail(tool, theme)
    };
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let reserved = if tail_width > 0 {
        tail_width + str_width(labels::SEP)
    } else {
        0
    };
    width.saturating_sub(TITLE_INDENT + usize::from(!nested) * 2 + str_width(name) + 1 + reserved)
}

/// 标题摘要（已按 `budget` 截断的分段着色 span），以及 bash 命令是否已整条放进标题。
///
/// bash 命令只出现一次：没有描述时命令默认进标题（单行截断），正文不再重复 `$` 行；
/// 只有完整档且命令放不下（多行或超宽）时才留空标题、交给正文的 `$` 行完整折行展示。
/// 有描述时标题给描述，命令同样交给正文。
pub(super) fn title_summary(
    tool: &ToolCallBlock,
    args: &Value,
    budget: usize,
    level: DetailLevel,
    theme: &Theme,
    cwd: &str,
) -> (Vec<Span<'static>>, bool) {
    if let Some(preview) = &tool.preview {
        let content = &preview.content;
        let text = if content.target.is_empty() {
            &content.summary
        } else {
            &content.target
        };
        let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
        return (
            clip_line(
                Line::from(Span::styled(flat, Style::new().fg(theme.gray_bright))),
                budget,
            )
            .spans,
            false,
        );
    }
    if tool.name == "bash" && ui_description(args).is_none() {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim();
        let fits = !command.contains('\n') && str_width(command) <= budget;
        if fits || level != DetailLevel::Full {
            let flat = command.split_whitespace().collect::<Vec<_>>().join(" ");
            let line = clip_line(
                Line::from(Span::styled(flat, Style::new().fg(theme.gray_bright))),
                budget,
            );
            return (line.spans, true);
        }
        return (Vec::new(), false);
    }
    let line = Line::from(summary_spans(tool, args, theme, cwd));
    if tool.name == "skill" && line.width() > budget {
        if budget < 3 {
            return (
                vec![Span::styled(
                    ".".repeat(budget),
                    Style::new().fg(theme.text_secondary),
                )],
                false,
            );
        }
        // 通用裁剪已预留一格省略号；再让出两格，将末尾标记换成三个点。
        let mut clipped = clip_line(line, budget - 2);
        clipped
            .spans
            .last_mut()
            .expect("超宽裁剪必带省略号")
            .content = "...".into();
        return (clipped.spans, false);
    }
    (clip_line(line, budget).spans, false)
}

/// read 分页 footer 给出的真实交付范围；结果里没有 footer 时返回 `None`。
pub(super) fn read_shown_range(result: &str) -> Option<String> {
    result.lines().find_map(|line| {
        line.trim_start()
            .strip_prefix("[Showing lines ")
            .and_then(|rest| rest.split_once(" of "))
            .map(|(range, _)| format!(":{range}"))
    })
}

/// 标题摘要分段着色：核心标识（`skill` 名、`todo` 操作）用 [`Theme::text_secondary`]，
/// 主参数摘要用 [`Theme::gray_bright`]，次要范围／描述／附注／未知参数用 [`Theme::gray`]，
/// ` · ` 分隔符用 [`Theme::gray_dim`]，改动计数与投递结果保留语义色；路径一律过
/// [`super::display_path`] 相对化。
///
/// 产出未截断的 span 序列；截断由调用方用 [`clip_line`] 统一完成。
pub(super) fn summary_spans(
    tool: &ToolCallBlock,
    args: &Value,
    theme: &Theme,
    cwd: &str,
) -> Vec<Span<'static>> {
    let styled =
        |text: String, color: ratatui::style::Color| Span::styled(text, Style::new().fg(color));
    let push_joined = |items: Vec<&str>, color: ratatui::style::Color| -> Vec<Span<'static>> {
        let mut spans = Vec::with_capacity(items.len().saturating_mul(2));
        for (index, item) in items.into_iter().enumerate() {
            if index > 0 {
                spans.push(styled(labels::SEP.to_owned(), theme.gray_dim));
            }
            spans.push(styled(item.to_owned(), color));
        }
        spans
    };
    let arg_str = |key: &str| {
        args.get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned()
    };
    let path_str = |key: &str| super::super::display_path(&arg_str(key), cwd).into_owned();
    let glob_str = |key: &str| {
        args.get(key)
            .and_then(Value::as_array)
            .map(|globs| {
                globs
                    .iter()
                    .filter_map(Value::as_str)
                    .map(|glob| super::super::display_path(glob, cwd).into_owned())
                    .collect::<Vec<_>>()
                    .join(" ")
            })
            .unwrap_or_default()
    };
    match tool.name.as_str() {
        "edit" | "write" => {
            let mut spans = vec![styled(path_str("path"), theme.gray_bright)];
            if let Some(diff) = &tool.diff {
                let stats = diff::stats(diff);
                // 空 diff 不渲染 +0/-0；截断时计数只是已交付的部分，
                // 用 … 标明，避免把部分值当成完整统计。
                if stats.insertions + stats.deletions > 0 {
                    spans.push(styled(
                        format!(" +{}", stats.insertions),
                        theme.accent_success,
                    ));
                    spans.push(styled(format!(" -{}", stats.deletions), theme.accent_error));
                    if diff.truncated {
                        spans.push(styled(" …".to_owned(), theme.gray_dim));
                    }
                }
            }
            spans
        }
        "bash" | "execute" => vec![styled(
            ui_description(args).unwrap_or_default().to_owned(),
            theme.gray_bright,
        )],
        "read" => {
            // 分页 footer 是真实交付的范围，优先于请求的 offset/limit；与 web readRange 同规则。
            let range = tool
                .result
                .as_deref()
                .and_then(read_shown_range)
                .unwrap_or_else(|| {
                    let offset = args.get("offset").and_then(Value::as_u64);
                    let limit = args.get("limit").and_then(Value::as_u64);
                    match (offset, limit) {
                        (Some(offset), Some(limit)) => {
                            format!(":{offset}-{}", offset + limit.saturating_sub(1))
                        }
                        (Some(offset), None) => format!(":{offset}-"),
                        _ => String::new(),
                    }
                });
            let globs = glob_str("glob");
            let globs = if globs.is_empty() {
                String::new()
            } else {
                format!(" {globs}")
            };
            vec![
                styled(path_str("path"), theme.gray_bright),
                styled(range, theme.gray),
                styled(globs, theme.gray),
            ]
        }
        "grep" => vec![styled(arg_str("pattern"), theme.gray_bright)],
        "find" => vec![styled(glob_str("glob"), theme.gray_bright)],
        "web_fetch" => {
            let urls = args
                .get("urls")
                .and_then(Value::as_array)
                .map(|urls| urls.iter().filter_map(Value::as_str).collect::<Vec<_>>())
                .unwrap_or_default();
            push_joined(urls, theme.gray_bright)
        }
        "spawn_agent" => {
            if let Some(tasks) = args.get("tasks").and_then(Value::as_array) {
                let titles = tasks
                    .iter()
                    .filter_map(|task| task.get("title").and_then(Value::as_str))
                    .collect::<Vec<_>>();
                push_joined(titles, theme.gray_bright)
            } else {
                vec![styled(arg_str("title"), theme.gray_bright)]
            }
        }
        "yield" => vec![styled(
            t!("tui.tool.yield_report").into_owned(),
            theme.gray_bright,
        )],
        "skill" => {
            let name = arg_str("name");
            let mut spans = Vec::new();
            if !name.is_empty() {
                spans.push(styled(name, theme.text_secondary));
            }
            if let Some(description) = tool
                .result
                .as_deref()
                .and_then(|result| result.lines().next())
                .and_then(|line| line.strip_prefix("Skill description: "))
            {
                if !spans.is_empty() {
                    spans.push(styled(labels::SEP.to_owned(), theme.gray_dim));
                }
                spans.push(styled(description.to_owned(), theme.gray));
            }
            spans
        }
        "clm_context" => vec![styled(arg_str("op"), theme.gray_bright)],
        "stop_agent" => vec![styled(arg_str("agent_id"), theme.gray_bright)],
        "todo" => todo_summary_spans(args, theme),
        "TodoReminder" => vec![styled(
            t!("tui.tool.todo_reminder").into_owned(),
            theme.gray_bright,
        )],
        "job" | "plan" | "exit_plan" => Vec::new(),
        "message_agent" | "sidekick" | "wait_agent" | "ask" => {
            agent_summary_spans(tool, args, theme)
        }
        _ => {
            // 未知工具（MCP 等）：参数截断展示
            let text = tool.arguments.replace('\n', " ");
            vec![styled(text.chars().take(60).collect(), theme.gray)]
        }
    }
}

/// todo 摘要：op——init 跟 `N tasks`，append 跟 `phase · N items`，rm 跟 task 或
/// phase 文本（rm 后目标已不在清单里，标题是唯一留痕）；start/done/drop/block/
/// unblock 只留 op，目标在下方快照树里可见，不重复。op 缺失时与 core 的
/// `infer_op` 同规则推断（非空 list → init，带 phase 的非空 items → append，否则 init）。
pub(super) fn todo_summary_spans(args: &Value, theme: &Theme) -> Vec<Span<'static>> {
    let styled =
        |text: String, color: ratatui::style::Color| Span::styled(text, Style::new().fg(color));
    let op = args
        .get("op")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .unwrap_or_else(|| {
            let list = args.get("list").and_then(Value::as_array);
            let items = args.get("items").and_then(Value::as_array);
            let phase = args.get("phase").and_then(Value::as_str);
            if list.is_some_and(|list| !list.is_empty()) {
                "init"
            } else if items.is_some_and(|items| !items.is_empty())
                && phase.is_some_and(|phase| !phase.is_empty())
            {
                "append"
            } else {
                "init"
            }
            .to_owned()
        });
    let mut spans = vec![styled(op.clone(), theme.text_secondary)];
    let arg_str = |key: &str| args.get(key).and_then(Value::as_str).unwrap_or_default();
    match op.as_str() {
        "init" => {
            let count = args
                .get("list")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .map(|phase| {
                            phase
                                .get("items")
                                .and_then(Value::as_array)
                                .map_or(0, Vec::len)
                        })
                        .sum()
                })
                .or_else(|| args.get("items").and_then(Value::as_array).map(Vec::len));
            if let Some(count) = count {
                spans.push(styled(
                    format!(" {count} tasks"),
                    if count > 0 {
                        theme.text_secondary
                    } else {
                        theme.gray
                    },
                ));
            }
        }
        "append" => {
            let phase = arg_str("phase");
            if !phase.is_empty() {
                spans.push(styled(format!(" {phase}"), theme.gray_bright));
            }
            if let Some(items) = args.get("items").and_then(Value::as_array) {
                spans.push(styled(labels::SEP.to_owned(), theme.gray_dim));
                spans.push(styled(
                    format!("{} items", items.len()),
                    if !items.is_empty() {
                        theme.text_secondary
                    } else {
                        theme.gray
                    },
                ));
            }
        }
        // 目标 op 只留 op 词：start/done/drop/block/unblock 的目标都在快照树里
        // （☑/▶/✗/⚠/☐ 标得出是哪条），重复一遍只是噪音。
        "rm" => {
            let target = {
                let task = arg_str("task");
                if task.is_empty() {
                    arg_str("phase")
                } else {
                    task
                }
            };
            if !target.is_empty() {
                spans.push(styled(format!(" {target}"), theme.gray_bright));
            }
        }
        _ => {}
    }
    spans
}

/// 模型给的 UI 描述：只取首个非空行，空串视为未提供。
pub(super) fn ui_description(args: &Value) -> Option<&str> {
    let text = args.get("description").and_then(Value::as_str)?;
    text.lines().map(str::trim).find(|line| !line.is_empty())
}

/// 代理交互与询问类工具的分段着色标题摘要。
fn agent_summary_spans(tool: &ToolCallBlock, args: &Value, theme: &Theme) -> Vec<Span<'static>> {
    let styled =
        |text: String, color: ratatui::style::Color| Span::styled(text, Style::new().fg(color));
    let push_sep = |spans: &mut Vec<Span<'static>>| {
        if !spans.is_empty() {
            spans.push(styled(labels::SEP.to_owned(), theme.gray_dim));
        }
    };
    match tool.name.as_str() {
        "message_agent" => {
            use moc_protocol::methods::AgentMessageOutcome;
            let target = args
                .get("target")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let mut spans = vec![styled(format!("▶ {target}"), theme.gray_bright)];
            if let Some(receipts) = message_receipts(tool) {
                if target == "all" {
                    push_sep(&mut spans);
                    spans.push(styled(labels::status::BROADCAST.to_owned(), theme.gray));
                }
                let failed = receipts
                    .iter()
                    .filter(|receipt| receipt.outcome == AgentMessageOutcome::Failed)
                    .count();
                push_sep(&mut spans);
                match receipts.as_slice() {
                    [receipt] => {
                        let (word, color) = outcome_label(receipt.outcome, theme);
                        spans.push(styled(word.to_owned(), color));
                    }
                    receipts => {
                        let delivered = receipts.len() - failed;
                        spans.push(styled(
                            format!("{delivered} {}", labels::status::DELIVERED),
                            if delivered > 0 {
                                theme.text_secondary
                            } else {
                                theme.gray
                            },
                        ));
                    }
                }
                if failed > 0 {
                    push_sep(&mut spans);
                    spans.push(styled(
                        format!("{failed} {}", labels::status::FAILED),
                        theme.accent_error,
                    ));
                }
            }
            spans
        }
        "sidekick" => {
            // 摘要只占一行：handoff 首行；mode 与后台执行（block=false）附注。
            let mode = args.get("mode").and_then(Value::as_str).unwrap_or("keep");
            let first = if mode == "retry" {
                t!("tui.hint.retry").into_owned()
            } else {
                args.get("message")
                    .and_then(Value::as_str)
                    .and_then(|text| text.lines().next())
                    .unwrap_or_default()
                    .to_owned()
            };
            let mut spans = Vec::new();
            if !first.is_empty() {
                spans.push(styled(first.to_owned(), theme.gray_bright));
            }
            let mut push_flag = |flag: &str| {
                push_sep(&mut spans);
                spans.push(styled(flag.to_owned(), theme.gray));
            };
            if mode == "compact" {
                push_flag(t!("tui.tool.flag_compact").as_ref());
            } else if mode == "fresh" {
                push_flag(t!("tui.tool.flag_fresh").as_ref());
            }
            if args.get("block").and_then(Value::as_bool) == Some(false) {
                push_flag(t!("tui.tool.flag_background").as_ref());
            }
            spans
        }
        "wait_agent" => {
            let main = match args.get("agent_ids").and_then(Value::as_array) {
                Some(ids) if !ids.is_empty() => {
                    t!("tui.tool.wait_count_fmt", count = ids.len()).into_owned()
                }
                _ => t!("tui.tool.wait_all").into_owned(),
            };
            let mut spans = vec![styled(main, theme.gray_bright)];
            if let Some(result) = wait_result(tool) {
                if result.messages_pending {
                    push_sep(&mut spans);
                    spans.push(styled(t!("tui.tool.new_messages").into_owned(), theme.gray));
                }
                if result.timed_out {
                    push_sep(&mut spans);
                    spans.push(styled(
                        t!("tui.tool.timed_out").into_owned(),
                        theme.accent_warning,
                    ));
                }
            }
            spans
        }
        "ask" => args
            .get("questions")
            .and_then(Value::as_array)
            .map(|questions| {
                // 摘要只占一行：问题正文可含换行，取首行避免控制字符在标题里粘连；
                // 多问时点明总量，后面的题不藏在首问后面。
                let first = questions
                    .first()
                    .and_then(|question| question.get("question"))
                    .and_then(Value::as_str)
                    .and_then(|text| text.lines().next())
                    .unwrap_or_default();
                let mut spans = Vec::new();
                if questions.len() > 1 {
                    spans.push(styled(
                        t!("tui.tool.question_count_fmt", count = questions.len()).into_owned(),
                        theme.gray,
                    ));
                    if !first.is_empty() {
                        push_sep(&mut spans);
                    }
                }
                if !first.is_empty() {
                    spans.push(styled(first.to_owned(), theme.gray_bright));
                }
                spans
            })
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// 成员行尾的状态列：失败 `✘` 与耗时（todo 是完成进度）；无正文的记账工具（skill 等）不报。
pub(super) fn batch_member_tail(tool: &ToolCallBlock, theme: &Theme) -> Vec<Span<'static>> {
    let mut tail = Vec::new();
    if tool_is_error(tool) {
        let (mark, color) = if prefer_builtin_redirect(tool) {
            ("→", theme.accent_warning)
        } else {
            ("✘", theme.accent_error)
        };
        tail.push(Span::styled(mark, Style::new().fg(color)));
    }
    if tool.has_body() {
        let spans = tool_title_tail(tool, theme);
        if !tail.is_empty() && !spans.is_empty() {
            tail.push(Span::raw(" "));
        }
        tail.extend(spans);
    }
    tail
}

/// 成员摘要的列宽预算：让出行首缩进与行尾状态列（状态列前留 `labels::SEP` 间距）。
///
/// `width` 是成员行在连线前缀之内的可用宽度。
pub(super) fn batch_summary_budget(tail: &[Span<'_>], width: usize) -> usize {
    let tail_width: usize = tail.iter().map(Span::width).sum();
    let reserved = if tail_width > 0 {
        tail_width + str_width(labels::SEP)
    } else {
        0
    };
    width.saturating_sub(TITLE_INDENT + reserved)
}

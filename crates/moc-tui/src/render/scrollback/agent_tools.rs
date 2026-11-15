use rust_i18n::t;

use super::super::labels;
use super::BodyEdge;
use super::agent_status_label;
use super::messages::outcome_label;
use super::text::clamp_rows;
use crate::theme::Theme;
use crate::transcript::ToolCallBlock;
use ratatui::style::Style;
use ratatui::text::Line;
use ratatui::text::Span;
use serde_json::Value;

/// `message_agent` 的正文行数：消息预览行 + 展开时的逐条回执行，
/// 与 [`message_agent_tool_rows`] 消耗同一份预算。
pub(super) fn message_agent_rows_count(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    limit: usize,
) -> (usize, usize) {
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let clamped = clamp_rows(message, width, limit, BodyEdge::Head);
    let mut rows = clamped.shown;
    let mut dropped = clamped.dropped;
    let Some(receipts) = message_receipts(tool) else {
        return (rows, dropped);
    };
    let expanded = receipts.len() > 1
        || receipts
            .iter()
            .any(|receipt| receipt.outcome == moc_protocol::methods::AgentMessageOutcome::Failed);
    if expanded {
        for _ in receipts {
            if rows >= limit {
                dropped += 1;
            } else {
                rows += 1;
            }
        }
    }
    (rows, dropped)
}

/// 解析 `message_agent` 的投递回执。
pub(super) fn message_receipts(
    tool: &ToolCallBlock,
) -> Option<Vec<moc_protocol::methods::AgentMessageReceipt>> {
    let result = tool.result.as_deref()?;
    serde_json::from_str::<moc_protocol::methods::AgentMessageResult>(result)
        .ok()
        .map(|result| result.receipts)
}

/// 解析 `wait_agent` 的等待结果。
pub(super) fn wait_result(tool: &ToolCallBlock) -> Option<moc_protocol::methods::AgentWaitResult> {
    let result = tool.result.as_deref()?;
    serde_json::from_str::<moc_protocol::methods::AgentWaitResult>(result).ok()
}

/// 发送侧 IRC 卡片正文：消息预览 + 收件人明细，返回（展示行、被略去的源行数）。
///
/// 明细只在「广播或有失败」时逐行展开：单目标成功时标题行的回执词已经说清结果，
/// 再列一遍只是重复。
pub(super) fn message_agent_tool_rows(
    tool: &ToolCallBlock,
    args: &Value,
    width: usize,
    theme: &Theme,
    limit: usize,
) -> (Vec<Line<'static>>, usize) {
    let mut lines = Vec::new();
    let mut dropped = 0usize;
    let message = args
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let clamped = clamp_rows(message, width, limit, BodyEdge::Head);
    dropped += clamped.dropped;
    for row in clamped.rows {
        lines.push(Line::from(Span::styled(
            row,
            Style::new().fg(theme.text_primary),
        )));
    }
    let Some(receipts) = message_receipts(tool) else {
        return (lines, dropped);
    };
    let expanded = receipts.len() > 1
        || receipts
            .iter()
            .any(|receipt| receipt.outcome == moc_protocol::methods::AgentMessageOutcome::Failed);
    if !expanded {
        return (lines, dropped);
    }
    for receipt in receipts {
        if lines.len() >= limit {
            dropped += 1;
            continue;
        }
        let (word, color) = outcome_label(receipt.outcome, theme);
        let mut spans = vec![
            Span::styled(
                format!("{} ", receipt.title),
                Style::new().fg(theme.text_secondary),
            ),
            Span::styled(word.to_owned(), Style::new().fg(color)),
        ];
        if let Some(error) = receipt.error {
            spans.push(Span::styled(format!("：{error}"), Style::new().fg(color)));
        }
        lines.push(Line::from(spans));
    }
    (lines, dropped)
}

/// 等待卡片的行：优先实时进度，工具结束后回落到结果快照，
/// 返回（展示行、被略去的成员数）。
pub(super) fn wait_agent_tool_rows(
    tool: &ToolCallBlock,
    theme: &Theme,
    limit: usize,
) -> (Vec<Line<'static>>, usize) {
    use moc_protocol::methods::AgentStatus;
    let rows: Vec<(String, AgentStatus, u64, Option<String>)> = match &tool.progress {
        Some(progress) => progress
            .iter()
            .map(|row| {
                (
                    row.title.clone(),
                    row.status,
                    row.elapsed_ms,
                    row.activity.clone(),
                )
            })
            .collect(),
        None => wait_result(tool).map_or_else(Vec::new, |result| {
            result
                .agents
                .into_iter()
                .map(|entry| (entry.title, entry.status, 0, None))
                .collect()
        }),
    };
    if rows.is_empty() {
        return (Vec::new(), 0);
    }
    let dropped = rows.len().saturating_sub(limit);
    let mut lines = Vec::with_capacity(rows.len().min(limit));
    for (title, status, elapsed_ms, activity) in rows.into_iter().take(limit) {
        let (glyph, color) = match status {
            AgentStatus::Queued | AgentStatus::Running => ("▸", theme.accent_running),
            AgentStatus::Completed => ("◆", theme.accent_tool),
            AgentStatus::Stopped => ("◼", theme.gray_dim),
            AgentStatus::Failed => ("✘", theme.accent_error),
            AgentStatus::Idle => ("○", theme.gray_dim),
        };
        let mut spans = vec![
            Span::styled(format!("{glyph} "), Style::new().fg(color)),
            Span::styled(title, Style::new().fg(theme.text_primary)),
            Span::styled(
                format!("{}{}", labels::SEP, agent_status_label(status)),
                Style::new().fg(color),
            ),
        ];
        if elapsed_ms > 0 {
            spans.push(Span::styled(
                format!("{}{}", labels::SEP, labels::duration(elapsed_ms)),
                Style::new().fg(theme.gray_dim),
            ));
        }
        if let Some(activity) = activity
            && !activity.is_empty()
        {
            spans.push(Span::styled(
                format!("{}{activity}", labels::SEP),
                Style::new().fg(theme.gray_dim),
            ));
        }
        lines.push(Line::from(spans));
    }
    (lines, dropped)
}

/// 等待卡片的行数：与 [`wait_agent_tool_rows`] 同规则（只数行、不构造行），
/// 返回（展示行数、被略去的成员数）。
pub(super) fn wait_agent_rows_count(tool: &ToolCallBlock, limit: usize) -> (usize, usize) {
    let total = match &tool.progress {
        Some(progress) => progress.len(),
        None => wait_result(tool).map_or(0, |result| result.agents.len()),
    };
    (total.min(limit), total.saturating_sub(limit))
}

/// `sidekick` 卡片正文：运行中沿用 `wait_agent` 的进度行；落定后解析 JSON
/// 结果——状态行（glyph + 交付方式 + 状态）、可选 note 与报告正文；
/// 非 JSON 正文（错误文本等）退回通用截断渲染。
pub(super) fn sidekick_tool_rows(
    tool: &ToolCallBlock,
    width: usize,
    theme: &Theme,
    limit: usize,
) -> (Vec<Line<'static>>, usize) {
    let Some(result) = tool.result.as_deref() else {
        return wait_agent_tool_rows(tool, theme, limit);
    };
    let Ok(json) = serde_json::from_str::<Value>(result) else {
        let clamped = clamp_rows(result, width, limit, BodyEdge::Head);
        let lines = clamped
            .rows
            .into_iter()
            .map(|row| Line::from(Span::styled(row, Style::new().fg(theme.text_secondary))))
            .collect();
        return (lines, clamped.dropped);
    };
    let mut lines = Vec::new();
    let mut dropped = 0usize;
    let status = json
        .get("status")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let (glyph, color) = match status {
        "completed" => ("◆", theme.accent_tool),
        "failed" => ("✘", theme.accent_error),
        "stopped" => ("◼", theme.gray_dim),
        _ => ("▸", theme.accent_running),
    };
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::new().fg(color)),
        Span::styled(labels::title::SIDEKICK, Style::new().fg(theme.text_primary)),
    ];
    if let Some(delivery) = json.get("delivery").and_then(Value::as_str) {
        spans.push(Span::styled(
            format!("{}{delivery}", labels::SEP),
            Style::new().fg(theme.gray_dim),
        ));
    }
    if !status.is_empty() {
        spans.push(Span::styled(
            format!("{}{status}", labels::SEP),
            Style::new().fg(color),
        ));
    }
    if let Some(model) = json.get("model").and_then(Value::as_str)
        && !model.is_empty()
    {
        spans.push(Span::styled(
            format!("{}{model}", labels::SEP),
            Style::new().fg(theme.gray_dim),
        ));
    }
    lines.push(Line::from(spans));
    // 结构化结果：状态摘要行 + 每条 open_issue 一行。
    if let Some(result_obj) = json.get("result").and_then(Value::as_object) {
        lines.push(Line::from(Span::styled(
            sidekick_result_summary(result_obj),
            Style::new().fg(theme.text_secondary),
        )));
        if let Some(issues) = result_obj.get("open_issues").and_then(Value::as_array) {
            let budget = limit.saturating_sub(lines.len());
            for issue in issues.iter().take(budget) {
                // 每条恒占一行：与 `sidekick_rows_count` 的计数保持一致。
                let text = crate::render::wrap::truncate(
                    issue.as_str().unwrap_or_default(),
                    width.saturating_sub(2),
                );
                lines.push(Line::from(Span::styled(
                    format!("! {text}"),
                    Style::new().fg(theme.accent_running),
                )));
            }
            dropped += issues.len().saturating_sub(budget);
        }
    }
    if let Some(note) = json.get("note").and_then(Value::as_str)
        && !note.is_empty()
    {
        lines.push(Line::from(Span::styled(
            note.to_owned(),
            Style::new().fg(theme.gray_dim),
        )));
    }
    if let Some(report) = json.get("report").and_then(Value::as_str)
        && !report.is_empty()
    {
        let budget = limit.saturating_sub(lines.len()).max(1);
        let clamped = clamp_rows(report, width, budget, BodyEdge::Head);
        dropped += clamped.dropped;
        for row in clamped.rows {
            lines.push(Line::from(Span::styled(
                row,
                Style::new().fg(theme.text_primary),
            )));
        }
    }
    (lines, dropped)
}

/// `sidekick` 卡片行数：与 [`sidekick_tool_rows`] 同规则。
pub(super) fn sidekick_rows_count(
    tool: &ToolCallBlock,
    width: usize,
    limit: usize,
) -> (usize, usize) {
    let Some(result) = tool.result.as_deref() else {
        return wait_agent_rows_count(tool, limit);
    };
    let Ok(json) = serde_json::from_str::<Value>(result) else {
        let clamped = clamp_rows(result, width, limit, BodyEdge::Head);
        return (clamped.shown, clamped.dropped);
    };
    let mut rows = 1 + usize::from(
        json.get("note")
            .and_then(Value::as_str)
            .is_some_and(|note| !note.is_empty()),
    );
    let mut dropped = 0;
    // 结构化结果的摘要行与 open_issue 行同样占用行数预算。
    if let Some(result_obj) = json.get("result").and_then(Value::as_object) {
        rows += 1;
        let issues = result_obj
            .get("open_issues")
            .and_then(Value::as_array)
            .map_or(0, Vec::len);
        let shown = issues.min(limit.saturating_sub(rows));
        rows += shown;
        dropped += issues.saturating_sub(shown);
    }
    if let Some(report) = json.get("report").and_then(Value::as_str)
        && !report.is_empty()
    {
        let budget = limit.saturating_sub(rows).max(1);
        let clamped = clamp_rows(report, width, budget, BodyEdge::Head);
        rows += clamped.shown;
        dropped += clamped.dropped;
    }
    (rows, dropped)
}

/// `result` 对象的一行摘要：`结果 <status> · 改动 N 个文件 · 验证 P 通过 F 失败 U 未跑`，
/// 为 0 的部分省略。
pub(super) fn sidekick_result_summary(result: &serde_json::Map<String, Value>) -> String {
    let mut parts = vec![
        t!(
            "tui.tool.result_fmt",
            status = result
                .get("status")
                .and_then(Value::as_str)
                .unwrap_or_default()
        )
        .into_owned(),
    ];
    let changes = result.get("changes").and_then(Value::as_array);
    if let Some(changes) = changes
        && !changes.is_empty()
    {
        parts.push(t!("tui.tool.changes_fmt", n = changes.len()).into_owned());
    }
    if let Some(verification) = result.get("verification").and_then(Value::as_array)
        && !verification.is_empty()
    {
        let count = |outcome: &str| {
            verification
                .iter()
                .filter(|item| item.get("outcome").and_then(Value::as_str) == Some(outcome))
                .count()
        };
        let (passed, failed, not_run) = (count("passed"), count("failed"), count("not_run"));
        let mut word = t!("tui.tool.verify_base").into_owned();
        if passed > 0 {
            word.push_str(&t!("tui.tool.verify_passed_fmt", n = passed));
        }
        if failed > 0 {
            word.push_str(&t!("tui.tool.verify_failed_fmt", n = failed));
        }
        if not_run > 0 {
            word.push_str(&t!("tui.tool.verify_notrun_fmt", n = not_run));
        }
        parts.push(word);
    }
    parts.join(" · ")
}

/// ask 结果解析成 `(key, 答案, 未回答)` 三元组，渲染与估高共用。
///
/// core 输出是给模型看的纯文本——多问是 `User answers:` 头加 `id: 答案`
/// 逐行，单问只有裸答案；取消与超时是整段哨兵文本。这里按已知 id 前缀
/// 逐行认领新条，认领不到的行并进上一条答案（自定义答案含换行时不丢
/// 内容），key 取 header 兜底 id。
pub(super) fn ask_answer_pairs(tool: &ToolCallBlock, args: &Value) -> Vec<(String, String, bool)> {
    const UNANSWERED: &str = "The user did not answer.";
    const TIMEOUT: &str =
        "The user did not answer within the time limit; continue without an answer.";
    let Some(result) = tool.result.as_deref() else {
        return Vec::new();
    };
    let result = result.trim_end();
    if result == TIMEOUT {
        return vec![
            (
                String::new(),
                t!("tui.ask.timeout_unanswered").into_owned(),
                true,
            ),
        ];
    }
    if result == UNANSWERED {
        return vec![(String::new(), t!("tui.ask.unanswered").into_owned(), true)];
    }
    let questions: Vec<(String, String)> = args
        .get("questions")
        .and_then(Value::as_array)
        .map(|questions| {
            questions
                .iter()
                .map(|question| {
                    let id = question
                        .get("id")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    let key = question
                        .get("header")
                        .and_then(Value::as_str)
                        .filter(|header| !header.trim().is_empty())
                        .map(str::to_owned)
                        .unwrap_or_else(|| id.clone());
                    (id, key)
                })
                .collect()
        })
        .unwrap_or_default();
    let mut pairs: Vec<(String, String)> = Vec::new();
    for line in result
        .lines()
        .skip(usize::from(result.starts_with("User answers:")))
    {
        if let Some((id, _)) = questions
            .iter()
            .find(|(id, _)| line.starts_with(&format!("{id}: ")))
        {
            pairs.push((id.clone(), line[id.len() + 2..].to_owned()));
        } else if let Some((_, answer)) = pairs.last_mut() {
            answer.push(' ');
            answer.push_str(line.trim());
        } else if questions.len() == 1 {
            // 单问输出是裸答案，没有 `id: ` 前缀，整段收下。
            pairs.push((questions[0].0.clone(), line.to_owned()));
        }
        // 既非已知 id 也无上一条可并的行直接丢。
    }
    pairs
        .into_iter()
        .enumerate()
        .map(|(index, (id, answer))| {
            let key = questions
                .iter()
                .find(|(qid, _)| *qid == id)
                .map(|(_, key)| key.clone())
                .or_else(|| questions.get(index).map(|(_, key)| key.clone()))
                .unwrap_or_else(|| t!("tui.ask.answer").into_owned());
            let unanswered = answer == UNANSWERED;
            (
                key,
                if unanswered {
                    t!("tui.ask.unanswered").into_owned()
                } else {
                    answer
                },
                unanswered,
            )
        })
        .collect()
}

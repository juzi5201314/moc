//! 用量刷新与统计展示。

use crate::app::App;
use crate::protocol::AppEvent;
use crate::protocol::EmbeddedConnection;
use crate::render::wrap::str_width;
use moc_protocol::SessionUsage;
use moc_protocol::SessionUsageParams;
use moc_i18n::t;

/// 刷新会话累计用量：状态栏与 `/usage` 读同一份 core replay 出的统计。
///
/// `show` 为真时把明细写进转写（`/usage`），否则只后台更新状态栏数字。
pub(super) fn refresh_usage(app: &mut App, connection: &EmbeddedConnection, show: bool) {
    let Some(session_id) = app.session.as_ref().map(|session| session.id.clone()) else {
        return;
    };
    let request = app.begin_usage_request();
    let client = connection.client.clone();
    let events = app.events.clone();
    tokio::spawn(async move {
        match client
            .call::<SessionUsage>(SessionUsageParams { session_id })
            .await
        {
            Ok(result) => {
                let _ = events.send(AppEvent::Usage {
                    request,
                    result,
                    show,
                });
            }
            Err(error) if show => {
                let _ = events.send(AppEvent::Notice(
                    t!("tui.usage.load_failed_fmt", err = error.to_string()).into_owned(),
                ));
            }
            Err(_) => {}
        }
    });
}

/// `/usage` 明细：会话统计 + 模型 × 指标的对齐表格，末行是全部模型的合计。
///
/// 表格宽度按转写区收敛（去掉 note 的行首缩进与标记）：模型名过长时截断，
/// 数字列右对齐；token 数走全 UI 统一的 k/m/b 分档，时间按时长量级换单位。
/// 窄终端按优先级丢列（思考 → 缓存率 → TTFT），至少保住模型、调用、输入、
/// 输出、生成与 TPS。`area_width` 是转写区宽度，首次绘制前为 0 时按 80 列终端估算。
pub(super) fn format_usage(result: &moc_protocol::SessionUsageResult, area_width: usize) -> String {
    // note 每行扣掉 1 格缩进与 2 格行首标记（标记或对齐空白）。
    let content_width = if area_width == 0 {
        71
    } else {
        area_width.saturating_sub(3)
    };
    // 再窄也先保住模型列，超出部分交给 note 折行（极窄终端才会发生）。
    let max_width = content_width.max(40);

    let mut lines = usage_summary(result);
    if !lines.is_empty() {
        lines.push(String::new());
    }
    lines.extend(usage_table(result, max_width));
    lines.join("\n")
}

/// 会话级统计：模型用时（生成 + 网络等待）、工具调用用时、平均 TTFT 与输出速度。
///
/// 没有模型调用（新会话）时不输出整块，表格自己会给出零值。
pub(super) fn usage_summary(result: &moc_protocol::SessionUsageResult) -> Vec<String> {
    let total = &result.total;
    if total.calls == 0 {
        return Vec::new();
    }
    let mut rows: Vec<(String, String)> = Vec::with_capacity(4);
    let split = if total.gen_ms > 0 {
        let waiting = total.active_ms.saturating_sub(total.gen_ms);
        t!(
            "tui.usage.split_fmt",
            gen = format_active(total.gen_ms),
            wait = format_active(waiting)
        )
        .into_owned()
    } else {
        String::new()
    };
    rows.push((
        t!("tui.usage.model_time").into_owned(),
        format!("{}{split}", format_active(total.active_ms)),
    ));
    if result.tool_ms > 0 {
        rows.push((
            t!("tui.usage.tool_time").into_owned(),
            format_active(result.tool_ms),
        ));
    }
    if let Some(ttft) = average_ttft(total) {
        rows.push((t!("tui.usage.ttft").into_owned(), format_active(ttft)));
    }
    if let Some(tps) = usage_tps(total) {
        rows.push((t!("tui.usage.tps").into_owned(), format!("{tps:.1} tok/s")));
    }
    let label_width = rows
        .iter()
        .map(|(label, _)| str_width(label))
        .max()
        .unwrap_or(0);
    let mut lines = vec![t!("tui.usage.title").into_owned()];
    lines.extend(rows.into_iter().map(|(label, value)| {
        let pad = label_width.saturating_sub(str_width(&label));
        format!("  {label}{}{value}", " ".repeat(pad + 2))
    }));
    lines
}

/// 模型明细表；列宽按 `max_width` 收敛，空间不够时按优先级丢列。
pub(super) fn usage_table(
    result: &moc_protocol::SessionUsageResult,
    max_width: usize,
) -> Vec<String> {
    let columns: [String; 9] = [
        t!("tui.usage.col_model").into_owned(),
        t!("tui.usage.col_calls").into_owned(),
        t!("tui.usage.col_input").into_owned(),
        t!("tui.usage.col_output").into_owned(),
        t!("tui.usage.col_cache").into_owned(),
        t!("tui.usage.col_thinking").into_owned(),
        t!("tui.usage.col_gen").into_owned(),
        "TTFT".to_owned(),
        "TPS".to_owned(),
    ];
    // 窄终端按顺序丢列：思考 → 缓存率 → TTFT（会话统计里还有平均 TTFT）。
    const DROP_ORDER: [usize; 3] = [5, 4, 7];
    const MIN_MODEL_WIDTH: usize = 14;

    let mut rows: Vec<[String; 9]> = result
        .models
        .iter()
        .map(|summary| {
            let label = crate::app::model_selector_label(&summary.model);
            // 后代会话的用量行标出来源：Fusion sidekick 与普通子代理。
            let label = match summary.source {
                moc_protocol::methods::UsageSource::Sidekick => format!("{label} (sidekick)"),
                moc_protocol::methods::UsageSource::Subagent => {
                    t!("tui.usage.subagent_fmt", label = label.as_str()).into_owned()
                }
                moc_protocol::methods::UsageSource::Session => label,
            };
            usage_row(summary, &label)
        })
        .collect();
    rows.push(usage_row(&result.total, &t!("tui.usage.total")));

    let mut widths: [usize; 9] = std::array::from_fn(|index| str_width(&columns[index]));
    for row in &rows {
        for (index, cell) in row.iter().enumerate() {
            widths[index] = widths[index].max(str_width(cell));
        }
    }
    let mut hidden = [false; 9];
    for _ in 0..DROP_ORDER.len() {
        let fixed: usize = (1..columns.len())
            .filter(|index| !hidden[*index])
            .map(|index| widths[index] + 2)
            .sum();
        if max_width.saturating_sub(fixed) >= MIN_MODEL_WIDTH {
            break;
        }
        let Some(index) = DROP_ORDER.iter().copied().find(|&index| !hidden[index]) else {
            break;
        };
        hidden[index] = true;
    }
    let visible: Vec<usize> = (0..columns.len()).filter(|index| !hidden[*index]).collect();
    // 模型列吃掉剩余预算，其余列按内容宽定宽。
    let fixed: usize = visible
        .iter()
        .copied()
        .filter(|&index| index != 0)
        .map(|index| widths[index] + 2)
        .sum();
    widths[0] = widths[0]
        .min(max_width.saturating_sub(fixed))
        .max(str_width(&columns[0]));
    for row in &mut rows {
        row[0] = crate::render::wrap::truncate(row[0].clone(), widths[0]).into_owned();
    }

    let line = |cells: &[String; 9]| {
        visible
            .iter()
            .copied()
            .map(|index| {
                let cell = &cells[index];
                let gap = widths[index].saturating_sub(str_width(cell));
                // 模型列左对齐，数字列右对齐。
                if index == 0 {
                    format!("{cell}{}", " ".repeat(gap))
                } else {
                    format!("{}{cell}", " ".repeat(gap))
                }
            })
            .collect::<Vec<_>>()
            .join("  ")
    };
    let header_cells = columns.clone();
    let header = line(&header_cells);
    let rule = "─".repeat(str_width(&header));
    let mut lines = vec![header, rule.clone()];
    let total = rows.pop().expect("合计行总会入列");
    for row in rows {
        lines.push(line(&row));
    }
    if !result.models.is_empty() {
        lines.push(rule);
    }
    lines.push(line(&total));
    lines
}

/// 表格里的一行：`[模型, 调用, 输入, 输出, 缓存率, 思考, 生成, TTFT, TPS]`。
pub(super) fn usage_row(summary: &moc_protocol::ModelUsageSummary, label: &str) -> [String; 9] {
    let cached = summary.cached_input_tokens.unwrap_or(0);
    let cache_rate = if summary.input_tokens == 0 {
        0.0
    } else {
        cached as f64 * 100.0 / summary.input_tokens as f64
    };
    [
        label.to_owned(),
        summary.calls.to_string(),
        crate::render::format_tokens(summary.input_tokens),
        crate::render::format_tokens(summary.output_tokens),
        format!("{cache_rate:.1}%"),
        summary.reasoning_tokens.map_or_else(
            || t!("tui.usage.unknown").into_owned(),
            crate::render::format_tokens,
        ),
        if summary.gen_ms > 0 {
            format_active(summary.gen_ms)
        } else {
            "—".to_owned()
        },
        average_ttft(summary).map_or_else(|| "—".to_owned(), format_active),
        usage_tps(summary).map_or_else(|| "—".to_owned(), |tps| format!("{tps:.1}")),
    ]
}

/// 平均首 token 等待；没有请求上报过首 token 时返回 `None`（旧会话或全部失败）。
pub(super) fn average_ttft(summary: &moc_protocol::ModelUsageSummary) -> Option<u64> {
    (summary.ttft_calls > 0).then(|| summary.ttft_ms / summary.ttft_calls)
}

/// 输出速率：优先用生成阶段耗时；没有可拆分数据（旧会话）时退回整段用时。
pub(super) fn usage_tps(summary: &moc_protocol::ModelUsageSummary) -> Option<f64> {
    if summary.output_tokens == 0 {
        return None;
    }
    let ms = if summary.gen_ms > 0 {
        summary.gen_ms
    } else if summary.ttft_calls == 0 {
        summary.active_ms
    } else {
        return None;
    };
    (ms > 0).then(|| summary.output_tokens as f64 * 1000.0 / ms as f64)
}

/// 时长：毫秒级给毫秒，分钟以上用 `1m02s` / `1h05m`，
/// 免得长会话在表格里写成 `3723.00 s` 撑爆列宽。
pub(super) fn format_active(active_ms: u64) -> String {
    let secs = active_ms / 1_000;
    if active_ms < 1_000 {
        format!("{active_ms} ms")
    } else if secs < 60 {
        format!("{secs}.{}s", active_ms % 1_000 / 100)
    } else if secs < 3_600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3_600, secs % 3_600 / 60)
    }
}

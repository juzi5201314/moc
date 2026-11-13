use anyhow::bail;
use moc_protocol::methods::ModelSwitchOverflow;
use moc_protocol::methods::SessionAutonomy;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionInfo;

pub(super) fn parse_engine(engine: &str) -> anyhow::Result<SessionEngine> {
    match engine {
        "standard" => Ok(SessionEngine::Standard),
        "ptc" => Ok(SessionEngine::Ptc),
        "clm" | "clm-experimental" => Ok(SessionEngine::ClmExperimental),
        _ => bail!("引擎必须是 standard、ptc 或 clm；计划状态用 /plan，单条只读问答用 /ask <问题>"),
    }
}

/// 状态栏标签：引擎 + plan 模式 + 非默认自主程度。
pub(super) fn state_label(session: &SessionInfo) -> String {
    let engine = match session.engine {
        SessionEngine::Standard => "standard",
        SessionEngine::Ptc => "ptc",
        SessionEngine::ClmExperimental => "(clm)",
    };
    let mut label = engine.to_owned();
    if session.plan_active {
        label.push_str("+plan");
    }
    match session.autonomy {
        SessionAutonomy::Default => {}
        SessionAutonomy::DontAsk => label.push_str("+dont_ask"),
        SessionAutonomy::Unattended => label.push_str("+unattended"),
    }
    label
}

/// 容量缺口的中文描述；token 按原值输出，与 REPL 其它统计一致。
pub(super) fn capacity_notice(overflow: &ModelSwitchOverflow, selector: &str) -> String {
    if overflow.tokens > overflow.context_window {
        format!(
            "当前上下文 {} token 超过 {} 的窗口 {} token",
            overflow.tokens, selector, overflow.context_window
        )
    } else {
        format!(
            "当前上下文 {} token 超过 {} 的压缩触发线 {} token（窗口 {} token）",
            overflow.tokens, selector, overflow.threshold, overflow.context_window
        )
    }
}

pub(super) fn print_sessions(sessions: &[SessionInfo]) {
    for session in sessions {
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}",
            session.id,
            state_label(session),
            session.model,
            session.permission.as_str(),
            session.title,
            session.cwd
        );
    }
}

pub(super) fn print_usage(result: &moc_protocol::SessionUsageResult) {
    for summary in result.models.iter().chain(std::iter::once(&result.total)) {
        let cache = summary.cached_input_tokens.unwrap_or(0);
        let cache_rate = if summary.input_tokens == 0 {
            0.0
        } else {
            cache as f64 * 100.0 / summary.input_tokens as f64
        };
        let reasoning = summary
            .reasoning_tokens
            .map_or_else(|| "未知".to_owned(), |tokens| tokens.to_string());
        let split = if summary.gen_ms > 0 {
            format!(
                "（生成 {}，网络等待 {}）",
                format_duration(summary.gen_ms),
                format_duration(summary.active_ms.saturating_sub(summary.gen_ms))
            )
        } else {
            String::new()
        };
        let ttft = average_ttft(summary).map_or_else(|| "—".to_owned(), format_duration);
        let tps = usage_tps(summary).map_or_else(|| "—".to_owned(), |tps| format!("{tps:.2}"));
        // 后代会话的用量行标出来源：Fusion sidekick 与普通子代理。
        let model_label = match summary.source {
            moc_protocol::methods::UsageSource::Sidekick => {
                format!("{} (sidekick)", summary.model)
            }
            moc_protocol::methods::UsageSource::Subagent => format!("{} (子代理)", summary.model),
            moc_protocol::methods::UsageSource::Session => summary.model.clone(),
        };
        println!(
            "模型 {}：调用 {} 次，输入 {}，输出 {}，缓存命中 {} ({cache_rate:.1}%)，缓存写入 {}，思考 {}，用时 {}{split}，首 token 平均 {ttft}，TPS {tps}",
            model_label,
            summary.calls,
            summary.input_tokens,
            summary.output_tokens,
            cache,
            summary.cache_creation_input_tokens.unwrap_or(0),
            reasoning,
            format_duration(summary.active_ms),
        );
    }
    println!("工具调用用时 {}", format_duration(result.tool_ms));
}

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

pub(super) fn format_duration(active_ms: u64) -> String {
    if active_ms < 1_000 {
        format!("{active_ms} ms")
    } else {
        format!("{:.2} s", active_ms as f64 / 1000.0)
    }
}

/// `/goal` 回执的渲染文本：目标原文、相位、已跑轮次与续跑状态。
pub(super) fn render_goal(goal: Option<&moc_protocol::methods::GoalView>) -> String {
    let Some(goal) = goal else {
        return "当前没有目标；/goal <objective> 创建\n".to_owned();
    };
    let phase = match goal.phase {
        moc_protocol::methods::GoalPhase::Active => "进行中",
        moc_protocol::methods::GoalPhase::Paused => "已暂停",
        moc_protocol::methods::GoalPhase::Blocked => "已阻塞",
        moc_protocol::methods::GoalPhase::Complete => "已完成",
    };
    let mut text = format!("目标：{}\n状态：{phase}\n", goal.objective);
    if let Some(reason) = &goal.blocked_reason {
        text.push_str(&format!("阻塞：{}: {}\n", reason.code, reason.message));
    }
    text.push_str(&format!(
        "轮次：{}\n续跑：{}\n命令：/goal edit <objective> | /goal pause | /goal resume | /goal clear\n",
        goal.rounds_started,
        if goal.armed {
            "自动"
        } else {
            "已停（/goal resume 恢复）"
        }
    ));
    text
}

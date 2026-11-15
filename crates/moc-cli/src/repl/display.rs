use anyhow::bail;
use moc_protocol::methods::ModelSwitchOverflow;
use moc_protocol::methods::SessionAutonomy;
use moc_protocol::methods::SessionEngine;
use moc_protocol::methods::SessionInfo;
use rust_i18n::t;

pub(super) fn parse_engine(engine: &str) -> anyhow::Result<SessionEngine> {
    match engine {
        "standard" => Ok(SessionEngine::Standard),
        "ptc" => Ok(SessionEngine::Ptc),
        "clm" | "clm-experimental" => Ok(SessionEngine::ClmExperimental),
        _ => bail!("{}", t!("cli.display.bad_engine")),
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

/// 容量缺口的描述文本；token 按原值输出，与 REPL 其它统计一致。
pub(super) fn capacity_notice(overflow: &ModelSwitchOverflow, selector: &str) -> String {
    if overflow.tokens > overflow.context_window {
        t!(
            "cli.display.over_window",
            tokens = overflow.tokens,
            selector = selector,
            window = overflow.context_window
        )
        .into_owned()
    } else {
        t!(
            "cli.display.over_threshold",
            tokens = overflow.tokens,
            selector = selector,
            threshold = overflow.threshold,
            window = overflow.context_window
        )
        .into_owned()
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
            .map_or_else(|| t!("cli.display.unknown").into_owned(), |tokens| {
                tokens.to_string()
            });
        let split = if summary.gen_ms > 0 {
            t!(
                "cli.display.time_split",
                gen = format_duration(summary.gen_ms),
                net = format_duration(summary.active_ms.saturating_sub(summary.gen_ms))
            )
            .into_owned()
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
            moc_protocol::methods::UsageSource::Subagent => {
                t!("cli.display.subagent_model", model = summary.model.as_str()).into_owned()
            }
            moc_protocol::methods::UsageSource::Session => summary.model.clone(),
        };
        println!(
            "{}",
            t!(
                "cli.display.usage_row",
                model = model_label,
                calls = summary.calls,
                input = summary.input_tokens,
                output = summary.output_tokens,
                cache_hit = cache,
                cache_rate = format!("{cache_rate:.1}"),
                cache_write = summary.cache_creation_input_tokens.unwrap_or(0),
                reasoning = reasoning,
                duration = format_duration(summary.active_ms),
                split = split,
                ttft = ttft,
                tps = tps
            )
        );
    }
    println!(
        "{}",
        t!("cli.display.tool_ms", ms = format_duration(result.tool_ms))
    );
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
        return t!("cli.display.no_goal").into_owned();
    };
    let phase = match goal.phase {
        moc_protocol::methods::GoalPhase::Active => t!("cli.display.goal_active"),
        moc_protocol::methods::GoalPhase::Paused => t!("cli.display.goal_paused"),
        moc_protocol::methods::GoalPhase::Blocked => t!("cli.display.goal_blocked"),
        moc_protocol::methods::GoalPhase::Complete => t!("cli.display.goal_complete"),
    };
    let mut text = t!(
        "cli.display.goal_head",
        objective = goal.objective,
        phase = phase.as_ref()
    )
    .into_owned();
    if let Some(reason) = &goal.blocked_reason {
        text.push_str(
            &t!(
                "cli.display.goal_blocked_by",
                code = reason.code,
                message = reason.message
            )
            .into_owned(),
        );
    }
    text.push_str(
        &t!(
            "cli.display.goal_tail",
            rounds = goal.rounds_started,
            armed = if goal.armed {
                t!("cli.display.goal_armed")
            } else {
                t!("cli.display.goal_disarmed")
            }
            .as_ref()
        )
        .into_owned(),
    );
    text
}

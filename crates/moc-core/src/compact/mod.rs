//! 上下文压缩：token 估算与预算、按配置依次尝试压缩方法、提交 checkpoint。

pub mod archive;
mod digest;
pub mod grok_build;
mod guard;
pub mod images;
pub mod remote;
pub mod replay;
pub mod shake;
pub mod soft;

use moc_llm::{ContentPart, Message, ToolDef};
use moc_protocol::RequestContext;
use moc_protocol::methods::{CompactResult, SessionUpdate};

use crate::config::{CompactionMethod, ResolvedConfig, SoftMethod};
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::entries::EntryPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    Threshold,
    Overflow,
    Manual,
    PreApprove,
    /// 切换模型后按目标模型窗口压缩。
    Switch,
}

/// 估算 token：每 4 个 ASCII 字节算 1 个，每个非 ASCII 字符算 1 个。
pub fn estimate_tokens(text: &str) -> u64 {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for character in text.chars() {
        if character.is_ascii() {
            ascii += character.len_utf8() as u64;
        } else {
            non_ascii += 1;
        }
    }
    ascii / 4 + non_ascii
}

/// 按同一套估算规则取不超过预算的 UTF-8 前缀，避免中文按字节截断时超出预算。
pub fn prefix_within_tokens(text: &str, budget: u64) -> &str {
    let mut ascii = 0u64;
    let mut non_ascii = 0u64;
    for (index, character) in text.char_indices() {
        if character.is_ascii() {
            ascii += 1;
        } else {
            non_ascii += 1;
        }
        if ascii / 4 + non_ascii > budget {
            return &text[..index];
        }
    }
    text
}

/// 按内容块估算消息的 token；图片按固定的视觉 token 计算，不把 Base64 字节当文本。
pub fn messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

/// 每条消息固定额外计入的 token 数，与内容无关。
pub(crate) const MESSAGE_TOKEN_OVERHEAD: u64 = 8;

pub(crate) fn message_tokens(message: &Message) -> u64 {
    MESSAGE_TOKEN_OVERHEAD.saturating_add(message.content.iter().map(content_part_tokens).sum())
}

fn content_part_tokens(part: &ContentPart) -> u64 {
    match part {
        ContentPart::Text(text) => estimate_tokens(text),
        ContentPart::Image { .. } => images::IMAGE_TOKEN_ESTIMATE,
        ContentPart::Reasoning(reasoning) => {
            serde_json::to_string(reasoning).map_or(0, |text| estimate_tokens(&text))
        }
        ContentPart::ToolCall(call) => {
            serde_json::to_string(call).map_or(0, |text| estimate_tokens(&text))
        }
        ContentPart::ToolResult(result) => estimate_tokens(&result.content).saturating_add(
            (result.images.len() as u64).saturating_mul(images::IMAGE_TOKEN_ESTIMATE),
        ),
    }
}

/// 把全部工具定义序列化后整体估算一次；schema 每次请求都会发送，必须计入。
pub fn tools_tokens(tools: &[ToolDef]) -> u64 {
    let Ok(text) = serde_json::to_string(tools) else {
        return 0;
    };
    estimate_tokens(&text)
}

/// 估算 Responses 原生压缩数据在本地回放视图里占用的 token。
fn native_input_tokens(state: &crate::session::state::SessionState) -> u64 {
    state
        .native_input
        .as_ref()
        .and_then(|items| serde_json::to_string(items).ok())
        .map_or(0, |serialized| estimate_tokens(&serialized))
}

/// 某个模型下的上下文基准：指令消息、工具定义，以及与提示执行时算法一致的完整估算。
pub(crate) struct ModelContext {
    /// 该模型的指令消息（压缩摘要请求也复用它）。
    pub system: Vec<Message>,
    /// 指令消息开头的静态块数；摘要请求沿用同样的缓存断点位置。
    pub static_prefix: Vec<usize>,
    /// 该模型可用的工具定义。
    pub tools: Vec<ToolDef>,
    /// 回放给模型的历史消息，不含系统提示。
    pub messages: Vec<Message>,
    /// 系统提示和工具定义本身的 token 数（不含 transcript 和附加内容）。
    pub fixed_tokens: u64,
    /// 完整的上下文估算，算法与提示执行时一致（含 `additional_tokens`）。
    pub tokens: u64,
}

/// 组装指定模型的系统提示和工具定义，并按提示执行时的算法估算当前上下文。
pub(crate) async fn model_context(
    inner: &CoreInner,
    ctx: &RequestContext,
    state: &crate::session::state::SessionState,
    config: &ResolvedConfig,
    model: &crate::ResolvedModel,
    additional_tokens: u64,
) -> Result<ModelContext, CoreError> {
    let cwd = std::path::Path::new(&state.session.cwd);
    let skills = inner.project_skills(cwd)?;
    let summary = inner.mcp_summary(config).await?;
    let tool_declarations = inner
        .tool_declarations(
            state.mode,
            config,
            model.context_window,
            inner.client_supports_ask(ctx.connection_id()),
        )
        .await?;
    let context_files = inner.project_context_files(&state.session.id, cwd).await;
    let plugins = inner.project_plugins(cwd).await?;
    let plugin_prompt = plugins
        .prompt_text(&crate::service::plugin_context(state, &model.selector))
        .await;
    // 子代理的身份与任务块在普通请求里进系统提示；压缩请求必须复用同一段前缀。
    let subagent = crate::agents::subagent_prompt(inner, &state.session.id).await;
    let prompt = crate::system_prompt::build(
        config,
        model,
        cwd,
        &context_files,
        &skills,
        &summary,
        &tool_declarations,
        &plugin_prompt,
        subagent.as_ref().map(|prompt| prompt.block.as_str()),
    );
    let system = prompt.messages;
    let plugins = inner.plugin_tools(cwd).await;
    let tools = inner.tools.definitions(
        state.mode,
        inner.client_supports_ask(ctx.connection_id()),
        config,
        crate::tools::ToolScope {
            control_agents: !state.session.is_subagent(),
            subagent: state.session.is_subagent(),
        },
        &plugins,
    );
    let messages = replay::model_messages(state, None, Some(model));
    let mut measured = Vec::with_capacity(system.len() + messages.len());
    measured.extend(system.iter().cloned());
    measured.extend(messages.iter().cloned());
    let tokens = context_tokens_with_additional(state, &measured, &tools, additional_tokens);
    let fixed_tokens = messages_tokens(&system).saturating_add(tools_tokens(&tools));
    Ok(ModelContext {
        system,
        static_prefix: prompt.static_prefix,
        tools,
        messages,
        fixed_tokens,
        tokens,
    })
}

/// 有上游返回的用量时以它为基准，只估算那次响应之后追加的消息；同时用完整的本地估算
/// 作下限，避免请求前对历史的变换导致低估。
pub(crate) fn context_tokens_with_additional(
    state: &crate::session::state::SessionState,
    messages: &[Message],
    tools: &[ToolDef],
    additional_tokens: u64,
) -> u64 {
    let stored_estimate = messages_tokens(messages)
        .saturating_add(tools_tokens(tools))
        .saturating_add(native_input_tokens(state))
        .saturating_add(additional_tokens);
    if state.usage.input_tokens == 0 || state.usage_at == 0 {
        return stored_estimate;
    }
    let added = state
        .transcript_token_counts
        .iter()
        .skip(state.usage_at)
        .copied()
        .sum::<u64>();
    let provider_anchor = state
        .usage
        .input_tokens
        .saturating_add(state.usage.output_tokens)
        .saturating_add(added)
        .saturating_add(additional_tokens);
    provider_anchor.max(stored_estimate)
}

/// 压缩触发线：只按模型上下文窗口计算，不扣除输出上限；上游报溢出时仍由自动压缩恢复。
pub fn threshold(config: &ResolvedConfig, context_window: u64) -> u64 {
    match config.compaction.threshold {
        crate::config::CompactionThreshold::Percent(percent) => {
            context_window.saturating_mul(percent) / 100
        }
        crate::config::CompactionThreshold::Tokens(tokens) => tokens,
    }
}
/// 一次压缩中，当前请求里不属于 transcript 的内容。
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct ContextBudget {
    pub additional_tokens: u64,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CompactionBudget {
    pub tokens_before: u64,
    pub fixed_tokens: u64,
}

#[derive(Debug)]
pub(crate) struct CompactReport {
    pub result: CompactResult,
    pub created_headroom: bool,
}

/// 自动压缩后占用必须降到触发线的这个百分比以下（参考 OMP 的 0.8 recovery band）。
const RECOVERY_BAND_PERCENT: u64 = 80;

pub(crate) fn recovery_band(config: &ResolvedConfig, context_window: u64) -> u64 {
    threshold(config, context_window).saturating_mul(RECOVERY_BAND_PERCENT) / 100
}

/// 按压缩原因决定压缩后占用必须低于多少。
fn required_tokens(config: &ResolvedConfig, context_window: u64, reason: CompactReason) -> u64 {
    match reason {
        CompactReason::Overflow => threshold(config, context_window),
        CompactReason::Threshold
        | CompactReason::Manual
        | CompactReason::PreApprove
        | CompactReason::Switch => recovery_band(config, context_window),
    }
}

/// 按配置依次尝试各压缩方法；用户给的压缩指令只交给摘要模型。
pub(crate) async fn manual(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    instructions: Option<&str>,
    model_selector: Option<&str>,
    plan: Option<&str>,
) -> Result<CompactResult, CoreError> {
    compact(
        inner,
        ctx,
        session_id,
        CompactReason::Manual,
        instructions,
        ContextBudget::default(),
        model_selector,
        plan,
    )
    .await
    .map(|report| report.result)
}

#[expect(
    clippy::too_many_arguments,
    reason = "压缩的入参各自独立，打包成结构体只是把同样多的字段搬一遍"
)]
pub(crate) async fn compact(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    reason: CompactReason,
    instructions: Option<&str>,
    context: ContextBudget,
    model_selector: Option<&str>,
    plan: Option<&str>,
) -> Result<CompactReport, CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let mut state = record.state.lock().await.clone();
    let config = inner.project_config(std::path::Path::new(&state.session.cwd))?;
    if !config.compaction.enabled {
        return Err(CoreError::Tool(
            "NotSupported: compaction is disabled".into(),
        ));
    }
    let model = config.resolve_model(model_selector.unwrap_or(&state.model))?;
    let mut measured = model_context(
        inner,
        ctx,
        &state,
        &config,
        &model,
        context.additional_tokens,
    )
    .await?;
    let budget = CompactionBudget {
        tokens_before: measured.tokens,
        fixed_tokens: measured
            .fixed_tokens
            .saturating_add(context.additional_tokens),
    };
    let required_tokens = required_tokens(&config, model.context_window, reason);
    let reason_name = match reason {
        CompactReason::Threshold => "threshold",
        CompactReason::Overflow => "overflow",
        CompactReason::Manual => "manual",
        CompactReason::PreApprove => "pre_approve",
        CompactReason::Switch => "switch",
    };
    let compact_started = std::time::Instant::now();
    let plugin_host =
        crate::service::plugin_events::host_for(inner, std::path::Path::new(&state.session.cwd))
            .await;
    let plugin_context = crate::service::plugin_events::context(
        session_id,
        &state.session.cwd,
        state.mode,
        state.permission,
        &model.selector,
        state.session.is_subagent(),
    );
    if let Some(host) = &plugin_host {
        crate::service::plugin_events::before_compaction(
            host,
            &plugin_context,
            reason_name,
            instructions,
            Some(&model.selector),
        )
        .await;
    }
    // 只有真正要等上游模型的压缩才让前端开始计时；纯本地裁剪瞬时可成。
    let mut started = false;
    let outcome: Result<CompactReport, CoreError> = async {
        if instructions.is_some() {
            emit_compaction_started(ctx, session_id).await?;
            started = true;
            let result = run_soft(SoftInput {
                inner,
                ctx,
                state: &state,
                config: &config,
                model: &model,
                system: &measured.system,
                static_prefix: &measured.static_prefix,
                tools: &measured.tools,
                instructions,
                plan,
                budget,
            })
            .await?;
            return Ok(CompactReport {
                created_headroom: result.tokens_after <= required_tokens,
                result,
            });
        }

        let mut failures = Vec::with_capacity(config.compaction.method_order.len());
        let mut last_result = None;
        for method in &config.compaction.method_order {
            // 只有真正要等上游模型的方法才值得让前端开始计时：不支持原生压缩的
            // remote 会立刻失败，shake 是本地裁剪，都不该先闪一条“压缩中”。
            let waits_for_model = match method {
                CompactionMethod::Remote => remote::available(inner, &config, &model),
                CompactionMethod::Soft => true,
                CompactionMethod::Shake => false,
            };
            if waits_for_model && !started {
                emit_compaction_started(ctx, session_id).await?;
                started = true;
            }
            let result = match method {
                CompactionMethod::Remote => {
                    remote::compact(
                        inner,
                        ctx,
                        &state,
                        &config,
                        &model,
                        &measured.messages,
                        budget,
                    )
                    .await
                }
                CompactionMethod::Shake => {
                    shake::compact(inner, &state, &config, &model, budget).await
                }
                CompactionMethod::Soft => {
                    run_soft(SoftInput {
                        inner,
                        ctx,
                        state: &state,
                        config: &config,
                        model: &model,
                        system: &measured.system,
                        static_prefix: &measured.static_prefix,
                        tools: &measured.tools,
                        instructions,
                        plan,
                        budget,
                    })
                    .await
                }
            };
            match result {
                Ok(result) => {
                    let report = CompactReport {
                        created_headroom: result.tokens_after <= required_tokens,
                        result,
                    };
                    if report.created_headroom || *method == CompactionMethod::Soft {
                        return Ok(report);
                    }
                    last_result = Some(report);
                    state = record.state.lock().await.clone();
                    measured.messages = replay::model_messages(&state, None, Some(&model));
                }
                Err(error @ (CoreError::Protocol(_) | CoreError::Cancelled)) => {
                    return Err(error);
                }
                Err(error @ CoreError::Llm(moc_llm::LlmError::Guard(_))) => return Err(error),
                Err(error) => {
                    tracing::warn!(session_id, method = ?method, %error, "压缩方法失败，改用下一个方法");
                    failures.push(format!("{method:?}: {error}"));
                    // 失败的方法可能已经改写了 transcript（shake 会整体重写）或写入了 usage：
                    // 后面的方法必须基于当前状态，否则会用旧快照覆盖这期间追加的 entry。
                    state = record.state.lock().await.clone();
                    measured.messages = replay::model_messages(&state, None, Some(&model));
                }
            }
        }
        if let Some(report) = last_result {
            return Ok(report);
        }
        Err(CoreError::ContextOverflow(format!(
            "所有压缩方法都失败了（{reason:?}）：{}",
            failures.join("；")
        )))
    }
    .await;

    // 一次压缩尝试只报一个终态：中间方法（例如 shake）成功了但没腾出空间时，
    // 不提前替换进行中提示，等最终结果出来再收尾。
    match &outcome {
        Ok(report) => {
            let update = SessionUpdate::ContextCompacted {
                method: report.result.method.clone(),
                tokens_before: report.result.tokens_before,
                tokens_after: report.result.tokens_after,
                full: report.result.full,
            };
            if let Err(emit_error) = ctx.updates().emit(session_id.to_owned(), update).await {
                tracing::warn!(session_id, %emit_error, "上下文压缩完成事件下发失败");
            }
        }
        Err(error) if started => {
            let message = error.to_string();
            if let Err(emit_error) = ctx
                .updates()
                .emit(
                    session_id.to_owned(),
                    SessionUpdate::ContextCompactionFailed { message },
                )
                .await
            {
                tracing::warn!(session_id, %emit_error, "上下文压缩失败事件下发失败");
            }
        }
        Err(_) => {}
    }
    if let Some(host) = &plugin_host {
        let duration_ms = u64::try_from(compact_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        match &outcome {
            Ok(report) => {
                crate::service::plugin_events::after_compaction(
                    host,
                    &plugin_context,
                    reason_name,
                    Some(&report.result.method),
                    Some(report.result.tokens_before),
                    Some(report.result.tokens_after),
                    Some(report.result.full),
                    duration_ms,
                    None,
                )
                .await;
            }
            Err(error) => {
                crate::service::plugin_events::after_compaction(
                    host,
                    &plugin_context,
                    reason_name,
                    None,
                    None,
                    None,
                    None,
                    duration_ms,
                    Some(&error.to_string()),
                )
                .await;
            }
        }
    }
    outcome
}

/// 通知前端压缩已进入等待上游模型的阶段；本地裁剪不调用它。
async fn emit_compaction_started(ctx: &RequestContext, session_id: &str) -> Result<(), CoreError> {
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::ContextCompactionStarted,
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))
}

/// soft 压缩的公共输入；两种实现只在摘要提示词和提交方式上不同。
pub(crate) struct SoftInput<'a> {
    pub inner: &'a CoreInner,
    pub ctx: &'a RequestContext,
    pub state: &'a crate::session::state::SessionState,
    pub config: &'a ResolvedConfig,
    pub model: &'a crate::ResolvedModel,
    pub system: &'a [Message],
    pub static_prefix: &'a [usize],
    pub tools: &'a [ToolDef],
    pub instructions: Option<&'a str>,
    /// 审批执行时传入的待执行计划 markdown；`None` 时使用已批准计划。
    pub plan: Option<&'a str>,
    pub budget: CompactionBudget,
}

/// 按 `compaction.soft_method` 选择 soft 实现；`remote` 和 `shake` 不走这里。
async fn run_soft(input: SoftInput<'_>) -> Result<CompactResult, CoreError> {
    let SoftInput {
        inner,
        ctx,
        state,
        config,
        model,
        system,
        static_prefix,
        tools,
        instructions,
        plan,
        budget,
    } = input;
    match config.compaction.soft_method {
        SoftMethod::Moc => {
            soft::compact(soft::SoftContext {
                inner,
                ctx,
                state,
                config,
                model,
                system,
                static_prefix,
                tools,
                instructions,
                plan,
                budget,
            })
            .await
        }
        SoftMethod::GrokBuild => {
            grok_build::compact(grok_build::GrokBuildContext {
                inner,
                ctx,
                state,
                config,
                model,
                system,
                static_prefix,
                tools,
                instructions,
                budget,
            })
            .await
        }
    }
}

/// checkpoint 的历史归档方式。
pub(crate) enum HistoryArchive {
    /// 记录 checkpoint 时按当前状态写出压缩前的历史。
    Write,
    /// 调用方已在改写 transcript 前完成归档，直接用它的结果。
    Precomputed(Option<String>),
    /// 不归档：Grok Build 整体替换历史，不带指向旧历史的链接。
    None,
}

pub(crate) struct Checkpoint {
    /// 生成 checkpoint 的方法；沿用上一份 summary/native 时必须保留它原本的方法名。
    pub method: String,
    /// 生成 checkpoint 的模型；决定换模型后原生压缩数据是否仍然有效。
    pub model: Option<String>,
    pub summary: String,
    /// Grok Build 整体替换历史时附带的运行时状态提醒；其它方法为 `None`。
    pub reminder: Option<String>,
    pub first_kept_id: Option<String>,
    pub tokens_before: u64,
    pub tokens_after: u64,
    /// 本次压缩是否重新生成了摘要；`false` 表示只裁剪历史（`shake`）。
    pub full: bool,
    pub instructions: Option<String>,
    pub native: Option<Box<serde_json::Value>>,
    pub archive: HistoryArchive,
}

pub(crate) async fn record_compaction(
    inner: &CoreInner,
    session_id: &str,
    checkpoint: Checkpoint,
) -> Result<CompactResult, CoreError> {
    let Checkpoint {
        method,
        model,
        mut summary,
        reminder,
        first_kept_id,
        tokens_before,
        tokens_after,
        full,
        instructions,
        native,
        archive,
    } = checkpoint;
    append_runtime_checkpoint(inner, session_id, &mut summary, archive).await?;
    inner.reset_loaded_skills(session_id).await;
    let summary_chars = summary.chars().count().min(u32::MAX as usize) as u32;
    inner
        .sessions
        .append(
            session_id,
            EntryPayload::Compaction {
                method: method.clone(),
                summary,
                first_kept_id,
                tokens_before,
                tokens_after,
                instructions,
                native,
                model,
                reminder,
            },
        )
        .await?;
    // 本次压缩已经提交，提前生成的摘要对后续压缩都不再有效。
    inner.runtime(session_id).await.grok_prefire.clear();
    Ok(CompactResult {
        method,
        tokens_before,
        tokens_after,
        full,
        summary_chars,
    })
}

async fn append_runtime_checkpoint(
    inner: &CoreInner,
    session_id: &str,
    summary: &mut String,
    archive: HistoryArchive,
) -> Result<(), CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let state = record.state.lock().await.clone();
    let archive_uri = match archive {
        // Grok Build 整体替换历史：不带旧历史链接，也不追加运行时快照。
        HistoryArchive::None => return Ok(()),
        HistoryArchive::Write => archive::write(&record.paths, &state).await?,
        HistoryArchive::Precomputed(uri) => uri,
    };
    let jobs = inner.runtime(session_id).await.jobs.list().await;
    if archive_uri.is_none() && state.todos.is_empty() && jobs.is_empty() {
        return Ok(());
    }
    if let Some(uri) = archive_uri {
        summary.push_str("\n\n<compaction-history>Full pre-compaction history is archived at ");
        summary.push_str(&uri);
        summary.push_str("; read this index when exact details are needed.</compaction-history>");
    }
    let has_runtime_state = !state.todos.is_empty() || !jobs.is_empty();
    if has_runtime_state {
        summary.push_str("\n\n<compaction-runtime>\n");
        if !state.todos.is_empty() {
            summary.push_str("Structured todo state (current snapshot):\n");
            summary.push_str(&serde_json::to_string(&state.todos).map_err(|error| {
                CoreError::Session(format!(
                    "failed to serialize compaction todo state: {error}"
                ))
            })?);
            summary.push('\n');
        }
        for job in jobs {
            let status = match job.status {
                crate::tools::job::JobStatus::Running => "running",
                crate::tools::job::JobStatus::Finished(Some(0)) => "finished(0)",
                crate::tools::job::JobStatus::Finished(Some(_)) => "finished(nonzero)",
                crate::tools::job::JobStatus::Finished(None) => "finished",
                crate::tools::job::JobStatus::Killing => "killing",
                crate::tools::job::JobStatus::Killed => "killed",
                crate::tools::job::JobStatus::TimedOut => "timed_out",
                crate::tools::job::JobStatus::OutputLimited => "output_limited",
            };
            let lifecycle = match job.lifecycle {
                crate::tools::job::JobLifecycle::Monitored => "monitored",
                crate::tools::job::JobLifecycle::Persistent => "persistent",
            };
            summary.push_str(&format!(
                "Background job {} [{} lifecycle={} origin={:?} warnings={}]: {}\n",
                job.id, status, lifecycle, job.origin, job.warning_count, job.command
            ));
        }
        summary.push_str("</compaction-runtime>");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entries::{Entry, EntryPayload, SessionOrigin};
    use crate::session::state::SessionState;
    use moc_protocol::methods::{SessionMode, SessionPermission};

    const MODEL_BASE: &str = "[models.roles]\ndefault='p/m'\nexplore='p/m'\nplan='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n";

    #[test]
    fn token_estimator_and_threshold_modes() {
        // 4 个 ASCII 字符算 1 个 token：abcd 记 1，中日韩字符各记 1。
        assert_eq!(estimate_tokens("abcd你好"), 3);
        let tokens =
            config_with("\n[compaction]\nthreshold = { mode = 'tokens', value = 50000 }\n");
        assert_eq!(threshold(&tokens, 200_000), 50_000);
        let percent = config_with("\n[compaction]\nthreshold = { mode = 'percent', value = 80 }\n");
        assert_eq!(threshold(&percent, 200_000), 160_000);
    }

    #[test]
    fn recovery_band_requires_headroom_below_trigger_threshold() {
        let config = test_config();
        assert_eq!(threshold(&config, 200_000), 170_000);
        assert_eq!(recovery_band(&config, 200_000), 136_000);
        assert_eq!(
            required_tokens(&config, 200_000, CompactReason::Threshold),
            136_000
        );
        assert_eq!(
            required_tokens(&config, 200_000, CompactReason::Overflow),
            170_000
        );
        assert_eq!(
            required_tokens(&config, 200_000, CompactReason::Switch),
            136_000
        );
        assert!(165_000 > recovery_band(&config, 200_000));
        assert!(130_000 <= recovery_band(&config, 200_000));

        let fixed = config_with("\n[compaction]\nthreshold = { mode = 'tokens', value = 50000 }\n");
        assert_eq!(recovery_band(&fixed, 200_000), 40_000);
    }

    #[test]
    fn threshold_ignores_max_output_tokens() {
        // 500k 窗口、384k 输出上限：触发线仍是窗口的 85%，不会因输出上限提前压缩。
        let config = config_with("max_output_tokens = 393216\n");
        assert_eq!(threshold(&config, 500_000), 425_000);
    }

    #[test]
    fn config_rejects_malformed_thresholds() {
        for suffix in [
            "\n[compaction]\nthreshold = { mode = 'tokens', value = 0 }\n",
            "\n[compaction]\nthreshold = { mode = 'tokens', value = -2 }\n",
            "\n[compaction]\nthreshold = { mode = 'percent', value = 0 }\n",
            "\n[compaction]\nthreshold = { mode = 'percent', value = 101 }\n",
            "\n[compaction]\nthreshold_tokens = 100\n",
        ] {
            assert!(
                load_config(&format!("{MODEL_BASE}{suffix}")).is_err(),
                "{suffix}"
            );
        }
        let Err(error) = load_config(&format!("{MODEL_BASE}context_window = 0\n")) else {
            panic!("context_window = 0 应被拒绝");
        };
        assert!(error.to_string().contains("context_window"), "{error}");
    }

    #[test]
    fn context_tokens_counts_tools_only_without_usage() {
        let tools = vec![ToolDef {
            name: "demo".into(),
            description: Some("工具描述".into()),
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
        }];
        let messages = vec![Message::user_text("abcd")];
        assert!(tools_tokens(&tools) > 0);
        let state = SessionState::from_entries(vec![header_entry()]).unwrap();
        assert_eq!(
            context_tokens_with_additional(&state, &messages, &tools, 0),
            messages_tokens(&messages) + tools_tokens(&tools)
        );
        // 有 usage 时上游的数字已经包含工具 schema，只追加新消息，不重复计入。
        let header = header_entry();
        let mut state =
            SessionState::from_entries(vec![header.clone(), message_entry(&header)]).unwrap();
        state.usage.input_tokens = 100;
        state.usage_at = state.transcript.len();
        assert_eq!(
            context_tokens_with_additional(&state, &messages, &tools, 0),
            100
        );
        assert_eq!(
            context_tokens_with_additional(&state, &messages, &tools, 20),
            120
        );

        let long_messages = vec![Message::user_text("x".repeat(800))];
        assert_eq!(
            context_tokens_with_additional(&state, &long_messages, &[], 0),
            messages_tokens(&long_messages)
        );
    }

    fn header_entry() -> Entry {
        Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "gpt-x".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "t".into(),
            1,
        )
    }

    fn message_entry(header: &Entry) -> Entry {
        Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text("hi"),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            header.id().to_owned(),
            2,
        )
    }

    fn load_config(body: &str) -> Result<ResolvedConfig, crate::error::CoreError> {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("home")).unwrap();
        std::fs::write(directory.path().join("home/config.toml"), body).unwrap();
        ResolvedConfig::load(&crate::CoreConfig {
            home: directory.path().join("home"),
            cwd: directory.path().to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
            ptc_worker: None,
        })
    }

    fn config_with(suffix: &str) -> ResolvedConfig {
        load_config(&format!("{MODEL_BASE}{suffix}")).unwrap()
    }

    fn test_config() -> ResolvedConfig {
        config_with("")
    }
}

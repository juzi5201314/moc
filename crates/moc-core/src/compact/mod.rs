//! 上下文预算、压缩方法调度与 checkpoint 提交。

pub mod archive;
pub mod images;
pub mod remote;
pub mod replay;
pub mod shake;
pub mod soft;

use moc_llm::{ContentPart, Message, ToolDef};
use moc_protocol::RequestContext;
use moc_protocol::methods::{CompactResult, SessionUpdate};

use crate::config::{CompactionMethod, ResolvedConfig};
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::session::entries::EntryPayload;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactReason {
    Threshold,
    Overflow,
    Manual,
    PreApprove,
}

/// 每四个 ASCII 字节估算一个 token，非 ASCII 字符各估算一个。
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

/// 用同一个 token 估算器确定 UTF-8 前缀，避免中文按字节裁剪时超预算。
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

/// 按内容块估算消息 token；图片按固定视觉 token 计量，不把 Base64 字节当成文本。
pub fn messages_tokens(messages: &[Message]) -> u64 {
    messages.iter().map(message_tokens).sum()
}

pub(crate) fn message_tokens(message: &Message) -> u64 {
    8u64.saturating_add(message.content.iter().map(content_part_tokens).sum())
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

/// 工具定义整体序列化后估算一次；工具 schema 随每次请求发送，必须计入基数。
pub fn tools_tokens(tools: &[ToolDef]) -> u64 {
    let Ok(text) = serde_json::to_string(tools) else {
        return 0;
    };
    estimate_tokens(&text)
}

/// 估算 Responses 原生压缩载荷在本地回放视图中的 token 占用。
fn native_input_tokens(state: &crate::session::state::SessionState) -> u64 {
    state
        .native_input
        .as_ref()
        .and_then(|items| serde_json::to_string(items).ok())
        .map_or(0, |serialized| estimate_tokens(&serialized))
}

/// 有上游用量时以其为基准，仅估算响应之后追加的消息；同时以完整本地
/// 会话估算作为下限，避免请求前变换导致上下文历史被低估。
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
        .transcript
        .iter()
        .skip(state.usage_at)
        .map(|entry| messages_tokens(std::slice::from_ref(&entry.message)))
        .sum::<u64>();
    let provider_anchor = state
        .usage
        .input_tokens
        .saturating_add(state.usage.output_tokens)
        .saturating_add(added)
        .saturating_add(additional_tokens);
    provider_anchor.max(stored_estimate)
}

/// 只使用模型上下文窗口，绝不扣除模型输出上限；上游溢出仍走自动压缩恢复。
pub fn threshold(config: &ResolvedConfig, context_window: u64) -> u64 {
    match config.compaction.threshold {
        crate::config::CompactionThreshold::Percent(percent) => {
            context_window.saturating_mul(percent) / 100
        }
        crate::config::CompactionThreshold::Tokens(tokens) => tokens,
    }
}
/// 一次压缩调用中不属于 transcript 的当前请求内容。
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

/// 触发阈值以下仍需保留的恢复空间比例，参考 OMP 核心的 0.8 recovery band。
const RECOVERY_BAND_PERCENT: u64 = 80;

pub(crate) fn recovery_band(config: &ResolvedConfig, context_window: u64) -> u64 {
    threshold(config, context_window).saturating_mul(RECOVERY_BAND_PERCENT) / 100
}

/// 当前压缩原因需要达到的占用上限。
fn required_tokens(config: &ResolvedConfig, context_window: u64, reason: CompactReason) -> u64 {
    match reason {
        CompactReason::Overflow => threshold(config, context_window),
        CompactReason::Threshold | CompactReason::Manual | CompactReason::PreApprove => {
            recovery_band(config, context_window)
        }
    }
}

/// 按配置执行压缩链；显式指令只交给摘要模型。
pub(crate) async fn manual(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    instructions: Option<&str>,
) -> Result<CompactResult, CoreError> {
    compact(
        inner,
        ctx,
        session_id,
        CompactReason::Manual,
        instructions,
        ContextBudget::default(),
    )
    .await
    .map(|report| report.result)
}

pub(crate) async fn compact(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    reason: CompactReason,
    instructions: Option<&str>,
    context: ContextBudget,
) -> Result<CompactReport, CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let mut state = record.state.lock().await.clone();
    let config = inner.project_config(std::path::Path::new(&state.session.cwd))?;
    if !config.compaction.enabled {
        return Err(CoreError::Tool(
            "NotSupported: compaction is disabled".into(),
        ));
    }
    let model = config.resolve_model(&state.model)?;
    let skills = inner.project_skills(std::path::Path::new(&state.session.cwd))?;
    let summary = inner.mcp_summary(&config).await?;
    let context_files = inner
        .project_context_files(session_id, std::path::Path::new(&state.session.cwd))
        .await;
    let system = crate::system_prompt::build(
        &config,
        &model,
        std::path::Path::new(&state.session.cwd),
        &context_files,
        &skills,
        &summary,
    );
    let tools = inner
        .tools
        .definitions(inner.client_supports_ask(ctx.connection_id()), &config);
    let fixed_tokens = messages_tokens(std::slice::from_ref(&system))
        .saturating_add(tools_tokens(&tools))
        .saturating_add(context.additional_tokens);
    let mut current_messages = replay::model_messages(&state, None, Some(&model));
    let mut measured_messages = Vec::with_capacity(current_messages.len() + 1);
    measured_messages.push(system.clone());
    measured_messages.extend(current_messages.iter().cloned());
    let tokens_before = context_tokens_with_additional(
        &state,
        &measured_messages,
        &tools,
        context.additional_tokens,
    );
    let budget = CompactionBudget {
        tokens_before,
        fixed_tokens,
    };
    let required_tokens = required_tokens(&config, model.context_window, reason);
    if instructions.is_some() {
        let result = soft::compact(soft::SoftContext {
            inner,
            ctx,
            state: &state,
            config: &config,
            system: &system,
            tools: &tools,
            instructions,
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
        let result = match method {
            CompactionMethod::Remote => {
                remote::compact(
                    inner,
                    ctx,
                    &state,
                    &config,
                    &model,
                    &current_messages,
                    budget,
                )
                .await
            }
            CompactionMethod::Shake => {
                shake::compact(inner, ctx, &state, &config, &model, budget).await
            }
            CompactionMethod::Soft => {
                soft::compact(soft::SoftContext {
                    inner,
                    ctx,
                    state: &state,
                    config: &config,
                    system: &system,
                    tools: &tools,
                    instructions,
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
                current_messages = replay::model_messages(&state, None, Some(&model));
            }
            Err(error @ (CoreError::Protocol(_) | CoreError::Cancelled)) => return Err(error),
            Err(error) => {
                tracing::warn!(session_id, method = ?method, %error, "上下文压缩方法失败，尝试下一方法");
                failures.push(format!("{method:?}: {error}"));
            }
        }
    }
    if let Some(report) = last_result {
        return Ok(report);
    }
    Err(CoreError::ContextOverflow(format!(
        "所有上下文压缩方法均失败（{reason:?}）：{}",
        failures.join("；")
    )))
}

pub(crate) struct Checkpoint {
    pub method: &'static str,
    pub summary: String,
    pub first_kept_id: Option<String>,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub instructions: Option<String>,
    pub native: Option<Box<serde_json::Value>>,
}

pub(crate) async fn record_compaction(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    checkpoint: Checkpoint,
) -> Result<CompactResult, CoreError> {
    let Checkpoint {
        method,
        mut summary,
        first_kept_id,
        tokens_before,
        tokens_after,
        instructions,
        native,
    } = checkpoint;
    append_runtime_checkpoint(inner, session_id, &mut summary).await?;
    inner.reset_loaded_skills(session_id).await;
    let summary_chars = summary.chars().count().min(u32::MAX as usize) as u32;
    inner
        .sessions
        .append(
            session_id,
            EntryPayload::Compaction {
                method: method.to_owned(),
                summary,
                first_kept_id,
                tokens_before,
                tokens_after,
                instructions,
                native,
            },
        )
        .await?;
    ctx.updates()
        .emit(
            session_id.to_owned(),
            SessionUpdate::ContextCompacted {
                method: method.to_owned(),
                tokens_before,
                tokens_after,
            },
        )
        .await
        .map_err(|error| CoreError::Protocol(error.to_string()))?;
    Ok(CompactResult {
        method: method.to_owned(),
        tokens_before,
        tokens_after,
        summary_chars,
    })
}

async fn append_runtime_checkpoint(
    inner: &CoreInner,
    session_id: &str,
    summary: &mut String,
) -> Result<(), CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let state = record.state.lock().await.clone();
    let archive_uri = archive::write(&record.paths, session_id, &state).await?;
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
            };
            summary.push_str(&format!(
                "Background job {} [{}]: {}\n",
                job.id, status, job.command
            ));
        }
        summary.push_str("</compaction-runtime>");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entries::{Entry, EntryPayload};
    use crate::session::state::SessionState;
    use moc_protocol::methods::SessionMode;

    const MODEL_BASE: &str = "[models.roles]\ndefault='p/m'\nsmol='p/m'\nslow='p/m'\nplan='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n";

    #[test]
    fn token_estimator_and_threshold_modes() {
        // 估算按 4 字符 1 token 折算：abcd 记 1，中日韩字符各记 1。
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
        assert!(165_000 > recovery_band(&config, 200_000));
        assert!(130_000 <= recovery_band(&config, 200_000));

        let fixed = config_with("\n[compaction]\nthreshold = { mode = 'tokens', value = 50000 }\n");
        assert_eq!(recovery_band(&fixed, 200_000), 40_000);
    }

    #[test]
    fn threshold_ignores_max_output_tokens() {
        // 500k 窗口 / 384k 最大输出：阈值仍为 85% 窗口，不因输出上限提前压缩。
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
        // 有 usage 时上游基数已含工具 schema，只追加新消息，不再重复计入。
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
            None,
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
        })
    }

    fn config_with(suffix: &str) -> ResolvedConfig {
        load_config(&format!("{MODEL_BASE}{suffix}")).unwrap()
    }

    fn test_config() -> ResolvedConfig {
        config_with("")
    }
}

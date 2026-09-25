//! 上下文预算、压缩方法调度与 checkpoint 提交。

pub mod remote;
pub mod replay;
pub mod shake;
pub mod soft;

use moc_llm::{Message, ToolDef};
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

pub fn messages_tokens(messages: &[Message]) -> u64 {
    messages
        .iter()
        .map(|message| {
            let text = serde_json::to_string(message).unwrap_or_default();
            estimate_tokens(&text)
        })
        .sum()
}

/// 工具定义整体序列化后估算一次；工具 schema 随每次请求发送，必须计入基数。
pub fn tools_tokens(tools: &[ToolDef]) -> u64 {
    let Ok(text) = serde_json::to_string(tools) else {
        return 0;
    };
    estimate_tokens(&text)
}

/// 有上游用量时以其为基准，仅估算响应之后追加的消息；无用量时全量估算并计入工具。
pub(crate) fn context_tokens(
    state: &crate::session::state::SessionState,
    messages: &[Message],
    tools: &[ToolDef],
) -> u64 {
    if state.usage.input_tokens == 0 || state.usage_at == 0 {
        return messages_tokens(messages).saturating_add(tools_tokens(tools));
    }
    let added = state
        .transcript
        .iter()
        .skip(state.usage_at)
        .map(|entry| messages_tokens(std::slice::from_ref(&entry.message)))
        .sum::<u64>();
    state
        .usage
        .input_tokens
        .saturating_add(state.usage.output_tokens)
        .saturating_add(added)
}

/// 阈值三层优先级：绝对 token 数、窗口百分比、预留回退（`窗口 − max(16384, 15%)`）。
///
/// 只按 `context_window` 计算，绝不扣减 `max_output_tokens`：大输出上限的模型
/// （如 500k 窗口 / 384k 输出）若按输出扣减会频繁提前压缩；真溢出由 Overflow 恢复兜底。
pub fn threshold(config: &ResolvedConfig, context_window: u64) -> u64 {
    let compaction = &config.compaction;
    if compaction.threshold_tokens > 0 {
        return compaction.threshold_tokens as u64;
    }
    if compaction.threshold_percent > 0 {
        return context_window
            .saturating_mul(compaction.threshold_percent as u64)
            .saturating_div(100);
    }
    let reserve = if compaction.reserve_tokens > 0 {
        compaction.reserve_tokens as u64
    } else {
        16_384u64.max(context_window.saturating_mul(15) / 100)
    };
    context_window.saturating_sub(reserve)
}

/// 按配置执行压缩链；显式指令只交给摘要模型。
pub(crate) async fn manual(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    instructions: Option<&str>,
) -> Result<CompactResult, CoreError> {
    compact(inner, ctx, session_id, CompactReason::Manual, instructions).await
}

pub(crate) async fn compact(
    inner: &CoreInner,
    ctx: &RequestContext,
    session_id: &str,
    reason: CompactReason,
    instructions: Option<&str>,
) -> Result<CompactResult, CoreError> {
    let record = inner.sessions.get(session_id).await?;
    let mut state = record.state.lock().await.clone();
    let config = inner.project_config(std::path::Path::new(&state.session.cwd))?;
    if !config.compaction.enabled {
        return Err(CoreError::Tool(
            "NotSupported: compaction is disabled".into(),
        ));
    }
    let model = config.resolve_model(&state.model)?;
    let mut current_messages = replay::model_messages(&state, None, Some(&model));
    let tokens_before = messages_tokens(&current_messages);
    if instructions.is_some() {
        return soft::compact(inner, ctx, &state, &config, instructions, tokens_before).await;
    }
    let mut failures = Vec::with_capacity(config.compaction.method_order.len());
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
                    tokens_before,
                )
                .await
            }
            CompactionMethod::Shake => {
                shake::compact(inner, ctx, &state, &config, tokens_before).await
            }
            CompactionMethod::Soft => {
                soft::compact(inner, ctx, &state, &config, instructions, tokens_before).await
            }
        };
        match result {
            Ok(result)
                if *method == CompactionMethod::Shake
                    && result.tokens_after >= threshold(&config, model.context_window) =>
            {
                state = record.state.lock().await.clone();
                current_messages = replay::model_messages(&state, None, Some(&model));
            }
            Ok(result) => return Ok(result),
            Err(error @ (CoreError::Protocol(_) | CoreError::Cancelled)) => return Err(error),
            Err(error) => {
                tracing::warn!(session_id, method = ?method, %error, "上下文压缩方法失败，尝试下一方法");
                failures.push(format!("{method:?}: {error}"));
            }
        }
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
        summary,
        first_kept_id,
        tokens_before,
        tokens_after,
        instructions,
        native,
    } = checkpoint;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::entries::{Entry, EntryPayload};
    use crate::session::state::SessionState;
    use moc_protocol::methods::SessionMode;

    const MODEL_BASE: &str = "[models.roles]\ndefault='p/m'\nsmol='p/m'\nslow='p/m'\nplan='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n";

    #[test]
    fn token_estimator_and_threshold_follow_configured_precedence() {
        assert_eq!(estimate_tokens("abcd你好"), 3);
        let config = test_config();
        assert_eq!(threshold(&config, 200_000), 170_000);
        let _ = (
            CompactReason::Threshold,
            CompactReason::Overflow,
            CompactReason::PreApprove,
        );
    }

    #[test]
    fn threshold_follows_tokens_percent_reserve_precedence() {
        let tokens =
            config_with("\n[compaction]\nthreshold_tokens = 50000\nthreshold_percent = 90\n");
        assert_eq!(threshold(&tokens, 200_000), 50_000);
        let percent = config_with("\n[compaction]\nthreshold_percent = 80\n");
        assert_eq!(threshold(&percent, 200_000), 160_000);
        let reserve =
            config_with("\n[compaction]\nthreshold_percent = -1\nreserve_tokens = 4096\n");
        assert_eq!(threshold(&reserve, 200_000), 195_904);
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
            "\n[compaction]\nthreshold_tokens = 0\n",
            "\n[compaction]\nthreshold_tokens = -2\n",
            "\n[compaction]\nthreshold_percent = 0\n",
            "\n[compaction]\nthreshold_percent = 101\n",
            "\n[compaction]\nthreshold_percent = -2\n",
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
            context_tokens(&state, &messages, &tools),
            messages_tokens(&messages) + tools_tokens(&tools)
        );
        // 有 usage 时上游基数已含工具 schema，只追加新消息，不再重复计入。
        let header = header_entry();
        let mut state =
            SessionState::from_entries(vec![header.clone(), message_entry(&header)]).unwrap();
        state.usage.input_tokens = 100;
        state.usage_at = state.transcript.len();
        assert_eq!(context_tokens(&state, &messages, &tools), 100);
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

//! 上下文预算、压缩方法调度与 checkpoint 提交。

pub mod remote;
pub mod replay;
pub mod shake;
pub mod soft;

use moc_llm::Message;
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

/// 有上游用量时以其为基准，仅估算响应之后追加的消息。
pub(crate) fn context_tokens(
    state: &crate::session::state::SessionState,
    messages: &[Message],
) -> u64 {
    if state.usage.input_tokens == 0 || state.usage_at == 0 {
        return messages_tokens(messages);
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

    fn test_config() -> ResolvedConfig {
        let directory = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(directory.path().join("home")).unwrap();
        std::fs::write(
            directory.path().join("home/config.toml"),
            "[models.roles]\ndefault='p/m'\nsmol='p/m'\nslow='p/m'\nplan='p/m'\n[providers.p]\napi='openai_chat'\nbase_url='https://example.invalid'\n[providers.p.models.m]\n",
        ).unwrap();
        ResolvedConfig::load(&crate::CoreConfig {
            home: directory.path().join("home"),
            cwd: directory.path().to_owned(),
            overlays: Vec::new(),
            session_dir: None,
            client: moc_llm::Client::new(),
        })
        .unwrap()
    }
}

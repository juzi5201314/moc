//! Grok Build 的 soft 压缩：9 段摘要、整体替换历史，以及在触发阈值之前先摘要前缀（预火）。
//!
//! 与 moc 自带的 soft 实现（[`super::soft`]）区别在压缩后的上下文形式：
//! `remote`/`shake` 不受影响，只有走到 `soft` 且配置选择 `grok_build` 时才用这里。

pub(crate) mod fit;
pub(crate) mod prefire;
pub(crate) mod prompt;
pub(crate) mod reminder;

use std::time::{Duration, Instant};

use moc_llm::{Message, Role, ToolDef};
use moc_protocol::RequestContext;
use tokio_util::sync::CancellationToken;

use crate::ResolvedModel;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::service::prompt::request::CacheAnchors;
use crate::session::entries::Entry;
use crate::session::state::SessionState;

use super::{CompactionBudget, HistoryArchive, messages_tokens, record_compaction};

/// 摘要请求最多尝试几次（含输出退化后的重试）。
const MAX_ATTEMPTS: u32 = 3;

/// 两次尝试之间的等待时间；可用 `GROK_COMPACT_RETRY_MS` 覆盖（集成测试用 1ms）。
const RETRY_DELAY: Duration = Duration::from_secs(3);

const RETRY_DELAY_ENV: &str = "GROK_COMPACT_RETRY_MS";

/// 整体替换历史时的续接前言；之后回放靠它识别出这是上一段会话的延续。
const CONTINUATION_PREAMBLE: &str = "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.";

fn retry_delay() -> Duration {
    if cfg!(test) {
        return Duration::from_millis(1);
    }
    std::env::var(RETRY_DELAY_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map_or(RETRY_DELAY, Duration::from_millis)
}

/// 摘要请求复用的提示骨架：指令消息、缓存断点位置和工具定义。
#[derive(Clone, Copy)]
pub(crate) struct PromptSkeleton<'a> {
    pub system: &'a [Message],
    pub static_prefix: &'a [usize],
    pub tools: &'a [ToolDef],
}

/// 一次摘要采样的固定上下文；同一会话的各档输入和每次尝试共用一份。
pub(crate) struct Sampler<'a> {
    inner: &'a CoreInner,
    config: &'a ResolvedConfig,
    session_id: &'a str,
    model: &'a ResolvedModel,
    skeleton: PromptSkeleton<'a>,
}

impl<'a> Sampler<'a> {
    pub(crate) fn new(
        inner: &'a CoreInner,
        config: &'a ResolvedConfig,
        session_id: &'a str,
        model: &'a ResolvedModel,
        skeleton: PromptSkeleton<'a>,
    ) -> Self {
        Self {
            inner,
            config,
            session_id,
            model,
            skeleton,
        }
    }

    pub(crate) fn model_selector(&self) -> &str {
        &self.model.selector
    }

    /// 发一次摘要请求，返回模型未经清洗的原始输出。
    async fn sample(
        &self,
        cancel: &CancellationToken,
        messages: &[Message],
        prompt: &str,
    ) -> Result<String, CoreError> {
        let PromptSkeleton {
            system,
            static_prefix,
            tools,
        } = self.skeleton;
        let mut request_messages = Vec::with_capacity(system.len() + messages.len() + 1);
        request_messages.extend_from_slice(system);
        request_messages.extend_from_slice(messages);
        request_messages.push(Message::user_text(prompt));
        let request = crate::service::prompt::request::build_request(
            self.model,
            self.config,
            request_messages,
            tools.to_vec(),
            None,
            self.session_id,
            CacheAnchors {
                instruction_static: static_prefix,
                // 摘要是本次请求特有的最后一条消息，不能放缓存断点。
                trailing_reminders: 1,
            },
        );
        let _permit = self.inner.provider_permit(self.model, cancel).await?;
        let request_started = Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            result = self.inner.client.complete(&self.model.llm, &request) => match result {
                Ok(response) => response,
                Err(error) => {
                    if let moc_llm::LlmError::Guard(violation) = &error
                        && let Some(guard) =
                            self.inner.runtime(self.session_id).await.guard.lock().clone()
                    {
                        guard.stop(violation.clone());
                    }
                    return Err(error.into());
                }
            },
        };
        let active_ms = u64::try_from(request_started.elapsed().as_millis()).unwrap_or(u64::MAX);
        crate::service::record_model_usage(
            self.inner,
            self.session_id,
            &self.model.selector,
            response.usage,
            active_ms,
        )
        .await?;
        Ok(response.message.text())
    }
}

pub(crate) struct GrokBuildContext<'a> {
    pub inner: &'a CoreInner,
    pub ctx: &'a RequestContext,
    pub state: &'a SessionState,
    pub config: &'a ResolvedConfig,
    pub model: &'a ResolvedModel,
    pub system: &'a [Message],
    /// 指令消息开头的静态块数；摘要请求沿用主循环的缓存断点位置。
    pub static_prefix: &'a [usize],
    pub tools: &'a [ToolDef],
    pub instructions: Option<&'a str>,
    pub budget: CompactionBudget,
}

pub(crate) async fn compact(
    input: GrokBuildContext<'_>,
) -> Result<moc_protocol::methods::CompactResult, CoreError> {
    let GrokBuildContext {
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
    } = input;
    let cancel = ctx.cancel();
    let messages = effective_messages(state, model);
    if messages.is_empty() && state.summary.is_none() {
        return Err(CoreError::ContextOverflow(
            "no history is available to summarize".into(),
        ));
    }
    let prompt = prompt::build_summary_prompt(instructions);
    let tool_tokens = super::tools_tokens(tools);
    let sampler = Sampler {
        inner,
        config,
        session_id: &state.session.id,
        model,
        skeleton: PromptSkeleton {
            system,
            static_prefix,
            tools,
        },
    };
    // 系统提示和工具定义每次都要发送；它们自己就超窗时，换哪一档输入都没用。
    if messages_tokens(system).saturating_add(tool_tokens)
        > fit::fitted_budget(model.context_window, tool_tokens)
    {
        return Err(CoreError::ContextOverflow(
            "Grok Build compaction: system prompt and tools exceed the context window".into(),
        ));
    }
    if let Some((note1, prefix_len)) = inner
        .runtime(&state.session.id)
        .await
        .grok_prefire
        .take_for_pass2(&cancel, model, &messages)
        .await
    {
        let mut pass2 = Vec::with_capacity(messages.len().saturating_sub(prefix_len) + 1);
        pass2.push(Message::user_text(carrier_from_raw(&note1)));
        pass2.extend_from_slice(&messages[prefix_len..]);
        match sampler.sample(&cancel, &pass2, &prompt).await {
            Ok(raw) if !prompt::is_degenerate_summary(&raw) => {
                return finish(
                    &sampler,
                    ctx,
                    state,
                    instructions,
                    prompt::clean_summary(&raw),
                    budget,
                )
                .await;
            }
            Ok(raw) => tracing::debug!(
                session_id = %state.session.id,
                chars = raw.chars().count(),
                "Grok Build 预火摘要输出退化，回退到单遍压缩"
            ),
            Err(error) => tracing::debug!(
                session_id = %state.session.id,
                %error,
                "Grok Build 预火摘要失败，回退到单遍压缩"
            ),
        }
    }
    let mut stage = fit::initial_stage(system, &messages, tool_tokens, model.context_window);
    loop {
        let candidate =
            fit::prepare_stage(stage, messages.clone(), model.context_window, tool_tokens);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match sampler.sample(&cancel, &candidate, &prompt).await {
                Ok(raw) if !prompt::is_degenerate_summary(&raw) => {
                    return finish(
                        &sampler,
                        ctx,
                        state,
                        instructions,
                        prompt::clean_summary(&raw),
                        budget,
                    )
                    .await;
                }
                Ok(raw) => {
                    tracing::warn!(
                        session_id = %state.session.id,
                        chars = raw.chars().count(),
                        attempt,
                        "Grok Build 摘要输出持续退化"
                    );
                    if attempt >= MAX_ATTEMPTS {
                        return Err(CoreError::ContextOverflow(
                            "Grok Build compaction: the summary model kept returning degenerate output"
                                .into(),
                        ));
                    }
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => return Err(CoreError::Cancelled),
                        _ = tokio::time::sleep(retry_delay()) => {}
                    }
                }
                // 上游判超窗是唯一值得换一档输入重试的失败。
                Err(error) if is_context_length(&error) => {
                    tracing::warn!(
                        session_id = %state.session.id,
                        stage = ?stage,
                        %error,
                        "Grok Build 摘要输入超窗"
                    );
                    break;
                }
                Err(error) => return Err(error),
            }
        }
        match stage.next() {
            Some(next) => stage = next,
            None => {
                return Err(CoreError::ContextOverflow(
                    "Grok Build compaction: every input stage exceeded the context window".into(),
                ));
            }
        }
    }
}

/// 提交 checkpoint：full-replace 历史 + 运行时提醒，不写历史归档。
async fn finish(
    sampler: &Sampler<'_>,
    ctx: &RequestContext,
    state: &SessionState,
    instructions: Option<&str>,
    summary: String,
    budget: CompactionBudget,
) -> Result<moc_protocol::methods::CompactResult, CoreError> {
    let reminder = reminder::render(sampler.inner, state).await;
    let mut tokens_after = budget.fixed_tokens;
    if let Some(query) = pending_last_query(state) {
        tokens_after = tokens_after.saturating_add(messages_tokens(std::slice::from_ref(&query)));
    }
    tokens_after =
        tokens_after.saturating_add(messages_tokens(&[Message::user_text(carrier(&summary))]));
    if let Some(reminder) = &reminder {
        tokens_after =
            tokens_after.saturating_add(messages_tokens(&[Message::user_text(reminder.clone())]));
    }
    record_compaction(
        sampler.inner,
        ctx,
        &state.session.id,
        super::Checkpoint {
            method: "grok_build".to_owned(),
            model: Some(sampler.model.selector.clone()),
            summary,
            reminder,
            first_kept_id: None,
            tokens_before: budget.tokens_before,
            tokens_after,
            instructions: instructions.map(str::to_owned),
            native: None,
            archive: HistoryArchive::None,
        },
    )
    .await
}

/// 摘要模型的输入视图。
///
/// 普通情况下与回放完全一致；只有此前用过 `remote` 原生压缩时，回放视图只剩不透明
/// 载荷与保留尾部，而 Grok 需要原始文本才能重写全文，因此展开 `ResetBoundary` 之后的
/// 原始消息。
pub(crate) fn effective_messages(state: &SessionState, model: &ResolvedModel) -> Vec<Message> {
    if state.native_input.is_none() {
        return super::replay::model_messages(state, None, Some(model));
    }
    let start = match state
        .entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::ResetBoundary { .. }))
    {
        Some(index) => state.entries[..=index]
            .iter()
            .filter(|entry| matches!(entry, Entry::Message { .. }))
            .count(),
        None => 0,
    };
    state.transcript[start.min(state.transcript.len())..]
        .iter()
        .filter_map(|entry| entry.context_message())
        .map(|message| super::replay::repair_tool_history(message, Some(model)))
        .collect()
}

/// full-replace 历史里承载摘要的消息正文。
pub(crate) fn carrier(summary: &str) -> String {
    format!("{CONTINUATION_PREAMBLE}\n\n{summary}")
}

/// 第一遍的 NOTE₁ 先清洗再承载，避免把 `<summary>` 标签带进第二遍的输入。
pub(crate) fn carrier_from_raw(raw: &str) -> String {
    carrier(&prompt::clean_summary(raw))
}

/// 回放时要重注入的最后一条真实用户消息（压缩点之前）。
pub(crate) fn last_user_message(state: &SessionState) -> Option<Message> {
    let boundary = state
        .entries
        .iter()
        .rposition(|entry| matches!(entry, Entry::Compaction { .. }))?;
    last_user_message_before(state, boundary)
}

/// 本次压缩提交后会被重注入的最后一条真实用户消息。
pub(crate) fn pending_last_query(state: &SessionState) -> Option<Message> {
    last_user_message_before(state, state.entries.len())
}

fn last_user_message_before(state: &SessionState, boundary: usize) -> Option<Message> {
    state.entries[..boundary]
        .iter()
        .rev()
        .find_map(|entry| match entry {
            Entry::Message {
                message,
                bash,
                agent,
                ..
            } if message.role == Role::User
                && bash.is_none()
                && agent.is_none()
                && !super::replay::is_injected_context(message)
                && !message.text().trim().is_empty() =>
            {
                Some(message.clone())
            }
            _ => None,
        })
}

/// 上游把请求判为超窗时的分类；它是唯一值得换一档输入的失败。
fn is_context_length(error: &CoreError) -> bool {
    match error {
        CoreError::Llm(error) => error
            .api_error()
            .is_some_and(|api| api.kind == moc_llm::ApiErrorKind::ContextLength),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use moc_llm::Message;
    use moc_protocol::methods::{SessionMode, SessionPermission};

    use super::*;
    use crate::session::entries::{EntryPayload, SessionOrigin};

    fn header() -> Entry {
        Entry::header(
            "ses_123456789abc".into(),
            "/tmp".into(),
            "p/m".into(),
            SessionMode::Normal,
            SessionPermission::Full,
            SessionOrigin::root(),
            "标题".into(),
            1,
        )
    }

    fn user_message(state: &mut SessionState, text: &str) -> Entry {
        let entry = Entry::from_payload(
            EntryPayload::Message {
                message: Message::user_text(text),
                tool: None,
                finish_reason: None,
                agent: None,
            },
            state.last_entry_id.clone(),
            2,
        );
        state.apply_entry(entry.clone());
        entry
    }

    fn compaction(state: &mut SessionState, method: &str, summary: &str) -> Entry {
        let entry = Entry::from_payload(
            EntryPayload::Compaction {
                method: method.into(),
                summary: summary.into(),
                first_kept_id: None,
                tokens_before: 1,
                tokens_after: 1,
                instructions: None,
                native: None,
                model: Some("p/m".into()),
                reminder: None,
            },
            state.last_entry_id.clone(),
            3,
        );
        state.apply_entry(entry.clone());
        entry
    }

    #[test]
    fn last_user_message_skips_tool_and_injected_turns() {
        let mut state = SessionState::from_entries(vec![header()]).unwrap();
        user_message(&mut state, "第一句");
        compaction(&mut state, "grok_build", "摘要");
        user_message(&mut state, "第二句");
        assert_eq!(
            pending_last_query(&state).map(|message| message.text()),
            Some("第二句".to_owned()),
            "pending 取整条历史里最新的真实用户消息"
        );
        assert_eq!(
            last_user_message(&state).map(|message| message.text()),
            Some("第一句".to_owned()),
            "回放取压缩点之前的最后一条用户消息"
        );

        compaction(&mut state, "grok_build", "再摘要");
        assert_eq!(
            last_user_message(&state).map(|message| message.text()),
            Some("第二句".to_owned()),
            "新的压缩点把上一条用户消息纳入可重注入范围"
        );
    }

    #[test]
    fn carrier_wraps_summary_with_continuation_preamble() {
        assert_eq!(
            carrier("摘要正文"),
            "This session is being continued from a previous conversation that ran out of context. The summary below covers the earlier portion of the conversation.\n\n摘要正文"
        );
        assert!(carrier_from_raw("<summary>\n正文\n</summary>").ends_with("\n\nSummary:\n正文"));
    }

    #[test]
    fn context_length_classification_only_matches_api_signal() {
        assert!(!is_context_length(&CoreError::Cancelled));
        assert!(!is_context_length(&CoreError::ContextOverflow(
            "本地判定".into()
        )));
        let api = moc_llm::ApiError::new(
            Some(400),
            moc_llm::ApiErrorKind::ContextLength,
            "too long",
            "{}",
        );
        assert!(is_context_length(&CoreError::Llm(moc_llm::LlmError::Api(
            api
        ))));
    }
}

//! 压缩算法共用的同会话采样：缓存前缀、取消、供应商限流与用量记账。

use std::time::Instant;

use moc_llm::{Message, ToolDef};
use tokio_util::sync::CancellationToken;

use crate::ResolvedModel;
use crate::config::ResolvedConfig;
use crate::error::CoreError;
use crate::service::CoreInner;
use crate::service::prompt::request::CacheAnchors;

/// 摘要请求复用的提示骨架：指令消息、缓存断点位置和工具定义。
#[derive(Clone, Copy)]
pub(crate) struct PromptSkeleton<'a> {
    pub system: &'a [Message],
    pub static_prefix: &'a [usize],
    pub tools: &'a [ToolDef],
}

/// 一次摘要采样的固定上下文；同一会话的各档输入和每次尝试共用一份。
pub(crate) struct Sampler<'a> {
    pub(super) inner: &'a CoreInner,
    pub(super) config: &'a ResolvedConfig,
    pub(super) session_id: &'a str,
    pub(super) model: &'a ResolvedModel,
    pub(super) skeleton: PromptSkeleton<'a>,
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
    pub(crate) async fn sample(
        &self,
        cancel: &CancellationToken,
        messages: &[Message],
        prompt: &str,
        max_output_tokens: Option<u32>,
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
        let mut request = crate::service::prompt::request::build_request(
            self.model,
            self.config,
            request_messages,
            tools.to_vec(),
            self.session_id,
            CacheAnchors {
                instruction_static: static_prefix,
                // 摘要是本次请求特有的最后一条消息，不能放缓存断点。
                trailing_reminders: 1,
            },
        );
        if let Some(limit) = max_output_tokens {
            request.max_output_tokens = Some(limit);
        }
        let _permit = self.inner.provider_permit(self.model, cancel).await?;
        // 摘要层统一负责换档和重试，避免供应商重试与三次预算相乘。
        let mut llm = self.model.llm.clone();
        llm.retry.max_attempts = 1;
        let request_started = Instant::now();
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(CoreError::Cancelled),
            result = self.inner.client.complete(&llm, &request) => match result {
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
            self.model.actual_selector(),
            response.usage,
            active_ms,
            // 摘要用非流式 complete，没有可观测的首 token。
            None,
        )
        .await?;
        // 有界交接不能把输出上限截断或工具调用当成完整状态；Grok 原采样语义不变。
        if max_output_tokens.is_some()
            && !matches!(
                response.finish_reason,
                moc_llm::FinishReason::Stop | moc_llm::FinishReason::StopSequence
            )
        {
            return Err(CoreError::ContextOverflow(format!(
                "有界交接未正常结束（{:?}），未替换历史",
                response.finish_reason
            )));
        }
        Ok(response.message.text())
    }
}

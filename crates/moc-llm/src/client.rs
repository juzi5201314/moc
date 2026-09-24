//! HTTP 客户端与重试。
//!
//! 一个 [`Client`] 持有一个连接池复用的 `reqwest::Client`，因此可以对同一个
//! 实例并发发起任意多个请求；[`crate::ChatStream`] 亦可 `tokio::spawn` 到独立任务
//! 中消费。

use std::pin::Pin;
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;

use crate::codec::{self, EncodedRequest};
use crate::config::LlmConfig;
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::stream::ChatStream;
use crate::types::{ChatRequest, ChatResponse};

/// HTTP 客户端。
#[derive(Debug, Clone)]
pub struct Client {
    http: reqwest::Client,
}

impl Client {
    /// 用默认配置构造；内部 `reqwest::Client::new()` 不会失败。
    pub fn new() -> Client {
        Client {
            http: reqwest::Client::new(),
        }
    }

    /// 自定义构造器（`user_agent` / 连接池 / 超时）。
    pub fn builder() -> ClientBuilder {
        ClientBuilder::new()
    }

    /// 非流式请求。
    ///
    /// 可重试错误按 [`crate::RetryPolicy`] 重放，耗尽后返回
    /// [`LlmError::RetriesExhausted`]。
    pub async fn complete(
        &self,
        cfg: &LlmConfig,
        req: &ChatRequest,
    ) -> Result<ChatResponse, LlmError> {
        let encoded = codec::encode(cfg, req, false)?;
        let started = Instant::now();
        let response = self
            .with_retry(cfg, || self.execute(cfg, &encoded, false))
            .await?;
        let body = response.bytes().await?;
        tracing::debug!(
            status = 200,
            elapsed_ms = started.elapsed().as_millis() as u64,
            body_bytes = body.len(),
            "收到完整响应"
        );
        codec::decode_response(cfg, req, &body)
    }

    /// 流式请求。
    ///
    /// 只在**建立连接与首个响应状态**阶段重试；一旦开始读取响应体就不再重试，
    /// 因为中途重放会产生重复计费与重复工具调用，恢复策略必须由调用方基于自身
    /// 历史决定。
    pub async fn stream(&self, cfg: &LlmConfig, req: &ChatRequest) -> Result<ChatStream, LlmError> {
        let encoded = codec::encode(cfg, req, true)?;
        let response = self
            .with_retry(cfg, || self.execute(cfg, &encoded, true))
            .await?;
        let body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
            Box::pin(response.bytes_stream());
        Ok(ChatStream::new(body, cfg, req))
    }

    /// 重试循环：`send` 只在「请求未被上游完整处理」时被重复调用。
    async fn with_retry<T, F, Fut>(&self, cfg: &LlmConfig, mut send: F) -> Result<T, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, LlmError>>,
    {
        let mut attempt = 1;
        loop {
            match send().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    if !error.is_retryable() {
                        return Err(error);
                    }
                    let retry_after = error.api_error().and_then(|api| api.retry_after);
                    let Some(delay) = cfg.retry.delay(attempt, retry_after) else {
                        // 一次都没重放过（`max_attempts == 1`）时直接交还原始错误，
                        // 不要用「重试耗尽」包装一个根本没发生重试的失败。
                        return Err(if attempt > 1 {
                            LlmError::RetriesExhausted {
                                attempts: attempt,
                                last: Box::new(error),
                            }
                        } else {
                            error
                        });
                    };
                    tracing::warn!(
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        error = %error,
                        "请求失败，按退避策略重试"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// 发送一次请求并把非 2xx 响应解码为 [`LlmError::Api`]。
    async fn execute(
        &self,
        cfg: &LlmConfig,
        encoded: &EncodedRequest,
        stream: bool,
    ) -> Result<reqwest::Response, LlmError> {
        let mut builder = self.http.post(&encoded.url).body(encoded.body.clone());
        for (name, value) in &encoded.headers {
            builder = builder.header(name.as_str(), value.as_str());
        }
        if !stream && let Some(timeout) = cfg.timeout_secs {
            // 非流式：`timeout_secs` 是整请求总时长（连接 + 响应体）。
            builder = builder.timeout(Duration::from_secs_f64(timeout));
        }

        tracing::debug!(
            url = %encoded.url,
            protocol = ?cfg.protocol,
            model = %cfg.model,
            stream,
            body_bytes = encoded.body.len(),
            "发送请求"
        );
        let response = match cfg.connect_timeout_secs {
            Some(secs) => {
                // reqwest 只在客户端级别提供 connect timeout，这里把
                // `connect_timeout_secs` 实现为「建立连接并拿到响应头」的时限：
                // 对流式请求正是「上游必须开始响应」的语义。
                tokio::time::timeout(Duration::from_secs_f64(secs), builder.send())
                    .await
                    .map_err(|_| {
                        LlmError::Api(ApiError::new(
                            None,
                            ApiErrorKind::Timeout,
                            format!("上游在 {secs} 秒内没有返回响应头"),
                            "",
                        ))
                    })??
            }
            None => builder.send().await?,
        };
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = response.bytes().await?;
            tracing::warn!(
                status = status.as_u16(),
                body_bytes = body.len(),
                "上游返回错误状态"
            );
            return Err(LlmError::Api(codec::decode_error(
                cfg,
                status.as_u16(),
                &headers,
                &body,
            )));
        }
        tracing::debug!(status = status.as_u16(), "收到响应头");
        Ok(response)
    }
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// [`Client`] 构造器。
#[derive(Debug, Default)]
pub struct ClientBuilder {
    user_agent: Option<String>,
    pool_max_idle_per_host: Option<usize>,
    read_timeout: Option<Duration>,
    connect_timeout: Option<Duration>,
}

impl ClientBuilder {
    /// 空构造器。
    pub fn new() -> Self {
        Self::default()
    }

    /// `User-Agent` 头。
    pub fn user_agent(mut self, user_agent: impl Into<String>) -> Self {
        self.user_agent = Some(user_agent.into());
        self
    }

    /// 每个 host 的最大空闲连接数。
    pub fn pool_max_idle_per_host(mut self, max: usize) -> Self {
        self.pool_max_idle_per_host = Some(max);
        self
    }

    /// 读取超时：两次读到数据之间的最长间隔。
    pub fn read_timeout(mut self, timeout: Duration) -> Self {
        self.read_timeout = Some(timeout);
        self
    }

    /// 连接建立超时。
    pub fn connect_timeout(mut self, timeout: Duration) -> Self {
        self.connect_timeout = Some(timeout);
        self
    }

    /// 构造客户端。
    pub fn build(self) -> Result<Client, LlmError> {
        let mut builder = reqwest::Client::builder();
        if let Some(user_agent) = self.user_agent {
            builder = builder.user_agent(user_agent);
        }
        if let Some(max) = self.pool_max_idle_per_host {
            builder = builder.pool_max_idle_per_host(max);
        }
        if let Some(timeout) = self.read_timeout {
            builder = builder.read_timeout(timeout);
        }
        if let Some(timeout) = self.connect_timeout {
            builder = builder.connect_timeout(timeout);
        }
        let http = builder.build()?;
        Ok(Client { http })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_constructs_a_client() {
        let client = Client::builder()
            .user_agent("moc-llm/0.1")
            .pool_max_idle_per_host(4)
            .read_timeout(Duration::from_secs(30))
            .connect_timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        // `Debug` 输出不含凭据，也不包含请求体。
        let debug = format!("{client:?}");
        assert!(debug.starts_with("Client"), "{debug}");
    }

    #[test]
    fn default_client_uses_the_shared_constructor() {
        let client = Client::default();
        assert_eq!(format!("{client:?}"), format!("{:?}", Client::new()));
    }

    #[test]
    fn client_is_send_sync_and_cloneable() {
        // 契约：core 可以对同一个 Client 并发发起任意多个请求。
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<Client>();
    }

    #[test]
    fn retries_exhausted_reports_attempt_count() {
        let policy = crate::RetryPolicy {
            max_attempts: 2,
            initial_backoff_secs: 0.0,
            jitter: false,
            ..Default::default()
        };
        assert_eq!(policy.delay(1, None), Some(Duration::from_millis(1)));
        assert_eq!(policy.delay(2, None), None);
    }
}

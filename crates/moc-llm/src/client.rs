//! HTTP 客户端与重试。
//!
//! 同一超时策略复用一个 reqwest 连接池；连接时限只约束 DNS/TCP/TLS，
//! 不把模型生成期间等待响应头的时间误算成连接建立时间。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;
use serde_json::Value;

use crate::codec::{self, EncodedRequest};
use crate::config::{LlmConfig, Protocol};
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::stream::ChatStream;
use crate::types::{ChatRequest, ChatResponse, Usage};
/// HTTP 客户端。
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

#[derive(Debug)]
struct ClientInner {
    http: reqwest::Client,
    options: ClientBuilder,
    /// 配置可提供任意时限；按完整 Duration 分组，冷路径建池、热路径点查。
    transports: Mutex<HashMap<Duration, reqwest::Client>>,
}

/// Responses 原生压缩结果。
#[derive(Debug, Clone, PartialEq)]
pub struct CompactResponse {
    /// 服务端压缩后可继续回放的输入 item。
    pub items: Vec<Value>,
    /// 服务端报告的 token 用量。
    pub usage: Usage,
    /// 上游完整响应体。
    pub raw: Option<Box<Value>>,
}

impl Client {
    /// 用默认选项构造共享连接池。
    pub fn new() -> Client {
        ClientBuilder::new()
            .build()
            .expect("默认 HTTP 客户端可构造")
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
        let guard = req.options.stream_guard;
        let body = tokio::time::timeout(
            Duration::from_secs(guard.max_response_duration_secs),
            async {
                let response = self
                    .with_retry(cfg, || self.execute(cfg, &encoded, false))
                    .await?;
                let limit = guard.max_response_bytes.saturating_mul(2);
                bounded_body(
                    response,
                    guard
                        .max_wire_bytes
                        .map_or(limit, |remaining| remaining.min(limit)),
                )
                .await
            },
        )
        .await
        .map_err(|_| crate::GuardViolation::Resource("完整响应超过本地时限".into()))??;
        tracing::debug!(
            status = 200,
            elapsed_ms = started.elapsed().as_millis() as u64,
            body_bytes = body.len(),
            "收到完整响应"
        );
        let response = codec::decode_response(cfg, req, &body)?;
        crate::guard::StreamGuard::new(guard).check_response(&response)?;
        Ok(response)
    }

    /// 调用 OpenAI Responses 原生压缩端点。
    pub async fn compact(
        &self,
        cfg: &LlmConfig,
        items: &[Value],
        guard: crate::StreamGuardConfig,
    ) -> Result<CompactResponse, LlmError> {
        if cfg.protocol != Protocol::OpenAiResponses {
            return Err(LlmError::Encode(
                "原生压缩端点仅支持 OpenAI Responses 协议".into(),
            ));
        }
        let mut body = cfg.extra_body.clone();
        body.insert("model".into(), Value::String(cfg.model.clone()));
        body.insert("input".into(), Value::Array(items.to_vec()));
        let body = serde_json::to_vec(&Value::Object(body))
            .map_err(|error| LlmError::Encode(format!("Responses 压缩请求序列化失败: {error}")))?;
        let request = ChatRequest::default();
        let encoded = EncodedRequest {
            url: cfg.compact_endpoint_url(),
            headers: codec::common_headers(cfg, &request).into_vec(),
            body,
        };
        let body = tokio::time::timeout(
            Duration::from_secs(guard.max_response_duration_secs),
            async {
                let response = self
                    .with_retry(cfg, || self.execute(cfg, &encoded, false))
                    .await?;
                let limit = guard.max_response_bytes.saturating_mul(2);
                bounded_body(
                    response,
                    guard
                        .max_wire_bytes
                        .map_or(limit, |remaining| remaining.min(limit)),
                )
                .await
            },
        )
        .await
        .map_err(|_| crate::GuardViolation::Resource("压缩响应超过本地时限".into()))??;
        let value: Value = serde_json::from_slice(&body).map_err(|error| LlmError::Decode {
            message: format!("Responses 压缩响应不是合法 JSON: {error}"),
            body: Some(codec::truncated(&String::from_utf8_lossy(&body), 512).into_boxed_str()),
        })?;
        let output = value
            .get("output")
            .and_then(Value::as_array)
            .ok_or_else(|| LlmError::Decode {
                message: "Responses 压缩响应缺少 output 数组".into(),
                body: Some(codec::truncated(&String::from_utf8_lossy(&body), 512).into_boxed_str()),
            })?;
        Ok(CompactResponse {
            items: output.clone(),
            usage: value
                .get("usage")
                .map(codec::openai::decode_usage)
                .unwrap_or_default(),
            raw: Some(Box::new(value)),
        })
    }

    /// 流式请求。
    ///
    /// 只在**建立连接与首个响应状态**阶段重试；一旦开始读取响应体就不再重试，
    /// 因为中途重放会产生重复计费与重复工具调用，恢复策略必须由调用方基于自身
    /// 历史决定。
    pub async fn stream(&self, cfg: &LlmConfig, req: &ChatRequest) -> Result<ChatStream, LlmError> {
        let encoded = codec::encode(cfg, req, true)?;
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(req.options.stream_guard.max_response_duration_secs);
        let response = tokio::time::timeout_at(
            deadline,
            self.with_retry(cfg, || self.execute(cfg, &encoded, true)),
        )
        .await
        .map_err(|_| crate::GuardViolation::Resource("模型连接与重试超过本地时限".into()))??;
        let body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
            Box::pin(response.bytes_stream());
        let mut stream = ChatStream::new(body, cfg, req);
        stream.set_deadline(deadline);
        Ok(stream)
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

    fn post(&self, cfg: &LlmConfig, url: &str) -> Result<reqwest::RequestBuilder, LlmError> {
        let Some(seconds) = cfg.connect_timeout_secs else {
            return Ok(self.inner.http.post(url));
        };
        let timeout = Duration::try_from_secs_f64(seconds)
            .map_err(|error| LlmError::Encode(format!("连接超时配置无效：{error}")))?;
        if self.inner.options.connect_timeout == Some(timeout) {
            return Ok(self.inner.http.post(url));
        }
        let mut transports = self.inner.transports.lock().expect("HTTP 连接池表");
        if let Some(http) = transports.get(&timeout) {
            return Ok(http.post(url));
        }
        let http = self
            .inner
            .options
            .clone()
            .connect_timeout(timeout)
            .build_http()?;
        let request = http.post(url);
        transports.insert(timeout, http);
        Ok(request)
    }

    /// 发送一次请求并把非 2xx 响应解码为 [`LlmError::Api`]。
    async fn execute(
        &self,
        cfg: &LlmConfig,
        encoded: &EncodedRequest,
        stream: bool,
    ) -> Result<reqwest::Response, LlmError> {
        let mut builder = self.post(cfg, &encoded.url)?.body(encoded.body.clone());
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
        let response = match cfg.timeout_secs.filter(|_| stream) {
            Some(secs) => {
                // 流式首包等待与后续读取同属空闲时限，不使用连接建立时限。
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

async fn bounded_body(mut response: reqwest::Response, limit: usize) -> Result<Vec<u8>, LlmError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await? {
        if chunk.len() > limit.saturating_sub(body.len()) {
            return Err(
                crate::GuardViolation::Resource(format!("完整响应体超过 {limit} 字节")).into(),
            );
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl Default for Client {
    fn default() -> Self {
        Self::new()
    }
}

/// [`Client`] 构造器。
#[derive(Debug, Clone, Default)]
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
        let http = self.build_http()?;
        Ok(Client {
            inner: Arc::new(ClientInner {
                http,
                options: self,
                transports: Mutex::new(HashMap::new()),
            }),
        })
    }

    fn build_http(&self) -> Result<reqwest::Client, LlmError> {
        let mut builder = reqwest::Client::builder();
        if let Some(user_agent) = &self.user_agent {
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
        Ok(builder.build()?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_is_send_sync_and_cloneable() {
        // 契约：core 可以对同一个 Client 并发发起任意多个请求。
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<Client>();
    }
}

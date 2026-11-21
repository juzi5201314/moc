//! HTTP 客户端与重试。
//!
//! 同一超时策略复用一个 reqwest 连接池；连接时限只约束 DNS/TCP/TLS，
//! 不把模型生成期间等待响应头的时间误算成连接建立时间。

use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use bytes::Bytes;
use futures_core::Stream;
use rust_i18n::t;

use crate::codec::{self, EncodedRequest};
use crate::config::LlmConfig;
use crate::error::{ApiError, ApiErrorKind, LlmError};
use crate::retry::RetryBudget;
use crate::stream::ChatStream;
use crate::traffic::{record_rx, record_tx};
use crate::types::{ChatRequest, ChatResponse};
/// HTTP 客户端。
#[derive(Debug, Clone)]
pub struct Client {
    inner: Arc<ClientInner>,
}

#[derive(Debug)]
struct ClientInner {
    http: OnceLock<Result<reqwest::Client, LlmError>>,
    options: ClientBuilder,
    /// 配置可提供任意时限；按完整 Duration 分组，冷路径建池、热路径点查。
    transports: Mutex<HashMap<Duration, reqwest::Client>>,
    images: crate::images::ImageCache,
    image_workers: Arc<tokio::sync::Semaphore>,
}

impl Client {
    /// 默认连接池在首次请求时创建；空会话不初始化 HTTP/TLS。
    pub fn new() -> Client {
        Self {
            inner: Arc::new(ClientInner {
                http: OnceLock::new(),
                options: ClientBuilder::new(),
                transports: Mutex::new(HashMap::new()),
                images: crate::images::ImageCache::default(),
                image_workers: Arc::new(tokio::sync::Semaphore::new(2)),
            }),
        }
    }

    fn default_http(&self) -> Result<&reqwest::Client, LlmError> {
        self.inner
            .http
            .get_or_init(|| self.inner.options.build_http())
            .as_ref()
            .map_err(Clone::clone)
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
        let prepared = self.prepare(cfg, req, false).await?;
        let encoded = &prepared.encoded;
        let started = Instant::now();
        let guard = req.options.stream_guard;
        let deadline_secs = req.options.connect_deadline_secs;
        tokio::time::timeout(
            Duration::from_secs(deadline_secs),
            self.with_retry(
                cfg,
                &mut RetryBudget::default(),
                || async {
                    let response = self
                        .execute(cfg, encoded, false, prepared.diagnostics)
                        .await?;
                    let headers = response.headers().clone();
                    let body = read_body(response).await?;
                    let response = codec::decode_response(cfg, req, &body)
                        .map_err(|error| response_metadata(error, &headers))?;
                    crate::guard::StreamGuard::new(guard).check_response(&response)?;
                    Ok(response)
                },
                |_, _, _| async {},
            ),
        )
        .await
        .map_err(|_| deadline_error("nonstream", deadline_secs))
        .and_then(|result| result)
        .map_err(|error| diagnose(error, prepared.diagnostics, started))
    }

    /// 流式请求。
    ///
    /// 首个有效内容前的错误自动重试；交付内容后由调用方撤销失败预览再恢复。
    pub async fn stream(&self, cfg: &LlmConfig, req: &ChatRequest) -> Result<ChatStream, LlmError> {
        self.stream_with_budget(cfg, req, &mut RetryBudget::default())
            .await
    }

    /// 与调用方的流中恢复共用预算，防止多层重试相乘。
    pub async fn stream_with_budget(
        &self,
        cfg: &LlmConfig,
        req: &ChatRequest,
        budget: &mut RetryBudget,
    ) -> Result<ChatStream, LlmError> {
        self.stream_with_budget_notified(cfg, req, budget, |_, _, _| async {})
            .await
    }

    /// 每次首个有效内容前退避时通知调用方；参数为下次尝试序号、等待时长与失败原因。
    pub async fn stream_with_budget_notified<F, Fut>(
        &self,
        cfg: &LlmConfig,
        req: &ChatRequest,
        budget: &mut RetryBudget,
        on_retry: F,
    ) -> Result<ChatStream, LlmError>
    where
        F: FnMut(u32, Duration, LlmError) -> Fut,
        Fut: Future<Output = ()>,
    {
        let prepared = self.prepare(cfg, req, true).await?;
        let encoded = &prepared.encoded;
        let started = Instant::now();
        let deadline_secs = req.options.connect_deadline_secs;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(deadline_secs);
        tokio::time::timeout_at(
            deadline,
            self.with_retry(
                cfg,
                budget,
                || async {
                    let response = self
                        .execute(cfg, encoded, true, prepared.diagnostics)
                        .await?;
                    let headers = response.headers().clone();
                    if headers
                        .get(reqwest::header::CONTENT_TYPE)
                        .and_then(|value| value.to_str().ok())
                        .is_some_and(|value| value.contains("application/json"))
                    {
                        let body = read_body(response).await?;
                        codec::decode_response(cfg, req, &body)
                            .map_err(|error| response_metadata(error, &headers))?;
                        return Err(LlmError::Stream(t!("llm.err.stream_json").into_owned()));
                    }
                    let body: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>> =
                        Box::pin(CountingStream::new(response.bytes_stream()));
                    let mut stream = ChatStream::new(body, cfg, req);
                    stream.set_response_headers(&headers);
                    stream.prime().await?;
                    Ok(stream)
                },
                on_retry,
            ),
        )
        .await
        .map_err(|_| deadline_error("stream", deadline_secs))
        .and_then(|result| result)
        .map_err(|error| diagnose(error, prepared.diagnostics, started))
    }

    /// 查看将实际发送的请求；调用方应在阻塞线程中执行图片处理。
    pub fn inspect_request(
        &self,
        cfg: &LlmConfig,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<crate::RequestInspection, LlmError> {
        let prepared = crate::images::prepare(&self.inner.images, cfg, req, stream)?;
        codec::inspect_request(cfg, &prepared.request, stream)
    }

    async fn prepare(
        &self,
        cfg: &LlmConfig,
        req: &ChatRequest,
        stream: bool,
    ) -> Result<crate::images::PreparedRequest, LlmError> {
        let permit = Arc::clone(&self.inner.image_workers)
            .acquire_owned()
            .await
            .map_err(|error| LlmError::Encode(error.to_string()))?;
        let inner = Arc::clone(&self.inner);
        let cfg = cfg.clone();
        let req = req.clone();
        tokio::task::spawn_blocking(move || {
            // 取消网络等待后也由实际压缩任务持有名额，避免遗留后台任务突破并发限制。
            let _permit = permit;
            crate::images::prepare(&inner.images, &cfg, &req, stream)
        })
        .await
        .map_err(|error| {
            LlmError::Encode(t!("llm.err.image_prepare", error = error).into_owned())
        })?
    }

    /// 请求失败共享同一个预算；响应体解码也在重试边界内。
    async fn with_retry<T, F, Fut, R, RetryFuture>(
        &self,
        cfg: &LlmConfig,
        budget: &mut RetryBudget,
        mut send: F,
        mut on_retry: R,
    ) -> Result<T, LlmError>
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = Result<T, LlmError>>,
        R: FnMut(u32, Duration, LlmError) -> RetryFuture,
        RetryFuture: Future<Output = ()>,
    {
        loop {
            budget.begin_attempt();
            match send().await {
                Ok(value) => return Ok(value),
                Err(error) => {
                    let delay = budget.next_delay(&cfg.retry, error.clone())?;
                    tracing::warn!(
                        attempt = budget.attempts(),
                        delay_ms = delay.as_millis() as u64,
                        error = %error,
                        "请求失败，按退避策略重试"
                    );
                    on_retry(budget.attempts() + 1, delay, error).await;
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    pub(crate) fn http_for(&self, cfg: &LlmConfig) -> Result<reqwest::Client, LlmError> {
        let Some(seconds) = cfg.connect_timeout_secs else {
            return Ok(self.default_http()?.clone());
        };
        let timeout = Duration::try_from_secs_f64(seconds).map_err(|error| {
            LlmError::Encode(t!("llm.err.timeout_cfg", error = error).into_owned())
        })?;
        if self.inner.options.connect_timeout == Some(timeout) {
            return Ok(self.default_http()?.clone());
        }
        let mut transports = self.inner.transports.lock().expect("HTTP 连接池表");
        if let Some(http) = transports.get(&timeout) {
            return Ok(http.clone());
        }
        let http = self
            .inner
            .options
            .clone()
            .connect_timeout(timeout)
            .build_http()?;
        transports.insert(timeout, http.clone());
        Ok(http)
    }

    fn post(&self, cfg: &LlmConfig, url: &str) -> Result<reqwest::RequestBuilder, LlmError> {
        Ok(self.http_for(cfg)?.post(url))
    }

    /// 发送一次请求并把非 2xx 响应解码为 [`LlmError::Api`]。
    async fn execute(
        &self,
        cfg: &LlmConfig,
        encoded: &EncodedRequest,
        stream: bool,
        mut diagnostics: crate::RequestDiagnostics,
    ) -> Result<reqwest::Response, LlmError> {
        let started = Instant::now();
        let mut builder = self.post(cfg, &encoded.url)?.body(encoded.body.clone());
        record_tx(encoded.body.len() as u64);
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
        if diagnostics.images > 0 {
            tracing::info!(diagnostics = %diagnostics, "图片请求已预处理");
        }
        let response = match cfg.timeout_secs.filter(|_| stream) {
            Some(secs) => {
                // 流式首包等待与后续读取同属空闲时限，不使用连接建立时限。
                tokio::time::timeout(Duration::from_secs_f64(secs), builder.send())
                    .await
                    .map_err(|_| {
                        LlmError::Api(Box::new(ApiError::new(
                            None,
                            ApiErrorKind::Timeout,
                            t!("llm.err.no_headers", secs = secs).into_owned(),
                            "",
                        )))
                    })??
            }
            None => builder.send().await?,
        };
        let status = response.status();
        if !status.is_success() {
            let headers = response.headers().clone();
            let body = error_body(response).await;
            tracing::warn!(
                status = status.as_u16(),
                body_bytes = body.len(),
                "上游返回错误状态"
            );
            diagnostics.elapsed_ms = started.elapsed().as_millis() as u64;
            let mut error = codec::decode_error(cfg, status.as_u16(), &headers, &body);
            error.request_diagnostics = Some(diagnostics);
            return Err(LlmError::Api(Box::new(error)));
        }
        tracing::debug!(status = status.as_u16(), "收到响应头");
        Ok(response)
    }
}

fn response_metadata(mut error: LlmError, headers: &reqwest::header::HeaderMap) -> LlmError {
    if let LlmError::Api(api) = &mut error {
        codec::error_metadata(api, headers);
    }
    error
}

fn diagnose(
    error: LlmError,
    mut diagnostics: crate::RequestDiagnostics,
    started: Instant,
) -> LlmError {
    diagnostics.elapsed_ms = started.elapsed().as_millis() as u64;
    error.with_request_diagnostics(diagnostics)
}

/// 非 2xx 响应体的读取上限。
///
/// 错误体只用于分类与排障，不进会话上下文，因此单独设界：
/// 异常上游或代理可以用错误响应把整份响应体塞进来，读到底只会白占内存。
const MAX_ERROR_BODY_BYTES: usize = 1024 * 1024;

/// 非流式成功响应体的读取上限。
const MAX_RESPONSE_BODY_BYTES: usize = 64 * 1024 * 1024;

/// 读取非 2xx 响应体，最多 [`MAX_ERROR_BODY_BYTES`] 字节。
///
/// 达到上限即停止读取（状态码与已读部分足以分类），读取中途失败也按已读内容
/// 继续：真正重要的是不要丢掉带状态码的 [`crate::ApiError`]。
async fn error_body(mut response: reqwest::Response) -> Vec<u8> {
    let mut body = Vec::new();
    loop {
        let room = MAX_ERROR_BODY_BYTES.saturating_sub(body.len());
        if room == 0 {
            tracing::warn!(limit = MAX_ERROR_BODY_BYTES, "错误响应体达到上限，停止读取");
            break;
        }
        match response.chunk().await {
            Ok(Some(chunk)) => {
                record_rx(chunk.len() as u64);
                if chunk.len() > room {
                    body.extend_from_slice(&chunk[..room]);
                    tracing::warn!(limit = MAX_ERROR_BODY_BYTES, "错误响应体超过上限，已截断");
                    break;
                }
                body.extend_from_slice(&chunk);
            }
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(error = %error, "读取错误响应体失败，按已读内容继续");
                break;
            }
        }
    }
    body
}

/// 连接与重试时限到点：请求未完成，恢复策略交给调用方。
fn deadline_error(slug: &str, secs: u64) -> LlmError {
    LlmError::Api(Box::new(ApiError::new(
        None,
        ApiErrorKind::Timeout,
        t!(format!("llm.err.{slug}_deadline"), secs = secs).into_owned(),
        "",
    )))
}

/// 读取完整的 2xx 响应体。
async fn read_body(response: reqwest::Response) -> Result<Vec<u8>, LlmError> {
    read_body_with_limit(response, MAX_RESPONSE_BODY_BYTES).await
}

async fn read_body_with_limit(
    mut response: reqwest::Response,
    limit: usize,
) -> Result<Vec<u8>, LlmError> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|error| {
        LlmError::Stream(t!("llm.err.read_body_failed", err = error).into_owned())
    })? {
        record_rx(chunk.len() as u64);
        let room = limit.saturating_sub(body.len());
        if chunk.len() > room {
            return Err(LlmError::Stream(format!("成功响应体超过 {limit} 字节上限")));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// `bytes_stream` 的计数包装：每交付一个 chunk 计入进程级 RX 计数器。
struct CountingStream {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Bytes>> + Send>>,
}

impl CountingStream {
    fn new(inner: impl Stream<Item = reqwest::Result<Bytes>> + Send + 'static) -> Self {
        Self {
            inner: Box::pin(inner),
        }
    }
}

impl Stream for CountingStream {
    type Item = reqwest::Result<Bytes>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        let poll = this.inner.as_mut().poll_next(cx);
        if let Poll::Ready(Some(Ok(bytes))) = &poll {
            record_rx(bytes.len() as u64);
        }
        poll
    }
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
                http: OnceLock::from(Ok(http)),
                options: self,
                transports: Mutex::new(HashMap::new()),
                images: crate::images::ImageCache::default(),
                image_workers: Arc::new(tokio::sync::Semaphore::new(2)),
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
    use wiremock::matchers::method;
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn response_body_over_limit_returns_stream_error() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(b"12345".to_vec()))
            .mount(&server)
            .await;
        let response = reqwest::get(server.uri()).await.unwrap();

        let error = read_body_with_limit(response, 4).await.unwrap_err();
        assert!(matches!(error, LlmError::Stream(message) if message.contains("超过 4 字节上限")));
    }

    #[test]
    fn default_transport_is_lazy_and_shared() {
        let client = Client::new();
        let clone = client.clone();
        assert!(client.inner.http.get().is_none());
        let config = LlmConfig::default();
        drop(client.post(&config, "http://127.0.0.1/").unwrap());
        assert!(client.inner.http.get().is_some());
        assert!(Arc::ptr_eq(&client.inner, &clone.inner));
        assert!(clone.inner.http.get().is_some());
    }

    #[test]
    fn explicit_builder_still_validates_immediately() {
        assert!(
            Client::builder()
                .user_agent("invalid\nheader")
                .build()
                .is_err()
        );
        assert!(
            Client::builder()
                .build()
                .unwrap()
                .inner
                .http
                .get()
                .is_some()
        );
    }

    #[test]
    fn client_is_send_sync_and_cloneable() {
        // 契约：core 可以对同一个 Client 并发发起任意多个请求。
        fn assert_send_sync<T: Send + Sync + Clone>() {}
        assert_send_sync::<Client>();
    }

    /// 精确计数：`CountingStream` 只计成功交付的 chunk 字节，错误项不计。
    #[test]
    fn counting_stream_accumulates_delivered_bytes() {
        use crate::traffic::HTTP_RX_BYTES;
        use std::collections::VecDeque;
        use std::sync::atomic::Ordering;

        struct Chunks(VecDeque<reqwest::Result<Bytes>>);
        impl Stream for Chunks {
            type Item = reqwest::Result<Bytes>;
            fn poll_next(
                mut self: Pin<&mut Self>,
                _: &mut Context<'_>,
            ) -> Poll<Option<Self::Item>> {
                Poll::Ready(self.0.pop_front())
            }
        }

        let waker = std::task::Waker::noop();
        let mut cx = Context::from_waker(waker);
        let before = HTTP_RX_BYTES.load(Ordering::Relaxed);
        let mut stream = CountingStream::new(Chunks(VecDeque::from([
            Ok(Bytes::from_static(b"abc")),
            Ok(Bytes::from_static(b"defg")),
        ])));
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(Some(Ok(_)))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(Some(Ok(_)))
        ));
        assert!(matches!(
            Pin::new(&mut stream).poll_next(&mut cx),
            Poll::Ready(None)
        ));
        // 本测试二进制内没有别的 HTTP 流量，差值可以精确断言。
        assert_eq!(HTTP_RX_BYTES.load(Ordering::Relaxed) - before, 7);
    }
}

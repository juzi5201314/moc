//! Tavily `/search` 的 HTTP 实现。
//!
//! 请求体遵循省 token 实践：恒定 `search_depth=basic`、`include_answer=advanced`、
//! 不请求 `include_raw_content`（正文由本地 web_fetch 抓取）。

use std::time::Duration;

use futures_util::future::BoxFuture;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::{
    SearchRequest, SearchResponse, SearchResult, TimeRange, WebSearchError, WebSearchProvider,
    client,
};

const DEFAULT_BASE_URL: &str = "https://api.tavily.com";

pub(super) struct TavilyProvider {
    base_url: String,
    api_key: String,
    timeout: Duration,
}

impl TavilyProvider {
    pub(super) fn new(base_url: &str, api_key: String, timeout: Duration) -> Self {
        let base_url = base_url.trim().trim_end_matches('/').to_owned();
        let base_url = if base_url.is_empty() {
            DEFAULT_BASE_URL.to_owned()
        } else {
            base_url
        };
        Self {
            base_url,
            api_key,
            timeout,
        }
    }

    /// 发送 JSON 请求并解析响应；统一状态码映射与取消处理。
    async fn post<T: for<'de> Deserialize<'de>>(
        &self,
        path: &str,
        body: &(impl Serialize + Sync),
        cancel: &CancellationToken,
    ) -> Result<T, WebSearchError> {
        let request = client()
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(&self.api_key)
            .timeout(self.timeout)
            .json(body);
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => {
                return Err(WebSearchError::new("E_CANCELLED", "web request cancelled"));
            }
            response = request.send() => response.map_err(|error| {
                if error.is_timeout() {
                    WebSearchError::new(
                        "E_TIMEOUT",
                        format!("web request timed out after {}s", self.timeout.as_secs()),
                    )
                } else {
                    WebSearchError::new("E_WEB_HTTP", format!("web request failed: {error}"))
                }
            })?,
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let text = response.text().await.map_err(|error| {
            WebSearchError::new("E_WEB_HTTP", format!("reading response failed: {error}"))
        })?;
        if !status.is_success() {
            return Err(error_from_status(status, &text, retry_after.as_deref()));
        }
        serde_json::from_str(&text).map_err(|error| {
            WebSearchError::new(
                "E_WEB_HTTP",
                format!("invalid response JSON from provider: {error}"),
            )
        })
    }
}

impl WebSearchProvider for TavilyProvider {
    fn search<'a>(
        &'a self,
        request: &'a SearchRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<SearchResponse, WebSearchError>> {
        Box::pin(async move {
            // time_range 与 start_date/end_date 同时存在会过度限制，只发送其一。
            let time_range = if request.start_date.is_none() && request.end_date.is_none() {
                request.time_range.map(TimeRange::as_str)
            } else {
                None
            };
            let body = SearchBody {
                query: &request.query,
                search_depth: "basic",
                include_answer: "advanced",
                include_raw_content: false,
                max_results: request.max_results,
                include_domains: &request.include_domains,
                exclude_domains: &request.exclude_domains,
                start_date: request.start_date.as_deref(),
                end_date: request.end_date.as_deref(),
                time_range,
            };
            let envelope: SearchEnvelope = self.post("/search", &body, cancel).await?;
            Ok(normalize_search(envelope, request.max_results))
        })
    }
}

#[derive(Serialize)]
struct SearchBody<'a> {
    query: &'a str,
    search_depth: &'static str,
    include_answer: &'static str,
    include_raw_content: bool,
    max_results: u32,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    include_domains: &'a Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    exclude_domains: &'a Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_date: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_date: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    time_range: Option<&'a str>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SearchEnvelope {
    answer: Option<String>,
    results: Vec<SearchItem>,
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct SearchItem {
    title: String,
    url: String,
    content: Option<String>,
    published_date: Option<String>,
}

/// 归一化搜索响应：丢弃无 URL 项、标题回退 URL、空 answer/snippet 转 None、截断到请求上限。
fn normalize_search(envelope: SearchEnvelope, max_results: u32) -> SearchResponse {
    let answer = envelope
        .answer
        .map(|answer| answer.trim().to_owned())
        .filter(|answer| !answer.is_empty());
    let mut results: Vec<SearchResult> = envelope
        .results
        .into_iter()
        .filter_map(|item| {
            let url = item.url.trim();
            if url.is_empty() {
                return None;
            }
            let title = item.title.trim();
            Some(SearchResult {
                title: if title.is_empty() {
                    url.to_owned()
                } else {
                    title.to_owned()
                },
                url: url.to_owned(),
                snippet: item
                    .content
                    .map(|content| content.trim().to_owned())
                    .filter(|content| !content.is_empty()),
                published_date: item
                    .published_date
                    .map(|date| date.trim().to_owned())
                    .filter(|date| !date.is_empty()),
            })
        })
        .collect();
    // u32 在 16 位目标上可能超出 usize；此时上限视为 usize::MAX。
    results.truncate(usize::try_from(max_results).unwrap_or(usize::MAX));
    SearchResponse { answer, results }
}

/// 非 2xx 响应的统一映射：鉴权、限流与其它 HTTP 错误。
fn error_from_status(
    status: reqwest::StatusCode,
    body: &str,
    retry_after: Option<&str>,
) -> WebSearchError {
    let message = extract_error_message(body).unwrap_or_else(|| {
        let text = body.trim();
        if text.is_empty() {
            status
                .canonical_reason()
                .unwrap_or("request failed")
                .to_owned()
        } else {
            text.to_owned()
        }
    });
    match status.as_u16() {
        401 | 403 => WebSearchError::new(
            "E_WEB_AUTH",
            format!(
                "provider rejected credentials ({}): {message}",
                status.as_u16()
            ),
        ),
        429 | 432 | 433 => {
            let error = WebSearchError::new(
                "E_WEB_RATE_LIMIT",
                format!("provider rate limited ({}): {message}", status.as_u16()),
            );
            match retry_after {
                Some(value) => error.with_hint(format!("Retry-After: {value}")),
                None => error,
            }
        }
        code => WebSearchError::new(
            "E_WEB_HTTP",
            format!("provider returned HTTP {code}: {message}"),
        ),
    }
}

/// 提取供应商错误文案：`detail.error` → `detail` → `error`/`message`。
fn extract_error_message(body: &str) -> Option<String> {
    let value: serde_json::Value = serde_json::from_str(body).ok()?;
    if let Some(error) = value
        .get("detail")
        .and_then(|detail| detail.get("error"))
        .and_then(serde_json::Value::as_str)
    {
        return Some(error.to_owned());
    }
    if let Some(detail) = value.get("detail") {
        match detail {
            serde_json::Value::Null => {}
            serde_json::Value::String(text) => return Some(text.clone()),
            other => return Some(other.to_string()),
        }
    }
    for key in ["error", "message"] {
        if let Some(text) = value.get(key).and_then(serde_json::Value::as_str) {
            return Some(text.to_owned());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use parking_lot::Mutex;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::websearch::search_with_fallback;

    /// 官方 `/search` 响应形状：包含空 title / 空 url / 空 content / published_date 边界项。
    const SEARCH_FIXTURE: &str = r#"{
        "query": "rust 2024",
        "answer": "  Rust 2024 is stable.  ",
        "follow_up_questions": null,
        "images": [],
        "request_id": "req-1",
        "response_time": 0.5,
        "results": [
            {"title": "Rust", "url": "https://www.rust-lang.org/", "content": "  Official site.  ", "score": 0.9, "raw_content": null, "id": 1},
            {"title": "", "url": "https://example.com/empty-title", "content": ""},
            {"title": "No URL", "url": "", "content": "dropped"},
            {"title": "Dated", "url": "https://example.com/dated", "content": "dated snippet", "published_date": "2025-08-01"}
        ],
        "usage": {"credits": 1}
    }"#;

    fn provider(server: &MockServer, timeout_secs: u64) -> TavilyProvider {
        TavilyProvider::new(
            &server.uri(),
            "test-key".into(),
            Duration::from_secs(timeout_secs),
        )
    }

    fn search_request() -> SearchRequest {
        SearchRequest {
            query: "rust async".into(),
            max_results: 5,
            include_domains: Vec::new(),
            exclude_domains: Vec::new(),
            start_date: None,
            end_date: None,
            time_range: None,
        }
    }

    #[tokio::test]
    async fn search_sends_expected_body_and_normalizes_response() {
        let server = MockServer::builder().start().await;
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        Mock::given(method("POST"))
            .and(path("/search"))
            .and(header("authorization", "Bearer test-key"))
            .respond_with(move |request: &Request| {
                sink.lock()
                    .push(serde_json::from_slice(&request.body).unwrap());
                ResponseTemplate::new(200).set_body_raw(SEARCH_FIXTURE, "application/json")
            })
            .mount(&server)
            .await;
        let mut request = search_request();
        request.max_results = 3;
        request.include_domains = vec!["example.com".into()];
        let response = provider(&server, 30)
            .search(&request, &CancellationToken::new())
            .await
            .unwrap();
        assert_eq!(response.answer.as_deref(), Some("Rust 2024 is stable."));
        assert_eq!(response.results.len(), 3);
        assert_eq!(response.results[0].title, "Rust");
        assert_eq!(
            response.results[0].snippet.as_deref(),
            Some("Official site.")
        );
        assert_eq!(
            response.results[1].title, "https://example.com/empty-title",
            "空标题回退 URL"
        );
        assert!(response.results[1].snippet.is_none());
        assert_eq!(
            response.results[2].published_date.as_deref(),
            Some("2025-08-01")
        );
        let bodies = captured.lock();
        let body = &bodies[0];
        assert_eq!(body["search_depth"], "basic");
        assert_eq!(body["include_answer"], "advanced");
        assert_eq!(body["include_raw_content"], false);
        assert_eq!(body["max_results"], 3);
        assert_eq!(body["include_domains"], serde_json::json!(["example.com"]));
        assert!(body.get("exclude_domains").is_none());
        assert!(body.get("time_range").is_none());
        assert!(body.get("start_date").is_none());
    }

    #[tokio::test]
    async fn search_sends_time_range_only_without_dates() {
        let server = MockServer::builder().start().await;
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(move |request: &Request| {
                sink.lock()
                    .push(serde_json::from_slice(&request.body).unwrap());
                ResponseTemplate::new(200).set_body_raw(SEARCH_FIXTURE, "application/json")
            })
            .mount(&server)
            .await;
        let provider = provider(&server, 30);
        let mut request = search_request();
        request.time_range = Some(TimeRange::Week);
        provider
            .search(&request, &CancellationToken::new())
            .await
            .unwrap();
        request.start_date = Some("2025-01-02".into());
        provider
            .search(&request, &CancellationToken::new())
            .await
            .unwrap();
        let bodies = captured.lock();
        assert_eq!(bodies[0]["time_range"], "week");
        assert!(
            bodies[1].get("time_range").is_none(),
            "start_date 存在时不发 time_range"
        );
        assert_eq!(bodies[1]["start_date"], "2025-01-02");
    }

    #[tokio::test]
    async fn auth_failure_maps_to_web_auth() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(401).set_body_raw(
                r#"{"detail": {"error": "Unauthorized: missing API key"}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
        let error = provider(&server, 5)
            .search(&search_request(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "E_WEB_AUTH");
        assert!(error.message.contains("Unauthorized: missing API key"));
        assert!(error.hint.is_none());
    }

    #[tokio::test]
    async fn rate_limit_carries_retry_after_hint() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(
                ResponseTemplate::new(432)
                    .insert_header("retry-after", "12")
                    .set_body_raw(
                        r#"{"detail":{"error":"Plan limit reached"}}"#,
                        "application/json",
                    ),
            )
            .mount(&server)
            .await;
        let error = provider(&server, 5)
            .search(&search_request(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "E_WEB_RATE_LIMIT");
        assert_eq!(error.hint.as_deref(), Some("Retry-After: 12"));
    }

    #[tokio::test]
    async fn server_error_maps_to_web_http() {
        let server = MockServer::builder().start().await;
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(
                ResponseTemplate::new(500)
                    .set_body_raw(r#"{"detail": "internal error"}"#, "application/json"),
            )
            .mount(&server)
            .await;
        let error = provider(&server, 5)
            .search(&search_request(), &CancellationToken::new())
            .await
            .unwrap_err();
        assert_eq!(error.code, "E_WEB_HTTP");
        assert!(error.message.contains("500"));
        assert!(error.message.contains("internal error"));
    }

    #[tokio::test]
    async fn cancelled_request_maps_to_cancelled() {
        let server = MockServer::builder().start().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = provider(&server, 5)
            .search(&search_request(), &cancel)
            .await
            .unwrap_err();
        assert_eq!(error.code, "E_CANCELLED");
    }

    #[tokio::test]
    async fn zero_result_search_with_time_filter_retries_once() {
        let server = MockServer::builder().start().await;
        let captured: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&captured);
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(move |request: &Request| {
                sink.lock()
                    .push(serde_json::from_slice(&request.body).unwrap());
                if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                    ResponseTemplate::new(200)
                        .set_body_raw(r#"{"results": [], "answer": null}"#, "application/json")
                } else {
                    ResponseTemplate::new(200).set_body_raw(SEARCH_FIXTURE, "application/json")
                }
            })
            .mount(&server)
            .await;
        let mut request = search_request();
        request.time_range = Some(TimeRange::Day);
        request.start_date = Some("2025-01-01".into());
        let response = search_with_fallback(
            &provider(&server, 5),
            &mut request,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(!response.results.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 2, "应重试一次");
        assert!(request.time_range.is_none());
        assert!(request.start_date.is_none());
        let bodies = captured.lock();
        assert_eq!(bodies[0]["start_date"], "2025-01-01");
        assert!(
            bodies[0].get("time_range").is_none(),
            "start_date 存在时不发 time_range"
        );
        assert!(
            bodies[1].get("time_range").is_none(),
            "重试请求不带时间过滤"
        );
        assert!(
            bodies[1].get("start_date").is_none(),
            "重试请求不带时间过滤"
        );
    }

    #[tokio::test]
    async fn zero_result_without_time_filter_does_not_retry() {
        let server = MockServer::builder().start().await;
        let calls = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&calls);
        Mock::given(method("POST"))
            .and(path("/search"))
            .respond_with(move |_: &Request| {
                counter.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_body_raw(r#"{"results": [], "answer": null}"#, "application/json")
            })
            .mount(&server)
            .await;
        let mut request = search_request();
        let response = search_with_fallback(
            &provider(&server, 5),
            &mut request,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(response.results.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

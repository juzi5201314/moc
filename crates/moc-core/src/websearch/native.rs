//! 本地抓取：通用 HTML→Markdown、字符集解码、相关块选择与批调度。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use reqwest::Url;
use reqwest::header::{ACCEPT, HeaderMap, HeaderValue};
use tokio_util::sync::CancellationToken;

use super::http::{
    BatchBudget, HttpFetcher, MAX_BATCH_BYTES, WebFetchFailure, content_type_essence, decode_body,
};
use super::sites::{self, SiteEndpoints};
use super::{ExtractFailed, ExtractRequest, ExtractResponse, ExtractResult, WebSearchError};
use crate::config::ResolvedWebFetch;

const FETCH_CONCURRENCY: usize = 4;
const CHUNKS_PER_SOURCE: usize = 3;
const CHUNK_CHARS: usize = 800;
const MAX_URLS: usize = 20;

pub(super) struct NativeFetcher {
    timeout: Duration,
    max_bytes: usize,
    max_chars: usize,
    endpoints: SiteEndpoints,
    cache_dir: PathBuf,
}

impl NativeFetcher {
    pub(super) fn new(config: &ResolvedWebFetch, cache_dir: &std::path::Path) -> Self {
        Self {
            timeout: Duration::from_secs(config.timeout_secs),
            max_bytes: config.max_bytes,
            max_chars: config.max_chars,
            endpoints: SiteEndpoints::from_env(),
            cache_dir: cache_dir.to_path_buf(),
        }
    }

    #[cfg(test)]
    pub(super) fn with_endpoints(
        config: &ResolvedWebFetch,
        endpoints: SiteEndpoints,
        cache_dir: &std::path::Path,
    ) -> Self {
        Self {
            timeout: Duration::from_secs(config.timeout_secs),
            max_bytes: config.max_bytes,
            max_chars: config.max_chars,
            endpoints,
            cache_dir: cache_dir.to_path_buf(),
        }
    }

    pub(super) async fn extract(
        &self,
        request: &ExtractRequest,
        cancel: &CancellationToken,
    ) -> Result<ExtractResponse, WebSearchError> {
        let query = request.query.as_deref();
        let batch = Arc::new(BatchBudget::new(MAX_BATCH_BYTES));
        let mut tasks = Vec::with_capacity(request.urls.len().min(MAX_URLS));
        for (index, url) in request.urls.iter().take(MAX_URLS).enumerate() {
            let batch = Arc::clone(&batch);
            let endpoints = self.endpoints.clone();
            tasks.push(async move {
                let fetcher = HttpFetcher {
                    timeout: self.timeout,
                    max_bytes: self.max_bytes,
                    batch: &batch,
                    cancel,
                };
                let outcome = self.fetch_one(url, query, &fetcher, &endpoints).await;
                (index, url.as_str(), outcome)
            });
        }

        let mut collected = stream::iter(tasks)
            .buffer_unordered(FETCH_CONCURRENCY)
            .collect::<Vec<_>>()
            .await;
        collected.sort_by_key(|(index, _, _)| *index);

        let mut results = Vec::new();
        let mut failed = Vec::new();
        for (_, url, outcome) in collected {
            match outcome {
                Ok(result) => results.push(result),
                Err(WebFetchFailure::Cancelled) => {
                    return Err(WebSearchError::new("E_CANCELLED", "web fetch cancelled"));
                }
                Err(WebFetchFailure::Message(error)) => failed.push(ExtractFailed {
                    url: url.to_owned(),
                    error,
                }),
            }
        }
        Ok(ExtractResponse { results, failed })
    }

    async fn fetch_one(
        &self,
        raw_url: &str,
        query: Option<&str>,
        fetcher: &HttpFetcher<'_>,
        endpoints: &SiteEndpoints,
    ) -> Result<ExtractResult, WebFetchFailure> {
        let url = Url::parse(raw_url)
            .map_err(|error| WebFetchFailure::message(format!("invalid URL: {error}")))?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(WebFetchFailure::message(format!(
                "unsupported scheme: {}",
                url.scheme()
            )));
        }
        if !url.username().is_empty() || url.password().is_some() {
            return Err(WebFetchFailure::message(
                "URLs with embedded credentials are not allowed",
            ));
        }

        if let Some(result) = sites::try_extract(&url, fetcher, endpoints, &self.cache_dir).await {
            match result {
                Ok(result) => {
                    let result = post_process(result, query, self.max_chars);
                    if !result.raw_content.is_empty() {
                        return Ok(result);
                    }
                }
                Err(WebFetchFailure::Cancelled) => return Err(WebFetchFailure::Cancelled),
                Err(WebFetchFailure::Message(_)) => {}
            }
        }

        let mut headers = HeaderMap::new();
        headers.insert(
            ACCEPT,
            HeaderValue::from_static(
                "text/html,application/xhtml+xml,text/plain;q=0.9,application/json;q=0.8,*/*;q=0.5",
            ),
        );
        let response = fetcher.get(url.as_str(), &headers).await?;
        if !(200..300).contains(&response.status) {
            return Err(WebFetchFailure::message(format!(
                "HTTP {}",
                response.status
            )));
        }

        let essence = content_type_essence(response.content_type.as_deref());
        let decoded = decode_body(&response.bytes, response.content_type.as_deref());
        let mut notes = Vec::new();
        if response.truncated {
            notes.push(format!("body truncated at {} bytes", self.max_bytes));
        }

        let text = match essence.as_deref() {
            Some(kind) if kind == "text/html" || kind == "application/xhtml+xml" => {
                html_to_markdown(&decoded).map_err(WebFetchFailure::message)?
            }
            Some(kind) if is_text_type(kind) => decoded,
            Some(kind) => {
                return Err(WebFetchFailure::message(format!(
                    "unsupported content type: {kind}"
                )));
            }
            None => {
                if looks_binary(&response.bytes) {
                    return Err(WebFetchFailure::message("binary content"));
                }
                if decoded.trim_start().starts_with('<') {
                    html_to_markdown(&decoded).map_err(WebFetchFailure::message)?
                } else {
                    decoded
                }
            }
        };

        let result = post_process(
            ExtractResult {
                url: raw_url.to_owned(),
                final_url: response.final_url,
                status: Some(response.status),
                content_type: response.content_type,
                method: "generic",
                notes,
                truncated: response.truncated,
                raw_content: text,
            },
            query,
            self.max_chars,
        );
        if result.raw_content.is_empty() {
            return Err(WebFetchFailure::message("empty body"));
        }
        Ok(result)
    }
}

fn post_process(mut result: ExtractResult, query: Option<&str>, max_chars: usize) -> ExtractResult {
    if let Some(query) = query {
        result.raw_content = select_relevant_chunks(&result.raw_content, query);
    }
    if cap_chars(&mut result.raw_content, max_chars) {
        result.truncated = true;
        result
            .notes
            .push(format!("truncated after {max_chars} characters"));
    }
    result.raw_content = result.raw_content.trim().to_owned();
    result
}

fn is_text_type(kind: &str) -> bool {
    kind.starts_with("text/")
        || matches!(
            kind,
            "application/json"
                | "application/xml"
                | "application/yaml"
                | "application/x-yaml"
                | "application/javascript"
                | "application/x-javascript"
                | "application/toml"
        )
}

fn looks_binary(bytes: &[u8]) -> bool {
    bytes.iter().take(4096).any(|byte| *byte == 0)
}

pub(super) fn html_to_markdown(html: &str) -> Result<String, String> {
    let converter = htmd::HtmlToMarkdown::builder()
        .skip_tags(vec!["script", "style", "noscript", "svg", "template"])
        .build();
    let title = extract_title(html);
    let body = converter
        .convert(&strip_title_tag(html))
        .map_err(|error| format!("html conversion failed: {error}"))?;
    let Some(title) = title else {
        return Ok(body);
    };
    Ok(format!("# {title}\n\n{body}"))
}

/// 去掉 `<title>` 标签，避免 htmd 把页标题当正文再输出一遍。
fn strip_title_tag(html: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let Some(open) = lower.find("<title") else {
        return html.to_owned();
    };
    let Some(open_end) = html[open..].find('>').map(|index| open + index + 1) else {
        return html.to_owned();
    };
    let Some(close_end) = lower[open_end..]
        .find("</title>")
        .map(|index| open_end + index + "</title>".len())
    else {
        return html.to_owned();
    };
    let mut cleaned = String::with_capacity(html.len());
    cleaned.push_str(&html[..open]);
    cleaned.push_str(&html[close_end..]);
    cleaned
}

/// 提取 `<title>` 文本；不处理嵌套标签，标题里只做空白折叠。
fn extract_title(html: &str) -> Option<String> {
    let lower = html.to_ascii_lowercase();
    let open = lower.find("<title")?;
    let open_end = html[open..].find('>')? + open;
    let close = lower[open_end..].find("</title>")? + open_end;
    let title = html[open_end + 1..close]
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    (!title.is_empty()).then(|| title.chars().take(200).collect())
}

/// 按空行切块并压到约 `CHUNK_CHARS` 字符；query 命中数最高的前 3 块按原文顺序拼接。
pub(super) fn select_relevant_chunks(text: &str, query: &str) -> String {
    let terms: Vec<String> = query
        .split_whitespace()
        .map(str::to_lowercase)
        .filter(|term| !term.is_empty())
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    if terms.is_empty() || text.chars().count() <= CHUNK_CHARS {
        return text.to_owned();
    }

    let blocks = split_blocks(text, CHUNK_CHARS);
    if blocks.len() <= CHUNKS_PER_SOURCE {
        return text.to_owned();
    }

    let mut scored: Vec<(usize, usize)> = blocks
        .iter()
        .enumerate()
        .map(|(index, block)| {
            let lower = block.to_lowercase();
            let score = terms
                .iter()
                .filter(|term| lower.contains(term.as_str()))
                .count();
            (index, score)
        })
        .collect();
    scored.sort_by_key(|(index, score)| (std::cmp::Reverse(*score), *index));
    let mut selected: Vec<usize> = scored
        .iter()
        .filter(|(_, score)| *score > 0)
        .map(|(index, _)| *index)
        .take(CHUNKS_PER_SOURCE)
        .collect();
    if selected.is_empty() {
        selected = (0..blocks.len().min(CHUNKS_PER_SOURCE)).collect();
    }
    selected.sort_unstable();
    selected
        .iter()
        .map(|index| blocks[*index].as_str())
        .collect::<Vec<_>>()
        .join("\n\n")
}

fn split_blocks(text: &str, chunk_chars: usize) -> Vec<String> {
    let mut blocks = Vec::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        let chars: Vec<char> = paragraph.chars().collect();
        for chunk in chars.chunks(chunk_chars) {
            let block: String = chunk.iter().collect();
            let block = block.trim();
            if !block.is_empty() {
                blocks.push(block.to_owned());
            }
        }
    }
    blocks
}

/// 按字符截断并追加标记；返回是否真的截断。
fn cap_chars(text: &mut String, max: usize) -> bool {
    if text.chars().count() <= max {
        return false;
    }
    let end = text
        .char_indices()
        .nth(max)
        .map(|(index, _)| index)
        .unwrap_or(text.len());
    text.truncate(end);
    text.push_str(&format!("\n\n[truncated after {max} characters]"));
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_html_and_strips_active_content() {
        let html = r#"<html><head><title>Demo &amp; Test</title></head><body>
            <script>alert(1)</script><style>.x{}</style>
            <h1>Heading</h1><p>Hello <strong>world</strong>.</p>
            <ul><li>One</li><li>Two</li></ul>
            <pre><code>fn main() {}</code></pre>
        </body></html>"#;
        let markdown = html_to_markdown(html).expect("convert");
        assert!(markdown.starts_with("# Demo & Test"));
        assert!(!markdown.contains("\nDemo & Test\n"), "标题不应重复");
        assert!(markdown.contains("# Heading"));
        assert!(markdown.contains("Hello **world**"));
        assert!(markdown.contains("*   One"));
        assert!(!markdown.contains("alert(1)"));
        assert!(!markdown.contains(".x{}"));
    }

    #[test]
    fn selects_relevant_blocks_and_keeps_order() {
        let text = format!("{}\n\n{}", "alpha ".repeat(200), "needle beta ".repeat(120));
        let selected = select_relevant_chunks(&text, "needle");
        assert!(selected.contains("needle beta"));
        assert!(!selected.contains("alpha alpha alpha"));
    }

    #[test]
    fn rejects_unsupported_content_types() {
        assert!(!is_text_type("application/pdf"));
        assert!(is_text_type("text/plain"));
        assert!(is_text_type("application/json"));
    }

    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use crate::config::ResolvedWebFetch;
    use crate::websearch::sites::SiteEndpoints;

    fn config(max_bytes: usize, max_chars: usize) -> ResolvedWebFetch {
        ResolvedWebFetch {
            enabled: true,
            timeout_secs: 5,
            max_bytes,
            max_chars,
        }
    }

    async fn fetch_one(
        server: &MockServer,
        cache: &std::path::Path,
        max_bytes: usize,
        max_chars: usize,
    ) -> ExtractResponse {
        let fetcher = NativeFetcher::with_endpoints(
            &config(max_bytes, max_chars),
            SiteEndpoints::default(),
            cache,
        );
        fetcher
            .extract(
                &ExtractRequest {
                    urls: vec![format!("{}/page", server.uri())],
                    query: None,
                },
                &CancellationToken::new(),
            )
            .await
            .expect("fetch")
    }

    #[tokio::test]
    async fn generic_html_fetch_reports_metadata() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<html><head><title>Page</title></head><body><h1>Hi</h1></body></html>",
                "text/html; charset=utf-8",
            ))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().expect("cache");
        let response = fetch_one(&server, cache.path(), 1024 * 1024, 100_000).await;
        assert_eq!(response.results.len(), 1);
        let result = &response.results[0];
        assert_eq!(result.status, Some(200));
        assert_eq!(result.method, "generic");
        assert_eq!(
            result.content_type.as_deref(),
            Some("text/html; charset=utf-8")
        );
        assert!(result.raw_content.starts_with("# Page"));
        assert!(result.raw_content.contains("# Hi"));
        assert!(!result.truncated);
    }

    #[tokio::test]
    async fn unsupported_content_type_is_a_per_url_failure() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("%PDF", "application/pdf"))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().expect("cache");
        let response = fetch_one(&server, cache.path(), 1024, 100).await;
        assert!(response.results.is_empty());
        assert!(
            response.failed[0]
                .error
                .contains("unsupported content type")
        );
    }

    #[tokio::test]
    async fn http_errors_are_accumulated_not_fatal() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(500).set_body_raw("boom", "text/plain"))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().expect("cache");
        let response = fetch_one(&server, cache.path(), 1024, 100).await;
        assert_eq!(response.failed[0].error, "HTTP 500");
    }

    #[tokio::test]
    async fn rejects_credentials_and_non_http_schemes() {
        let cache = tempfile::tempdir().expect("cache");
        let fetcher = NativeFetcher::with_endpoints(
            &config(1024, 100),
            SiteEndpoints::default(),
            cache.path(),
        );
        let response = fetcher
            .extract(
                &ExtractRequest {
                    urls: vec![
                        "https://user:pass@example.com/".into(),
                        "ftp://example.com/".into(),
                        "not a url".into(),
                    ],
                    query: None,
                },
                &CancellationToken::new(),
            )
            .await
            .expect("fetch");
        assert_eq!(response.failed.len(), 3);
        assert!(response.failed[0].error.contains("credentials"));
        assert!(response.failed[1].error.contains("unsupported scheme"));
        assert!(response.failed[2].error.contains("invalid URL"));
    }

    #[tokio::test]
    async fn cancellation_aborts_the_whole_call() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_string("hello"))
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().expect("cache");
        let fetcher = NativeFetcher::with_endpoints(
            &config(1024, 100),
            SiteEndpoints::default(),
            cache.path(),
        );
        let cancel = CancellationToken::new();
        cancel.cancel();
        let error = fetcher
            .extract(
                &ExtractRequest {
                    urls: vec![format!("{}/page", server.uri())],
                    query: None,
                },
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "E_CANCELLED");
    }
}

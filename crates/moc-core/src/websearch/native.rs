//! 本地抓取：会话快照复用、站点 handler 与 Markdown 优先的正文获取。

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::stream::{self, StreamExt};
use reqwest::Url;
use tokio_util::sync::CancellationToken;

use super::documents::DocumentStore;
use super::http::{
    BatchBudget, HttpFetcher, MAX_BATCH_BYTES, WebFetchFailure, content_type_essence, decode_body,
};
use super::sites::{self, SiteEndpoints};
use super::{ExtractFailed, ExtractRequest, ExtractResponse, ExtractResult, WebSearchError};
use crate::config::ResolvedWebFetch;

const FETCH_CONCURRENCY: usize = 4;
const MAX_URLS: usize = 20;

pub(super) struct NativeFetcher {
    timeout: Duration,
    max_bytes: usize,
    endpoints: SiteEndpoints,
    cache_dir: PathBuf,
}

impl NativeFetcher {
    pub(super) fn new(config: &ResolvedWebFetch, cache_dir: &std::path::Path) -> Self {
        Self {
            timeout: Duration::from_secs(config.timeout_secs),
            max_bytes: config.max_bytes,
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
            endpoints,
            cache_dir: cache_dir.to_path_buf(),
        }
    }

    pub(super) async fn extract(
        &self,
        request: &ExtractRequest,
        cancel: &CancellationToken,
    ) -> Result<ExtractResponse, WebSearchError> {
        let refresh = request.refresh;
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
                let outcome = self.fetch_one(url, refresh, &fetcher, &endpoints).await;
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
                Err(error) => failed.push(ExtractFailed {
                    url: url.to_owned(),
                    error: error.to_string(),
                }),
            }
        }
        Ok(ExtractResponse { results, failed })
    }

    async fn fetch_one(
        &self,
        raw_url: &str,
        refresh: bool,
        fetcher: &HttpFetcher<'_>,
        endpoints: &SiteEndpoints,
    ) -> Result<ExtractResult, WebFetchFailure> {
        let mut url = Url::parse(raw_url)
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
        url.set_fragment(None);
        let store = DocumentStore::new(&self.cache_dir, url.as_str());
        let _guard = store.lock(fetcher.cancel).await?;
        if !refresh && let Some(page) = store.load(url.as_str()).await? {
            return Ok(page);
        }
        let mut result = self.fetch_document(&url, fetcher, endpoints).await?;
        result.url = url.to_string();
        result.raw_content = result.raw_content.trim().to_owned();
        if result.raw_content.is_empty() {
            return Err(WebFetchFailure::message("empty body"));
        }
        if result.title.is_none() {
            result.title = result
                .raw_content
                .lines()
                .find_map(|line| line.strip_prefix("# "))
                .map(|title| title.trim().chars().take(200).collect());
        }
        store.save(&mut result).await?;
        Ok(result)
    }

    async fn fetch_document(
        &self,
        url: &Url,
        fetcher: &HttpFetcher<'_>,
        endpoints: &SiteEndpoints,
    ) -> Result<ExtractResult, WebFetchFailure> {
        if let Some(result) = sites::try_extract(url, fetcher, endpoints, &self.cache_dir).await {
            match result {
                Ok(result) if !result.raw_content.trim().is_empty() => return Ok(result),
                Err(WebFetchFailure::Cancelled) => return Err(WebFetchFailure::Cancelled),
                _ => {}
            }
        }
        // 优先协商 Markdown；不支持的服务仍可返回 HTML。
        let headers = sites::headers(
            "text/markdown,text/x-markdown;q=0.95,text/html;q=0.9,text/plain;q=0.8,*/*;q=0.5",
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
        let is_html = matches!(
            essence.as_deref(),
            Some("text/html" | "application/xhtml+xml")
        ) || (essence.is_none() && decoded.trim_start().starts_with('<'));
        let mut result = ExtractResult {
            url: url.to_string(),
            final_url: response.final_url,
            title: None,
            status: Some(response.status),
            content_type: response.content_type,
            method: "generic".into(),
            notes: Vec::new(),
            truncated: response.truncated,
            artifact: None,
            cached: false,
            view_repeated: false,
            raw_content: String::new(),
        };
        if is_html {
            let page = super::html::render(&decoded)?;
            let base = Url::parse(&result.final_url).expect("HTTP 最终 URL 已解析");
            let mut candidates: Vec<Url> = page
                .alternates
                .iter()
                .filter_map(|href| base.join(href).ok())
                .filter(|url| {
                    matches!(url.scheme(), "http" | "https")
                        && url.username().is_empty()
                        && url.password().is_none()
                })
                .take(2)
                .collect();
            if !base.path().ends_with(".md") {
                let mut markdown = base.clone();
                markdown.set_path(&format!("{}.md", base.path().trim_end_matches('/')));
                if !candidates.contains(&markdown) {
                    candidates.push(markdown);
                }
            }
            for candidate in candidates {
                match fetcher.get(candidate.as_str(), &headers).await {
                    Ok(response)
                        if (200..300).contains(&response.status)
                            && matches!(
                                content_type_essence(response.content_type.as_deref()).as_deref(),
                                Some("text/markdown" | "text/x-markdown" | "text/plain")
                            ) =>
                    {
                        let text = decode_body(&response.bytes, response.content_type.as_deref());
                        if text.trim().is_empty() || text.trim_start().starts_with('<') {
                            continue;
                        }
                        result.final_url = response.final_url;
                        result.content_type = response.content_type;
                        result.truncated = response.truncated;
                        result.method = "markdown".into();
                        result.title = page.title;
                        result.raw_content = text;
                        return Ok(mark_truncated(result));
                    }
                    Err(WebFetchFailure::Cancelled) => return Err(WebFetchFailure::Cancelled),
                    _ => {}
                }
            }
            result.title = page.title;
            result.raw_content = page.text;
        } else {
            if let Some(kind) = essence.as_deref()
                && !is_text_type(kind)
            {
                return Err(WebFetchFailure::message(format!(
                    "unsupported content type: {kind}"
                )));
            }
            if essence.is_none() && looks_binary(&response.bytes) {
                return Err(WebFetchFailure::message("binary content"));
            }
            result.raw_content = decoded;
        }
        Ok(mark_truncated(result))
    }
}

fn mark_truncated(mut result: ExtractResult) -> ExtractResult {
    if result.truncated {
        result.notes.push("Source incomplete: response or batch byte limit reached; the saved document contains only the received prefix.".into());
    }
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

pub(super) fn html_to_markdown(html: &str) -> std::io::Result<String> {
    super::html::render(html).map(|page| page.text)
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
                    refresh: false,
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
                    refresh: false,
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
                    refresh: false,
                },
                &cancel,
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, "E_CANCELLED");
    }

    #[tokio::test]
    async fn document_cache_preserves_source_and_refresh_keeps_old_artifacts() {
        let server = MockServer::builder().start().await;
        let original = format!(
            "# API\n\n## Parameters\n\n- `temperature: number`\n\n{}\nEND-OF-DOCUMENT\n",
            "正文\n".repeat(100_000)
        );
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(original.clone(), "text/markdown"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        let config = config(2_000_000, 200);
        let fetcher =
            NativeFetcher::with_endpoints(&config, SiteEndpoints::default(), cache.path());
        let cancel = CancellationToken::new();
        let request = ExtractRequest {
            urls: vec![format!("{}/page", server.uri())],
            refresh: false,
        };
        let (first, concurrent) = tokio::join!(
            fetcher.extract(&request, &cancel),
            fetcher.extract(&request, &cancel)
        );
        let first = first.unwrap();
        let concurrent = concurrent.unwrap();
        assert_eq!(first.results[0].artifact, concurrent.results[0].artifact);
        assert_ne!(first.results[0].cached, concurrent.results[0].cached);
        assert!(!first.results[0].truncated);
        assert!(first.results[0].raw_content.ends_with("END-OF-DOCUMENT"));
        let saved = first.results[0].artifact.as_ref().unwrap();
        let saved_path = cache
            .path()
            .join(saved.strip_prefix("artifact://").unwrap());
        assert_eq!(
            tokio::fs::read_to_string(&saved_path).await.unwrap(),
            original.trim()
        );
        let view = super::super::view::render(&first, Some("temperature"), config.max_chars);
        assert!(view.contains(saved));
        assert!(!view.contains("END-OF-DOCUMENT"));
        server.verify().await;
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("# New\nnew source", "text/markdown"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let reloaded =
            NativeFetcher::with_endpoints(&config, SiteEndpoints::default(), cache.path());
        let cached = reloaded.extract(&request, &cancel).await.unwrap();
        assert!(cached.results[0].cached);
        assert_eq!(cached.results[0].artifact.as_ref(), Some(saved));
        let refreshed = reloaded
            .extract(
                &ExtractRequest {
                    refresh: true,
                    ..request
                },
                &cancel,
            )
            .await
            .unwrap();
        assert_ne!(refreshed.results[0].artifact.as_ref(), Some(saved));
        assert_eq!(refreshed.results[0].raw_content, "# New\nnew source");
        assert_eq!(
            tokio::fs::read_to_string(saved_path).await.unwrap(),
            original.trim()
        );
    }

    #[tokio::test]
    async fn prefers_markdown_alternates_and_reports_real_source_truncation() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET")).and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                "<title>API</title><link rel='alternate' type='text/markdown' href='/docs.md'><main>HTML fallback</main>", "text/html"))
            .mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/docs.md"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw("# API\nMarkdown body", "text/markdown"),
            )
            .expect(1)
            .mount(&server)
            .await;
        let cache = tempfile::tempdir().unwrap();
        let response = fetch_one(&server, cache.path(), 1024, 1).await;
        assert_eq!(response.results[0].raw_content, "# API\nMarkdown body");
        assert_eq!(response.results[0].method, "markdown");
        assert!(!response.results[0].truncated);
        server.reset().await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("1234567890", "text/plain"))
            .mount(&server)
            .await;
        for (limit, truncated) in [(10, false), (5, true)] {
            let cache = tempfile::tempdir().unwrap();
            let response = fetch_one(&server, cache.path(), limit, 1).await;
            assert_eq!(response.results[0].truncated, truncated);
            assert_eq!(response.results[0].raw_content.len(), limit);
        }
    }
}

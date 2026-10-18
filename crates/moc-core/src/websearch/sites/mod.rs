//! 站点感知抓取：命中已知站点时返回结构化 Markdown，失败回退通用抓取。

mod arxiv;
mod docs_rs;
mod github;
mod mdn;
mod npm;
mod pypi;

use std::path::Path;

use reqwest::Url;

use super::ExtractResult;
use super::http::{HttpFetcher, WebFetchFailure};

/// 站点 API 基地址；测试通过构造器注入 wiremock URI。
#[derive(Debug, Clone)]
pub(super) struct SiteEndpoints {
    pub github_api: String,
    pub github_raw: String,
    pub docs_rs: String,
    pub mdn: String,
    pub arxiv_api: String,
    pub npm_registry: String,
    pub pypi: String,
    pub github_token: Option<String>,
}

impl Default for SiteEndpoints {
    fn default() -> Self {
        Self {
            github_api: "https://api.github.com".into(),
            github_raw: "https://raw.githubusercontent.com".into(),
            docs_rs: "https://docs.rs".into(),
            mdn: "https://developer.mozilla.org".into(),
            arxiv_api: "https://export.arxiv.org".into(),
            npm_registry: "https://registry.npmjs.org".into(),
            pypi: "https://pypi.org".into(),
            github_token: None,
        }
    }
}

impl SiteEndpoints {
    pub(super) fn from_env() -> Self {
        let github_token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("GH_TOKEN"))
            .ok()
            .filter(|token| !token.trim().is_empty());
        Self {
            github_token,
            ..Default::default()
        }
    }
}

/// 返回 `None` 表示该 URL 不归这些 handler 管，直接走通用抓取。
pub(super) async fn try_extract(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    cache_dir: &Path,
) -> Option<Result<ExtractResult, WebFetchFailure>> {
    let host = url.host_str()?.to_ascii_lowercase();
    match host.as_str() {
        "github.com" => github::extract(url, http, endpoints).await,
        "docs.rs" => docs_rs::extract(url, http, endpoints, cache_dir).await,
        "developer.mozilla.org" => mdn::extract(url, http, endpoints).await,
        "arxiv.org" => arxiv::extract(url, http, endpoints).await,
        "www.npmjs.com" | "npmjs.com" => npm::extract(url, http, endpoints).await,
        "pypi.org" | "www.pypi.org" => pypi::extract(url, http, endpoints).await,
        _ => None,
    }
}

/// 统一站点结果形状；正文的 query/字符上限由调用方统一后处理。
pub(super) fn markdown_result(
    url: &Url,
    final_url: Option<String>,
    status: Option<u16>,
    method: &'static str,
    notes: Vec<String>,
    content: String,
) -> ExtractResult {
    ExtractResult {
        url: url.to_string(),
        final_url: final_url.unwrap_or_else(|| url.to_string()),
        status,
        content_type: Some("text/markdown".into()),
        method,
        notes,
        truncated: false,
        raw_content: content,
    }
}

/// 把 `Accept` 与可选 token 组装成请求头。
pub(super) fn headers(accept: &'static str) -> reqwest::header::HeaderMap {
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        reqwest::header::HeaderValue::from_static(accept),
    );
    headers
}

/// 站点 API 非 2xx 的统一失败文案。
pub(super) fn status_error(label: &str, status: u16) -> WebFetchFailure {
    WebFetchFailure::message(format!("{label} HTTP {status}"))
}

#[cfg(test)]
pub(super) mod test_support {
    use std::time::Duration;

    use tokio_util::sync::CancellationToken;
    use wiremock::MockServer;

    use super::super::http::{BatchBudget, HttpFetcher};
    use super::SiteEndpoints;

    pub(super) fn endpoints(server: &MockServer) -> SiteEndpoints {
        SiteEndpoints {
            github_api: server.uri(),
            github_raw: server.uri(),
            docs_rs: server.uri(),
            mdn: server.uri(),
            arxiv_api: server.uri(),
            npm_registry: server.uri(),
            pypi: server.uri(),
            github_token: None,
        }
    }

    pub(super) fn fetcher<'a>(
        batch: &'a BatchBudget,
        cancel: &'a CancellationToken,
    ) -> HttpFetcher<'a> {
        HttpFetcher {
            timeout: Duration::from_secs(5),
            max_bytes: 1024 * 1024,
            batch,
            cancel,
        }
    }

    pub(super) fn gzip(bytes: &[u8]) -> Vec<u8> {
        use std::io::Write;
        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        encoder.write_all(bytes).expect("gzip write");
        encoder.finish().expect("gzip finish")
    }
}

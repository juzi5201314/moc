//! PyPI JSON API 的结构化抓取。

use std::collections::BTreeMap;

use reqwest::Url;
use serde::Deserialize;

use super::super::ExtractResult;
use super::super::http::{HttpFetcher, WebFetchFailure};
use super::{SiteEndpoints, markdown_result, status_error};

#[derive(Debug, Deserialize)]
struct PyPiEnvelope {
    info: PyPiInfo,
    #[serde(default)]
    requires_dist: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct PyPiInfo {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default)]
    license: Option<String>,
    #[serde(default)]
    home_page: Option<String>,
    #[serde(default)]
    project_urls: BTreeMap<String, String>,
    #[serde(default)]
    requires_python: Option<String>,
    #[serde(default)]
    keywords: Option<String>,
}

pub(super) async fn extract(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
) -> Option<Result<ExtractResult, WebFetchFailure>> {
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.first().copied() != Some("project") {
        return None;
    }
    let name = *segments.get(1)?;
    Some(fetch(url, http, endpoints, name).await)
}

async fn fetch(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    name: &str,
) -> Result<ExtractResult, WebFetchFailure> {
    let api = format!("{}/pypi/{name}/json", endpoints.pypi.trim_end_matches('/'));
    let response = http.get(&api, &super::headers("application/json")).await?;
    if !(200..300).contains(&response.status) {
        return Err(status_error("PyPI", response.status));
    }
    let envelope: PyPiEnvelope = serde_json::from_slice(&response.bytes)
        .map_err(|error| WebFetchFailure::message(format!("PyPI JSON: {error}")))?;
    let info = envelope.info;

    let mut markdown = format!("# {}\n\n", info.name);
    if let Some(summary) = &info.summary
        && !summary.trim().is_empty()
    {
        markdown.push_str(summary.trim());
        markdown.push_str("\n\n");
    }
    markdown.push_str(&format!("**Latest:** {}", info.version));
    if let Some(license) = &info.license
        && !license.trim().is_empty()
    {
        markdown.push_str(&format!(" · **License:** {}", license.trim()));
    }
    markdown.push('\n');
    if let Some(author) = &info.author
        && !author.trim().is_empty()
    {
        markdown.push_str(&format!("**Author:** {}\n", author.trim()));
    }
    if let Some(python) = &info.requires_python
        && !python.trim().is_empty()
    {
        markdown.push_str(&format!("**Python:** {}\n", python.trim()));
    }
    if let Some(home) = &info.home_page
        && !home.trim().is_empty()
    {
        markdown.push_str(&format!("**Homepage:** {}\n", home.trim()));
    }
    if !info.project_urls.is_empty() {
        markdown.push_str("\n**Project URLs:**\n");
        for (label, url) in &info.project_urls {
            markdown.push_str(&format!("- {label}: {url}\n"));
        }
    }
    if let Some(keywords) = &info.keywords
        && !keywords.trim().is_empty()
    {
        markdown.push_str(&format!("\n**Keywords:** {}\n", keywords.trim()));
    }
    if let Some(requires) = &envelope.requires_dist
        && !requires.is_empty()
    {
        markdown.push_str("\n## Dependencies\n\n");
        for dependency in requires {
            markdown.push_str(&format!("- {dependency}\n"));
        }
    }
    if let Some(description) = &info.description
        && !description.trim().is_empty()
    {
        markdown.push_str("\n---\n\n## Description\n\n");
        markdown.push_str(description.trim());
        markdown.push('\n');
    }
    Ok(markdown_result(
        url,
        None,
        Some(200),
        "pypi",
        vec!["PyPI JSON API".into()],
        markdown,
    ))
}

#[cfg(test)]
mod tests {
    use super::super::super::http::BatchBudget;
    use super::super::test_support::{endpoints, fetcher};
    use super::*;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn renders_json_metadata_and_description() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/pypi/foo/json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "info": {
                    "name": "foo",
                    "version": "1.2.3",
                    "summary": "A demo package",
                    "description": "Long description.",
                    "author": "Alice",
                    "license": "MIT",
                    "home_page": "https://example.com",
                    "project_urls": {"Source": "https://github.com/o/foo"},
                    "requires_python": ">=3.9",
                    "keywords": "demo"
                },
                "requires_dist": ["bar>=1.0"]
            })))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://pypi.org/project/foo/").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.method, "pypi");
        assert!(result.raw_content.contains("# foo"));
        assert!(result.raw_content.contains("**Latest:** 1.2.3"));
        assert!(result.raw_content.contains("bar>=1.0"));
        assert!(result.raw_content.contains("Long description."));
    }
}

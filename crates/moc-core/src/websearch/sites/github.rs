//! GitHub issue / pull / blob / repo README 的结构化抓取。

use reqwest::Url;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use serde_json::Value;

use super::super::ExtractResult;
use super::super::http::{HttpFetcher, WebFetchFailure, decode_body};
use super::{SiteEndpoints, markdown_result, status_error};

const API_ACCEPT: &str = "application/vnd.github+json";
const RAW_ACCEPT: &str = "application/vnd.github.raw+json";
const API_VERSION: &str = "2022-11-28";

pub(super) async fn extract(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
) -> Option<Result<ExtractResult, WebFetchFailure>> {
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    if segments.len() < 2 {
        return None;
    }
    let owner = segments[0];
    let repo_name = segments[1];
    match segments.get(2).copied() {
        None => Some(repo(url, http, endpoints, owner, repo_name).await),
        Some("issues") | Some("pull") => {
            let number: u64 = segments.get(3)?.parse().ok()?;
            Some(issue(url, http, endpoints, owner, repo_name, number).await)
        }
        Some("blob") => {
            let reference = segments.get(3)?;
            let path = segments.get(4..)?.join("/");
            if path.is_empty() {
                None
            } else {
                Some(blob(url, http, endpoints, owner, repo_name, reference, &path).await)
            }
        }
        _ => None,
    }
}

fn api_headers(endpoints: &SiteEndpoints) -> HeaderMap {
    let mut headers = super::headers(API_ACCEPT);
    headers.insert(
        "X-GitHub-Api-Version",
        HeaderValue::from_static(API_VERSION),
    );
    if let Some(token) = &endpoints.github_token
        && let Ok(value) = HeaderValue::from_str(&format!("Bearer {token}"))
    {
        headers.insert(AUTHORIZATION, value);
    }
    headers
}

async fn get_json(
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    path: &str,
) -> Result<(u16, Value), WebFetchFailure> {
    let url = format!("{}{path}", endpoints.github_api.trim_end_matches('/'));
    let response = http.get(&url, &api_headers(endpoints)).await?;
    if !(200..300).contains(&response.status) {
        return Err(status_error("GitHub API", response.status));
    }
    let value: Value = serde_json::from_slice(&response.bytes)
        .map_err(|error| WebFetchFailure::message(format!("GitHub API JSON: {error}")))?;
    Ok((response.status, value))
}

async fn issue(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    owner: &str,
    repo: &str,
    number: u64,
) -> Result<ExtractResult, WebFetchFailure> {
    let path = format!("/repos/{owner}/{repo}/issues/{number}");
    let (status, issue) = get_json(http, endpoints, &path).await?;
    let title = text(&issue, "title").unwrap_or_else(|| format!("#{number}"));
    let state = text(&issue, "state").unwrap_or_else(|| "unknown".into());
    let login = nested_text(&issue, "user", "login").unwrap_or_else(|| "unknown".into());
    let created = text(&issue, "created_at").unwrap_or_default();
    let html_url = text(&issue, "html_url").unwrap_or_else(|| url.to_string());
    let body = text(&issue, "body").unwrap_or_default();
    let total_comments = issue
        .get("comments")
        .and_then(Value::as_u64)
        .unwrap_or_default();
    let is_pull = issue.get("pull_request").is_some();

    let mut markdown = format!("# {title}\n\n");
    markdown.push_str(&format!(
        "{} · @{login}{}{}\n\n",
        state,
        if created.is_empty() {
            String::new()
        } else {
            format!(" · {created}")
        },
        if is_pull { " · pull request" } else { "" },
    ));
    markdown.push_str(if body.trim().is_empty() {
        "(no body)"
    } else {
        body.trim()
    });
    markdown.push('\n');

    let mut notes = vec!["GitHub API".to_owned()];
    if total_comments > 0 {
        let path = format!(
            "/repos/{owner}/{repo}/issues/{number}/comments?per_page={}",
            total_comments.min(20)
        );
        match get_json(http, endpoints, &path).await {
            Ok((_, comments)) => {
                let items = comments.as_array().cloned().unwrap_or_default();
                if !items.is_empty() {
                    markdown.push_str(&format!(
                        "\n## Comments ({}/{total_comments})\n",
                        items.len()
                    ));
                    for comment in items {
                        let login = nested_text(&comment, "user", "login")
                            .unwrap_or_else(|| "unknown".into());
                        let created = text(&comment, "created_at").unwrap_or_default();
                        let body = text(&comment, "body").unwrap_or_default();
                        markdown.push_str(&format!("\n### @{login} · {created}\n\n"));
                        markdown.push_str(if body.trim().is_empty() {
                            "(empty)"
                        } else {
                            body.trim()
                        });
                        markdown.push('\n');
                    }
                }
            }
            Err(WebFetchFailure::Cancelled) => {
                return Err(WebFetchFailure::Cancelled);
            }
            Err(WebFetchFailure::Message(error)) => {
                notes.push(format!("comments unavailable: {error}"));
            }
        }
    }

    Ok(markdown_result(
        url,
        Some(html_url),
        Some(status),
        "github",
        notes,
        markdown,
    ))
}

async fn repo(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    owner: &str,
    repo: &str,
) -> Result<ExtractResult, WebFetchFailure> {
    let path = format!("/repos/{owner}/{repo}");
    let (status, data) = get_json(http, endpoints, &path).await?;
    let full_name = text(&data, "full_name").unwrap_or_else(|| format!("{owner}/{repo}"));
    let description = text(&data, "description").unwrap_or_default();
    let html_url = text(&data, "html_url").unwrap_or_else(|| url.to_string());
    let stars = data.get("stargazers_count").and_then(Value::as_u64);
    let language = text(&data, "language");
    let license = data
        .get("license")
        .and_then(|license| text(license, "name"));
    let branch = text(&data, "default_branch");

    let mut markdown = format!("# {full_name}\n\n");
    if !description.trim().is_empty() {
        markdown.push_str(description.trim());
        markdown.push_str("\n\n");
    }
    if let Some(stars) = stars {
        markdown.push_str(&format!("Stars: {stars}"));
        if let Some(language) = &language {
            markdown.push_str(&format!(" · Language: {language}"));
        }
        if let Some(license) = &license {
            markdown.push_str(&format!(" · License: {license}"));
        }
        markdown.push('\n');
    }
    if let Some(branch) = &branch {
        markdown.push_str(&format!("Default branch: {branch}\n"));
    }

    let mut notes = vec!["GitHub API".to_owned()];
    let readme_url = format!(
        "{}/repos/{owner}/{repo}/readme",
        endpoints.github_api.trim_end_matches('/')
    );
    let mut readme_headers = api_headers(endpoints);
    readme_headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static(RAW_ACCEPT),
    );
    match http.get(&readme_url, &readme_headers).await {
        Ok(response) if (200..300).contains(&response.status) => {
            let readme = decode_body(&response.bytes, response.content_type.as_deref());
            if !readme.trim().is_empty() {
                markdown.push_str("\n---\n\n");
                markdown.push_str(readme.trim());
                markdown.push('\n');
                notes.push("README".into());
            }
        }
        Ok(response) => notes.push(format!("README unavailable (HTTP {})", response.status)),
        Err(WebFetchFailure::Cancelled) => return Err(WebFetchFailure::Cancelled),
        Err(WebFetchFailure::Message(error)) => {
            notes.push(format!("README unavailable: {error}"));
        }
    }

    Ok(markdown_result(
        url,
        Some(html_url),
        Some(status),
        "github",
        notes,
        markdown,
    ))
}

async fn blob(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    owner: &str,
    repo: &str,
    reference: &str,
    path: &str,
) -> Result<ExtractResult, WebFetchFailure> {
    let raw_url = format!(
        "{}/{owner}/{repo}/{reference}/{path}",
        endpoints.github_raw.trim_end_matches('/')
    );
    let mut headers = HeaderMap::new();
    headers.insert(
        reqwest::header::ACCEPT,
        HeaderValue::from_static("text/plain,*/*"),
    );
    let response = http.get(&raw_url, &headers).await?;
    if !(200..300).contains(&response.status) {
        return Err(status_error("GitHub raw", response.status));
    }
    if response.bytes.iter().take(4096).any(|byte| *byte == 0) {
        return Err(WebFetchFailure::message("binary content"));
    }
    let content = decode_body(&response.bytes, response.content_type.as_deref());
    Ok(ExtractResult {
        url: url.to_string(),
        final_url: response.final_url,
        status: Some(response.status),
        content_type: response.content_type,
        method: "github",
        notes: vec!["raw file".into()],
        truncated: response.truncated,
        raw_content: content,
    })
}

fn text(value: &Value, key: &str) -> Option<String> {
    value
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn nested_text(value: &Value, outer: &str, inner: &str) -> Option<String> {
    value.get(outer).and_then(|value| text(value, inner))
}

#[cfg(test)]
mod tests {
    use super::super::super::http::BatchBudget;
    use super::super::test_support::{endpoints, fetcher};
    use super::*;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn token_endpoints(server: &MockServer) -> SiteEndpoints {
        SiteEndpoints {
            github_token: Some("token".into()),
            ..endpoints(server)
        }
    }

    #[tokio::test]
    async fn issue_renders_body_and_comments() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1"))
            .and(header("authorization", "Bearer token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "title": "Fix the bug",
                "state": "open",
                "body": "Details.",
                "user": {"login": "alice"},
                "created_at": "2024-01-02T03:04:05Z",
                "html_url": "https://github.com/o/r/issues/1",
                "comments": 1
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1/comments"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(serde_json::json!([{
                    "user": {"login": "bob"},
                    "created_at": "2024-01-03T03:04:05Z",
                    "body": "LGTM"
                }])),
            )
            .mount(&server)
            .await;
        let endpoints = token_endpoints(&server);
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://github.com/o/r/issues/1").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.method, "github");
        assert!(result.raw_content.contains("# Fix the bug"));
        assert!(result.raw_content.contains("@alice"));
        assert!(result.raw_content.contains("Details."));
        assert!(result.raw_content.contains("## Comments (1/1)"));
        assert!(result.raw_content.contains("@bob"));
        assert!(result.raw_content.contains("LGTM"));
    }

    #[tokio::test]
    async fn api_failure_is_returned_for_fallback() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/issues/1"))
            .respond_with(ResponseTemplate::new(403).set_body_string("rate limited"))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let error = extract(
            &Url::parse("https://github.com/o/r/issues/1").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap_err();
        assert!(matches!(error, WebFetchFailure::Message(message) if message.contains("403")));
    }

    #[tokio::test]
    async fn blob_reads_raw_file() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/o/r/main/src/lib.rs"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("pub fn demo() {}", "text/plain"))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://github.com/o/r/blob/main/src/lib.rs").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.raw_content, "pub fn demo() {}");
        assert_eq!(result.notes, vec!["raw file".to_owned()]);
    }

    #[tokio::test]
    async fn repo_renders_metadata_and_readme() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "full_name": "o/r",
                "description": "Demo repo",
                "html_url": "https://github.com/o/r",
                "stargazers_count": 7,
                "language": "Rust",
                "license": {"name": "MIT"},
                "default_branch": "main"
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/o/r/readme"))
            .respond_with(ResponseTemplate::new(200).set_body_raw("# README\n", "text/plain"))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://github.com/o/r").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert!(result.raw_content.contains("# o/r"));
        assert!(result.raw_content.contains("Demo repo"));
        assert!(result.raw_content.contains("Stars: 7"));
        assert!(result.raw_content.contains("# README"));
    }
}

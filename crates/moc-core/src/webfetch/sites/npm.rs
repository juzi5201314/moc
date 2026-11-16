//! npm registry 的结构化抓取。

use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

use super::super::ExtractResult;
use super::super::http::{HttpFetcher, WebFetchFailure};
use super::{SiteEndpoints, markdown_result, status_error};

#[derive(Debug, Deserialize)]
struct NpmLatest {
    name: String,
    #[serde(default)]
    version: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    license: Option<Value>,
    #[serde(default)]
    homepage: Option<String>,
    #[serde(default)]
    repository: Option<Value>,
    #[serde(default)]
    keywords: Vec<String>,
    #[serde(default)]
    maintainers: Vec<NpmPerson>,
    #[serde(default)]
    dependencies: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    readme: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NpmPerson {
    #[serde(default)]
    name: String,
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
    if segments.first().copied() != Some("package") || segments.len() < 2 {
        return None;
    }
    let name = if segments[1].starts_with('@') {
        format!("{}/{}", segments[1], segments.get(2)?)
    } else {
        segments[1].to_owned()
    };
    Some(fetch(url, http, endpoints, &name).await)
}

async fn fetch(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    name: &str,
) -> Result<ExtractResult, WebFetchFailure> {
    let api = format!(
        "{}/{name}/latest",
        endpoints.npm_registry.trim_end_matches('/')
    );
    let response = http.get(&api, &super::headers("application/json")).await?;
    if !(200..300).contains(&response.status) {
        return Err(status_error("npm registry", response.status));
    }
    let package: NpmLatest = serde_json::from_slice(&response.bytes)
        .map_err(|error| WebFetchFailure::message(format!("npm registry JSON: {error}")))?;

    let mut markdown = format!("# {}\n\n", package.name);
    if let Some(description) = &package.description
        && !description.trim().is_empty()
    {
        markdown.push_str(description.trim());
        markdown.push_str("\n\n");
    }
    markdown.push_str(&format!("**Latest:** {}", package.version));
    if let Some(license) = &package.license
        && let Some(license) = license_text(license)
    {
        markdown.push_str(&format!(" · **License:** {license}"));
    }
    markdown.push('\n');
    if let Some(homepage) = &package.homepage {
        markdown.push_str(&format!("**Homepage:** {homepage}\n"));
    }
    if let Some(repository) = &package.repository
        && let Some(repository) = repository_text(repository)
    {
        markdown.push_str(&format!("**Repository:** {repository}\n"));
    }
    if !package.keywords.is_empty() {
        markdown.push_str(&format!("**Keywords:** {}\n", package.keywords.join(", ")));
    }
    if !package.maintainers.is_empty() {
        markdown.push_str(&format!(
            "**Maintainers:** {}\n",
            package
                .maintainers
                .iter()
                .map(|person| person.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !package.dependencies.is_empty() {
        markdown.push_str("\n## Dependencies\n\n");
        for (name, version) in &package.dependencies {
            markdown.push_str(&format!("- {name}: {version}\n"));
        }
    }
    if let Some(readme) = &package.readme
        && !readme.trim().is_empty()
    {
        markdown.push_str("\n---\n\n## README\n\n");
        markdown.push_str(readme.trim());
        markdown.push('\n');
    }
    Ok(markdown_result(
        url,
        None,
        Some(200),
        "npm",
        vec!["npm registry".into()],
        markdown,
    ))
}

fn license_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("type").and_then(Value::as_str).map(str::to_owned))
}

fn repository_text(value: &Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_owned)
        .or_else(|| value.get("url").and_then(Value::as_str).map(str::to_owned))
        .map(|url| {
            url.trim_start_matches("git+")
                .trim_end_matches(".git")
                .to_owned()
        })
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
    async fn renders_registry_metadata_and_readme() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/foo/latest"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "name": "foo",
                "version": "1.2.3",
                "description": "A demo package",
                "license": "MIT",
                "homepage": "https://example.com",
                "repository": {"url": "git+https://github.com/o/foo.git"},
                "keywords": ["demo"],
                "maintainers": [{"name": "alice"}],
                "dependencies": {"bar": "^1.0.0"},
                "readme": "# Foo\n\nUsage."
            })))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://www.npmjs.com/package/foo").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.method, "npm");
        assert!(result.raw_content.contains("# foo"));
        assert!(result.raw_content.contains("**Latest:** 1.2.3"));
        assert!(result.raw_content.contains("bar: ^1.0.0"));
        assert!(result.raw_content.contains("# Foo"));
    }
}

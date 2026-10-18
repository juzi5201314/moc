//! MDN Web Docs 的 `index.json` 结构化抓取。

use reqwest::Url;
use serde::Deserialize;
use serde_json::Value;

use super::super::ExtractResult;
use super::super::http::{HttpFetcher, WebFetchFailure};
use super::super::native::html_to_markdown;
use super::{SiteEndpoints, markdown_result, status_error};

#[derive(Debug, Deserialize)]
struct MdnEnvelope {
    doc: MdnDoc,
}

#[derive(Debug, Deserialize)]
struct MdnDoc {
    title: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    mdn_url: Option<String>,
    #[serde(default)]
    body: Vec<MdnSection>,
}

#[derive(Debug, Deserialize)]
struct MdnSection {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    value: Value,
}

pub(super) async fn extract(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
) -> Option<Result<ExtractResult, WebFetchFailure>> {
    if !url.path().contains("/docs/") {
        return None;
    }
    Some(fetch(url, http, endpoints).await)
}

async fn fetch(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
) -> Result<ExtractResult, WebFetchFailure> {
    let path = format!("{}/index.json", url.path().trim_end_matches('/'));
    let json_url = format!("{}{path}", endpoints.mdn.trim_end_matches('/'));
    let response = http
        .get(&json_url, &super::headers("application/json"))
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(status_error("MDN", response.status));
    }
    let envelope: MdnEnvelope = serde_json::from_slice(&response.bytes)
        .map_err(|error| WebFetchFailure::message(format!("MDN JSON: {error}")))?;
    let doc = envelope.doc;
    let mut markdown = format!("# {}\n\n", doc.title);
    if let Some(summary) = &doc.summary {
        push_html(&mut markdown, summary)?;
    }
    for section in &doc.body {
        render_section(&mut markdown, section)?;
    }
    Ok(markdown_result(
        url,
        doc.mdn_url,
        Some(200),
        "mdn",
        vec!["MDN JSON API".into()],
        markdown,
    ))
}

fn render_section(markdown: &mut String, section: &MdnSection) -> Result<(), WebFetchFailure> {
    let value = &section.value;
    match section.kind.as_str() {
        "prose" => {
            if let Some(title) = value.get("title").and_then(Value::as_str) {
                let level = if value.get("isH3").and_then(Value::as_bool).unwrap_or(false) {
                    "###"
                } else {
                    "##"
                };
                markdown.push_str(&format!("{level} {title}\n\n"));
            }
            if let Some(content) = value.get("content").and_then(Value::as_str) {
                push_html(markdown, content)?;
            }
        }
        "code_example" => {
            if let Some(title) = value.get("title").and_then(Value::as_str) {
                markdown.push_str(&format!("### {title}\n\n"));
            }
            if let Some(code) = value.get("code").and_then(Value::as_str) {
                let language = value.get("language").and_then(Value::as_str).unwrap_or("");
                markdown.push_str(&format!("```{language}\n{code}\n```\n\n"));
            }
        }
        "definition_list" => {
            if let Some(items) = value.get("items").and_then(Value::as_array) {
                for item in items {
                    let term = item.get("term").and_then(Value::as_str).unwrap_or("");
                    markdown.push_str(&format!("**{term}**\n\n"));
                    if let Some(description) = item.get("description").and_then(Value::as_str) {
                        push_html(markdown, description)?;
                    }
                }
            }
        }
        "table" => {
            if let Some(rows) = value.get("rows").and_then(Value::as_array)
                && !rows.is_empty()
            {
                let cells = |row: &Value| -> Vec<String> {
                    row.as_array()
                        .into_iter()
                        .flatten()
                        .map(|cell| {
                            cell.as_str()
                                .map(str::to_owned)
                                .unwrap_or_else(|| cell.to_string())
                        })
                        .collect()
                };
                let header = cells(&rows[0]);
                markdown.push_str(&format!("| {} |\n", header.join(" | ")));
                markdown.push_str(&format!(
                    "| {} |\n",
                    header.iter().map(|_| "---").collect::<Vec<_>>().join(" | ")
                ));
                for row in rows.iter().skip(1) {
                    markdown.push_str(&format!("| {} |\n", cells(row).join(" | ")));
                }
                markdown.push('\n');
            }
        }
        "browser_compatibility" | "specifications" => {
            let title = value
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("Reference");
            markdown.push_str(&format!("## {title}\n\n(See MDN.)\n\n"));
        }
        _ => {}
    }
    Ok(())
}

fn push_html(markdown: &mut String, html: &str) -> Result<(), WebFetchFailure> {
    let converted = html_to_markdown(html).map_err(WebFetchFailure::message)?;
    markdown.push_str(converted.trim());
    markdown.push_str("\n\n");
    Ok(())
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
    async fn renders_prose_and_code_examples() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/en-US/docs/Web/API/Fetch_API/index.json"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "doc": {
                    "title": "Fetch API",
                    "summary": "<p>The Fetch API.</p>",
                    "mdn_url": "https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API",
                    "body": [
                        {"type": "prose", "value": {"title": "Usage", "content": "<p>Use fetch.</p>"}},
                        {"type": "code_example", "value": {"title": "Example", "code": "fetch('/');", "language": "js"}}
                    ]
                }
            })))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://developer.mozilla.org/en-US/docs/Web/API/Fetch_API").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.method, "mdn");
        assert!(result.raw_content.contains("# Fetch API"));
        assert!(result.raw_content.contains("The Fetch API."));
        assert!(result.raw_content.contains("## Usage"));
        assert!(result.raw_content.contains("Use fetch."));
        assert!(result.raw_content.contains("### Example"));
        assert!(result.raw_content.contains("fetch('/');"));
    }
}

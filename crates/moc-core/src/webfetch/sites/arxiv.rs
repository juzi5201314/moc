//! arXiv Atom API 的结构化抓取；本轮不转换 PDF 全文。

use reqwest::Url;

use super::super::ExtractResult;
use super::super::http::{HttpFetcher, WebFetchFailure, decode_body};
use super::{SiteEndpoints, markdown_result, status_error};

pub(super) async fn extract(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
) -> Option<Result<ExtractResult, WebFetchFailure>> {
    let segments: Vec<&str> = url
        .path_segments()?
        .filter(|segment| !segment.is_empty())
        .collect();
    let kind = *segments.first()?;
    if !matches!(kind, "abs" | "pdf") {
        return None;
    }
    let mut paper = segments.get(1..)?.join("/");
    if let Some(stripped) = paper.strip_suffix(".pdf") {
        paper = stripped.to_owned();
    }
    if paper.is_empty() {
        None
    } else {
        Some(fetch(url, http, endpoints, &paper, kind == "pdf").await)
    }
}

async fn fetch(
    url: &Url,
    http: &HttpFetcher<'_>,
    endpoints: &SiteEndpoints,
    paper: &str,
    pdf_requested: bool,
) -> Result<ExtractResult, WebFetchFailure> {
    let api = format!(
        "{}/api/query?id_list={paper}",
        endpoints.arxiv_api.trim_end_matches('/')
    );
    let response = http
        .get(&api, &super::headers("application/atom+xml"))
        .await?;
    if !(200..300).contains(&response.status) {
        return Err(status_error("arXiv API", response.status));
    }
    let text = decode_body(&response.bytes, response.content_type.as_deref());
    let document = roxmltree::Document::parse(&text)
        .map_err(|error| WebFetchFailure::message(format!("arXiv XML: {error}")))?;
    let entry = document
        .descendants()
        .find(|node| node.has_tag_name("entry"))
        .ok_or_else(|| WebFetchFailure::message("arXiv entry missing"))?;

    let title = child_text(&entry, "title").unwrap_or_else(|| "arXiv Paper".into());
    let summary = child_text(&entry, "summary").unwrap_or_default();
    let published = child_text(&entry, "published")
        .map(|value| value.split('T').next().unwrap_or(&value).to_owned());
    let authors: Vec<String> = entry
        .children()
        .filter(|node| node.has_tag_name("author"))
        .filter_map(|node| child_text(&node, "name"))
        .collect();
    let categories: Vec<String> = entry
        .children()
        .filter(|node| node.has_tag_name("category"))
        .filter_map(|node| node.attribute("term").map(str::to_owned))
        .collect();
    let pdf = entry
        .children()
        .filter(|node| node.has_tag_name("link"))
        .find(|node| node.attribute("title") == Some("pdf"))
        .and_then(|node| node.attribute("href"));

    let mut markdown = format!("# {title}\n\n");
    if !authors.is_empty() {
        markdown.push_str(&format!("**Authors:** {}\n", authors.join(", ")));
    }
    if let Some(published) = &published {
        markdown.push_str(&format!("**Published:** {published}\n"));
    }
    if !categories.is_empty() {
        markdown.push_str(&format!("**Categories:** {}\n", categories.join(", ")));
    }
    markdown.push_str(&format!("**arXiv:** {paper}\n\n"));
    markdown.push_str("---\n\n## Abstract\n\n");
    markdown.push_str(summary.trim());
    markdown.push('\n');
    if let Some(pdf) = pdf {
        markdown.push_str(&format!("\n**PDF:** {pdf}\n"));
    }
    let mut notes = vec!["arXiv API".to_owned()];
    if pdf_requested {
        notes.push("PDF full text is not converted".into());
    }
    Ok(markdown_result(
        url,
        None,
        Some(200),
        "arxiv",
        notes,
        markdown,
    ))
}

fn child_text(node: &roxmltree::Node<'_, '_>, name: &str) -> Option<String> {
    let text = node
        .children()
        .find(|child| child.has_tag_name(name))
        .and_then(|child| child.text())?;
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::super::super::http::BatchBudget;
    use super::super::test_support::{endpoints, fetcher};
    use super::*;
    use tokio_util::sync::CancellationToken;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const ATOM: &str = r#"<feed xmlns="http://www.w3.org/2005/Atom">
        <entry>
            <title>Paper Title</title>
            <summary>Abstract text.</summary>
            <author><name>Alice</name></author>
            <author><name>Bob</name></author>
            <published>2024-01-02T03:04:05Z</published>
            <category term="cs.AI"/>
            <link title="pdf" href="https://arxiv.org/pdf/1234.5678"/>
        </entry>
    </feed>"#;

    #[tokio::test]
    async fn renders_atom_metadata() {
        let server = MockServer::builder().start().await;
        Mock::given(method("GET"))
            .and(path("/api/query"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(ATOM, "application/atom+xml"))
            .mount(&server)
            .await;
        let endpoints = endpoints(&server);
        let batch = BatchBudget::new(1024 * 1024);
        let cancel = CancellationToken::new();
        let http = fetcher(&batch, &cancel);
        let result = extract(
            &Url::parse("https://arxiv.org/abs/1234.5678").unwrap(),
            &http,
            &endpoints,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.method, "arxiv");
        assert!(result.raw_content.contains("# Paper Title"));
        assert!(result.raw_content.contains("Alice, Bob"));
        assert!(result.raw_content.contains("2024-01-02"));
        assert!(result.raw_content.contains("cs.AI"));
        assert!(result.raw_content.contains("Abstract text."));
    }
}

//! web_search 与 web_fetch 工具：Tavily 联网搜索与本地网页抓取（实现见 [`crate::websearch`]）。

use std::collections::HashSet;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, schema};
use crate::config::{ResolvedWebFetch, ResolvedWebSearch};
use crate::websearch::{ExtractRequest, SearchRequest, SearchResponse, TimeRange};

/// 每条结果的 snippet 截断长度（字符，非字节）。
const SNIPPET_MAX_CHARS: usize = 240;
const DEFAULT_RESULTS_LIMIT: u32 = 5;
const MAX_RESULTS_LIMIT: u32 = 20;
const MAX_URLS: usize = 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SearchArgs {
    query: String,
    recency: Option<Recency>,
    limit: Option<u32>,
}

#[derive(Debug, Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Recency {
    Day,
    Week,
    Month,
    Year,
}

impl From<Recency> for TimeRange {
    fn from(value: Recency) -> Self {
        match value {
            Recency::Day => TimeRange::Day,
            Recency::Week => TimeRange::Week,
            Recency::Month => TimeRange::Month,
            Recency::Year => TimeRange::Year,
        }
    }
}

pub struct WebSearchTool;

impl Tool for WebSearchTool {
    fn name(&self) -> &'static str {
        "web_search"
    }

    fn description(&self) -> &'static str {
        "Search the web for current information."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "web_search",
            schema(
                serde_json::json!({
                    "query": {
                        "type": "string",
                        "description": "Supports site:host, -site:host, after:YYYY-MM-DD, before:YYYY-MM-DD, \"phrases\", and -term."
                    },
                    "recency": {
                        "type": "string",
                        "enum": ["day", "week", "month", "year"],
                        "description": "Restrict to a recent window."
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Default 5."
                    }
                }),
                &["query"],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: SearchArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            ensure_search_enabled(&ctx.config.web_search)?;
            let query = args.query.trim().to_owned();
            if query.is_empty() {
                return Err(ToolError::new("E_BAD_PAYLOAD", "query must not be empty"));
            }
            let mut request = build_search_request(&query, args.recency, args.limit);
            let provider = crate::websearch::create_provider(&ctx.config.web_search)?;
            let response = crate::websearch::search_with_fallback(
                provider.as_ref(),
                &mut request,
                &ctx.cancel,
            )
            .await
            .map_err(crate::websearch::WebSearchError::into_tool_error)?;
            Ok(ToolOutcome::success(render_search(
                &request.query,
                &response,
            )))
        })
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    urls: Vec<String>,
    query: Option<String>,
}

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Extract page text of URLs as markdown. With query, return only the most relevant ~800-char chunks per page. Large results are archived; read the artifact URI for specific lines."
    }

    fn tier(&self) -> Tier {
        Tier::Read
    }

    fn parallel_safe(&self) -> bool {
        // 单页上限 50 MiB，多个 web_fetch 并行会放大峰值内存；批次内顺序执行。
        false
    }

    fn def(&self) -> ToolDef {
        ToolDef::new(
            "web_fetch",
            schema(
                serde_json::json!({
                    "urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "minItems": 1,
                        "maxItems": 20,
                        "description": "Duplicates are fetched once."
                    },
                    "query": {
                        "type": "string",
                        "description": "Relevance filter; omit for full page text."
                    }
                }),
                &["urls"],
            ),
        )
    }

    fn run<'a>(
        &'a self,
        ctx: &'a ToolCtx,
        args: Value,
    ) -> BoxFuture<'a, Result<ToolOutcome, ToolError>> {
        Box::pin(async move {
            let args: FetchArgs = serde_json::from_value(args)
                .map_err(|error| ToolError::new("E_BAD_PAYLOAD", error.to_string()))?;
            ensure_fetch_enabled(&ctx.config.web_fetch)?;
            let urls = normalize_urls(&args.urls)?;
            let query = args
                .query
                .map(|query| query.trim().to_owned())
                .filter(|query| !query.is_empty());
            let request = ExtractRequest { urls, query };
            let response = crate::websearch::extract_urls(
                &ctx.config.web_fetch,
                &request,
                &ctx.cancel,
                &ctx.paths.sidecar,
            )
            .await
            .map_err(crate::websearch::WebSearchError::into_tool_error)?;
            Ok(ToolOutcome::success(render_extract(&response)))
        })
    }
}

/// `web_search` 未启用或缺少 api_key 时拒绝执行。
fn ensure_search_enabled(config: &ResolvedWebSearch) -> Result<(), ToolError> {
    if config.available() {
        return Ok(());
    }
    Err(ToolError::new("E_TOOL_DISABLED", "web_search is disabled")
        .with_hint("Set [web_search] api_key in the moc config to enable web_search."))
}

/// `web_fetch` 关闭时拒绝执行。
fn ensure_fetch_enabled(config: &ResolvedWebFetch) -> Result<(), ToolError> {
    if config.enabled {
        return Ok(());
    }
    Err(ToolError::new("E_TOOL_DISABLED", "web_fetch is disabled")
        .with_hint("Set [web_fetch] enabled=true in the moc config to enable web_fetch."))
}

/// 组装搜索请求：解析查询里的操作符，并把 limit 限制在有效范围内。
fn build_search_request(
    query: &str,
    recency: Option<Recency>,
    limit: Option<u32>,
) -> SearchRequest {
    let parsed = parse_query_ops(query);
    SearchRequest {
        query: parsed.query,
        max_results: limit
            .unwrap_or(DEFAULT_RESULTS_LIMIT)
            .clamp(1, MAX_RESULTS_LIMIT),
        include_domains: parsed.include_domains,
        exclude_domains: parsed.exclude_domains,
        start_date: parsed.start_date,
        end_date: parsed.end_date,
        time_range: recency.map(TimeRange::from),
    }
}

/// 去掉空白 URL 并保序去重；剩下的数量必须在 1..=20 之间。
fn normalize_urls(urls: &[String]) -> Result<Vec<String>, ToolError> {
    let mut seen = HashSet::with_capacity(urls.len());
    let mut normalized = Vec::with_capacity(urls.len());
    for url in urls {
        let url = url.trim();
        if !url.is_empty() && seen.insert(url) {
            normalized.push(url.to_owned());
        }
    }
    if normalized.is_empty() || normalized.len() > MAX_URLS {
        return Err(ToolError::new(
            "E_BAD_PAYLOAD",
            format!("urls must contain 1..={MAX_URLS} non-empty URLs after trimming"),
        ));
    }
    Ok(normalized)
}

/// 渲染搜索结果：answer 原文在前，后面是来源数量和编号列表。
fn render_search(query: &str, response: &SearchResponse) -> String {
    if response.answer.is_none() && response.results.is_empty() {
        return format!("No results found for: {query}");
    }
    let mut text = String::new();
    if let Some(answer) = &response.answer {
        text.push_str(answer);
        text.push_str("\n\n");
    }
    text.push_str(&format!("## Sources ({})\n", response.results.len()));
    for (index, result) in response.results.iter().enumerate() {
        text.push_str(&format!("[{}] {}", index + 1, result.title));
        if let Some(date) = &result.published_date {
            text.push_str(&format!(" ({date})"));
        }
        text.push('\n');
        text.push_str(&format!("    {}\n", result.url));
        if let Some(snippet) = &result.snippet {
            text.push_str(&format!(
                "    {}\n",
                truncate_chars(snippet, SNIPPET_MAX_CHARS)
            ));
        }
    }
    text.trim_end().to_owned()
}

/// 渲染提取结果：每个成功项一段正文，失败项在末尾逐行列出；全部失败也按成功返回。
fn render_extract(response: &crate::websearch::ExtractResponse) -> String {
    let mut text = String::new();
    for result in &response.results {
        text.push_str(&format!("## {}\n", result.url));
        if result.final_url != result.url {
            text.push_str(&format!("Final: {}\n", result.final_url));
        }
        let mut meta = Vec::new();
        if let Some(status) = result.status {
            meta.push(format!("HTTP {status}"));
        }
        if let Some(content_type) = &result.content_type {
            meta.push(content_type.clone());
        }
        meta.push(result.method.to_owned());
        text.push_str(&meta.join(" · "));
        text.push('\n');
        if !result.notes.is_empty() {
            text.push_str(&format!("Notes: {}\n", result.notes.join("; ")));
        }
        if result.truncated {
            text.push_str("Truncated: yes\n");
        }
        text.push('\n');
        text.push_str(&result.raw_content);
        text.push_str("\n\n");
    }
    for failed in &response.failed {
        text.push_str(&format!("Failed: {} ({})\n", failed.url, failed.error));
    }
    let text = text.trim_end();
    if text.is_empty() {
        "No content extracted.".to_owned()
    } else {
        text.to_owned()
    }
}

/// 按字符（不是字节）截断到 `max`；不足 `max` 时原样返回。
fn truncate_chars(value: &str, max: usize) -> &str {
    match value.char_indices().nth(max) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

/// [`parse_query_ops`] 的解析结果；操作符已从查询文本里去掉。
#[derive(Debug, Default, PartialEq, Eq)]
struct ParsedQuery {
    query: String,
    include_domains: Vec<String>,
    exclude_domains: Vec<String>,
    start_date: Option<String>,
    end_date: Option<String>,
}

/// 解析查询里的域名和时间操作符，其余记号重新拼成查询文本。
///
/// 识别 `site:host`、`-site:host`（host 只保留域名）以及 `after:YYYY-MM-DD`、
/// `before:YYYY-MM-DD`；只认小写前缀，校验不通过的记号原样保留。
fn parse_query_ops(input: &str) -> ParsedQuery {
    let mut parsed = ParsedQuery::default();
    let mut kept: Vec<&str> = Vec::new();
    for token in split_query_tokens(input) {
        if let Some(host) = token.strip_prefix("-site:").map(site_host)
            && !host.is_empty()
        {
            parsed.exclude_domains.push(host.to_owned());
            continue;
        }
        if let Some(host) = token.strip_prefix("site:").map(site_host)
            && !host.is_empty()
        {
            parsed.include_domains.push(host.to_owned());
            continue;
        }
        if let Some(date) = token.strip_prefix("after:")
            && is_iso_date(date)
        {
            parsed.start_date = Some(date.to_owned());
            continue;
        }
        if let Some(date) = token.strip_prefix("before:")
            && is_iso_date(date)
        {
            parsed.end_date = Some(date.to_owned());
            continue;
        }
        kept.push(token);
    }
    parsed.query = kept.join(" ");
    parsed
}

/// 按空白切分记号；引号包住的短语（连同引号）算一个记号。
fn split_query_tokens(input: &str) -> Vec<&str> {
    let mut tokens = Vec::new();
    let mut rest = input;
    loop {
        let trimmed = rest.trim_start();
        if trimmed.is_empty() {
            return tokens;
        }
        if let Some(quoted) = trimmed.strip_prefix('"') {
            let Some(end) = quoted.find('"') else {
                // 引号没闭合：剩下的整段算一个记号。
                tokens.push(trimmed);
                return tokens;
            };
            tokens.push(&trimmed[..end + 2]);
            rest = &trimmed[end + 2..];
            continue;
        }
        let end = trimmed.find(char::is_whitespace).unwrap_or(trimmed.len());
        tokens.push(&trimmed[..end]);
        rest = &trimmed[end..];
    }
}

/// `site:` 的值只保留域名，去掉路径和查询串。
fn site_host(value: &str) -> &str {
    value.split('/').next().unwrap_or_default()
}

/// 只检查字面格式是否为 `YYYY-MM-DD`，不校验日期是否真实存在。
fn is_iso_date(value: &str) -> bool {
    value.len() == 10
        && value.bytes().enumerate().all(|(index, byte)| match index {
            4 | 7 => byte == b'-',
            _ => byte.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::websearch::{ExtractFailed, ExtractResponse, ExtractResult};

    #[test]
    fn parses_site_and_date_operators() {
        let parsed = parse_query_ops(
            "site:example.com -site:spam.example after:2025-01-02 before:2025-02-03 rust async",
        );
        assert_eq!(parsed.query, "rust async");
        assert_eq!(parsed.include_domains, ["example.com"]);
        assert_eq!(parsed.exclude_domains, ["spam.example"]);
        assert_eq!(parsed.start_date.as_deref(), Some("2025-01-02"));
        assert_eq!(parsed.end_date.as_deref(), Some("2025-02-03"));
    }

    #[test]
    fn clamps_limit_and_applies_recency() {
        let request = build_search_request("rust", Some(Recency::Week), Some(100));
        assert_eq!(request.max_results, 20);
        assert_eq!(request.time_range, Some(TimeRange::Week));
        assert_eq!(build_search_request("rust", None, Some(0)).max_results, 1);
        assert_eq!(build_search_request("rust", None, None).max_results, 5);
    }

    #[test]
    fn truncates_snippets_by_chars() {
        let long = "x".repeat(300);
        assert_eq!(
            truncate_chars(&long, SNIPPET_MAX_CHARS).len(),
            SNIPPET_MAX_CHARS
        );
        assert_eq!(truncate_chars("héllo", 2), "hé");
        assert_eq!(truncate_chars("abc", 5), "abc");
    }

    #[test]
    fn renders_extract_sections_and_failures() {
        let response = ExtractResponse {
            results: vec![ExtractResult {
                url: "https://a.example".into(),
                final_url: "https://a.example".into(),
                status: Some(200),
                content_type: Some("text/markdown".into()),
                method: "generic",
                notes: Vec::new(),
                truncated: false,
                raw_content: "body".into(),
            }],
            failed: vec![ExtractFailed {
                url: "https://b.example".into(),
                error: "timeout".into(),
            }],
        };
        assert_eq!(
            render_extract(&response),
            "## https://a.example\nHTTP 200 · text/markdown · generic\n\nbody\n\nFailed: https://b.example (timeout)"
        );

        let redirected = ExtractResponse {
            results: vec![ExtractResult {
                url: "https://a.example".into(),
                final_url: "https://a.example/final".into(),
                status: Some(200),
                content_type: Some("text/markdown".into()),
                method: "github",
                notes: vec!["GitHub API".into()],
                truncated: true,
                raw_content: "body".into(),
            }],
            failed: Vec::new(),
        };
        assert_eq!(
            render_extract(&redirected),
            "## https://a.example\nFinal: https://a.example/final\nHTTP 200 · text/markdown · github\nNotes: GitHub API\nTruncated: yes\n\nbody"
        );
        let empty = ExtractResponse {
            results: Vec::new(),
            failed: Vec::new(),
        };
        assert_eq!(render_extract(&empty), "No content extracted.");
    }

    #[test]
    fn rejects_unknown_arguments() {
        assert!(
            serde_json::from_value::<SearchArgs>(serde_json::json!({"query": "q", "extra": 1}))
                .is_err()
        );
        assert!(
            serde_json::from_value::<FetchArgs>(
                serde_json::json!({"urls": ["https://a.example"], "extra": 1})
            )
            .is_err()
        );
        assert!(
            serde_json::from_value::<SearchArgs>(
                serde_json::json!({"query": "q", "recency": "hour"})
            )
            .is_err()
        );
    }
}

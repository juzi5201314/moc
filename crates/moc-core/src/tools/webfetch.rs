//! web_fetch 工具：本地网页抓取（实现见 [`crate::webfetch`]）。

use std::collections::HashSet;

use futures_util::future::BoxFuture;
use moc_llm::ToolDef;
use serde::Deserialize;
use serde_json::Value;

use super::{Tier, Tool, ToolCtx, ToolError, ToolOutcome, ToolOutput, schema};
use crate::config::ResolvedWebFetch;
use crate::webfetch::ExtractRequest;

const MAX_URLS: usize = 20;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FetchArgs {
    urls: Vec<String>,
    query: Option<String>,
    #[serde(default)]
    refresh: bool,
}

pub struct WebFetchTool;

impl Tool for WebFetchTool {
    fn name(&self) -> &'static str {
        "web_fetch"
    }

    fn description(&self) -> &'static str {
        "Fetch and save page documents as session artifacts; returns bounded outline or query excerpts with line numbers and URIs. Read/grep artifacts for full details; query is not an exhaustive inventory."
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
                        "description": "URLs to read; repeated URLs reuse saved session documents."
                    },
                    "query": {
                        "type": "string",
                        "description": "Select relevant sections from the saved document; omit for an outline and preview."
                    },
                    "refresh": {
                        "type": "boolean",
                        "description": "Fetch a new snapshot only when current website content is required; default false."
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
            let request = ExtractRequest {
                urls,
                refresh: args.refresh,
            };
            let mut response = crate::webfetch::extract_urls(
                &ctx.config.web_fetch,
                &request,
                &ctx.cancel,
                &ctx.paths.sidecar,
            )
            .await
            .map_err(crate::webfetch::WebFetchError::into_tool_error)?;
            crate::webfetch::view::mark_seen(&ctx.paths.sidecar, &mut response, query.as_deref())
                .await
                .map_err(|error| ToolError::new("E_ARTIFACT", error.to_string()))?;
            let text = crate::webfetch::view::render(
                &response,
                query.as_deref(),
                ctx.config.web_fetch.max_chars,
            );
            let content = if let [page] = response.results.as_slice() {
                let resource = ctx
                    .artifact_store()
                    .resolve_uri(page.artifact.as_deref().expect("网页已归档"))
                    .map_err(|error| ToolError::new(error.code, error.message))?;
                let crate::resources::ResourceBacking::File(path) = resource.backing else {
                    unreachable!("网页快照是文件");
                };
                ToolOutput::Excerpt { text, path }
            } else {
                ToolOutput::Verbatim(text)
            };
            let mut outcome = ToolOutcome::success(content);
            outcome.structured_content = Some(
                serde_json::json!({"pages": response.results.iter().map(|page| {
                serde_json::json!({"url": page.url, "artifact": page.artifact,
                    "cached": page.cached, "source_truncated": page.truncated,
                    "lines": page.raw_content.lines().count()})
            }).collect::<Vec<_>>()}),
            );
            Ok(outcome)
        })
    }
}

/// `web_fetch` 关闭时拒绝执行。
fn ensure_fetch_enabled(config: &ResolvedWebFetch) -> Result<(), ToolError> {
    if config.enabled {
        return Ok(());
    }
    Err(ToolError::new("E_TOOL_DISABLED", "web_fetch is disabled")
        .with_hint("Set [web_fetch] enabled=true in the moc config to enable web_fetch."))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_unknown_arguments() {
        assert!(
            serde_json::from_value::<FetchArgs>(
                serde_json::json!({"urls": ["https://a.example"], "extra": 1})
            )
            .is_err()
        );
    }
}

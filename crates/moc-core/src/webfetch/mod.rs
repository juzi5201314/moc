//! web_fetch 抓取抽象：请求归一化、抓取结果与统一错误。
//!
//! 抓取走本地 `native` 与 `sites` 站点 handler，工具层只依赖本模块的类型与入口。

mod documents;
mod html;
mod http;
mod native;
mod sites;
pub(crate) mod view;

use std::sync::LazyLock;

use tokio_util::sync::CancellationToken;

use crate::config::ResolvedWebFetch;
use crate::tools::ToolError;

#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub urls: Vec<String>,
    pub refresh: bool,
}

/// 单页抓取结果；结果头由 `tools::webfetch` 渲染给模型。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ExtractResult {
    pub url: String,
    pub final_url: String,
    /// 保存正文时提取的标题，不受展示选段影响。
    pub title: Option<String>,
    pub status: Option<u16>,
    pub content_type: Option<String>,
    /// generic / github / docs.rs / mdn / arxiv / npm / pypi
    pub method: String,
    pub notes: Vec<String>,
    pub truncated: bool,
    pub artifact: Option<String>,
    #[serde(skip)]
    pub cached: bool,
    #[serde(skip)]
    pub view_repeated: bool,
    #[serde(skip)]
    pub raw_content: String,
}

#[derive(Debug, Clone)]
pub struct ExtractFailed {
    pub url: String,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct ExtractResponse {
    pub results: Vec<ExtractResult>,
    pub failed: Vec<ExtractFailed>,
}

/// 抓取边界上的结构化错误；由工具层转成 [`ToolError`]。
#[derive(Debug, Clone)]
pub struct WebFetchError {
    pub code: &'static str,
    pub message: String,
}

impl WebFetchError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    /// 转为工具边界错误；`E_CANCELLED` 保留原有取消语义。
    pub fn into_tool_error(self) -> ToolError {
        ToolError::new(self.code, self.message)
    }
}

/// 本地抓取入口：所有 URL 都走 `NativeFetcher`，站点 handler 在其内部完成。
pub async fn extract_urls(
    config: &ResolvedWebFetch,
    request: &ExtractRequest,
    cancel: &CancellationToken,
    cache_dir: &std::path::Path,
) -> Result<ExtractResponse, WebFetchError> {
    native::NativeFetcher::new(config, cache_dir)
        .extract(request, cancel)
        .await
}

/// 全部抓取请求共享的连接池；请求级超时由各请求自身设置。
pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(format!("moc/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("构建 reqwest 客户端失败")
    });
    &CLIENT
}

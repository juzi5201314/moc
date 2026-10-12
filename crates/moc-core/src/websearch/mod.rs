//! web search / web fetch 供应商抽象：请求归一化、提取与统一错误。
//!
//! 当前只有 Tavily 一个实现；工具层只依赖本模块的类型与 [`WebSearchProvider`]，
//! 新增供应商时实现 trait 并扩展 [`create_provider`] 即可。

mod tavily;

use std::sync::{Arc, LazyLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio_util::sync::CancellationToken;

use crate::config::ResolvedWebSearch;
use crate::tools::ToolError;

/// 搜索时间范围；与 Tavily `time_range` 参数一一对应。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TimeRange {
    Day,
    Week,
    Month,
    Year,
}

impl TimeRange {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
            Self::Month => "month",
            Self::Year => "year",
        }
    }
}

/// 归一化后的搜索请求；`time_range` 与 `start_date/end_date` 只发送其一。
#[derive(Debug, Clone)]
pub struct SearchRequest {
    pub query: String,
    pub max_results: u32,
    pub include_domains: Vec<String>,
    pub exclude_domains: Vec<String>,
    pub start_date: Option<String>,
    pub end_date: Option<String>,
    pub time_range: Option<TimeRange>,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: Option<String>,
    /// 供应商给出的原文日期（ISO 形式），不做相对时间换算。
    pub published_date: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SearchResponse {
    pub answer: Option<String>,
    pub results: Vec<SearchResult>,
}

#[derive(Debug, Clone)]
pub struct ExtractRequest {
    pub urls: Vec<String>,
    pub query: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ExtractResult {
    pub url: String,
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

/// 供应商边界上的结构化错误；由工具层转成 [`ToolError`]。
#[derive(Debug, Clone)]
pub struct WebSearchError {
    pub code: &'static str,
    pub message: String,
    pub hint: Option<String>,
}

impl WebSearchError {
    pub(crate) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            hint: None,
        }
    }

    pub(crate) fn with_hint(mut self, hint: impl Into<String>) -> Self {
        self.hint = Some(hint.into());
        self
    }

    /// 转为工具边界错误；`E_CANCELLED` 保留原有取消语义。
    pub fn into_tool_error(self) -> ToolError {
        match self.hint {
            Some(hint) => ToolError::new(self.code, self.message).with_hint(hint),
            None => ToolError::new(self.code, self.message),
        }
    }
}

pub trait WebSearchProvider: Send + Sync {
    fn search<'a>(
        &'a self,
        request: &'a SearchRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<SearchResponse, WebSearchError>>;

    fn extract<'a>(
        &'a self,
        request: &'a ExtractRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<ExtractResponse, WebSearchError>>;
}

/// 带时间过滤的查询零结果时去掉时间过滤重试一次，省一轮模型往返。
pub async fn search_with_fallback(
    provider: &dyn WebSearchProvider,
    request: &mut SearchRequest,
    cancel: &CancellationToken,
) -> Result<SearchResponse, WebSearchError> {
    let response = provider.search(request, cancel).await?;
    let has_time_filter =
        request.time_range.is_some() || request.start_date.is_some() || request.end_date.is_some();
    if response.answer.is_some() || !response.results.is_empty() || !has_time_filter {
        return Ok(response);
    }
    request.start_date = None;
    request.end_date = None;
    request.time_range = None;
    provider.search(request, cancel).await
}

/// 按配置创建供应商实例；未知供应商返回 `E_TOOL_DISABLED`。
pub fn create_provider(
    config: &ResolvedWebSearch,
) -> Result<Arc<dyn WebSearchProvider>, ToolError> {
    match config.provider.as_str() {
        "tavily" => Ok(Arc::new(tavily::TavilyProvider::new(
            &config.base_url,
            config.api_key.clone().unwrap_or_default(),
            Duration::from_secs(config.timeout_secs),
        ))),
        other => Err(ToolError::new(
            "E_TOOL_DISABLED",
            format!("未知 web_search 供应商: {other}"),
        )),
    }
}

/// 全部供应商共享的连接池；请求级超时由各请求自身设置。
pub(crate) fn client() -> &'static reqwest::Client {
    static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
        reqwest::Client::builder()
            .user_agent(format!("moc/{}", env!("CARGO_PKG_VERSION")))
            .build()
            .expect("构建 reqwest 客户端失败")
    });
    &CLIENT
}

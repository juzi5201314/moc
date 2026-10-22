//! 抓取共用的 HTTP 层：有界读取、取消、批预算与字符集解码。

use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reqwest::header::{CONTENT_TYPE, HeaderMap};
use tokio_util::sync::CancellationToken;

/// 一次 `web_fetch` 调用所有 URL 合计的读取上限，避免 20 × max_bytes 的峰值。
pub(super) const MAX_BATCH_BYTES: usize = 100 * 1024 * 1024;

/// 单 URL 抓取失败：取消必须中止整个工具调用，普通失败只记一条 `Failed`。
#[derive(Debug)]
pub(super) enum WebFetchFailure {
    Cancelled,
    Message(String),
}

impl WebFetchFailure {
    pub(super) fn message(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }
}

/// 并发任务共享的字节预算；按实际读取字节扣减。
pub(super) struct BatchBudget {
    remaining: AtomicUsize,
}

impl BatchBudget {
    pub(super) fn new(limit: usize) -> Self {
        Self {
            remaining: AtomicUsize::new(limit),
        }
    }

    /// 预留 `want` 字节，返回实际拿到的数量；返回 0 表示预算耗尽。
    pub(super) fn reserve(&self, want: usize) -> usize {
        let mut current = self.remaining.load(Ordering::Relaxed);
        loop {
            let granted = current.min(want);
            if granted == 0 {
                return 0;
            }
            match self.remaining.compare_exchange_weak(
                current,
                current - granted,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return granted,
                Err(actual) => current = actual,
            }
        }
    }
}

pub(super) struct HttpFetcher<'a> {
    pub timeout: Duration,
    pub max_bytes: usize,
    pub batch: &'a BatchBudget,
    pub cancel: &'a CancellationToken,
}

pub(super) struct HttpResponse {
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
    pub truncated: bool,
}

impl HttpFetcher<'_> {
    /// 发一次 GET；非 2xx 也正常返回，由调用方决定是否失败。
    pub(super) async fn get(
        &self,
        url: &str,
        headers: &HeaderMap,
    ) -> Result<HttpResponse, WebFetchFailure> {
        let request = super::client()
            .get(url)
            .timeout(self.timeout)
            .headers(headers.clone());
        let response = tokio::select! {
            biased;
            _ = self.cancel.cancelled() => return Err(WebFetchFailure::Cancelled),
            response = request.send() => response.map_err(|error| {
                if error.is_timeout() {
                    WebFetchFailure::message(format!(
                        "timeout after {}s",
                        self.timeout.as_secs()
                    ))
                } else {
                    WebFetchFailure::message(format!("request failed: {error}"))
                }
            })?,
        };
        let status = response.status().as_u16();
        let final_url = response.url().to_string();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let (bytes, truncated) =
            read_body(response, self.max_bytes, self.batch, self.cancel).await?;
        Ok(HttpResponse {
            status,
            final_url,
            content_type,
            bytes,
            truncated,
        })
    }
}

/// 流式读取到单页上限与批预算上限，任一触发都标记截断。
async fn read_body(
    mut response: reqwest::Response,
    max_bytes: usize,
    batch: &BatchBudget,
    cancel: &CancellationToken,
) -> Result<(Vec<u8>, bool), WebFetchFailure> {
    let mut bytes = Vec::new();
    let mut truncated = false;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(WebFetchFailure::Cancelled),
            chunk = response.chunk() => chunk.map_err(|error| {
                WebFetchFailure::message(format!("reading response failed: {error}"))
            })?,
        };
        let Some(chunk) = chunk else {
            break;
        };
        let page_remaining = max_bytes.saturating_sub(bytes.len());
        if page_remaining == 0 {
            truncated = true;
            break;
        }
        let want = chunk.len().min(page_remaining);
        let granted = batch.reserve(want);
        if granted == 0 {
            truncated = true;
            break;
        }
        bytes.extend_from_slice(&chunk[..granted]);
        if granted < want {
            truncated = true;
            break;
        }
        if bytes.len() >= max_bytes {
            truncated = true;
            break;
        }
    }
    Ok((bytes, truncated))
}

/// Content-Type 的 essence（小写、去掉参数）。
pub(super) fn content_type_essence(content_type: Option<&str>) -> Option<String> {
    content_type
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase)
}

/// 按 Content-Type 声明的 charset 解码；缺失或非法时 UTF-8 lossy。
pub(super) fn decode_body(bytes: &[u8], content_type: Option<&str>) -> String {
    let label = content_type
        .and_then(|value| {
            value.split(';').skip(1).find_map(|part| {
                let (name, value) = part.split_once('=')?;
                name.trim()
                    .eq_ignore_ascii_case("charset")
                    .then(|| value.trim().trim_matches('"'))
            })
        })
        .filter(|label| !label.is_empty());
    let encoding = label
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = encoding.decode(bytes);
    text.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_budget_never_overdraws() {
        let budget = BatchBudget::new(10);
        assert_eq!(budget.reserve(4), 4);
        assert_eq!(budget.reserve(9), 6);
        assert_eq!(budget.reserve(1), 0);
    }
}

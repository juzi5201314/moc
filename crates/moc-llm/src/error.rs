//! 错误类型与分类。
//!
//! 公开错误类型是 [`LlmError`]；`anyhow` 只用于 [`crate::Auth::from_env`] 这类
//! 边界辅助函数与测试。

use std::fmt;
use std::time::Duration;

/// 统一错误类型。
///
/// 实现 `Clone` 是因为流式错误需要同时交付给 [`crate::ChatStream::next_event`]
/// 与 [`crate::ChatStream::finish`]，调用方读到一个也不会让另一个丢失信息。
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum LlmError {
    /// 构造请求失败：请求类型无法表达为该协议的线上格式（历史不完整、参数缺失等）。
    #[error("构建请求失败: {0}")]
    Encode(String),
    /// 网络传输失败。
    #[error("网络传输失败: {0}")]
    Transport(TransportError),
    /// 响应体解析失败。
    #[error("响应体解析失败: {message}")]
    Decode {
        /// 失败原因。
        message: String,
        /// 原始响应体，便于定位上游格式变化。
        body: Option<Box<str>>,
    },
    /// 上游返回错误响应。
    #[error("上游返回错误: {0}")]
    Api(ApiError),
    /// 流式响应异常（SSE 帧结构错误、流中断等）。
    #[error("流式响应异常: {0}")]
    Stream(String),
    /// 本地循环保护停止；不得作为传输失败重试。
    #[error(transparent)]
    Guard(#[from] crate::LoopViolation),
    /// 重试预算耗尽。
    #[error("共尝试 {attempts} 次（含首次）后仍失败: {last}")]
    RetriesExhausted {
        /// 总尝试次数，含首次：`attempts == 3` 表示首发一次、重试两次。
        attempts: u32,
        /// 最后一次失败原因。
        #[source]
        last: Box<LlmError>,
    },
}

impl LlmError {
    /// 是否值得原样重放。
    ///
    /// 只有「请求未被上游完整处理」的错误返回真：
    ///
    /// - 传输层的连接失败与超时（请求尚未送达）；
    /// - 上游明确标记为可重试的错误（限流、过载、5xx）。
    ///
    /// 响应体截断（`reqwest::Error::is_body`）不在此列：上游可能已经完成处理，
    /// 重放会重复计费，恢复策略必须由调用方基于自身历史决定。流式请求开始读体后
    /// 从不重试，同理。
    pub fn is_retryable(&self) -> bool {
        match self {
            LlmError::Transport(err) => err.is_retryable(),
            LlmError::Api(err) => err.is_retryable(),
            LlmError::RetriesExhausted { .. }
            | LlmError::Encode(_)
            | LlmError::Decode { .. }
            | LlmError::Stream(_)
            | LlmError::Guard(_) => false,
        }
    }

    /// 展开重试包装后的上游错误。
    pub fn api_error(&self) -> Option<&ApiError> {
        match self {
            LlmError::Api(err) => Some(err),
            LlmError::RetriesExhausted { last, .. } => last.api_error(),
            _ => None,
        }
    }

    /// 上游 HTTP 状态码；无状态码（传输失败、流中错误事件）时为 `None`。
    pub fn status(&self) -> Option<u16> {
        self.api_error().and_then(|err| err.status)
    }
}

/// 传输层失败。
///
/// 不直接持有 `reqwest::Error`：它不是 `Clone`，而流式错误需要同时交付给
/// [`crate::ChatStream::next_event`] 与 [`crate::ChatStream::finish`]。这里保留
/// 分类标志与完整消息，两者足以支撑重试判定与排障。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransportError {
    /// 失败原因分类。
    kind: TransportKind,
    /// 完整错误消息（含 `reqwest` 给出的底层原因）。
    message: String,
}

impl TransportError {
    /// 失败原因分类。
    pub fn kind(&self) -> TransportKind {
        self.kind
    }

    /// 是否值得重放：请求尚未送达上游时才算。
    pub fn is_retryable(&self) -> bool {
        matches!(self.kind, TransportKind::Connect | TransportKind::Timeout)
    }

    /// 错误消息。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl From<reqwest::Error> for TransportError {
    fn from(error: reqwest::Error) -> Self {
        let kind = if error.is_connect() {
            TransportKind::Connect
        } else if error.is_timeout() {
            TransportKind::Timeout
        } else if error.is_body() {
            TransportKind::Body
        } else {
            TransportKind::Other
        };
        Self {
            kind,
            message: error.to_string(),
        }
    }
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for TransportError {}

impl From<reqwest::Error> for LlmError {
    fn from(error: reqwest::Error) -> Self {
        LlmError::Transport(error.into())
    }
}

/// 传输失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportKind {
    /// 连接建立失败（DNS、TCP、TLS）。
    Connect,
    /// 超时。
    Timeout,
    /// 响应体传输中断。
    Body,
    /// 其余传输失败。
    Other,
}

/// 上游错误响应。
#[derive(Debug, Clone)]
pub struct ApiError {
    /// HTTP 状态码；流中 error 事件可能没有状态码。
    pub status: Option<u16>,
    /// 错误分类。
    pub kind: ApiErrorKind,
    /// 上游错误消息原文。
    pub message: String,
    /// `error.code` / `error.type` 原文。
    pub code: Option<String>,
    /// 上游请求 ID，便于向其排查。
    pub request_id: Option<String>,
    /// 上游要求的等待时长（`retry-after` 头）。
    pub retry_after: Option<Duration>,
    /// 原始错误响应体。
    pub raw_body: Box<str>,
}

impl ApiError {
    /// 构造上游错误；`raw_body` 为原始响应体，解析失败时也照样保留。
    pub fn new(
        status: Option<u16>,
        kind: ApiErrorKind,
        message: impl Into<String>,
        raw_body: impl Into<Box<str>>,
    ) -> Self {
        Self {
            status,
            kind,
            message: message.into(),
            code: None,
            request_id: None,
            retry_after: None,
            raw_body: raw_body.into(),
        }
    }

    /// 是否值得重放（额度耗尽不重试，等待不会改变结果）。
    pub fn is_retryable(&self) -> bool {
        self.kind.is_retryable()
    }
}

impl fmt::Display for ApiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.status {
            Some(status) => write!(f, "HTTP {status} {}", self.kind),
            None => f.write_str(self.kind.as_str()),
        }?;
        if !self.message.is_empty() {
            write!(f, ": {}", self.message)?;
        }
        if let Some(code) = &self.code {
            write!(f, " (code={code})")?;
        }
        if let Some(request_id) = &self.request_id {
            write!(f, " (request_id={request_id})")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

/// 上游错误分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ApiErrorKind {
    /// 认证失败（401）。
    Auth,
    /// 权限不足（403）。
    Permission,
    /// 计费问题（402）。
    Billing,
    /// 限流（429）。
    RateLimit,
    /// 额度耗尽（429 + `insufficient_quota` / `enforced_spend_limit_reached`）。
    QuotaExceeded,
    /// 上下文超限（400/422 + 相关关键词）。
    ContextLength,
    /// 内容被安全策略拦截。
    ContentFilter,
    /// 请求参数非法（400/422）。
    InvalidRequest,
    /// 资源不存在（404）。
    NotFound,
    /// 请求体过大（413）。
    RequestTooLarge,
    /// 状态冲突（409）。
    Conflict,
    /// 上游过载（529）。
    Overloaded,
    /// 上游服务错误（500/502/503）。
    ServerError,
    /// 上游超时（504）。
    Timeout,
    /// 其余状态码。
    Unknown,
}

impl ApiErrorKind {
    /// 状态码到分类的兜底映射；协议解码器可依据错误体进一步细化
    /// （见 [`ApiErrorKind::refine`]）。
    pub fn from_status(status: u16) -> Self {
        match status {
            400 | 422 => ApiErrorKind::InvalidRequest,
            401 => ApiErrorKind::Auth,
            402 => ApiErrorKind::Billing,
            403 => ApiErrorKind::Permission,
            404 => ApiErrorKind::NotFound,
            409 => ApiErrorKind::Conflict,
            413 => ApiErrorKind::RequestTooLarge,
            429 => ApiErrorKind::RateLimit,
            500 => ApiErrorKind::ServerError,
            502 | 503 => ApiErrorKind::ServerError,
            504 => ApiErrorKind::Timeout,
            529 => ApiErrorKind::Overloaded,
            _ => ApiErrorKind::Unknown,
        }
    }

    /// 是否值得重放。
    pub fn is_retryable(self) -> bool {
        matches!(
            self,
            ApiErrorKind::RateLimit
                | ApiErrorKind::Overloaded
                | ApiErrorKind::ServerError
                | ApiErrorKind::Timeout
        )
    }

    /// 分类名，供日志与 UI 展示。
    pub fn as_str(self) -> &'static str {
        match self {
            ApiErrorKind::Auth => "Auth",
            ApiErrorKind::Permission => "Permission",
            ApiErrorKind::Billing => "Billing",
            ApiErrorKind::RateLimit => "RateLimit",
            ApiErrorKind::QuotaExceeded => "QuotaExceeded",
            ApiErrorKind::ContextLength => "ContextLength",
            ApiErrorKind::ContentFilter => "ContentFilter",
            ApiErrorKind::InvalidRequest => "InvalidRequest",
            ApiErrorKind::NotFound => "NotFound",
            ApiErrorKind::RequestTooLarge => "RequestTooLarge",
            ApiErrorKind::Conflict => "Conflict",
            ApiErrorKind::Overloaded => "Overloaded",
            ApiErrorKind::ServerError => "ServerError",
            ApiErrorKind::Timeout => "Timeout",
            ApiErrorKind::Unknown => "Unknown",
        }
    }

    /// 依据错误体文本细化分类，未命中关键词时原样返回。
    ///
    /// 关键词匹配在错误路径上执行，允许为此分配一次小写化的副本。
    pub(crate) fn refine(self, code: Option<&str>, message: &str) -> Self {
        let code_lower = code.unwrap_or_default().to_ascii_lowercase();
        let haystack = if message.is_empty() {
            code_lower
        } else {
            let mut buf = String::with_capacity(message.len() + code_lower.len() + 1);
            buf.push_str(&code_lower);
            buf.push(' ');
            buf.push_str(&message.to_ascii_lowercase());
            buf
        };

        const QUOTA: [&str; 2] = ["insufficient_quota", "enforced_spend_limit_reached"];
        const CONTEXT: [&str; 6] = [
            "context length",
            "context_length",
            "context window",
            "maximum context",
            "too many tokens",
            "prompt is too long",
        ];
        const FILTER: [&str; 4] = [
            "content_policy",
            "content filter",
            "content_filter",
            "safety system",
        ];

        if QUOTA.iter().any(|needle| haystack.contains(needle)) {
            return ApiErrorKind::QuotaExceeded;
        }
        if CONTEXT.iter().any(|needle| haystack.contains(needle)) {
            return ApiErrorKind::ContextLength;
        }
        if FILTER.iter().any(|needle| haystack.contains(needle)) {
            return ApiErrorKind::ContentFilter;
        }
        self
    }
}

impl fmt::Display for ApiErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_mapping_covers_every_documented_row() {
        let cases = [
            (400, ApiErrorKind::InvalidRequest),
            (401, ApiErrorKind::Auth),
            (402, ApiErrorKind::Billing),
            (403, ApiErrorKind::Permission),
            (404, ApiErrorKind::NotFound),
            (409, ApiErrorKind::Conflict),
            (413, ApiErrorKind::RequestTooLarge),
            (422, ApiErrorKind::InvalidRequest),
            (429, ApiErrorKind::RateLimit),
            (500, ApiErrorKind::ServerError),
            (502, ApiErrorKind::ServerError),
            (503, ApiErrorKind::ServerError),
            (504, ApiErrorKind::Timeout),
            (529, ApiErrorKind::Overloaded),
            (418, ApiErrorKind::Unknown),
        ];
        for (status, expected) in cases {
            assert_eq!(
                ApiErrorKind::from_status(status),
                expected,
                "status {status}"
            );
        }
    }

    #[test]
    fn retryable_kinds_are_rate_limit_overload_and_5xx() {
        let retryable = [
            ApiErrorKind::RateLimit,
            ApiErrorKind::Overloaded,
            ApiErrorKind::ServerError,
            ApiErrorKind::Timeout,
        ];
        for kind in retryable {
            assert!(kind.is_retryable(), "{kind} 应可重试");
        }
        let terminal = [
            ApiErrorKind::Auth,
            ApiErrorKind::Permission,
            ApiErrorKind::Billing,
            ApiErrorKind::QuotaExceeded,
            ApiErrorKind::ContextLength,
            ApiErrorKind::ContentFilter,
            ApiErrorKind::InvalidRequest,
            ApiErrorKind::NotFound,
            ApiErrorKind::RequestTooLarge,
            ApiErrorKind::Conflict,
            ApiErrorKind::Unknown,
        ];
        for kind in terminal {
            assert!(!kind.is_retryable(), "{kind} 不应重试");
        }
    }

    #[test]
    fn refine_promotes_documented_cases() {
        assert_eq!(
            ApiErrorKind::RateLimit.refine(
                Some("insufficient_quota"),
                "You exceeded your current quota"
            ),
            ApiErrorKind::QuotaExceeded
        );
        assert_eq!(
            ApiErrorKind::InvalidRequest.refine(
                Some("invalid_request_error"),
                "This model's maximum context length is 65536 tokens"
            ),
            ApiErrorKind::ContextLength
        );
        assert_eq!(
            ApiErrorKind::Permission.refine(Some("content_policy_violation"), "rejected"),
            ApiErrorKind::ContentFilter
        );
        assert_eq!(
            ApiErrorKind::InvalidRequest.refine(None, "Output blocked by content filtering policy"),
            ApiErrorKind::ContentFilter
        );
        assert_eq!(
            ApiErrorKind::Unknown.refine(None, "enforced_spend_limit_reached"),
            ApiErrorKind::QuotaExceeded
        );
        assert_eq!(
            ApiErrorKind::InvalidRequest.refine(Some("bad_param"), "温度超范围"),
            ApiErrorKind::InvalidRequest
        );
    }

    #[test]
    fn api_error_display_mentions_status_kind_and_code() {
        let mut err = ApiError::new(
            Some(429),
            ApiErrorKind::QuotaExceeded,
            "You exceeded your current quota",
            "{\"error\":{}}",
        );
        err.code = Some("insufficient_quota".into());
        err.request_id = Some("req_1".into());
        assert_eq!(
            err.to_string(),
            "HTTP 429 QuotaExceeded: You exceeded your current quota (code=insufficient_quota) (request_id=req_1)"
        );

        let streamed = ApiError::new(None, ApiErrorKind::Overloaded, "overloaded_error", "");
        assert_eq!(streamed.to_string(), "Overloaded: overloaded_error");
    }

    #[test]
    fn llm_error_delegates_classification() {
        let api = LlmError::Api(ApiError::new(
            Some(503),
            ApiErrorKind::ServerError,
            "unavailable",
            "",
        ));
        assert!(api.is_retryable());
        assert_eq!(api.status(), Some(503));
        assert_eq!(
            api.api_error().map(|err| err.kind),
            Some(ApiErrorKind::ServerError)
        );

        let exhausted = LlmError::RetriesExhausted {
            attempts: 3,
            last: Box::new(api),
        };
        assert!(!exhausted.is_retryable());
        assert_eq!(exhausted.status(), Some(503));
        assert_eq!(
            exhausted.api_error().map(|err| err.kind),
            Some(ApiErrorKind::ServerError)
        );
        assert!(exhausted.to_string().contains("unavailable"));
        assert!(
            exhausted
                .to_string()
                .contains("共尝试 3 次（含首次）后仍失败"),
            "{exhausted}"
        );

        let encode = LlmError::Encode("缺少 max_tokens".into());
        assert!(!encode.is_retryable());
        assert_eq!(encode.status(), None);
        assert!(encode.api_error().is_none());
    }
}

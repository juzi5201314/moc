//! 错误模型。
//!
//! 错误分三层，互不混淆：
//!
//! | 类型 | 出现位置 | 处置 |
//! |---|---|---|
//! | [`RpcError`] | 线上 `error` 对象 | 原样透传；未知码保留原始 `i32` |
//! | [`ProtocolError`] | 本地连接 / 编解码失败 | 关闭连接，不写成线上报文 |
//! | [`RpcCallError`] | `call` 与反向请求的调用方 | 按变体决定重试或放弃 |
//!
//! [`ProtocolError`] 与 [`FrameError`] 实现 `Clone`：I/O 错误降级为「分类 + 完整消息」
//! 而不自持 `io::Error`（后者不可克隆），与 `moc_llm::TransportError` 同思路，便于
//! 同一个失败同时交付给多个观察者。
//!
//! `-32000` 段里 core 侧（`-32001` / `-32010` / `-32014` / `-32015`）与协议层侧
//! （`-32002` / `-32003` / `-32011`）共用一张表：码的含义是全局约定，谁发出由
//! [`crate::AgentService`] 的实现决定。

use std::io;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// JSON-RPC 契约里的错误对象，也是线上 `error` 字段的形状。
///
/// `code` 保持 `i32` 而不是枚举：core 升级后新增的错误码必须能透传到老前端，
/// 前端据 [`RpcErrorCode::from_code`] 判定能否识别，识别不了就按通用失败展示。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, thiserror::Error)]
#[serde(deny_unknown_fields)]
#[error("{code}: {message}")]
pub struct RpcError {
    /// 错误码，取值见 [`RpcErrorCode`]；未知码原样保留。
    pub code: i32,
    /// 人类可读消息（中文，技术名词保留原文）。
    pub message: String,
    /// 结构化补充数据；语义由错误码决定（如 `-32003` 携带 `server_version`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl RpcError {
    /// 按错误码构造，消息取该码的默认文案。
    pub fn from_code(code: i32) -> Self {
        match RpcErrorCode::from_code(code) {
            Some(known) => Self {
                code,
                message: known.default_message().to_owned(),
                data: None,
            },
            None => Self {
                code,
                message: "未分类的协议错误".to_owned(),
                data: None,
            },
        }
    }

    /// 按错误码构造；`code` 为已知码时等价于 [`RpcError::from_code`]。
    pub fn of(code: RpcErrorCode) -> Self {
        Self::from_code(code.code())
    }

    /// 自定义消息。
    pub fn new(code: i32, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            data: None,
        }
    }

    /// 挂上结构化补充数据。
    #[must_use]
    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }
}

impl From<RpcErrorCode> for RpcError {
    fn from(code: RpcErrorCode) -> Self {
        Self::of(code)
    }
}

/// 标准错误码。
///
/// 权威定义见 crate 文档的方法表；`#[non_exhaustive]` 允许后续补码而不破坏下游。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum RpcErrorCode {
    /// `-32700`：收到的字节不是合法 JSON。
    ParseError,
    /// `-32600`：envelope 非法（含 batch、重复 initialize）。
    InvalidRequest,
    /// `-32601`：未知方法，或客户端未注册反向请求处理器。
    MethodNotFound,
    /// `-32602`：params 反序列化失败。
    InvalidParams,
    /// `-32603`：本地序列化失败、响应超帧长。
    InternalError,
    /// `-32000`：连接内并发请求超过上限。
    ServerBusy,
    /// `-32001`：请求因取消无法给出结果。
    RequestCancelled,
    /// `-32002`：initialize 之前调用其它方法。
    ServerNotInitialized,
    /// `-32003`：major 版本不一致。
    UnsupportedProtocolVersion,
    /// `-32010`：会话不存在。
    SessionNotFound,
    /// `-32011`：同一会话已有进行中的轮次。
    SessionBusy,
    /// `-32014`：服务端不支持该操作。
    NotSupported,
    /// `-32015`：会话被其它进程锁定，只读。
    SessionLocked,
}

impl RpcErrorCode {
    /// 线上码值。
    pub const fn code(self) -> i32 {
        match self {
            Self::ParseError => -32700,
            Self::InvalidRequest => -32600,
            Self::MethodNotFound => -32601,
            Self::InvalidParams => -32602,
            Self::InternalError => -32603,
            Self::ServerBusy => -32000,
            Self::RequestCancelled => -32001,
            Self::ServerNotInitialized => -32002,
            Self::UnsupportedProtocolVersion => -32003,
            Self::SessionNotFound => -32010,
            Self::SessionBusy => -32011,
            Self::NotSupported => -32014,
            Self::SessionLocked => -32015,
        }
    }

    /// 从码值还原；未知码返回 `None`。
    pub const fn from_code(code: i32) -> Option<Self> {
        let known = match code {
            -32700 => Self::ParseError,
            -32600 => Self::InvalidRequest,
            -32601 => Self::MethodNotFound,
            -32602 => Self::InvalidParams,
            -32603 => Self::InternalError,
            -32000 => Self::ServerBusy,
            -32001 => Self::RequestCancelled,
            -32002 => Self::ServerNotInitialized,
            -32003 => Self::UnsupportedProtocolVersion,
            -32010 => Self::SessionNotFound,
            -32011 => Self::SessionBusy,
            -32014 => Self::NotSupported,
            -32015 => Self::SessionLocked,
            _ => return None,
        };
        Some(known)
    }

    /// 默认消息。
    pub const fn default_message(self) -> &'static str {
        match self {
            Self::ParseError => "报文不是合法 JSON",
            Self::InvalidRequest => "报文结构非法",
            Self::MethodNotFound => "未知方法",
            Self::InvalidParams => "参数非法",
            Self::InternalError => "服务端内部错误",
            Self::ServerBusy => "服务端并发请求已达上限",
            Self::RequestCancelled => "请求已被取消",
            Self::ServerNotInitialized => "连接尚未完成 initialize",
            Self::UnsupportedProtocolVersion => "协议主版本不兼容",
            Self::SessionNotFound => "会话不存在",
            Self::SessionBusy => "该会话已有进行中的轮次",
            Self::NotSupported => "服务端不支持该操作",
            Self::SessionLocked => "会话被其它进程锁定，只读",
        }
    }
}

/// 分帧失败。
///
/// 全部变体都是**致命**的：分帧一旦错位就无法重新同步（报文体长度不可信），
/// 唯一的处置是关闭连接。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum FrameError {
    /// 头部块超过 8 KiB 仍未结束。
    #[error("分帧头部超过 8 KiB")]
    HeaderTooLarge,
    /// `Content-Length` 头存在但取值不可用。
    #[error("Content-Length 非法：{0}")]
    InvalidContentLength(String),
    /// 报文长度超过连接配置的 `max_frame_bytes`。
    #[error("报文长度 {len} 超过上限 {max}")]
    TooLarge {
        /// 声明或实际长度。
        len: usize,
        /// 上限。
        max: usize,
    },
    /// `Content-Length` 取值不是十进制数字。
    #[error("Content-Length 不是十进制数字")]
    NonNumericLength,
    /// 报文体之外出现无法解释的字节，分帧已错位。
    #[error("报文前出现非分帧字节，连接无法重新同步")]
    UnexpectedPrefix,
}

/// 连接 I/O 失败的分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum IoKind {
    /// 对端正常关闭（EOF、已断开）。
    Closed,
    /// 对端异常中断（RST、管道破裂）。
    Broken,
    /// 其余 I/O 失败。
    Other,
}

impl IoKind {
    /// 按 [`io::ErrorKind`] 分类。
    pub fn classify(kind: io::ErrorKind) -> Self {
        match kind {
            io::ErrorKind::UnexpectedEof
            | io::ErrorKind::NotConnected
            | io::ErrorKind::ConnectionAborted => Self::Closed,
            io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset => Self::Broken,
            _ => Self::Other,
        }
    }
}

/// 本地协议失败（不上线）。
///
/// 这些失败说明连接本身不可用或本端实现有 bug，不应该伪装成对端能理解的 RPC 错误。
/// 对端**违约**（发来非法 JSON）不算本地失败：报文边界是清楚的，协议层回 `-32700`
/// 后继续读，因此这里没有对应变体。
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum ProtocolError {
    /// 连接读写失败。
    #[error("连接 I/O 失败（{kind:?}）：{message}")]
    Io {
        /// 失败分类。
        kind: IoKind,
        /// 完整错误消息。
        message: String,
    },
    /// 分帧失败，连接必须关闭。
    #[error("分帧失败：{0}")]
    Frame(#[from] FrameError),
    /// 本地报文序列化失败（本端实现 bug）。
    #[error("本地报文序列化失败：{0}")]
    Encode(String),
    /// 连接已关闭。
    #[error("连接已关闭")]
    ConnectionClosed,
    /// `ClientBuilder` 对同一方法注册了两次处理器。
    #[error("重复注册方法处理器：{0}")]
    DuplicateHandler(&'static str),
}

impl From<io::Error> for ProtocolError {
    fn from(error: io::Error) -> Self {
        Self::Io {
            kind: IoKind::classify(error.kind()),
            message: error.to_string(),
        }
    }
}

/// 请求类调用的统一失败类型。
///
/// 与 [`ProtocolError`] 的分工：`call` / 反向请求只需要「对端拒绝」「被取消」「连接没了」
/// 三种判断，不需要知道底层是分帧错了还是 I/O 断了。
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum RpcCallError {
    /// 对端回了 `error` 对象（含 `-32601` 这类可预期拒绝）。
    #[error("RPC 错误：{0}")]
    Rpc(#[from] RpcError),
    /// 请求被取消：反向请求在轮次取消或连接关闭时触发。
    #[error("请求已被取消")]
    Cancelled,
    /// 连接已死，请求未送达或结果丢失。
    #[error("连接已关闭")]
    Closed,
}

impl RpcCallError {
    /// 是否因连接关闭而失败。
    pub fn is_closed(&self) -> bool {
        matches!(self, Self::Closed)
    }

    /// 是否因取消而失败。
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }

    /// 取出对端错误码（本地失败时为 `None`）。
    pub fn code(&self) -> Option<i32> {
        match self {
            Self::Rpc(error) => Some(error.code),
            _ => None,
        }
    }
}

/// `connect()` 的失败类型：既可能是连接建立/握手传输失败，也可能是 initialize 被拒。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConnectError {
    /// 连接建立或分帧失败。
    #[error("连接建立失败：{0}")]
    Protocol(#[from] ProtocolError),
    /// 服务端返回了 RPC 错误（典型是 `-32003` 版本不兼容）。
    #[error("initialize 失败：{0}")]
    Rpc(#[from] RpcError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn code_values_round_trip() {
        let all = [
            RpcErrorCode::ParseError,
            RpcErrorCode::InvalidRequest,
            RpcErrorCode::MethodNotFound,
            RpcErrorCode::InvalidParams,
            RpcErrorCode::InternalError,
            RpcErrorCode::ServerBusy,
            RpcErrorCode::RequestCancelled,
            RpcErrorCode::ServerNotInitialized,
            RpcErrorCode::UnsupportedProtocolVersion,
            RpcErrorCode::SessionNotFound,
            RpcErrorCode::SessionBusy,
            RpcErrorCode::NotSupported,
            RpcErrorCode::SessionLocked,
        ];
        for code in all {
            assert_eq!(RpcErrorCode::from_code(code.code()), Some(code));
            assert!(!code.default_message().is_empty());
        }
        assert_eq!(RpcErrorCode::ParseError.code(), -32700);
        assert_eq!(RpcErrorCode::SessionLocked.code(), -32015);
        assert_eq!(RpcErrorCode::from_code(-32999), None);
    }

    #[test]
    fn unknown_code_is_preserved() {
        let raw = json!({ "code": -33000, "message": "来自未来的错误" });
        let error: RpcError = serde_json::from_value(raw).expect("合法错误对象");
        assert_eq!(error.code, -33000);
        assert_eq!(error.message, "来自未来的错误");
        assert_eq!(error.data, None);
    }

    #[test]
    fn error_object_shape_is_strict() {
        // data 省略时不出现在线上形状里
        let error = RpcError::of(RpcErrorCode::MethodNotFound);
        assert_eq!(
            serde_json::to_value(&error).expect("可序列化"),
            json!({ "code": -32601, "message": "未知方法" }),
        );
        let with_data = RpcError::of(RpcErrorCode::UnsupportedProtocolVersion)
            .with_data(json!({ "server_version": "1.0" }));
        assert_eq!(
            serde_json::to_value(&with_data).expect("可序列化"),
            json!({ "code": -32003, "message": "协议主版本不兼容", "data": { "server_version": "1.0" } }),
        );
        // 未知字段与非法码类型必须被拒绝
        assert!(
            serde_json::from_value::<RpcError>(json!({ "code": -1, "message": "x", "extra": 1 }))
                .is_err()
        );
        assert!(
            serde_json::from_value::<RpcError>(json!({ "code": "x", "message": "x" })).is_err()
        );
        assert!(serde_json::from_value::<RpcError>(json!({ "code": -1 })).is_err());
    }

    #[test]
    fn from_code_uses_default_message_for_known_and_generic_for_unknown() {
        assert_eq!(
            RpcError::from_code(-32015).message,
            "会话被其它进程锁定，只读"
        );
        assert_eq!(RpcError::from_code(-33001).message, "未分类的协议错误");
    }

    #[test]
    fn io_kind_classification() {
        assert_eq!(
            IoKind::classify(io::ErrorKind::UnexpectedEof),
            IoKind::Closed
        );
        assert_eq!(IoKind::classify(io::ErrorKind::BrokenPipe), IoKind::Broken);
        assert_eq!(
            IoKind::classify(io::ErrorKind::PermissionDenied),
            IoKind::Other
        );
        let error: ProtocolError =
            io::Error::new(io::ErrorKind::ConnectionReset, "连接被重置").into();
        assert!(matches!(
            error,
            ProtocolError::Io {
                kind: IoKind::Broken,
                ..
            }
        ));
    }

    #[test]
    fn rpc_call_error_helpers() {
        assert!(RpcCallError::Cancelled.is_cancelled());
        assert!(RpcCallError::Closed.is_closed());
        assert_eq!(
            RpcCallError::from(RpcError::of(RpcErrorCode::SessionNotFound)).code(),
            Some(-32010)
        );
        assert_eq!(RpcCallError::Cancelled.code(), None);
    }
}

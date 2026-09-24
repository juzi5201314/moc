//! JSON-RPC 2.0 报文层。
//!
//! 三种 envelope，形状与解析规则都按规范严格实现：
//!
//! ```text
//! 请求        {"jsonrpc":"2.0","id":1,"method":"session/prompt","params":{…}}
//! 通知        {"jsonrpc":"2.0","method":"session/update","params":{…}}
//! 成功响应    {"jsonrpc":"2.0","id":1,"result":{…}}
//! 失败响应    {"jsonrpc":"2.0","id":1,"error":{"code":-32602,"message":"…"}}
//! ```
//!
//! 解析刻意比规范更严（双方同仓同步发布，早期暴露拼写错误优于静默丢弃）：
//!
//! - 不支持 batch：顶层是数组直接 [`RpcErrorCode::InvalidRequest`]。
//! - 小数 `id` 非法；数字类 `id` 一律按 `i64` 解析，浮点与超范围都视为 envelope 非法。
//! - `params` 出现时必须是对象或数组，**不允许 `null`**（用省略字段表达「无参数」）。
//! - envelope 与 params/result DTO 都拒绝未知字段。唯一例外是内部标记（internally
//!   tagged）枚举：serde 不支持它与 `deny_unknown_fields` 组合，因此
//!   [`crate::methods::update::SessionUpdate`] 这类带 `type` 标签的枚举忽略未知字段，
//!   但未知 `type` 值仍然报错。
//!
//! 解析失败时尽量从原报文里回收 `id`——错误响应要回显它能读到的 ID，规范也要求
//! 读不到时用 `null`。

use std::borrow::Cow;
use std::fmt;

use serde::de::{self, Unexpected, Visitor};
use serde::ser::SerializeMap;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::error::{RpcError, RpcErrorCode};

/// `jsonrpc` 字段的固定取值。
pub const JSONRPC_VERSION: &str = "2.0";

/// 报文 ID：整数或字符串（规范允许，且不推荐小数——本实现直接拒绝）。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Id {
    /// 整数 ID。
    Number(i64),
    /// 字符串 ID。
    String(String),
}

impl Id {
    /// 整数 ID。
    pub fn number(value: i64) -> Self {
        Self::Number(value)
    }

    /// 字符串 ID。
    pub fn string(value: impl Into<String>) -> Self {
        Self::String(value.into())
    }
}

impl fmt::Display for Id {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number(value) => write!(f, "{value}"),
            Self::String(value) => f.write_str(value),
        }
    }
}

impl Serialize for Id {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Number(value) => serializer.serialize_i64(*value),
            Self::String(value) => serializer.serialize_str(value),
        }
    }
}

impl<'de> Deserialize<'de> for Id {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct IdVisitor;

        impl Visitor<'_> for IdVisitor {
            type Value = Id;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("整数或字符串 ID")
            }

            fn visit_i64<E: de::Error>(self, value: i64) -> Result<Id, E> {
                Ok(Id::Number(value))
            }

            fn visit_u64<E: de::Error>(self, value: u64) -> Result<Id, E> {
                i64::try_from(value)
                    .map(Id::Number)
                    .map_err(|_| E::invalid_value(Unexpected::Unsigned(value), &self))
            }

            fn visit_str<E: de::Error>(self, value: &str) -> Result<Id, E> {
                Ok(Id::String(value.to_owned()))
            }

            fn visit_string<E: de::Error>(self, value: String) -> Result<Id, E> {
                Ok(Id::String(value))
            }
        }

        deserializer.deserialize_any(IdVisitor)
    }
}

/// 请求：期待响应。
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    /// 报文 ID。
    pub id: Id,
    /// 方法名。
    ///
    /// `Cow<'static, str>`：本端构造的报文（方法名来自 [`crate::Method::NAME`]，
    /// 是 `&'static str`）与解析到的报文（`String`）共用一种类型，组帧时零拷贝。
    pub method: Cow<'static, str>,
    /// 参数；`None` 表示报文省略了 `params`。
    pub params: Option<Value>,
}

impl Request {
    /// 构造请求。
    pub fn new(id: Id, method: impl Into<Cow<'static, str>>, params: Option<Value>) -> Self {
        Self {
            id,
            method: method.into(),
            params,
        }
    }
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3 + usize::from(self.params.is_some())))?;
        map.serialize_entry("jsonrpc", JSONRPC_VERSION)?;
        map.serialize_entry("id", &self.id)?;
        map.serialize_entry("method", &self.method)?;
        if let Some(params) = &self.params {
            map.serialize_entry("params", params)?;
        }
        map.end()
    }
}

/// 通知：不期待响应（服务端→客户端单向推送）。
#[derive(Debug, Clone, PartialEq)]
pub struct Notification {
    /// 方法名；`Cow` 的理由同 [`Request::method`]。
    pub method: Cow<'static, str>,
    /// 参数；`None` 表示报文省略了 `params`。
    pub params: Option<Value>,
}

impl Notification {
    /// 构造通知。
    pub fn new(method: impl Into<Cow<'static, str>>, params: Option<Value>) -> Self {
        Self {
            method: method.into(),
            params,
        }
    }
}

impl Serialize for Notification {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(2 + usize::from(self.params.is_some())))?;
        map.serialize_entry("jsonrpc", JSONRPC_VERSION)?;
        map.serialize_entry("method", &self.method)?;
        if let Some(params) = &self.params {
            map.serialize_entry("params", params)?;
        }
        map.end()
    }
}

/// 响应：`payload` 决定线上出现 `result` 还是 `error`。
#[derive(Debug, Clone, PartialEq)]
pub struct Response {
    /// 被响应请求的 ID；`None` 只在解析错误的响应里出现（回 `null`）。
    pub id: Option<Id>,
    /// 成功结果或失败对象。
    pub payload: Result<Value, RpcError>,
}

impl Response {
    /// 成功响应。
    pub fn result(id: Id, result: Value) -> Self {
        Self {
            id: Some(id),
            payload: Ok(result),
        }
    }

    /// 失败响应。
    pub fn error(id: Option<Id>, error: RpcError) -> Self {
        Self {
            id,
            payload: Err(error),
        }
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut map = serializer.serialize_map(Some(3))?;
        map.serialize_entry("jsonrpc", JSONRPC_VERSION)?;
        map.serialize_entry("id", &self.id)?;
        match &self.payload {
            Ok(result) => map.serialize_entry("result", result)?,
            Err(error) => map.serialize_entry("error", error)?,
        }
        map.end()
    }
}

impl Serialize for RpcMessage {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Request(request) => request.serialize(serializer),
            Self::Notification(notification) => notification.serialize(serializer),
            Self::Response(response) => response.serialize(serializer),
        }
    }
}

/// 一条完整的报文。
#[derive(Debug, Clone, PartialEq)]
pub enum RpcMessage {
    /// 对端请求。
    Request(Request),
    /// 对端通知。
    Notification(Notification),
    /// 对端响应。
    Response(Response),
}

impl RpcMessage {
    /// 序列化到字节（组帧前的报文体）。
    ///
    /// 直接用 `serde_json::to_vec(&message)`，三个 envelope 各自的 `Serialize` 实现
    /// 保证 `jsonrpc` 字段恒为 `"2.0"` 且不写空 `params`。
    pub fn to_vec(&self) -> Result<Vec<u8>, serde_json::Error> {
        serde_json::to_vec(self)
    }

    /// 解析一条报文。
    pub fn parse(bytes: &[u8]) -> Result<Self, ParseFailure> {
        let value: Value = serde_json::from_slice(bytes).map_err(|error| {
            ParseFailure::new(
                RpcErrorCode::ParseError,
                format!("报文不是合法 JSON：{error}"),
                None,
            )
        })?;
        let Value::Object(mut fields) = value else {
            return Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                "报文顶层必须是 JSON 对象（不支持 batch）".to_owned(),
                None,
            ));
        };
        if fields.get("jsonrpc").and_then(Value::as_str) != Some(JSONRPC_VERSION) {
            let id = recover_id(&fields);
            return Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                format!("jsonrpc 字段必须为 \"{JSONRPC_VERSION}\""),
                id,
            ));
        }
        // 取出已知字段（move 而非 clone：result 可能是数 MiB 的 transcript）
        fields.remove("jsonrpc");
        let id_field = fields.remove("id");
        let method = fields.remove("method");
        let params = fields.remove("params");
        let result = fields.remove("result");
        let error = fields.remove("error");
        if let Some(key) = fields.keys().next() {
            return Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                format!("报文包含未知字段 {key}"),
                recover_id_from(id_field.as_ref()),
            ));
        }
        match (method, result, error) {
            (Some(method), None, None) => parse_request(method, id_field, params),
            (Some(method), _, _) => Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                format!("请求报文（method = {method:?}）不得携带 result / error"),
                recover_id_from(id_field.as_ref()),
            )),
            (None, Some(result), None) => parse_response(id_field, result),
            (None, None, Some(error)) => match serde_json::from_value::<RpcError>(error) {
                Ok(error) => {
                    let id = match id_field {
                        None | Some(Value::Null) => None,
                        Some(other) => Some(parse_id(other)?),
                    };
                    Ok(RpcMessage::Response(Response {
                        id,
                        payload: Err(error),
                    }))
                }
                Err(error) => Err(ParseFailure::new(
                    RpcErrorCode::InvalidRequest,
                    format!("error 字段不是合法错误对象：{error}"),
                    recover_id_from(id_field.as_ref()),
                )),
            },
            (None, Some(_), Some(_)) => Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                "响应不得同时携带 result 与 error".to_owned(),
                recover_id_from(id_field.as_ref()),
            )),
            (None, None, None) => Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                "报文必须包含 method 或 result / error".to_owned(),
                recover_id_from(id_field.as_ref()),
            )),
        }
    }
}

/// 解析失败的报文；`id` 决定错误响应里回显什么。
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{code:?}: {message}")]
pub struct ParseFailure {
    /// 回给对端的错误码。
    pub code: RpcErrorCode,
    /// 失败细节（中文）。
    pub message: String,
    /// 能从原报文里回收到的 ID；读不到时为 `None`（响应里回 `null`）。
    pub id: Option<Id>,
}

impl ParseFailure {
    /// 构造解析失败。
    pub fn new(code: RpcErrorCode, message: String, id: Option<Id>) -> Self {
        Self { code, message, id }
    }

    /// 转成可以直接回给对端的错误响应。
    pub fn into_response(self) -> Response {
        Response {
            id: self.id,
            payload: Err(RpcError::new(self.code.code(), self.message)),
        }
    }
}

fn parse_request(
    method: Value,
    id_field: Option<Value>,
    params: Option<Value>,
) -> Result<RpcMessage, ParseFailure> {
    let id = recover_id_from(id_field.as_ref());
    let Value::String(method) = method else {
        return Err(ParseFailure::new(
            RpcErrorCode::InvalidRequest,
            "method 必须是字符串".to_owned(),
            id,
        ));
    };
    let method: Cow<'static, str> = Cow::Owned(method);
    if method.is_empty() {
        return Err(ParseFailure::new(
            RpcErrorCode::InvalidRequest,
            "method 不能为空".to_owned(),
            id,
        ));
    }
    let params = match params {
        None => None,
        Some(value @ (Value::Object(_) | Value::Array(_))) => Some(value),
        Some(_) => {
            return Err(ParseFailure::new(
                RpcErrorCode::InvalidRequest,
                "params 必须是对象或数组".to_owned(),
                id,
            ));
        }
    };
    match id_field {
        None => Ok(RpcMessage::Notification(Notification { method, params })),
        Some(Value::Null) => Err(ParseFailure::new(
            RpcErrorCode::InvalidRequest,
            "请求的 id 不能为 null".to_owned(),
            None,
        )),
        Some(value) => {
            let id = parse_id(value)?;
            Ok(RpcMessage::Request(Request { id, method, params }))
        }
    }
}

fn parse_response(id_field: Option<Value>, result: Value) -> Result<RpcMessage, ParseFailure> {
    match id_field {
        None | Some(Value::Null) => Err(ParseFailure::new(
            RpcErrorCode::InvalidRequest,
            "携带 result 的响应必须有非 null 的 id".to_owned(),
            None,
        )),
        Some(value) => Ok(RpcMessage::Response(Response {
            id: Some(parse_id(value)?),
            payload: Ok(result),
        })),
    }
}

/// 解析 `id` 字段；浮点、超范围、布尔、`null` 都是 envelope 非法。
fn parse_id(value: Value) -> Result<Id, ParseFailure> {
    serde_json::from_value::<Id>(value).map_err(|error| {
        ParseFailure::new(
            RpcErrorCode::InvalidRequest,
            format!("id 必须是整数或字符串：{error}"),
            None,
        )
    })
}

/// 从已解析的字段表里回收 ID（尽力而为，失败则 `None`）。
fn recover_id(fields: &Map<String, Value>) -> Option<Id> {
    recover_id_from(fields.get("id"))
}

/// 从 `id` 字段的原始值回收 ID。
fn recover_id_from(value: Option<&Value>) -> Option<Id> {
    match value {
        None | Some(Value::Null) => None,
        Some(other) => serde_json::from_value::<Id>(other.clone()).ok(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn parse(bytes: &[u8]) -> Result<RpcMessage, ParseFailure> {
        RpcMessage::parse(bytes)
    }

    #[test]
    fn id_round_trips_and_rejects_floats() {
        assert_eq!(serde_json::to_value(Id::Number(7)).unwrap(), json!(7));
        assert_eq!(serde_json::to_value(Id::Number(-1)).unwrap(), json!(-1));
        assert_eq!(serde_json::to_value(Id::String("a".into())).unwrap(), json!("a"));
        assert_eq!(serde_json::from_value::<Id>(json!(12)).unwrap(), Id::Number(12));
        assert_eq!(
            serde_json::from_value::<Id>(json!("ses_1")).unwrap(),
            Id::String("ses_1".into())
        );
        assert!(serde_json::from_value::<Id>(json!(1.5)).is_err());
        assert!(serde_json::from_value::<Id>(json!(null)).is_err());
        assert!(serde_json::from_value::<Id>(json!(true)).is_err());
        assert!(serde_json::from_value::<Id>(json!(u64::MAX)).is_err());
    }

    #[test]
    fn request_wire_shape() {
        let request = RpcMessage::Request(Request::new(
            Id::Number(1),
            "session/prompt",
            Some(json!({ "session_id": "ses_7f3a", "input": [{ "type": "text", "text": "看一下 README" }] })),
        ));
        assert_eq!(
            serde_json::to_value(&request).unwrap(),
            json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "session/prompt",
                "params": { "session_id": "ses_7f3a", "input": [{ "type": "text", "text": "看一下 README" }] }
            })
        );
    }

    #[test]
    fn notification_wire_shape_omits_absent_params() {
        let notification = RpcMessage::Notification(Notification::new("session/cancel_ping", None));
        assert_eq!(
            serde_json::to_value(&notification).unwrap(),
            json!({ "jsonrpc": "2.0", "method": "session/cancel_ping" })
        );
    }

    #[test]
    fn response_wire_shapes() {
        let ok = RpcMessage::Response(Response::result(Id::Number(1), json!({ "cancelled": true })));
        assert_eq!(
            serde_json::to_value(&ok).unwrap(),
            json!({ "jsonrpc": "2.0", "id": 1, "result": { "cancelled": true } })
        );
        let failed = RpcMessage::Response(Response::error(
            Some(Id::String("x".into())),
            RpcError::new(-32601, "未知方法"),
        ));
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({ "jsonrpc": "2.0", "id": "x", "error": { "code": -32601, "message": "未知方法" } })
        );
        let null_id = RpcMessage::Response(Response::error(None, RpcError::of(RpcErrorCode::ParseError)));
        assert_eq!(
            serde_json::to_value(&null_id).unwrap(),
            json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32700, "message": "报文不是合法 JSON" } })
        );
    }

    #[test]
    fn invalid_json_reports_parse_error() {
        let failure = parse(b"{oops").unwrap_err();
        assert_eq!(failure.code, RpcErrorCode::ParseError);
        assert_eq!(failure.id, None);
        assert_eq!(failure.into_response().id, None);
    }

    #[test]
    fn batch_is_rejected() {
        let failure = parse(br#"[{"jsonrpc":"2.0","id":1,"method":"ping"}]"#).unwrap_err();
        assert_eq!(failure.code, RpcErrorCode::InvalidRequest);
        assert_eq!(failure.id, None);
    }

    #[test]
    fn version_and_method_rules() {
        for bytes in [
            br#"{"id":1,"method":"ping"}"#.as_slice(),
            br#"{"jsonrpc":"1.0","id":1,"method":"ping"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","method":""}"#.as_slice(),
            br#"{"jsonrpc":"2.0","method":1}"#.as_slice(),
            br#"{"jsonrpc":"2.0"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":"x"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":null}"#.as_slice(),
            br#"{"jsonrpc":"2.0","method":"ping","extra":1}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1.5,"method":"ping"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":null,"method":"ping"}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"result":{},"error":{"code":-1,"message":"x"}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":null,"result":{}}"#.as_slice(),
            br#"{"jsonrpc":"2.0","id":1,"error":{"code":-1,"message":"x","extra":1}}"#.as_slice(),
        ] {
            let failure = parse(bytes).unwrap_err();
            assert_eq!(
                failure.code,
                RpcErrorCode::InvalidRequest,
                "报文应被拒绝：{}",
                String::from_utf8_lossy(bytes)
            );
        }
    }

    #[test]
    fn invalid_request_echoes_recoverable_id() {
        let failure = parse(br#"{"jsonrpc":"2.0","id":42,"method":""}"#).unwrap_err();
        assert_eq!(failure.id, Some(Id::Number(42)));
        // 版本错误时也尽量回收 ID
        let failure = parse(br#"{"jsonrpc":"3.0","id":"abc","method":"ping"}"#).unwrap_err();
        assert_eq!(failure.id, Some(Id::String("abc".into())));
        // ID 本身非法时无法回收
        let failure = parse(br#"{"jsonrpc":"3.0","id":1.5,"method":"ping"}"#).unwrap_err();
        assert_eq!(failure.id, None);
    }

    #[test]
    fn parses_valid_messages() {
        let request = parse(br#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[]}"#).unwrap();
        assert_eq!(
            request,
            RpcMessage::Request(Request::new(Id::Number(1), "ping", Some(json!([]))))
        );
        let notification = parse(br#"{"jsonrpc":"2.0","method":"session/update"}"#).unwrap();
        assert_eq!(
            notification,
            RpcMessage::Notification(Notification::new("session/update", None))
        );
        let response = parse(br#"{"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"x"}}"#).unwrap();
        assert_eq!(
            response,
            RpcMessage::Response(Response::error(None, RpcError::new(-32700, "x")))
        );
        let response = parse(br#"{"jsonrpc":"2.0","id":7,"result":{"ok":true}}"#).unwrap();
        assert_eq!(
            response,
            RpcMessage::Response(Response::result(Id::Number(7), json!({ "ok": true })))
        );
    }
}

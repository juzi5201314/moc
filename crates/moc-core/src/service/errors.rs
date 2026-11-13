use super::Core;
use crate::error::CoreError;
use moc_protocol::RpcError;
use moc_protocol::RpcErrorCode;
use serde_json::json;
use std::sync::Arc;

/// 写租约失败时把持有者投影一并返回，前端不用再发一次 `session/list` 才能提示。
pub(super) fn write_lease_error(
    core: &Core,
    session_id: &str,
    record: &Arc<crate::session::SessionRecord>,
    error: CoreError,
) -> RpcError {
    let message = error.to_string();
    if message.contains("SESSION_LOCKED") {
        return RpcError::new(RpcErrorCode::SessionLocked.code(), message).with_data(json!({
            "lock": core.inner.sessions.lease_info(session_id, record),
        }));
    }
    to_rpc(error)
}

pub(crate) fn to_rpc(error: CoreError) -> RpcError {
    let message = error.to_string();
    let code = if message.contains("SESSION_NOT_FOUND") {
        RpcErrorCode::SessionNotFound
    } else if message.contains("SESSION_BUSY") {
        RpcErrorCode::SessionBusy
    } else if message.contains("SESSION_LOCKED") {
        RpcErrorCode::SessionLocked
    } else if matches!(error, CoreError::Config(_)) || message.contains("INVALID_PARAMS") {
        RpcErrorCode::InvalidParams
    } else if message.contains("NotSupported") {
        RpcErrorCode::NotSupported
    } else if matches!(error, CoreError::Cancelled) {
        RpcErrorCode::RequestCancelled
    } else {
        RpcErrorCode::InternalError
    };
    RpcError::new(code.code(), message)
}

pub(crate) fn invalid_params(message: impl Into<String>) -> RpcError {
    RpcError::new(RpcErrorCode::InvalidParams.code(), message)
}

/// 任务定位失败按非法参数报给前端（`job/*` 的错误码表约定 `-32602`）。
pub(super) fn job_error(error: crate::tools::ToolError) -> RpcError {
    let code = match error.code {
        "E_JOB_NOT_FOUND" => RpcErrorCode::InvalidParams,
        _ => RpcErrorCode::InternalError,
    };
    RpcError::new(code.code(), error.to_string())
}

pub(crate) fn protocol_to_rpc(error: impl std::fmt::Display) -> RpcError {
    RpcError::new(RpcErrorCode::InternalError.code(), error.to_string())
}

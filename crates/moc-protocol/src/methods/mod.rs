//! 方法契约集合：标记类型、方法名常量与 DTO。
//!
//! 方法表（方向 = 谁能发起）：
//!
//! | 方法 | 方向 | 处理方 | 前置 | 可能的失败码 |
//! |---|---|---|---|---|
//! | [`initialize::Initialize`] | C→S | core | 未初始化 | `-32003` |
//! | [`initialize::Ping`] | C→S | 协议层自答 | 无 | — |
//! | [`config::ConfigGet`] | C→S | core | 已初始化 | `-32014` |
//! | [`session::SessionNew`] | C→S | core | 已初始化 | `-32602` / `-32014` |
//! | [`session::SessionList`] | C→S | core | 已初始化 | — |
//! | [`session::SessionLoad`] | C→S | core | 已初始化 | `-32010` |
//! | [`session::SessionUnload`] | C→S | core | 已初始化 | `-32010` |
//! | [`session::SessionRename`] | C→S | core | 已初始化 | `-32010` / `-32015` |
//! | [`session::SessionDelete`] | C→S | core | 已初始化 | `-32010` / `-32015` |
//! | [`session::SessionUnlock`] | C→S | core | 已初始化 | `-32010` |
//! | [`turn::SessionPrompt`] | C→S | core | 已初始化 + 该会话无在飞轮次 | `-32010` / `-32015` / `-32011` |
//! | [`turn::SessionCancel`] | C→S | 协议层自答 | 已初始化 | — |
//! | [`permission::SessionRequestPermission`] | S→C | 前端 | 前端已注册处理器 | 前端自定义 |
//! | [`update::SessionUpdateMethod`] | S→C | 前端 | — | — |
//!
//! 方向隔离是强制的：客户端向服务端发 `session/update` /
//! `session/request_permission`，或服务端收到 `session/prompt` 之外的未知名字，
//! 一律 [`crate::RpcErrorCode::MethodNotFound`]。

pub mod config;
pub mod initialize;
pub mod permission;
pub mod session;
pub mod turn;
pub mod update;

use serde::{Deserialize, Serialize};

pub use config::{AgentInfo, ConfigGet, ConfigGetParams, ConfigGetResult, ModelInfo};
pub use initialize::{
    ClientCapabilities, ClientInfo, EmptyParams, Initialize, InitializeParams, InitializeResult,
    Ping, PingResult, ServerCapabilities, ServerInfo,
};
pub use permission::{
    PermissionOption, PermissionOptionKind, PermissionRequestParams, PermissionResult,
    SessionRequestPermission, default_options,
};
pub use session::{
    SessionDelete, SessionDeleteParams, SessionInfo, SessionList, SessionListParams,
    SessionListResult, SessionLoad, SessionLoadParams, SessionLoadResult, SessionLockInfo,
    SessionNew, SessionNewParams, SessionRename, SessionRenameParams, SessionUnload,
    SessionUnloadParams, SessionUnlock, SessionUnlockParams, ToolExecution, ToolStatus,
    TranscriptEntry,
};
pub use turn::{
    InputPart, PromptParams, SessionCancel, SessionCancelParams, SessionCancelResult, SessionPrompt,
    TurnResult,
};
pub use update::{AssistantDelta, SessionUpdate, SessionUpdateMethod, SessionUpdateParams};

/// 空结果：序列化为 `{}`。
///
/// 不用 `null` 表达「无结果」：`null` 与「字段缺失」「结构错误」在前端难以区分，
/// 空对象则能直接反序列化成零字段结构体。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Empty {}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use crate::method::{Method, ServerNotification, ServerRequest};

    use super::*;

    /// 方法名唯一，且与各标记类型的 `Method::NAME` 一致。
    #[test]
    fn method_names_are_unique_and_consistent() {
        let client_methods = [
            <Initialize as Method>::NAME,
            <Ping as Method>::NAME,
            <ConfigGet as Method>::NAME,
            <SessionNew as Method>::NAME,
            <SessionList as Method>::NAME,
            <SessionLoad as Method>::NAME,
            <SessionUnload as Method>::NAME,
            <SessionRename as Method>::NAME,
            <SessionDelete as Method>::NAME,
            <SessionUnlock as Method>::NAME,
            <SessionPrompt as Method>::NAME,
            <SessionCancel as Method>::NAME,
        ];
        let server_methods = [
            <SessionRequestPermission as Method>::NAME,
            <SessionUpdateMethod as Method>::NAME,
        ];
        let expected: HashSet<&str> = [
            "initialize",
            "ping",
            "config/get",
            "session/new",
            "session/list",
            "session/load",
            "session/unload",
            "session/rename",
            "session/delete",
            "session/unlock",
            "session/prompt",
            "session/cancel",
        ]
        .into_iter()
        .collect();
        assert_eq!(client_methods.into_iter().collect::<HashSet<_>>(), expected);
        assert_eq!(
            server_methods.into_iter().collect::<HashSet<_>>(),
            ["session/request_permission", "session/update"]
                .into_iter()
                .collect::<HashSet<_>>()
        );
    }

    /// 标记类型的方法名与 crate 文档里那张表逐字一致。
    #[test]
    fn names_match_documented_table() {
        assert_eq!(<Initialize as Method>::NAME, "initialize");
        assert_eq!(<Ping as Method>::NAME, "ping");
        assert_eq!(<ConfigGet as Method>::NAME, "config/get");
        assert_eq!(<SessionNew as Method>::NAME, "session/new");
        assert_eq!(<SessionList as Method>::NAME, "session/list");
        assert_eq!(<SessionLoad as Method>::NAME, "session/load");
        assert_eq!(<SessionUnload as Method>::NAME, "session/unload");
        assert_eq!(<SessionRename as Method>::NAME, "session/rename");
        assert_eq!(<SessionDelete as Method>::NAME, "session/delete");
        assert_eq!(<SessionUnlock as Method>::NAME, "session/unlock");
        assert_eq!(<SessionPrompt as Method>::NAME, "session/prompt");
        assert_eq!(<SessionCancel as Method>::NAME, "session/cancel");
        assert_eq!(
            <SessionRequestPermission as Method>::NAME,
            "session/request_permission"
        );
        assert_eq!(<SessionUpdateMethod as Method>::NAME, "session/update");
    }

    /// 方向标记的静态证据：编译期就需要这些 impl 存在。
    #[test]
    fn direction_markers_hold() {
        fn client<M: crate::method::ClientRequest>() {}
        fn server_request<M: ServerRequest>() {}
        fn server_notification<M: ServerNotification>() {}
        client::<Initialize>();
        client::<SessionPrompt>();
        server_request::<SessionRequestPermission>();
        server_notification::<SessionUpdateMethod>();
        assert_eq!(Empty {}, Empty::default());
    }
}

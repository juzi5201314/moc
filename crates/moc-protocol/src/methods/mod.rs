//! 方法契约集合：标记类型、方法名常量与 DTO。
//!
//! 方法表（方向 = 谁能发起）：
//!
//! | 方法 | 方向 | 处理方 | 前置 | 可能的失败码 |
//! |---|---|---|---|---|
//! | [`initialize::Initialize`] | C→S | core | 未初始化 | `-32003` |
//! | [`initialize::Ping`] | C→S | 协议层自答 | 无 | — |
//! | [`config::ConfigGet`] | C→S | core | 已初始化 | `-32014` |
//! | [`session::SessionNew`] / `SessionList` / `SessionLoad` / `SessionRewind` / `SessionUnload` | C→S | core | 已初始化 | `-32010` / `-32014` |
//! | [`session::SessionRename`] / `SessionDelete` / `SessionUnlock` | C→S | core | 已初始化 | `-32010` / `-32015` |
//! | [`session::SessionSetMode`] / [`session::SessionSetModel`] | C→S | core | 已初始化 | `-32010` / `-32015` |
//! | [`plan::SessionPlanApprove`] | C→S | core | 已初始化 | `-32010` / `-32602` |
//! | [`session::SessionCompact`] | C→S | core | 会话无进行中的提示 | `-32010` / `-32011` / `-32014` |
//! | [`prompt::SessionPrompt`] | C→S | core | 已初始化 + 该会话无进行中的提示 | `-32010` / `-32015` / `-32011` |
//! | [`bash::SessionBash`] | C→S | core | 会话无进行中的提示或命令 | `-32010` / `-32011` / `-32015` |
//! | [`prompt::SessionCancel`] | C→S | 协议层自答 | 已初始化 | — |
//! | [`permission::SessionRequestPermission`] / [`ask::SessionRequestAsk`] | S→C | 前端 | 前端已注册处理器 | 前端自定义 |
//! | [`update::SessionUpdateMethod`] | S→C | 前端 | — | — |
//!
//! 方向隔离是强制的：客户端向服务端发 `session/update` /
//! `session/request_permission` / `session/request_ask`，或服务端收到 `session/prompt`
//! 之外的未知名字，一律 [`crate::RpcErrorCode::MethodNotFound`]。

pub mod ask;
pub mod bash;
pub mod config;
pub mod diff;
pub mod initialize;
pub mod management;
pub mod openviking;
pub mod permission;
pub mod plan;
pub mod prompt;
pub mod resources;
pub mod session;
pub mod todo;
pub mod update;

use serde::{Deserialize, Serialize};

pub use ask::{AskAnswer, AskOption, AskParams, AskQuestion, AskResult, SessionRequestAsk};
pub use bash::{BashCommand, BashParams, BashResult, SessionBash};
pub use config::{AgentInfo, ConfigGet, ConfigGetParams, ConfigGetResult, ModelInfo, ModelPricing};
pub use diff::{DiffHunk, DiffLine, DiffLineKind, ToolDiff, ToolFileDiff, build_hunks};
pub use initialize::{
    ClientCapabilities, ClientInfo, EmptyParams, Initialize, InitializeParams, InitializeResult,
    Ping, PingResult, ServerCapabilities, ServerInfo,
};
pub use management::{
    CatalogModel, ChoiceOption, ConfigAction, ConfigField, ConfigManage, ConfigReply, ConfigScope,
    FieldKind, MaintenanceReport, SessionSteer, SettingsDocument, SteerParams, SteerResult,
    WorkspaceBinding,
};
pub use openviking::{
    OpenVikingDelete, OpenVikingDeleteParams, OpenVikingDeleteResult, OpenVikingEntry,
    OpenVikingList, OpenVikingListParams, OpenVikingListResult, OpenVikingRead,
    OpenVikingReadParams, OpenVikingReadResult, OpenVikingWrite, OpenVikingWriteParams,
    OpenVikingWriteResult,
};
pub use permission::{
    PermissionOption, PermissionOptionKind, PermissionRequestParams, PermissionResult,
    SessionRequestPermission, default_options,
};
pub use plan::{
    PlanAction, PlanInfo, PlanStatus, SessionPlanApprove, SessionPlanApproveParams,
    SessionPlanApproveResult,
};
pub use prompt::{
    InputPart, PromptParams, PromptResult, SessionCancel, SessionCancelParams, SessionCancelResult,
    SessionPrompt,
};
pub use resources::{
    ResourceCandidate, ResourceComplete, ResourceCompleteParams, ResourceCompleteResult,
};
pub use session::{
    CompactResult, ContextUsageResult, ContextUsageSection, ModelUsageSummary, SessionCompact,
    SessionCompactParams, SessionContext, SessionContextParams, SessionDelete, SessionDeleteParams,
    SessionInfo, SessionList, SessionListParams, SessionListResult, SessionLoad, SessionLoadPage,
    SessionLoadPageParams, SessionLoadPageResult, SessionLoadParams, SessionLoadResult,
    SessionLockInfo, SessionMode, SessionNew, SessionNewParams, SessionPermission, SessionRename,
    SessionRenameParams, SessionRewind, SessionRewindParams, SessionSetMode, SessionSetModeParams,
    SessionSetModel, SessionSetModelParams, SessionSetPermission, SessionSetPermissionParams,
    SessionUnload, SessionUnloadParams, SessionUnlock, SessionUnlockParams, SessionUsage,
    SessionUsageParams, SessionUsageResult, ToolExecution, ToolStatus, TranscriptChildCall,
    TranscriptEntry,
};
pub use todo::{TodoItem, TodoPhase, TodoStatus};
pub use update::{
    AssistantDelta, PathChange, SessionUpdate, SessionUpdateMethod, SessionUpdateParams,
    ToolBatchCall, UpstreamField, UpstreamFieldMismatch,
};

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

    /// 方法名表：逐类型对照文档字面量，并校验集合无重复。
    #[test]
    fn method_names_match_documented_table() {
        let client_methods = [
            (<Initialize as Method>::NAME, "initialize"),
            (<Ping as Method>::NAME, "ping"),
            (<ConfigGet as Method>::NAME, "config/get"),
            (<SessionNew as Method>::NAME, "session/new"),
            (<SessionList as Method>::NAME, "session/list"),
            (<SessionLoadPage as Method>::NAME, "session/load_page"),
            (<SessionLoad as Method>::NAME, "session/load"),
            (<SessionRewind as Method>::NAME, "session/rewind"),
            (<SessionUnload as Method>::NAME, "session/unload"),
            (<SessionRename as Method>::NAME, "session/rename"),
            (<SessionDelete as Method>::NAME, "session/delete"),
            (<SessionUnlock as Method>::NAME, "session/unlock"),
            (<SessionSetMode as Method>::NAME, "session/set_mode"),
            (
                <SessionSetPermission as Method>::NAME,
                "session/set_permission",
            ),
            (<SessionSetModel as Method>::NAME, "session/set_model"),
            (<SessionPlanApprove as Method>::NAME, "session/plan_approve"),
            (<SessionCompact as Method>::NAME, "session/compact"),
            (<SessionContext as Method>::NAME, "session/context"),
            (<SessionUsage as Method>::NAME, "session/usage"),
            (<SessionPrompt as Method>::NAME, "session/prompt"),
            (<OpenVikingList as Method>::NAME, "openviking/list"),
            (<OpenVikingRead as Method>::NAME, "openviking/read"),
            (<OpenVikingWrite as Method>::NAME, "openviking/write"),
            (<OpenVikingDelete as Method>::NAME, "openviking/delete"),
            (<ResourceComplete as Method>::NAME, "resource/complete"),
        ];
        for (actual, expected) in client_methods {
            assert_eq!(actual, expected);
        }
        let unique: HashSet<&str> = client_methods.iter().map(|(name, _)| *name).collect();
        assert_eq!(unique.len(), client_methods.len(), "客户端方法名不得重复");

        let server_methods = [
            (
                <SessionRequestPermission as Method>::NAME,
                "session/request_permission",
            ),
            (<SessionRequestAsk as Method>::NAME, "session/request_ask"),
            (<SessionUpdateMethod as Method>::NAME, "session/update"),
        ];
        for (actual, expected) in server_methods {
            assert_eq!(actual, expected);
        }
        let unique: HashSet<&str> = server_methods.iter().map(|(name, _)| *name).collect();
        assert_eq!(unique.len(), server_methods.len(), "服务端方法名不得重复");
    }

    /// 方向标记的静态证据：编译期就需要这些 impl 存在。
    #[test]
    fn direction_markers_hold() {
        fn client<M: crate::method::ClientRequest>() {}
        fn server_request<M: ServerRequest>() {}
        fn server_notification<M: ServerNotification>() {}
        client::<Initialize>();
        client::<SessionPrompt>();
        client::<SessionRewind>();
        server_request::<SessionRequestPermission>();
        client::<SessionSetPermission>();
        client::<SessionSetMode>();
        client::<SessionSetModel>();
        client::<SessionPlanApprove>();
        client::<SessionCompact>();
        server_request::<SessionRequestAsk>();
        server_notification::<SessionUpdateMethod>();
        assert_eq!(Empty {}, Empty::default());
    }
}

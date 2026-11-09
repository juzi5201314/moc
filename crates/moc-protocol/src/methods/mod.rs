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
//! | [`session::SessionSetEngine`] / [`session::SessionSetAutonomy`] / [`session::SessionSetModel`] / [`session::SessionSetFastMode`] | C→S | core | 已初始化 | `-32010` / `-32015` |
//! | [`plan::SessionPlanApprove`] | C→S | core | 已初始化 | `-32010` / `-32602` |
//! | [`plan::SessionPlanRevise`] | C→S | core | 已初始化 + 该会话无进行中的提示 | `-32010` / `-32015` / `-32602` |
//! | [`plan::SessionPlanDiff`] | C→S | core | 已初始化 | `-32010` / `-32602` |
//! | [`plugin::PluginList`] / [`plugin::PluginRunCommand`] / [`plugin::PluginReload`] | C→S | core | 已初始化 + 插件可用 | `-32010` / `-32014` |
//! | [`session::SessionCompact`] | C→S | core | 会话无进行中的提示 | `-32010` / `-32011` / `-32014` |
//! | [`prompt::SessionPrompt`] | C→S | core | 已初始化 + 该会话无进行中的提示 | `-32010` / `-32015` / `-32011` |
//! | [`prompt::SessionRetry`] | C→S | core | 已初始化 + 该会话无进行中的提示 + 已有上下文且上一轮未正常结束 | `-32010` / `-32015` / `-32011` / `-32602` |
//! | [`btw::SessionBtw`] | C→S | core | 已初始化 + 该会话无进行中的旁路 | `-32010` / `-32011` / `-32014` |
//! | [`btw::SessionBtwCancel`] | C→S | 协议层自答 | 已初始化 | — |
//! | [`bash::SessionBash`] | C→S | core | 会话无进行中的提示或命令 | `-32010` / `-32011` / `-32015` |
//! | [`job::JobList`] / [`job::JobReadOutput`] / [`job::JobKill`] / [`job::JobBackground`] | C→S | core | 已初始化 | `-32010` / `-32602` |
//! | [`management::SessionSteer`] / [`management::SessionUnsteer`] | C→S | core | 已初始化 | `-32010` / `-32014` |
//! | [`prompt::SessionCancel`] | C→S | 协议层自答 | 已初始化 | — |
//! | [`resources::ResourceComplete`] / [`resources::ResourceList`] / [`resources::ResourceRead`] | C→S | core | 已初始化 | `-32602` / `-32010` |
//! | [`fs::FsListDir`] / [`fs::FsCreateDir`] / [`fs::FsReadFile`] / [`fs::FsWriteFile`] / [`fs::FsCreateFile`] / [`fs::FsRename`] / [`fs::FsDelete`] / [`chart::ChartAnalyze`] | C→S | core | 已初始化 | `-32602` / `-32014` / `-32016` |
//! | [`workspace::WorkspaceGitBranch`] | C→S | core | 已初始化 | `-32602` |
//! | [`web::WebStateGet`] / [`web::WebStateSave`] | C→S | core | 已初始化 | `-32602` / `-32014` |
//! | [`update::SessionUpdateMethod`] / [`update::BtwUpdateMethod`] | S→C | 前端 | — | — |
//! | [`web::WebStateUpdated`] | S→C | 前端 | — | — |
//!
//! 方向隔离是强制的：客户端向服务端发 `session/update` /
//! `session/request_permission` / `session/request_ask`，或服务端收到 `session/prompt`
//! 之外的未知名字，一律 [`crate::RpcErrorCode::MethodNotFound`]。

pub mod agent;
pub mod ask;
pub mod bash;
pub mod btw;
pub mod chart;
pub mod config;
pub mod diff;
pub mod fs;
pub mod history;
pub mod initialize;
pub mod job;
pub mod management;
pub mod permission;
pub mod plan;
pub mod plugin;
pub mod prompt;
pub mod resources;
pub mod session;
pub mod todo;
pub mod update;
pub mod web;
pub mod workflow;
pub mod workspace;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

pub use agent::{
    AgentInfo, AgentList, AgentListParams, AgentListResult, AgentMessage, AgentMessageMeta,
    AgentMessageOutcome, AgentMessageParams, AgentMessageReceipt, AgentMessageResult, AgentRole,
    AgentSpawn, AgentSpawnParams, AgentSpawnResult, AgentStatus, AgentStop, AgentStopParams,
    AgentStopResult, AgentWait, AgentWaitEntry, AgentWaitParams, AgentWaitResult, ToolProgressRow,
};
pub use ask::{AskAnswer, AskOption, AskParams, AskQuestion, AskResult, SessionRequestAsk};
pub use bash::{BashCommand, BashParams, BashResult, SessionBash};
pub use btw::{
    BtwCancelParams, BtwCancelResult, BtwExchange, BtwParams, BtwResult, BtwUpdate,
    BtwUpdateMethod, BtwUpdateParams, MAX_BTW_HISTORY_TURNS, SessionBtw, SessionBtwCancel,
};
pub use chart::{ChartAnalyze, ChartAnalyzeParams, ChartAnalyzeResult};
pub use config::{
    AgentProfile, ConfigGet, ConfigGetParams, ConfigGetResult, FusionPair, ModelInfo, ModelInput,
    ModelPricing,
};
pub use diff::{
    DiffHunk, DiffLine, DiffLineKind, LineChange, ToolDiff, ToolFileDiff, build_hunks,
    build_hunks_from_changes,
};
pub use fs::{
    FS_LIST_DIR_MAX_ENTRIES, FS_READ_FILE_DEFAULT_MAX_BYTES, FS_READ_FILE_MAX_BYTES, FsCreateDir,
    FsCreateDirParams, FsCreateDirResult, FsCreateFile, FsCreateFileParams, FsCreateFileResult,
    FsDelete, FsDeleteParams, FsDirEntry, FsEntryKind, FsFileEncoding, FsListDir, FsListDirParams,
    FsListDirResult, FsReadFile, FsReadFileParams, FsReadFileResult, FsRename, FsRenameParams,
    FsRenameResult, FsWriteFile, FsWriteFileParams, FsWriteFileResult,
};
pub use history::{SessionHistory, SessionHistoryParams, SessionHistoryResult};
pub use initialize::{
    ClientCapabilities, ClientInfo, EmptyParams, Initialize, InitializeParams, InitializeResult,
    Ping, PingResult, ServerCapabilities, ServerInfo,
};
pub use job::{
    JobBackground, JobInfo, JobKill, JobLifecycle, JobList, JobListParams, JobListResult,
    JobOrigin, JobOutputParams, JobOutputResult, JobReadOutput, JobResult, JobStatus, JobTarget,
};
pub use management::{
    CatalogModel, ChoiceOption, ConfigAction, ConfigField, ConfigManage, ConfigReply, ConfigScope,
    FieldKind, MaintenanceReport, SessionSteer, SessionUnsteer, SettingsDocument, SteerParams,
    SteerResult, UnsteerParams, UnsteerResult,
};
pub use permission::{
    PermissionOption, PermissionOptionKind, PermissionRequestParams, PermissionResult,
    SessionRequestPermission, command_options, default_options, path_options,
};
pub use plan::{
    PlanAction, PlanAnnotation, PlanInfo, PlanStatus, SessionPlanApprove, SessionPlanApproveParams,
    SessionPlanApproveResult, SessionPlanDiff, SessionPlanDiffParams, SessionPlanDiffResult,
    SessionPlanMode, SessionPlanModeParams, SessionPlanRevise, SessionPlanReviseParams,
    SessionPlanReviseResult,
};
pub use plugin::{
    PluginCommandInfo, PluginInfo, PluginList, PluginListParams, PluginListResult, PluginReload,
    PluginReloadParams, PluginReloadResult, PluginRunCommand, PluginRunCommandParams,
    PluginRunCommandResult, PluginToolInfo,
};
pub use prompt::{
    InputPart, PromptMode, PromptParams, PromptResult, SessionCancel, SessionCancelParams,
    SessionCancelResult, SessionPrompt, SessionRetry, SessionRetryParams,
};
pub use resources::{
    ResourceCandidate, ResourceComplete, ResourceCompleteParams, ResourceCompleteResult,
    ResourceList, ResourceListEntry, ResourceListParams, ResourceListResult, ResourceRead,
    ResourceReadParams, ResourceReadResult,
};
pub use session::{
    CompactResult, ContextContentResult, ContextContentSection, ContextReadResult, ContextSource,
    ContextUsageResult, ContextUsageSection, GoalAction, GoalBlockReason, GoalPhase, GoalRoundMeta,
    GoalView, ModelSwitchOverflow, ModelUsageSummary, ReminderItem, ReminderStamp, SessionAutonomy,
    SessionCompact, SessionCompactParams, SessionContext, SessionContextClose,
    SessionContextCloseParams, SessionContextContent, SessionContextContentParams,
    SessionContextParams, SessionContextRead, SessionContextReadParams, SessionDelete,
    SessionDeleteParams, SessionEngine, SessionGoal, SessionGoalParams, SessionGoalResult,
    SessionInfo, SessionList, SessionListParams, SessionListResult, SessionLoad, SessionLoadPage,
    SessionLoadPageParams, SessionLoadPageResult, SessionLoadParams, SessionLoadResult,
    SessionLockInfo, SessionModelOverflowAction, SessionNew, SessionNewParams, SessionPermission,
    SessionReask, SessionReaskParams, SessionRedo, SessionRedoParams, SessionRename,
    SessionRenameParams, SessionRewind, SessionRewindParams, SessionSearch, SessionSearchHit,
    SessionSearchMatch, SessionSearchParams, SessionSearchResult, SessionSetAutonomy,
    SessionSetAutonomyParams, SessionSetEngine, SessionSetEngineParams, SessionSetFastMode,
    SessionSetFastModeParams, SessionSetModel, SessionSetModelParams, SessionSetModelResult,
    SessionSetPermission, SessionSetPermissionParams, SessionTools, SessionToolsParams,
    SessionUnload, SessionUnloadParams, SessionUnlock, SessionUnlockParams, SessionUsage,
    SessionUsageParams, SessionUsageResult, ToolExecution, ToolStatus, TranscriptChildCall,
    TranscriptEntry, UsageSource,
};
pub use todo::{TodoItem, TodoPhase, TodoStatus, parse_todos, render_todos};
pub use update::{
    AssistantDelta, PathChange, SessionUpdate, SessionUpdateMethod, SessionUpdateParams,
    ToolBatchCall, UpstreamField, UpstreamFieldMismatch,
};
pub use web::{
    WebDetailLevel, WebDiffMode, WebState, WebStateDocument, WebStateGet, WebStateGetParams,
    WebStateSave, WebStateSaveParams, WebStateUpdated, WebSubmitAction, WebTheme,
};
pub use workflow::{
    SessionWorkflowActivate, SessionWorkflowActivateParams, SessionWorkflowApprove,
    SessionWorkflowApproveParams, SessionWorkflowArtifact, SessionWorkflowArtifactParams,
    SessionWorkflowResume, SessionWorkflowRun, SessionWorkflowRunParams, SessionWorkflowRuns,
    SessionWorkflowRunsParams, SessionWorkflowRunsResult, SessionWorkflowSetRoleModel,
    SessionWorkflowSetRoleModelParams, SessionWorkflowStop, WorkflowAck, WorkflowApproval,
    WorkflowApprovalDefaults, WorkflowApprovalKind, WorkflowArtifactResult, WorkflowDecision,
    WorkflowEvent, WorkflowPhaseState, WorkflowRoleState, WorkflowRunSnapshot, WorkflowRunStatus,
    WorkflowRunSummary, WorkflowStopReason,
};
pub use workspace::{WorkspaceGitBranch, WorkspaceGitBranchParams, WorkspaceGitBranchResult};

/// 空结果：序列化为 `{}`。
///
/// 不用 `null` 表达「无结果」：`null` 与「字段缺失」「结构错误」在前端难以区分，
/// 空对象则能直接反序列化成零字段结构体。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
            (<ConfigManage as Method>::NAME, "config/manage"),
            (<SessionNew as Method>::NAME, "session/new"),
            (<SessionList as Method>::NAME, "session/list"),
            (<SessionSearch as Method>::NAME, "session/search"),
            (<SessionLoadPage as Method>::NAME, "session/load_page"),
            (<SessionLoad as Method>::NAME, "session/load"),
            (<SessionHistory as Method>::NAME, "session/history"),
            (<SessionRewind as Method>::NAME, "session/rewind"),
            (<SessionRedo as Method>::NAME, "session/redo"),
            (<SessionReask as Method>::NAME, "session/reask"),
            (<SessionUnload as Method>::NAME, "session/unload"),
            (<SessionRename as Method>::NAME, "session/rename"),
            (<SessionDelete as Method>::NAME, "session/delete"),
            (<SessionUnlock as Method>::NAME, "session/unlock"),
            (<SessionSetEngine as Method>::NAME, "session/set_engine"),
            (<SessionSetAutonomy as Method>::NAME, "session/set_autonomy"),
            (
                <SessionSetPermission as Method>::NAME,
                "session/set_permission",
            ),
            (<SessionSetModel as Method>::NAME, "session/set_model"),
            (
                <SessionSetFastMode as Method>::NAME,
                "session/set_fast_mode",
            ),
            (<SessionGoal as Method>::NAME, "session/goal"),
            (<SessionPlanMode as Method>::NAME, "session/plan_mode"),
            (<SessionPlanApprove as Method>::NAME, "session/plan_approve"),
            (<SessionPlanRevise as Method>::NAME, "session/plan_revise"),
            (<SessionPlanDiff as Method>::NAME, "session/plan_diff"),
            (<SessionSteer as Method>::NAME, "session/steer"),
            (<SessionUnsteer as Method>::NAME, "session/unsteer"),
            (<SessionCompact as Method>::NAME, "session/compact"),
            (<SessionContext as Method>::NAME, "session/context"),
            (
                <SessionContextContent as Method>::NAME,
                "session/context_content",
            ),
            (<SessionContextRead as Method>::NAME, "session/context_read"),
            (
                <SessionContextClose as Method>::NAME,
                "session/context_close",
            ),
            (<SessionUsage as Method>::NAME, "session/usage"),
            (<SessionPrompt as Method>::NAME, "session/prompt"),
            (<SessionRetry as Method>::NAME, "session/retry"),
            (<SessionBtw as Method>::NAME, "session/btw"),
            (<SessionBtwCancel as Method>::NAME, "session/btw_cancel"),
            (<SessionBash as Method>::NAME, "session/bash"),
            (<SessionCancel as Method>::NAME, "session/cancel"),
            (<PluginList as Method>::NAME, "plugin/list"),
            (<PluginReload as Method>::NAME, "plugin/reload"),
            (<PluginRunCommand as Method>::NAME, "plugin/run_command"),
            (<ResourceComplete as Method>::NAME, "resource/complete"),
            (<ResourceList as Method>::NAME, "resource/list"),
            (<ResourceRead as Method>::NAME, "resource/read"),
            (<FsListDir as Method>::NAME, "fs/list_dir"),
            (<FsCreateDir as Method>::NAME, "fs/create_dir"),
            (<FsReadFile as Method>::NAME, "fs/read_file"),
            (<FsWriteFile as Method>::NAME, "fs/write_file"),
            (<FsCreateFile as Method>::NAME, "fs/create_file"),
            (<FsRename as Method>::NAME, "fs/rename"),
            (<FsDelete as Method>::NAME, "fs/delete"),
            (<ChartAnalyze as Method>::NAME, "chart/analyze"),
            (<WorkspaceGitBranch as Method>::NAME, "workspace/git_branch"),
            (<WebStateGet as Method>::NAME, "web/state"),
            (<WebStateSave as Method>::NAME, "web/state_save"),
            (<AgentList as Method>::NAME, "agent/list"),
            (<AgentMessage as Method>::NAME, "agent/message"),
            (<AgentStop as Method>::NAME, "agent/stop"),
            (<AgentWait as Method>::NAME, "agent/wait"),
            (<JobList as Method>::NAME, "job/list"),
            (<JobReadOutput as Method>::NAME, "job/output"),
            (<JobKill as Method>::NAME, "job/kill"),
            (<JobBackground as Method>::NAME, "job/background"),
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
            (<BtwUpdateMethod as Method>::NAME, "session/btw_update"),
            (<WebStateUpdated as Method>::NAME, "web/state_updated"),
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
        client::<SessionRetry>();
        client::<SessionBtw>();
        client::<SessionBtwCancel>();
        client::<SessionRewind>();
        client::<SessionRedo>();
        server_request::<SessionRequestPermission>();
        client::<SessionSetPermission>();
        client::<SessionSetEngine>();
        client::<SessionSetAutonomy>();
        client::<SessionSetModel>();
        client::<SessionSetFastMode>();
        client::<SessionGoal>();
        client::<SessionPlanMode>();
        client::<SessionPlanApprove>();
        client::<AgentSpawn>();
        client::<AgentList>();
        client::<AgentMessage>();
        client::<AgentStop>();
        client::<AgentWait>();
        client::<SessionCompact>();
        client::<SessionContextContent>();
        client::<SessionContextRead>();
        client::<SessionContextClose>();
        server_request::<SessionRequestAsk>();
        server_notification::<SessionUpdateMethod>();
        server_notification::<BtwUpdateMethod>();
        server_notification::<WebStateUpdated>();
        client::<ChartAnalyze>();
        client::<WorkspaceGitBranch>();
        assert_eq!(Empty {}, Empty::default());
    }
}

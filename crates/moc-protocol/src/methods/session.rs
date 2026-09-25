//! 会话生命周期方法：`session/new` / `list` / `load` / `unload` / `rename` / `delete` / `unlock` / `set_mode` / `compact`。
//!
//! 会话状态、持久化与锁的实际持有全在 core；协议层只搬运 [`SessionInfo::lock`] 这类
//! 投影。锁语义只有一条：**某进程正在该会话里跑一轮 agent 工作时上锁；其它 core
//! 可读不可写**。
//!
//! | 操作 | 被他人持锁时 |
//! |---|---|
//! | `session/list`、`session/load`、`config/get` | 始终允许 |
//! | `session/prompt`、`session/rename`、`session/delete`、`session/set_mode` | [`crate::RpcErrorCode::SessionLocked`] |
//! | `session/unlock{force:true}` | 无条件清除锁记录（强夺） |
//!
//! `force=false` 的 `session/unlock` 只释放**本进程**持有的锁；对他人持有的锁，
//! core 返回 [`crate::RpcErrorCode::SessionLocked`]，`force=true` 则无条件清除。

use super::{plan::PlanInfo, todo::TodoPhase};
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// `session/new`：新建会话。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionNew;

/// `session/list`：列出全部会话。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionList;

/// `session/load`：读取会话 transcript。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionLoad;

/// `session/unload`：释放会话的常驻资源（不删除会话、不影响列表）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUnload;

/// `session/rename`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionRename;

/// `session/delete`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionDelete;

/// `session/unlock`。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionUnlock;

/// 会话工作模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMode {
    #[default]
    Normal,
    Ask,
    Plan,
}

/// `session/set_mode`：切换会话工作模式。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionSetMode;

/// `session/compact`：手动压缩会话上下文。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SessionCompact;

/// `session/new` 的参数。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionNewParams {
    /// 工作目录；`None` 由 core 取自己的当前目录。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 模型 ID；`None` 用 core 的默认模型。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// 标题；`None` 由 core 生成（如取首条用户输入）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 新会话初始模式。
    #[serde(default)]
    pub mode: SessionMode,
}

/// 一条会话的元信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionInfo {
    /// 会话 ID，由 core 生成（如 `ses_7f3a`）。
    pub id: String,
    /// 标题。
    pub title: String,
    /// 当前模型 ID。
    pub model: String,
    /// 当前工作模式。
    #[serde(default)]
    pub mode: SessionMode,
    /// 工作目录。
    pub cwd: String,
    /// 创建时间的 UNIX 毫秒时间戳。
    pub created_at_ms: u64,
    /// 最近一次更新的 UNIX 毫秒时间戳。
    pub updated_at_ms: u64,
    /// 锁投影；`None` 表示当前没有其它进程在该会话里跑轮次。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lock: Option<SessionLockInfo>,
}

/// 会话锁的投影。
///
/// 只用于**展示与强夺**：锁的判定与持有在 core，协议层不参与互斥。前端看到
/// `Some(_)` 时应当把该会话渲染为只读，并把 `unlock{force:true}` 提供给用户
/// 处理崩溃进程留下的陈旧锁。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLockInfo {
    /// 持锁进程 PID。
    pub pid: u32,
    /// 持锁前端形态（`tui` / `webui` / `desktop`）。
    pub frontend: String,
    /// 进程名；`None` 表示 core 未取到。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_name: Option<String>,
    /// 上锁时间的 UNIX 毫秒时间戳。
    pub acquired_at_ms: u64,
}

/// `session/list` 的参数：空对象。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListParams {}

/// `session/list` 的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    /// 会话列表；core 按 `updated_at_ms` 降序排列。
    pub sessions: Vec<SessionInfo>,
}

/// `session/load` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadParams {
    /// 目标会话。
    pub session_id: String,
    /// 最多返回多少条 transcript 条目（从最新往前数）；`None` 表示全部。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_entries: Option<u32>,
}

/// `session/load` 的结果。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionLoadResult {
    /// 会话元信息（含锁投影）。
    pub session: SessionInfo,
    /// transcript 条目；`truncated` 为真时只含最新的 `max_entries` 条。
    pub entries: Vec<TranscriptEntry>,
    /// 是否因 `max_entries` 截断。
    pub truncated: bool,
    /// 当前 todo 阶段；空值兼容旧 core。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub todos: Vec<TodoPhase>,
    /// 当前会话计划；空值兼容旧 core。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub plans: Vec<PlanInfo>,
}

/// 一条 transcript 条目。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscriptEntry {
    /// 条目 ID，由 core 生成。
    pub id: String,
    /// 记录时间的 UNIX 毫秒时间戳。
    pub at_ms: u64,
    /// 内容；复用 `moc-llm` 的消息类型。
    pub message: crate::Message,
    /// 该消息对应的工具执行状态；`None` 表示与工具无关。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool: Option<ToolExecution>,
}

/// 一次工具执行的终态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolExecution {
    /// 对应的 [`crate::ToolCall::id`]。
    pub tool_call_id: String,
    /// 终态。
    pub status: ToolStatus,
    /// 执行耗时毫秒；`None` 表示不计时（拒绝、取消等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
}

/// 工具执行终态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    /// 正常结束。
    Completed,
    /// 执行失败（工具报错、超时等）。
    Error,
    /// 未获授权。
    Denied,
    /// 轮次被取消。
    Cancelled,
}

/// `session/unload` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUnloadParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/rename` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionRenameParams {
    /// 目标会话。
    pub session_id: String,
    /// 新标题。
    pub title: String,
}

/// `session/delete` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionDeleteParams {
    /// 目标会话。
    pub session_id: String,
}

/// `session/unlock` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionUnlockParams {
    /// 目标会话。
    pub session_id: String,
    /// `false` 只释放本进程持有的锁；`true` 无条件清除锁记录（强夺）。
    pub force: bool,
}

/// `session/set_mode` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionSetModeParams {
    pub session_id: String,
    pub mode: SessionMode,
}

/// `session/compact` 的参数。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionCompactParams {
    pub session_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// 一次上下文压缩的结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompactResult {
    pub method: String,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub summary_chars: u32,
}

impl Method for SessionNew {
    const NAME: &'static str = "session/new";
    type Params = SessionNewParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionNew {}

impl Method for SessionList {
    const NAME: &'static str = "session/list";
    type Params = SessionListParams;
    type Result = SessionListResult;
}

impl ClientRequest for SessionList {}

impl Method for SessionLoad {
    const NAME: &'static str = "session/load";
    type Params = SessionLoadParams;
    type Result = SessionLoadResult;
}

impl ClientRequest for SessionLoad {}

impl Method for SessionUnload {
    const NAME: &'static str = "session/unload";
    type Params = SessionUnloadParams;
    type Result = crate::methods::Empty;
}

impl ClientRequest for SessionUnload {}

impl Method for SessionRename {
    const NAME: &'static str = "session/rename";
    type Params = SessionRenameParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionRename {}

impl Method for SessionDelete {
    const NAME: &'static str = "session/delete";
    type Params = SessionDeleteParams;
    type Result = crate::methods::Empty;
}

impl ClientRequest for SessionDelete {}

impl Method for SessionUnlock {
    const NAME: &'static str = "session/unlock";
    type Params = SessionUnlockParams;
    type Result = crate::methods::Empty;
}

impl ClientRequest for SessionUnlock {}

impl Method for SessionSetMode {
    const NAME: &'static str = "session/set_mode";
    type Params = SessionSetModeParams;
    type Result = SessionInfo;
}

impl ClientRequest for SessionSetMode {}

impl Method for SessionCompact {
    const NAME: &'static str = "session/compact";
    type Params = SessionCompactParams;
    type Result = CompactResult;
}

impl ClientRequest for SessionCompact {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn method_names() {
        assert_eq!(<SessionNew as Method>::NAME, "session/new");
        assert_eq!(<SessionList as Method>::NAME, "session/list");
        assert_eq!(<SessionLoad as Method>::NAME, "session/load");
        assert_eq!(<SessionUnload as Method>::NAME, "session/unload");
        assert_eq!(<SessionRename as Method>::NAME, "session/rename");
        assert_eq!(<SessionDelete as Method>::NAME, "session/delete");
        assert_eq!(<SessionUnlock as Method>::NAME, "session/unlock");
        assert_eq!(<SessionSetMode as Method>::NAME, "session/set_mode");
        assert_eq!(<SessionCompact as Method>::NAME, "session/compact");
    }

    #[test]
    fn session_new_accepts_partial_params() {
        let parsed: SessionNewParams =
            serde_json::from_value(serde_json::json!({})).expect("可反序列化");
        assert_eq!(parsed, SessionNewParams::default());
        let parsed: SessionNewParams =
            serde_json::from_value(serde_json::json!({ "cwd": "/tmp/ws" })).expect("可反序列化");
        assert_eq!(parsed.cwd.as_deref(), Some("/tmp/ws"));
        assert!(parsed.model.is_none());
        // 未知字段必须暴露
        assert!(
            serde_json::from_value::<SessionNewParams>(serde_json::json!({ "cwdly": 1 })).is_err()
        );
        // 空结果序列化为 {}
        assert_eq!(
            serde_json::to_value(crate::methods::Empty {}).expect("可序列化"),
            serde_json::json!({})
        );
    }

    #[test]
    fn load_and_lock_shapes() {
        let params = SessionLoadParams {
            session_id: "ses_1".into(),
            max_entries: None,
        };
        assert_eq!(
            serde_json::to_value(&params).expect("可序列化"),
            serde_json::json!({ "session_id": "ses_1" })
        );
        let lock = SessionLockInfo {
            pid: 4242,
            frontend: "tui".into(),
            process_name: Some("moc-tui".into()),
            acquired_at_ms: 1_724_000_000_000,
        };
        assert_eq!(
            serde_json::to_value(&lock).expect("可序列化"),
            serde_json::json!({
                "pid": 4242,
                "frontend": "tui",
                "process_name": "moc-tui",
                "acquired_at_ms": 1_724_000_000_000u64
            })
        );
        assert_eq!(
            serde_json::to_value(ToolStatus::Denied).expect("可序列化"),
            serde_json::json!("denied")
        );
        let info = SessionInfo {
            id: "ses_1".into(),
            title: "t".into(),
            model: "m".into(),
            cwd: "/tmp".into(),
            created_at_ms: 1,
            mode: SessionMode::Plan,
            updated_at_ms: 2,
            lock: None,
        };
        assert_eq!(
            serde_json::to_value(&info).expect("可序列化"),
            serde_json::json!({
                "id": "ses_1", "title": "t", "model": "m", "mode": "plan", "cwd": "/tmp",
                "created_at_ms": 1, "updated_at_ms": 2
            })
        );
    }
}

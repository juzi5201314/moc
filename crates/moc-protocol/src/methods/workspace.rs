use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// Git 分支快速查询：按工作目录探测当前分支与仓库根。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceGitBranch;

/// 当前工作目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitBranchParams {
    pub cwd: String,
}

/// 目录是否处于 Git 仓库中及当前 HEAD 概要。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitBranchResult {
    /// 当前分支名；游离头指针（detached）或非仓库时为 `None`，空仓库（unborn）为初始分支名。
    pub branch: Option<String>,
    /// 仓库根目录（canonical/toplevel）；非仓库时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// HEAD 短提交哈希；空仓库或非仓库时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    /// 是否处于游离头指针状态（detached HEAD）。
    #[serde(default)]
    pub detached: bool,
    /// 是否为尚未有首次提交的空仓库（unborn HEAD）。
    #[serde(default)]
    pub unborn: bool,
}

impl Method for WorkspaceGitBranch {
    const NAME: &'static str = "workspace/git_branch";
    type Params = WorkspaceGitBranchParams;
    type Result = WorkspaceGitBranchResult;
    crate::method::json_codec!();
}

impl ClientRequest for WorkspaceGitBranch {}

/// 工作区 Git 脏状态计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitWorktreeStatus {
    /// 已暂存（index）的变更条目数。
    pub staged: u32,
    /// 工作区未暂存的已跟踪变更条目数。
    pub unstaged: u32,
    /// 未跟踪文件/目录条目数。
    pub untracked: u32,
    /// 未解决冲突（unmerged）的条目数。
    pub conflicted: u32,
}

impl GitWorktreeStatus {
    /// 是否存在任意未提交或未跟踪更改（不含未解决冲突时另由 `conflicted` 判定）。
    pub const fn has_changes(&self) -> bool {
        self.staged > 0 || self.unstaged > 0 || self.untracked > 0
    }
}

/// 仓库正在进行中的多步 Git 操作。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitOperation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
}

/// 本地分支条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitBranchEntry {
    /// 本地分支短名（如 `master`、`feat/git-switch`）。
    pub name: String,
    /// 是否为当前 worktree 正在检出的分支。
    pub current: bool,
    /// 若已被其他 git worktree 检出，返回该 worktree 的路径；当前 worktree 为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree_path: Option<String>,
    /// 关联的上游分支短名（如 `origin/master`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upstream: Option<String>,
}

/// 远程跟踪分支条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitRemoteBranchEntry {
    /// 远程引用短名（如 `origin/feat-x`）。
    pub name: String,
    /// 远程名（如 `origin`）。
    pub remote: String,
    /// 去掉远程前缀后的分支名（如 `feat-x`）。
    pub branch: String,
    /// 本地是否已存在同名分支。
    pub has_local: bool,
}

/// 列举工作区 Git 仓库状态、本地分支、远程分支与并发占用。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceGitBranches;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitBranchesParams {
    pub cwd: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitBranchesResult {
    /// 是否处于 Git 仓库中。
    pub in_repo: bool,
    /// 仓库根目录；非仓库时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root: Option<String>,
    /// 当前分支名；detached HEAD 时为 `None`，unborn 时为初始分支名。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current: Option<String>,
    /// HEAD 短提交哈希；unborn 或非仓库时为 `None`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_oid: Option<String>,
    /// 是否处于 detached HEAD。
    #[serde(default)]
    pub detached: bool,
    /// 是否为尚无提交的空仓库。
    #[serde(default)]
    pub unborn: bool,
    /// 工作区未提交/未跟踪/冲突摘要。
    pub status: GitWorktreeStatus,
    /// 正在进行中的仓库操作（存在时禁止切分支）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub in_progress: Option<GitOperation>,
    /// 同仓库下正在运行 prompt / 子代理 / 后台任务的会话标题列表。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub busy_sessions: Vec<String>,
    /// 本地分支（当前分支置顶，其余按提交时间降序）。
    #[serde(default)]
    pub local: Vec<GitBranchEntry>,
    /// 远程分支（已过滤 `*/HEAD` 符号引用）。
    #[serde(default)]
    pub remote: Vec<GitRemoteBranchEntry>,
}

impl Method for WorkspaceGitBranches {
    const NAME: &'static str = "workspace/git_branches";
    type Params = WorkspaceGitBranchesParams;
    type Result = WorkspaceGitBranchesResult;
    crate::method::json_codec!();
}

impl ClientRequest for WorkspaceGitBranches {}

/// 脏工作区切换策略。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitSwitchDirtyPolicy {
    /// 发现未提交/未跟踪更改或 detached HEAD 时不切换，返回 `NeedsConfirmation`。
    #[default]
    Check,
    /// 先 `git stash push --include-untracked` 保留改动再切换；失败时尽力自动回滚。
    Stash,
    /// 尝试带着更改安全切换（不强行覆盖）；发生文件覆盖冲突时返回 `NeedsConfirmation`。
    Carry,
}

/// 切换分支被硬阻断的原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum GitSwitchBlockReason {
    NotARepo,
    BusySessions,
    OperationInProgress,
    UnresolvedConflicts,
    WorktreeInUse,
    InvalidBranchName,
    BranchExists,
    BranchNotFound,
    UnbornRepo,
    LockFile,
    GitFailed,
}

/// 切换已有本地/远程分支，或基于当前 HEAD 新建并切换分支。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceGitSwitch;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitSwitchParams {
    pub cwd: String,
    /// 目标分支名：`create=true` 时为新分支名；`remote=true` 时为远程引用短名（如 `origin/feat`）。
    pub branch: String,
    /// 基于当前 HEAD 新建并切换分支（未提交更改会无损带入新分支）。
    #[serde(default)]
    pub create: bool,
    /// 目标是否为远程跟踪分支（`create=true` 时忽略）。
    #[serde(default)]
    pub remote: bool,
    /// 脏工作区处理策略。
    #[serde(default)]
    pub dirty: GitSwitchDirtyPolicy,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum WorkspaceGitSwitchResult {
    /// 切换或新建分支成功。
    Switched {
        /// 切换后的本地分支名。
        branch: String,
        /// 切换前的分支名；detached HEAD 或未知时为 `None`。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        previous_branch: Option<String>,
        /// 执行了自动暂存时携带 stash message。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stash_message: Option<String>,
        /// 是否新建了本地分支（含 `create=true` 或首次 track 远程分支）。
        created: bool,
    },
    /// 需要用户确认脏工作区处置或 detached HEAD 风险。
    NeedsConfirmation {
        status: GitWorktreeStatus,
        /// 受影响的本地变更路径预览（已截断）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        dirty_files: Vec<String>,
        /// 是否从 detached HEAD 切走。
        #[serde(default)]
        detached: bool,
        /// `Carry` 策略下 Git 因本地文件会被目标分支覆盖而拒绝。
        #[serde(default)]
        overwrite_conflict: bool,
        /// Git 报告的具体冲突文件路径。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        conflict_files: Vec<String>,
    },
    /// 硬阻断：当前状态不可安全切换。
    Blocked {
        reason: GitSwitchBlockReason,
        message: String,
        /// 补充条目（运行中会话、冲突路径或占用 worktree 路径）。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        details: Vec<String>,
    },
}

impl Method for WorkspaceGitSwitch {
    const NAME: &'static str = "workspace/git_switch";
    type Params = WorkspaceGitSwitchParams;
    type Result = WorkspaceGitSwitchResult;
    crate::method::json_codec!();
}

impl ClientRequest for WorkspaceGitSwitch {}

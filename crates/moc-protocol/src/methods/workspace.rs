use serde::{Deserialize, Serialize};

use crate::method::{ClientRequest, Method};

/// Git 分支查询：与 TUI 相同，按工作目录调用本机 git。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct WorkspaceGitBranch;

/// 当前工作目录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitBranchParams {
    pub cwd: String,
}

/// 目录是否处于 Git 仓库中及当前分支。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceGitBranchResult {
    pub branch: Option<String>,
}

impl Method for WorkspaceGitBranch {
    const NAME: &'static str = "workspace/git_branch";
    type Params = WorkspaceGitBranchParams;
    type Result = WorkspaceGitBranchResult;
}

impl ClientRequest for WorkspaceGitBranch {}

//! 工作区 Git 分支探测、列举与安全切换。
//!
//! 职责边界：
//! - `git_branch`：优先零子进程读取 `.git/HEAD`（兼容子目录、linked worktree、packed-refs、
//!   unborn 与 detached HEAD），遇到 reftable 等非常规布局时回退到 `git` CLI，便于高频轮询与即时刷新。
//! - `git_branches`：汇总仓库状态、脏文件统计、进行中操作、worktree 占用、同仓库活跃会话与本地/远程分支。
//! - `git_switch`：串行化执行安全切换与新建分支；绝不静默丢弃改动，脏工作区需显式策略（`Check`/`Stash`/`Carry`），
//!   `Stash` 失败回滚；切换成功后失效同仓库会话的锚点与 `AGENTS.md` 缓存并注入一次性模型通知。

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use moc_i18n::t;
use moc_protocol::methods::{
    GitBranchEntry, GitOperation, GitRemoteBranchEntry, GitSwitchBlockReason, GitSwitchDirtyPolicy,
    GitWorktreeStatus, WorkspaceGitBranchResult, WorkspaceGitBranchesResult,
    WorkspaceGitSwitchParams, WorkspaceGitSwitchResult,
};

use super::CoreInner;
use crate::session::state::SessionNotice;

/// 只读探测类 git 命令超时。
const GIT_READ_TIMEOUT: Duration = Duration::from_secs(15);
/// 写操作（switch / stash）git 命令超时，留足大仓库与 hook 余量。
const GIT_WRITE_TIMEOUT: Duration = Duration::from_secs(30);
/// 确认弹窗中展示的受影响文件预览上限。
const MAX_DIRTY_PREVIEW: usize = 20;
/// 切分支后写入会话的一次性通知前缀，用于连续切换时去重。
const BRANCH_NOTICE_PREFIX: &str = "Workspace git branch switched ";

// ---------- 快速零子进程探测（.git/HEAD） ----------

struct DiscoveredGit {
    work_tree: PathBuf,
    git_dir: PathBuf,
}

/// 沿目录树向上查找 `.git`（目录或 linked worktree 文件），不启动子进程。
fn discover_git_fast(cwd: &Path) -> Option<DiscoveredGit> {
    if !cwd.is_dir() {
        return None;
    }
    let start = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let mut dir = Some(start.as_path());
    while let Some(current) = dir {
        let dot_git = current.join(".git");
        let Ok(meta) = std::fs::symlink_metadata(&dot_git) else {
            dir = current.parent();
            continue;
        };
        if meta.is_dir() {
            return Some(DiscoveredGit {
                work_tree: current.to_path_buf(),
                git_dir: dot_git,
            });
        }
        if meta.is_file() {
            let text = std::fs::read_to_string(&dot_git).ok()?;
            let first = text.lines().next()?.trim();
            let raw = first.strip_prefix("gitdir:")?.trim();
            let git_dir = if Path::new(raw).is_absolute() {
                PathBuf::from(raw)
            } else {
                current.join(raw)
            };
            let git_dir = std::fs::canonicalize(&git_dir).unwrap_or(git_dir);
            return Some(DiscoveredGit {
                work_tree: current.to_path_buf(),
                git_dir,
            });
        }
        dir = current.parent();
    }
    None
}

/// 零子进程定位当前工作目录所属的 Git 工作树根（canonical）；用于同仓库会话匹配。
fn find_repo_root_fast(cwd: &Path) -> Option<PathBuf> {
    discover_git_fast(cwd).map(|found| found.work_tree)
}

/// 读取 linked worktree 的 `commondir`（普通仓库返回 `git_dir` 自身）。
fn common_git_dir(git_dir: &Path) -> PathBuf {
    let marker = git_dir.join("commondir");
    let Ok(text) = std::fs::read_to_string(marker) else {
        return git_dir.to_path_buf();
    };
    let raw = text.trim();
    if raw.is_empty() {
        return git_dir.to_path_buf();
    }
    let joined = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        git_dir.join(raw)
    };
    std::fs::canonicalize(&joined).unwrap_or(joined)
}

fn is_hex_oid(value: &str) -> bool {
    (value.len() == 40 || value.len() == 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn short_oid(oid: &str) -> String {
    oid.chars().take(7).collect()
}

/// 沿松散引用与 `packed-refs` 解析引用指向的 commit OID；未找到表示 unborn。
fn resolve_ref_oid(git_dir: &Path, common_dir: &Path, ref_name: &str, depth: u8) -> Option<String> {
    if depth > 8 {
        return None;
    }
    for base in [git_dir, common_dir] {
        let candidate = base.join(ref_name);
        if let Ok(content) = std::fs::read_to_string(candidate) {
            let trimmed = content.trim();
            if let Some(target) = trimmed.strip_prefix("ref:") {
                return resolve_ref_oid(git_dir, common_dir, target.trim(), depth + 1);
            }
            if is_hex_oid(trimmed) {
                return Some(trimmed.to_owned());
            }
        }
    }
    let packed = std::fs::read_to_string(common_dir.join("packed-refs")).ok()?;
    for line in packed.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed.starts_with('^') {
            continue;
        }
        if let Some((oid, name)) = trimmed.split_once(' ')
            && name.trim() == ref_name
            && is_hex_oid(oid)
        {
            return Some(oid.to_owned());
        }
    }
    None
}

/// 尝试直接从 `.git/HEAD` 读出分支快照；返回 `Err(())` 时由调用方回退到 `git` CLI。
fn probe_head_fast(cwd: &Path) -> Result<Option<WorkspaceGitBranchResult>, ()> {
    let Some(found) = discover_git_fast(cwd) else {
        return Ok(None);
    };
    let common_dir = common_git_dir(&found.git_dir);
    if common_dir.join("reftable").exists() {
        return Err(());
    }
    let head = std::fs::read_to_string(found.git_dir.join("HEAD")).map_err(|_| ())?;
    let trimmed = head.trim();
    let root = Some(found.work_tree.to_string_lossy().into_owned());
    if let Some(branch) = trimmed.strip_prefix("ref: refs/heads/") {
        if branch.is_empty() || branch == ".invalid" {
            return Err(());
        }
        let ref_name = format!("refs/heads/{branch}");
        let oid = resolve_ref_oid(&found.git_dir, &common_dir, &ref_name, 0);
        return Ok(Some(WorkspaceGitBranchResult {
            branch: Some(branch.to_owned()),
            root,
            head_oid: oid.as_deref().map(short_oid),
            detached: false,
            unborn: oid.is_none(),
        }));
    }
    if trimmed.starts_with("ref:") {
        return Err(());
    }
    if is_hex_oid(trimmed) {
        return Ok(Some(WorkspaceGitBranchResult {
            branch: None,
            root,
            head_oid: Some(short_oid(trimmed)),
            detached: true,
            unborn: false,
        }));
    }
    Err(())
}

/// `workspace/git_branch`：优先零子进程读 `.git/HEAD`，无法解析时回退 `git` CLI。
pub(super) async fn git_branch(cwd: &str) -> WorkspaceGitBranchResult {
    let path = PathBuf::from(cwd);
    let fast_path = path.clone();
    let fast = tokio::task::spawn_blocking(move || probe_head_fast(&fast_path)).await;
    match fast {
        Ok(Ok(Some(result))) => return result,
        Ok(Ok(None)) => return WorkspaceGitBranchResult::default(),
        Ok(Err(())) | Err(_) => {}
    }

    let Some(root) = repo_toplevel(&path).await else {
        return WorkspaceGitBranchResult::default();
    };
    let Ok(output) = run_git(
        &root,
        &["-c", "core.quotePath=false", "status", "--porcelain=v2", "--branch"],
        GIT_READ_TIMEOUT,
    )
    .await
    else {
        return WorkspaceGitBranchResult::default();
    };
    if !output.success {
        return WorkspaceGitBranchResult::default();
    }
    let parsed = parse_status_v2(&output.stdout);
    WorkspaceGitBranchResult {
        branch: parsed.current,
        root: Some(root.to_string_lossy().into_owned()),
        head_oid: parsed.head_oid,
        detached: parsed.detached,
        unborn: parsed.unborn,
    }
}

// ---------- Git CLI 执行与输出解析 ----------

struct GitOutput {
    success: bool,
    stdout: String,
    stderr: String,
}

enum GitExecError {
    Timeout,
    Spawn(String),
}

async fn run_git(cwd: &Path, args: &[&str], timeout: Duration) -> Result<GitOutput, GitExecError> {
    let mut cmd = tokio::process::Command::new("git");
    cmd.args(args)
        .current_dir(cwd)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let output = tokio::time::timeout(timeout, cmd.output())
        .await
        .map_err(|_| GitExecError::Timeout)?
        .map_err(|error| GitExecError::Spawn(error.to_string()))?;
    Ok(GitOutput {
        success: output.status.success(),
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    })
}

async fn repo_toplevel(cwd: &Path) -> Option<PathBuf> {
    if !cwd.is_dir() {
        return None;
    }
    let output = run_git(cwd, &["rev-parse", "--show-toplevel"], GIT_READ_TIMEOUT)
        .await
        .ok()?;
    if !output.success {
        return None;
    }
    let raw = output.stdout.trim();
    if raw.is_empty() {
        return None;
    }
    let path = PathBuf::from(raw);
    Some(std::fs::canonicalize(&path).unwrap_or(path))
}

#[derive(Default)]
struct ParsedStatus {
    current: Option<String>,
    head_oid: Option<String>,
    detached: bool,
    unborn: bool,
    status: GitWorktreeStatus,
    dirty_files: Vec<String>,
    conflicted_files: Vec<String>,
}

/// 解析 `git -c core.quotePath=false status --porcelain=v2 --branch`。
fn parse_status_v2(stdout: &str) -> ParsedStatus {
    let mut parsed = ParsedStatus::default();
    let mut seen_dirty = HashSet::new();
    let mut push_dirty = |path: &str, out: &mut Vec<String>| {
        if out.len() < MAX_DIRTY_PREVIEW && seen_dirty.insert(path.to_owned()) {
            out.push(path.to_owned());
        }
    };

    for line in stdout.lines() {
        if let Some(oid) = line.strip_prefix("# branch.oid ") {
            let oid = oid.trim();
            if oid == "(initial)" {
                parsed.unborn = true;
                parsed.head_oid = None;
            } else if !oid.is_empty() {
                parsed.head_oid = Some(short_oid(oid));
            }
            continue;
        }
        if let Some(head) = line.strip_prefix("# branch.head ") {
            let head = head.trim();
            if head == "(detached)" {
                parsed.detached = true;
                parsed.current = None;
            } else if !head.is_empty() {
                parsed.detached = false;
                parsed.current = Some(head.to_owned());
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("1 ") {
            let mut fields = rest.splitn(8, ' ');
            let xy = fields.next().unwrap_or("..");
            let bytes = xy.as_bytes();
            if bytes.first().copied().unwrap_or(b'.') != b'.' {
                parsed.status.staged = parsed.status.staged.saturating_add(1);
            }
            if bytes.get(1).copied().unwrap_or(b'.') != b'.' {
                parsed.status.unstaged = parsed.status.unstaged.saturating_add(1);
            }
            if let Some(path) = fields.nth(6) {
                push_dirty(path, &mut parsed.dirty_files);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("2 ") {
            let mut fields = rest.splitn(9, ' ');
            let xy = fields.next().unwrap_or("..");
            let bytes = xy.as_bytes();
            if bytes.first().copied().unwrap_or(b'.') != b'.' {
                parsed.status.staged = parsed.status.staged.saturating_add(1);
            }
            if bytes.get(1).copied().unwrap_or(b'.') != b'.' {
                parsed.status.unstaged = parsed.status.unstaged.saturating_add(1);
            }
            if let Some(path_pair) = fields.nth(7) {
                let path = path_pair.split('\t').next().unwrap_or(path_pair);
                push_dirty(path, &mut parsed.dirty_files);
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("u ") {
            parsed.status.conflicted = parsed.status.conflicted.saturating_add(1);
            if let Some(path) = rest.splitn(10, ' ').nth(9) {
                if parsed.conflicted_files.len() < MAX_DIRTY_PREVIEW {
                    parsed.conflicted_files.push(path.to_owned());
                }
                push_dirty(path, &mut parsed.dirty_files);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("? ") {
            parsed.status.untracked = parsed.status.untracked.saturating_add(1);
            push_dirty(path, &mut parsed.dirty_files);
        }
    }
    parsed
}

/// 探测仓库是否处于 merge / rebase / cherry-pick / revert / bisect 中间态。
async fn detect_in_progress(root: &Path) -> Option<GitOperation> {
    let output = run_git(
        root,
        &[
            "rev-parse",
            "--git-path",
            "rebase-merge",
            "--git-path",
            "rebase-apply",
            "--git-path",
            "REBASE_HEAD",
            "--git-path",
            "MERGE_HEAD",
            "--git-path",
            "CHERRY_PICK_HEAD",
            "--git-path",
            "REVERT_HEAD",
            "--git-path",
            "BISECT_LOG",
        ],
        GIT_READ_TIMEOUT,
    )
    .await
    .ok()?;
    if !output.success {
        return None;
    }
    let ops = [
        GitOperation::Rebase,
        GitOperation::Rebase,
        GitOperation::Rebase,
        GitOperation::Merge,
        GitOperation::CherryPick,
        GitOperation::Revert,
        GitOperation::Bisect,
    ];
    for (line, op) in output.stdout.lines().zip(ops) {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = if Path::new(trimmed).is_absolute() {
            PathBuf::from(trimmed)
        } else {
            root.join(trimmed)
        };
        if path.exists() {
            return Some(op);
        }
    }
    None
}

/// 查询除当前工作树外、其他 worktree 正在占用的本地分支：`branch -> worktree_path`。
async fn other_worktree_branches(root: &Path) -> HashMap<String, String> {
    let Ok(output) = run_git(root, &["worktree", "list", "--porcelain"], GIT_READ_TIMEOUT).await
    else {
        return HashMap::new();
    };
    if !output.success {
        return HashMap::new();
    }
    let mut map = HashMap::new();
    let mut current_wt: Option<PathBuf> = None;
    let mut current_branch: Option<String> = None;

    let flush = |wt: &mut Option<PathBuf>, br: &mut Option<String>, out: &mut HashMap<String, String>| {
        if let (Some(wt_path), Some(branch)) = (wt.take(), br.take()) {
            let canonical = std::fs::canonicalize(&wt_path).unwrap_or_else(|_| wt_path.clone());
            if canonical != root {
                out.insert(branch, wt_path.to_string_lossy().into_owned());
            }
        }
    };

    for line in output.stdout.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            flush(&mut current_wt, &mut current_branch, &mut map);
            continue;
        }
        if let Some(path) = trimmed.strip_prefix("worktree ") {
            flush(&mut current_wt, &mut current_branch, &mut map);
            current_wt = Some(PathBuf::from(path));
        } else if let Some(branch) = trimmed.strip_prefix("branch refs/heads/") {
            current_branch = Some(branch.to_owned());
        }
    }
    flush(&mut current_wt, &mut current_branch, &mut map);
    map
}

/// 列出本地分支与远程分支。
async fn list_branches(
    root: &Path,
    other_worktrees: &HashMap<String, String>,
) -> (Vec<GitBranchEntry>, Vec<GitRemoteBranchEntry>) {
    let (local_out, remote_out, remotes_out) = tokio::join!(
        run_git(
            root,
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)\t%(HEAD)\t%(upstream:short)",
                "refs/heads",
            ],
            GIT_READ_TIMEOUT,
        ),
        run_git(
            root,
            &[
                "for-each-ref",
                "--sort=-committerdate",
                "--format=%(refname:short)\t%(symref)",
                "refs/remotes",
            ],
            GIT_READ_TIMEOUT,
        ),
        run_git(root, &["remote"], GIT_READ_TIMEOUT),
    );

    let mut local = Vec::new();
    let mut local_names = HashSet::new();
    if let Ok(output) = local_out
        && output.success
    {
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let Some(name) = parts.next().filter(|value| !value.is_empty()) else {
                continue;
            };
            let current = parts.next().is_some_and(|mark| mark == "*");
            let upstream = parts
                .next()
                .filter(|value| !value.is_empty())
                .map(str::to_owned);
            let worktree_path = if current {
                None
            } else {
                other_worktrees.get(name).cloned()
            };
            local_names.insert(name.to_owned());
            local.push(GitBranchEntry {
                name: name.to_owned(),
                current,
                worktree_path,
                upstream,
            });
        }
    }
    // 当前分支始终排在最前面，其余维持按最近提交时间降序。
    if let Some(pos) = local.iter().position(|entry| entry.current)
        && pos > 0
    {
        let current_entry = local.remove(pos);
        local.insert(0, current_entry);
    }

    let mut remote_names: Vec<String> = remotes_out
        .ok()
        .filter(|output| output.success)
        .map(|output| {
            output
                .stdout
                .lines()
                .map(str::trim)
                .filter(|line| !line.is_empty())
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    // 远程名按长度降序匹配，避免某个远程名是另一个的前缀时切错。
    remote_names.sort_by(|left, right| right.len().cmp(&left.len()));

    let mut remote = Vec::new();
    if let Ok(output) = remote_out
        && output.success
    {
        for line in output.stdout.lines() {
            let mut parts = line.split('\t');
            let Some(name) = parts.next().filter(|value| !value.is_empty()) else {
                continue;
            };
            let symref = parts.next().unwrap_or("");
            if !symref.is_empty() || name.ends_with("/HEAD") {
                continue;
            }
            let (remote_name, branch) = split_remote_branch(name, &remote_names);
            if branch.is_empty() {
                continue;
            }
            remote.push(GitRemoteBranchEntry {
                name: name.to_owned(),
                remote: remote_name.to_owned(),
                branch: branch.to_owned(),
                has_local: local_names.contains(branch),
            });
        }
    }
    (local, remote)
}

fn split_remote_branch<'a>(full: &'a str, remotes: &[String]) -> (&'a str, &'a str) {
    for remote in remotes {
        if let Some(rest) = full.strip_prefix(remote.as_str())
            && let Some(branch) = rest.strip_prefix('/')
        {
            return (&full[..remote.len()], branch);
        }
    }
    full.split_once('/').unwrap_or(("", full))
}

// ---------- 同仓库并发会话检查与切后同步 ----------

/// 检查同 Git 工作树根下是否有正在运行的 prompt、后台任务或活跃子代理。
async fn busy_sessions_for_repo(inner: &CoreInner, repo_root: &Path) -> Vec<String> {
    let records = inner.sessions.loaded_records();
    let runtimes = inner.runtimes.lock().await.clone();
    let live_agent_ids: HashSet<String> = {
        let registry = inner.agents.lock().await;
        registry
            .items
            .values()
            .filter(|runtime| crate::agents::live(runtime.info.status))
            .map(|runtime| runtime.info.id.clone())
            .collect()
    };

    let mut busy = Vec::new();
    for (session_id, record) in records {
        let (cwd, title) = {
            let state = record.state.lock().await;
            (state.session.cwd.clone(), state.session.title.clone())
        };
        let same_repo = find_repo_root_fast(Path::new(&cwd))
            .is_some_and(|session_root| session_root == repo_root);
        if !same_repo {
            continue;
        }
        let prompt_running = record.prompt_lock.try_lock().is_err();
        let live_agent = live_agent_ids.contains(&session_id);
        let runtime_busy = runtimes.get(&session_id).is_some_and(|runtime| {
            runtime.agent_finishing.load(Ordering::Acquire)
                || runtime
                    .jobs
                    .list()
                    .iter()
                    .any(|job| !job.status.is_terminal())
        });
        if prompt_running || live_agent || runtime_busy {
            let label = if title.trim().is_empty() {
                session_id
            } else {
                title
            };
            busy.push(label);
        }
    }
    busy
}

/// 分支切换成功后，同步同工作树下所有已加载会话：
/// 失效文件锚点与 `AGENTS.md` 缓存，并为已有对话历史的会话写入一条去重的一次性模型通知。
async fn sync_sessions_after_switch(
    inner: &CoreInner,
    repo_root: &Path,
    previous_branch: Option<&str>,
    new_branch: &str,
    stashed: bool,
) {
    let records = inner.sessions.loaded_records();
    let runtimes = inner.runtimes.lock().await.clone();
    let from = previous_branch.unwrap_or("detached HEAD");
    // 模型可见文本按仓库规范使用简洁英语。
    let notice_text = if stashed {
        format!(
            "{BRANCH_NOTICE_PREFIX}from `{from}` to `{new_branch}` (previous working tree changes were stashed). Re-read workspace files before editing."
        )
    } else {
        format!(
            "{BRANCH_NOTICE_PREFIX}from `{from}` to `{new_branch}`. Re-read workspace files before editing."
        )
    };

    for (session_id, record) in records {
        let same_repo = {
            let cwd = record.state.lock().await.session.cwd.clone();
            find_repo_root_fast(Path::new(&cwd))
                .is_some_and(|session_root| session_root == repo_root)
        };
        if !same_repo {
            continue;
        }
        if let Some(runtime) = runtimes.get(&session_id) {
            runtime.anchors.invalidate_all();
            *runtime.context_files.lock().await = None;
        }
        let mut state = record.state.lock().await;
        if !state.transcript.is_empty() {
            state
                .notices
                .retain(|notice| !notice.text.starts_with(BRANCH_NOTICE_PREFIX));
            state.notices.push(SessionNotice::new(notice_text.clone()));
        }
    }
}

// ---------- RPC 实现：workspace/git_branches & workspace/git_switch ----------

pub(super) async fn git_branches(inner: &CoreInner, cwd: &str) -> WorkspaceGitBranchesResult {
    let Some(root) = repo_toplevel(Path::new(cwd)).await else {
        return WorkspaceGitBranchesResult::default();
    };
    let (status_out, in_progress, other_worktrees, busy_sessions) = tokio::join!(
        run_git(
            &root,
            &["-c", "core.quotePath=false", "status", "--porcelain=v2", "--branch"],
            GIT_READ_TIMEOUT,
        ),
        detect_in_progress(&root),
        other_worktree_branches(&root),
        busy_sessions_for_repo(inner, &root),
    );
    let parsed = status_out
        .ok()
        .filter(|output| output.success)
        .map(|output| parse_status_v2(&output.stdout))
        .unwrap_or_default();
    let (local, remote) = list_branches(&root, &other_worktrees).await;

    WorkspaceGitBranchesResult {
        in_repo: true,
        root: Some(root.to_string_lossy().into_owned()),
        current: parsed.current,
        head_oid: parsed.head_oid,
        detached: parsed.detached,
        unborn: parsed.unborn,
        status: parsed.status,
        in_progress,
        busy_sessions,
        local,
        remote,
    }
}

fn operation_label(op: GitOperation) -> String {
    match op {
        GitOperation::Merge => t!("core.git.op_merge").into_owned(),
        GitOperation::Rebase => t!("core.git.op_rebase").into_owned(),
        GitOperation::CherryPick => t!("core.git.op_cherry_pick").into_owned(),
        GitOperation::Revert => t!("core.git.op_revert").into_owned(),
        GitOperation::Bisect => t!("core.git.op_bisect").into_owned(),
    }
}

fn is_lock_error(stderr: &str) -> bool {
    stderr.contains(".lock")
        && (stderr.contains("Unable to create")
            || stderr.contains("File exists")
            || stderr.contains("Another git process"))
}

fn is_overwrite_conflict(stderr: &str) -> bool {
    stderr.contains("would be overwritten by checkout")
        || stderr.contains("would be overwritten by merge")
        || stderr.contains("untracked working tree files would be overwritten")
}

/// 提取 `LC_ALL=C` 下 Git 覆盖冲突输出里 `\t<path>` 缩进行。
fn parse_overwrite_files(stderr: &str) -> Vec<String> {
    let mut files = Vec::new();
    for line in stderr.lines() {
        if let Some(rest) = line.strip_prefix('\t') {
            let trimmed = rest.trim();
            if !trimmed.is_empty() && files.len() < MAX_DIRTY_PREVIEW {
                files.push(trimmed.to_owned());
            }
        }
    }
    files
}

fn first_error_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("git exited with non-zero status")
        .to_owned()
}

fn blocked(reason: GitSwitchBlockReason, message: String, details: Vec<String>) -> WorkspaceGitSwitchResult {
    WorkspaceGitSwitchResult::Blocked {
        reason,
        message,
        details,
    }
}

async fn ref_exists(root: &Path, full_ref: &str) -> bool {
    run_git(
        root,
        &["show-ref", "--verify", "--quiet", full_ref],
        GIT_READ_TIMEOUT,
    )
    .await
    .is_ok_and(|output| output.success)
}

async fn rev_oid(root: &Path, rev: &str) -> Option<String> {
    let output = run_git(root, &["rev-parse", "--verify", "-q", rev], GIT_READ_TIMEOUT)
        .await
        .ok()?;
    if !output.success {
        return None;
    }
    let oid = output.stdout.trim();
    (!oid.is_empty()).then(|| oid.to_owned())
}

pub(super) async fn git_switch(
    inner: &Arc<CoreInner>,
    params: WorkspaceGitSwitchParams,
) -> WorkspaceGitSwitchResult {
    let _switch_guard = inner.git_switch_lock.lock().await;

    let Some(root) = repo_toplevel(Path::new(&params.cwd)).await else {
        return blocked(
            GitSwitchBlockReason::NotARepo,
            t!("core.git.not_a_repo").into_owned(),
            Vec::new(),
        );
    };

    let (status_out, in_progress, other_worktrees, busy_sessions) = tokio::join!(
        run_git(
            &root,
            &["-c", "core.quotePath=false", "status", "--porcelain=v2", "--branch"],
            GIT_READ_TIMEOUT,
        ),
        detect_in_progress(&root),
        other_worktree_branches(&root),
        busy_sessions_for_repo(inner, &root),
    );

    if !busy_sessions.is_empty() {
        return blocked(
            GitSwitchBlockReason::BusySessions,
            t!("core.git.busy_sessions").into_owned(),
            busy_sessions,
        );
    }
    if let Some(op) = in_progress {
        return blocked(
            GitSwitchBlockReason::OperationInProgress,
            t!("core.git.operation_in_progress", op = operation_label(op)).into_owned(),
            Vec::new(),
        );
    }

    let parsed = match status_out {
        Ok(output) if output.success => parse_status_v2(&output.stdout),
        Ok(output) => {
            let reason = if is_lock_error(&output.stderr) {
                GitSwitchBlockReason::LockFile
            } else {
                GitSwitchBlockReason::GitFailed
            };
            let message = if reason == GitSwitchBlockReason::LockFile {
                t!("core.git.lock_file").into_owned()
            } else {
                t!("core.git.switch_failed", error = first_error_line(&output.stderr)).into_owned()
            };
            return blocked(reason, message, Vec::new());
        }
        Err(GitExecError::Timeout) => {
            return blocked(
                GitSwitchBlockReason::GitFailed,
                t!("core.git.timeout").into_owned(),
                Vec::new(),
            );
        }
        Err(GitExecError::Spawn(error)) => {
            return blocked(
                GitSwitchBlockReason::GitFailed,
                t!("core.git.switch_failed", error = error).into_owned(),
                Vec::new(),
            );
        }
    };

    if parsed.status.conflicted > 0 {
        return blocked(
            GitSwitchBlockReason::UnresolvedConflicts,
            t!("core.git.unresolved_conflicts", n = parsed.status.conflicted).into_owned(),
            parsed.conflicted_files,
        );
    }

    let target = params.branch.trim();
    if params.create {
        return create_and_switch(inner, &root, target, parsed).await;
    }

    if parsed.unborn {
        return blocked(
            GitSwitchBlockReason::UnbornRepo,
            t!("core.git.unborn_repo").into_owned(),
            Vec::new(),
        );
    }

    // 解析目标本地分支名及是否需要首次跟踪远程分支。
    let (local_name, track_remote_ref): (String, Option<String>) = if params.remote {
        let full_remote = format!("refs/remotes/{target}");
        if target.is_empty() || target.starts_with('-') || !ref_exists(&root, &full_remote).await {
            return blocked(
                GitSwitchBlockReason::BranchNotFound,
                t!("core.git.branch_not_found", branch = target).into_owned(),
                Vec::new(),
            );
        }
        let mut remotes: Vec<String> = run_git(&root, &["remote"], GIT_READ_TIMEOUT)
            .await
            .ok()
            .filter(|output| output.success)
            .map(|output| {
                output
                    .stdout
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        remotes.sort_by(|left, right| right.len().cmp(&left.len()));
        let (_, branch_name) = split_remote_branch(target, &remotes);
        if branch_name.is_empty() || branch_name.starts_with('-') {
            return blocked(
                GitSwitchBlockReason::InvalidBranchName,
                t!("core.git.invalid_branch_name", branch = target).into_owned(),
                Vec::new(),
            );
        }
        let local_ref = format!("refs/heads/{branch_name}");
        if ref_exists(&root, &local_ref).await {
            (branch_name.to_owned(), None)
        } else {
            (branch_name.to_owned(), Some(full_remote))
        }
    } else {
        let local_ref = format!("refs/heads/{target}");
        if target.is_empty() || target.starts_with('-') || !ref_exists(&root, &local_ref).await {
            return blocked(
                GitSwitchBlockReason::BranchNotFound,
                t!("core.git.branch_not_found", branch = target).into_owned(),
                Vec::new(),
            );
        }
        (target.to_owned(), None)
    };

    // 已在目标分支：幂等成功。
    if !parsed.detached && parsed.current.as_deref() == Some(local_name.as_str()) {
        return WorkspaceGitSwitchResult::Switched {
            branch: local_name,
            previous_branch: parsed.current,
            stash_message: None,
            created: false,
        };
    }

    if let Some(wt_path) = other_worktrees.get(&local_name) {
        return blocked(
            GitSwitchBlockReason::WorktreeInUse,
            t!(
                "core.git.worktree_in_use",
                branch = local_name,
                path = wt_path
            )
            .into_owned(),
            vec![wt_path.clone()],
        );
    }

    let has_dirty = parsed.status.has_changes();
    if params.dirty == GitSwitchDirtyPolicy::Check && (has_dirty || parsed.detached) {
        return WorkspaceGitSwitchResult::NeedsConfirmation {
            status: parsed.status,
            dirty_files: parsed.dirty_files,
            detached: parsed.detached,
            overwrite_conflict: false,
            conflict_files: Vec::new(),
        };
    }

    let previous_branch = parsed.current.clone();
    let created = track_remote_ref.is_some();

    // 策略 1：先暂存（含未跟踪文件，不含 ignored），切换失败时自动恢复。
    if params.dirty == GitSwitchDirtyPolicy::Stash && has_dirty {
        let pre_head = rev_oid(&root, "HEAD").await;
        let pre_stash = rev_oid(&root, "refs/stash").await;
        let prev_label = previous_branch
            .as_deref()
            .or(parsed.head_oid.as_deref())
            .unwrap_or("HEAD");
        let stash_msg = format!("moc: auto-stash from {prev_label} before switching to {local_name}");
        match run_git(
            &root,
            &["stash", "push", "--include-untracked", "-m", &stash_msg],
            GIT_WRITE_TIMEOUT,
        )
        .await
        {
            Ok(output) if output.success => {}
            Ok(output) => {
                let reason = if is_lock_error(&output.stderr) {
                    GitSwitchBlockReason::LockFile
                } else {
                    GitSwitchBlockReason::GitFailed
                };
                let message = if reason == GitSwitchBlockReason::LockFile {
                    t!("core.git.lock_file").into_owned()
                } else {
                    t!("core.git.stash_failed", error = first_error_line(&output.stderr))
                        .into_owned()
                };
                return blocked(reason, message, Vec::new());
            }
            Err(GitExecError::Timeout) => {
                return blocked(
                    GitSwitchBlockReason::GitFailed,
                    t!("core.git.timeout").into_owned(),
                    Vec::new(),
                );
            }
            Err(GitExecError::Spawn(error)) => {
                return blocked(
                    GitSwitchBlockReason::GitFailed,
                    t!("core.git.stash_failed", error = error).into_owned(),
                    Vec::new(),
                );
            }
        }
        let post_stash = rev_oid(&root, "refs/stash").await;
        let created_stash = post_stash.is_some() && post_stash != pre_stash;

        let switch_res = run_switch_cmd(&root, &local_name, track_remote_ref.as_deref()).await;
        match switch_res {
            Ok(output) if output.success => {
                sync_sessions_after_switch(
                    inner,
                    &root,
                    previous_branch.as_deref(),
                    &local_name,
                    created_stash,
                )
                .await;
                return WorkspaceGitSwitchResult::Switched {
                    branch: local_name,
                    previous_branch,
                    stash_message: created_stash.then_some(stash_msg),
                    created,
                };
            }
            other => {
                let (reason, err_text) = match other {
                    Ok(output) => (
                        if is_lock_error(&output.stderr) {
                            GitSwitchBlockReason::LockFile
                        } else {
                            GitSwitchBlockReason::GitFailed
                        },
                        first_error_line(&output.stderr),
                    ),
                    Err(GitExecError::Timeout) => (
                        GitSwitchBlockReason::GitFailed,
                        t!("core.git.timeout").into_owned(),
                    ),
                    Err(GitExecError::Spawn(error)) => (GitSwitchBlockReason::GitFailed, error),
                };
                if created_stash && rev_oid(&root, "HEAD").await == pre_head {
                    let restored = rollback_stash(&root).await;
                    let message = if restored {
                        t!("core.git.switch_failed_restored", error = err_text).into_owned()
                    } else {
                        t!(
                            "core.git.switch_failed_stashed",
                            error = err_text,
                            stash = stash_msg
                        )
                        .into_owned()
                    };
                    return blocked(reason, message, Vec::new());
                }
                return blocked(
                    reason,
                    t!("core.git.switch_failed", error = err_text).into_owned(),
                    Vec::new(),
                );
            }
        }
    }

    // 策略 2：干净工作区或用户显式选择 Carry（安全模式，不传 -f/-m）。
    match run_switch_cmd(&root, &local_name, track_remote_ref.as_deref()).await {
        Ok(output) if output.success => {
            sync_sessions_after_switch(
                inner,
                &root,
                previous_branch.as_deref(),
                &local_name,
                false,
            )
            .await;
            WorkspaceGitSwitchResult::Switched {
                branch: local_name,
                previous_branch,
                stash_message: None,
                created,
            }
        }
        Ok(output) => {
            if is_overwrite_conflict(&output.stderr) {
                let conflict_files = parse_overwrite_files(&output.stderr);
                return WorkspaceGitSwitchResult::NeedsConfirmation {
                    status: parsed.status,
                    dirty_files: parsed.dirty_files,
                    detached: parsed.detached,
                    overwrite_conflict: true,
                    conflict_files,
                };
            }
            let reason = if is_lock_error(&output.stderr) {
                GitSwitchBlockReason::LockFile
            } else {
                GitSwitchBlockReason::GitFailed
            };
            let message = if reason == GitSwitchBlockReason::LockFile {
                t!("core.git.lock_file").into_owned()
            } else {
                t!(
                    "core.git.switch_failed",
                    error = first_error_line(&output.stderr)
                )
                .into_owned()
            };
            blocked(reason, message, Vec::new())
        }
        Err(GitExecError::Timeout) => blocked(
            GitSwitchBlockReason::GitFailed,
            t!("core.git.timeout").into_owned(),
            Vec::new(),
        ),
        Err(GitExecError::Spawn(error)) => blocked(
            GitSwitchBlockReason::GitFailed,
            t!("core.git.switch_failed", error = error).into_owned(),
            Vec::new(),
        ),
    }
}

async fn create_and_switch(
    inner: &Arc<CoreInner>,
    root: &Path,
    name: &str,
    parsed: ParsedStatus,
) -> WorkspaceGitSwitchResult {
    if name.is_empty() || name.starts_with('-') {
        return blocked(
            GitSwitchBlockReason::InvalidBranchName,
            t!("core.git.invalid_branch_name", branch = name).into_owned(),
            Vec::new(),
        );
    }
    let valid = run_git(
        root,
        &["check-ref-format", "--branch", name],
        GIT_READ_TIMEOUT,
    )
    .await
    .is_ok_and(|output| output.success);
    if !valid {
        return blocked(
            GitSwitchBlockReason::InvalidBranchName,
            t!("core.git.invalid_branch_name", branch = name).into_owned(),
            Vec::new(),
        );
    }
    let full_ref = format!("refs/heads/{name}");
    if ref_exists(root, &full_ref).await {
        return blocked(
            GitSwitchBlockReason::BranchExists,
            t!("core.git.branch_exists", branch = name).into_owned(),
            Vec::new(),
        );
    }
    match run_git(root, &["switch", "-c", name], GIT_WRITE_TIMEOUT).await {
        Ok(output) if output.success => {
            sync_sessions_after_switch(inner, root, parsed.current.as_deref(), name, false).await;
            WorkspaceGitSwitchResult::Switched {
                branch: name.to_owned(),
                previous_branch: parsed.current,
                stash_message: None,
                created: true,
            }
        }
        Ok(output) => {
            let reason = if is_lock_error(&output.stderr) {
                GitSwitchBlockReason::LockFile
            } else {
                GitSwitchBlockReason::GitFailed
            };
            let message = if reason == GitSwitchBlockReason::LockFile {
                t!("core.git.lock_file").into_owned()
            } else {
                t!(
                    "core.git.switch_failed",
                    error = first_error_line(&output.stderr)
                )
                .into_owned()
            };
            blocked(reason, message, Vec::new())
        }
        Err(GitExecError::Timeout) => blocked(
            GitSwitchBlockReason::GitFailed,
            t!("core.git.timeout").into_owned(),
            Vec::new(),
        ),
        Err(GitExecError::Spawn(error)) => blocked(
            GitSwitchBlockReason::GitFailed,
            t!("core.git.switch_failed", error = error).into_owned(),
            Vec::new(),
        ),
    }
}

async fn run_switch_cmd(
    root: &Path,
    local_name: &str,
    track_remote_ref: Option<&str>,
) -> Result<GitOutput, GitExecError> {
    if let Some(remote_ref) = track_remote_ref {
        run_git(
            root,
            &["switch", "--track", "-c", local_name, remote_ref],
            GIT_WRITE_TIMEOUT,
        )
        .await
    } else {
        run_git(root, &["switch", local_name], GIT_WRITE_TIMEOUT).await
    }
}

async fn rollback_stash(root: &Path) -> bool {
    if run_git(root, &["stash", "pop", "--index"], GIT_WRITE_TIMEOUT)
        .await
        .is_ok_and(|output| output.success)
    {
        return true;
    }
    run_git(root, &["stash", "pop"], GIT_WRITE_TIMEOUT)
        .await
        .is_ok_and(|output| output.success)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Core;
    use crate::CoreConfig;
    use crate::session::entries::{EntryPayload, SessionOrigin};
    use moc_llm::Message;
    use moc_protocol::methods::{SessionEngine, SessionPermission};
    use std::process::Command;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "moc-test")
            .env("GIT_AUTHOR_EMAIL", "test@moc.local")
            .env("GIT_COMMITTER_NAME", "moc-test")
            .env("GIT_COMMITTER_EMAIL", "test@moc.local")
            .env("LC_ALL", "C")
            .output()
            .expect("git 可执行");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_owned()
    }

    fn init_repo(dir: &Path) {
        git(dir, &["init", "-b", "master"]);
        std::fs::write(dir.join("a.txt"), "base\n").unwrap();
        git(dir, &["add", "a.txt"]);
        git(dir, &["commit", "-m", "initial"]);
    }

    fn test_core(dir: &Path) -> Arc<Core> {
        let config = CoreConfig {
            home: dir.join(".moc-home"),
            cwd: dir.to_path_buf(),
            user_home: Some(dir.join(".user-home")),
            session_dir: Some(dir.join(".moc-sessions")),
            ..CoreConfig::default()
        };
        Core::new(config).expect("core 创建成功")
    }

    #[tokio::test]
    async fn fast_probe_handles_normal_subdir_detached_unborn_worktree_and_non_repo() {
        let tmp = TempDir::new().unwrap();
        let non_repo = tmp.path().join("plain");
        std::fs::create_dir_all(&non_repo).unwrap();
        let none = git_branch(non_repo.to_str().unwrap()).await;
        assert!(none.branch.is_none() && none.root.is_none() && !none.detached);

        // Unborn repo
        let unborn_dir = tmp.path().join("unborn");
        std::fs::create_dir_all(&unborn_dir).unwrap();
        git(&unborn_dir, &["init", "-b", "main"]);
        let unborn = git_branch(unborn_dir.to_str().unwrap()).await;
        assert_eq!(unborn.branch.as_deref(), Some("main"));
        assert!(unborn.unborn);
        assert!(unborn.head_oid.is_none());

        // Normal repo + subdir
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        let sub = repo.join("nested/dir");
        std::fs::create_dir_all(&sub).unwrap();
        let probed = git_branch(sub.to_str().unwrap()).await;
        assert_eq!(probed.branch.as_deref(), Some("master"));
        assert!(!probed.detached && !probed.unborn);
        assert!(probed.head_oid.is_some());
        assert_eq!(
            probed.root.as_deref(),
            Some(std::fs::canonicalize(&repo).unwrap().to_str().unwrap())
        );

        // Linked worktree
        let wt = tmp.path().join("repo-wt");
        git(&repo, &["worktree", "add", "-b", "wt-branch", wt.to_str().unwrap()]);
        let wt_probed = git_branch(wt.to_str().unwrap()).await;
        assert_eq!(wt_probed.branch.as_deref(), Some("wt-branch"));
        assert!(!wt_probed.unborn && wt_probed.head_oid.is_some());

        // Detached HEAD
        let oid = git(&repo, &["rev-parse", "HEAD"]);
        git(&repo, &["checkout", "--detach", &oid]);
        let detached = git_branch(repo.to_str().unwrap()).await;
        assert!(detached.branch.is_none());
        assert!(detached.detached);
        assert_eq!(detached.head_oid.as_deref(), Some(&oid[..7]));
    }

    #[tokio::test]
    async fn switch_guards_dirty_conflict_stash_create_remote_worktree_and_busy_session() {
        let tmp = TempDir::new().unwrap();
        let origin = tmp.path().join("origin.git");
        std::fs::create_dir_all(&origin).unwrap();
        git(&origin, &["init", "--bare", "-b", "master"]);

        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&repo, &["push", "-u", "origin", "master"]);

        // 构造远程独有分支 origin/remote-only
        git(&repo, &["switch", "-c", "remote-only"]);
        std::fs::write(repo.join("remote.txt"), "from remote\n").unwrap();
        git(&repo, &["add", "remote.txt"]);
        git(&repo, &["commit", "-m", "remote commit"]);
        git(&repo, &["push", "-u", "origin", "remote-only"]);
        git(&repo, &["switch", "master"]);
        git(&repo, &["branch", "-D", "remote-only"]);

        // 构造另一个与 master 改同一文件的本地分支 conflict-br
        git(&repo, &["switch", "-c", "conflict-br"]);
        std::fs::write(repo.join("a.txt"), "branch version\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "modify a.txt"]);
        git(&repo, &["switch", "master"]);

        // 构造 worktree 占用分支 wt-busy
        let wt = tmp.path().join("wt-busy");
        git(&repo, &["worktree", "add", "-b", "wt-busy", wt.to_str().unwrap()]);

        let core_dir = tmp.path().join("core-home");
        std::fs::create_dir_all(&core_dir).unwrap();
        let core = test_core(&core_dir);
        let cwd = repo.to_string_lossy().into_owned();

        // 1. 列出分支：包含 worktree 占用与远程分支
        let listed = git_branches(&core.inner, &cwd).await;
        assert!(listed.in_repo);
        assert_eq!(listed.current.as_deref(), Some("master"));
        assert!(
            listed
                .local
                .iter()
                .any(|entry| entry.name == "wt-busy" && entry.worktree_path.is_some())
        );
        assert!(
            listed
                .remote
                .iter()
                .any(|entry| entry.name == "origin/remote-only" && !entry.has_local)
        );

        // 2. 切到被其他 worktree 占用的分支 -> 硬阻断
        let wt_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "wt-busy".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(
            wt_res,
            WorkspaceGitSwitchResult::Blocked {
                reason: GitSwitchBlockReason::WorktreeInUse,
                ..
            }
        ));

        // 3. 脏工作区（已改 a.txt + 未跟踪 untracked.txt）：
        //    - Check 返回 NeedsConfirmation
        //    - Carry 遇覆盖冲突返回 overwrite_conflict=true 并列出 a.txt
        //    - Stash 暂存（含未跟踪文件）并成功切换
        std::fs::write(repo.join("a.txt"), "local dirty\n").unwrap();
        std::fs::write(repo.join("untracked.txt"), "wip\n").unwrap();

        let check_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "conflict-br".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(
            check_res,
            WorkspaceGitSwitchResult::NeedsConfirmation {
                overwrite_conflict: false,
                ..
            }
        ));

        let carry_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "conflict-br".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Carry,
            },
        )
        .await;
        match carry_res {
            WorkspaceGitSwitchResult::NeedsConfirmation {
                overwrite_conflict,
                conflict_files,
                ..
            } => {
                assert!(overwrite_conflict);
                assert!(conflict_files.iter().any(|path| path == "a.txt"));
            }
            other => panic!("期望覆盖冲突确认，实际得到 {other:?}"),
        }

        // 在脏工作区下新建分支应直接放行并保留本地改动
        let create_dirty = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "feat/keep-wip".into(),
                create: true,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(
            create_dirty,
            WorkspaceGitSwitchResult::Switched {
                ref branch,
                created: true,
                stash_message: None,
                ..
            } if branch == "feat/keep-wip"
        ));
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "local dirty\n");
        assert!(repo.join("untracked.txt").exists());

        // Stash & switch 到 conflict-br：应自动暂存并清空未跟踪文件
        let stash_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "conflict-br".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Stash,
            },
        )
        .await;
        assert!(matches!(
            stash_res,
            WorkspaceGitSwitchResult::Switched {
                ref branch,
                stash_message: Some(_),
                ..
            } if branch == "conflict-br"
        ));
        assert!(!repo.join("untracked.txt").exists());
        assert_eq!(std::fs::read_to_string(repo.join("a.txt")).unwrap(), "branch version\n");

        // 4. 切换到远程分支 origin/remote-only：自动创建本地跟踪分支
        let remote_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "origin/remote-only".into(),
                create: false,
                remote: true,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(
            remote_res,
            WorkspaceGitSwitchResult::Switched {
                ref branch,
                created: true,
                ..
            } if branch == "remote-only"
        ));
        assert!(repo.join("remote.txt").exists());

        // 5. 同仓库会话运行中 -> 硬阻断；释放锁切换后 -> 失效锚点并写入模型通知
        let info = core
            .inner
            .sessions
            .create(
                &repo,
                "test/model",
                SessionEngine::Standard,
                SessionPermission::Full,
                Some("Active Task"),
                SessionOrigin::default(),
                false,
            )
            .await
            .unwrap();
        core.inner
            .sessions
            .append(
                &info.id,
                EntryPayload::Message {
                    message: Message::user_text("hello"),
                    tool: None,
                    finish_reason: None,
                    agent: None,
                    goal: None,
                },
            )
            .await
            .unwrap();
        let record = core.inner.sessions.get(&info.id).await.unwrap();
        let prompt_guard = record.try_prompt_lock().unwrap();

        let busy_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "master".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(
            busy_res,
            WorkspaceGitSwitchResult::Blocked {
                reason: GitSwitchBlockReason::BusySessions,
                ref details,
                ..
            } if details.iter().any(|name| name == "Active Task")
        ));

        drop(prompt_guard);
        let ok_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "master".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(ok_res, WorkspaceGitSwitchResult::Switched { .. }));
        let state = record.state.lock().await;
        assert!(
            state
                .notices
                .iter()
                .any(|notice| notice.text.contains("from `remote-only` to `master`"))
        );
    }

    #[tokio::test]
    async fn blocks_merge_conflict_and_invalid_branch_name() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        init_repo(&repo);
        git(&repo, &["switch", "-c", "side"]);
        std::fs::write(repo.join("a.txt"), "side\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "side"]);
        git(&repo, &["switch", "master"]);
        std::fs::write(repo.join("a.txt"), "master\n").unwrap();
        git(&repo, &["add", "a.txt"]);
        git(&repo, &["commit", "-m", "master"]);

        // 触发真实 merge 冲突
        let merge = Command::new("git")
            .args(["merge", "side"])
            .current_dir(&repo)
            .output()
            .unwrap();
        assert!(!merge.status.success());

        let core_dir = tmp.path().join("core-home");
        std::fs::create_dir_all(&core_dir).unwrap();
        let core = test_core(&core_dir);
        let cwd = repo.to_string_lossy().into_owned();

        let listed = git_branches(&core.inner, &cwd).await;
        assert_eq!(listed.in_progress, Some(GitOperation::Merge));
        assert_eq!(listed.status.conflicted, 1);

        let blocked_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd: cwd.clone(),
                branch: "side".into(),
                create: false,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Stash,
            },
        )
        .await;
        assert!(matches!(
            blocked_res,
            WorkspaceGitSwitchResult::Blocked {
                reason: GitSwitchBlockReason::OperationInProgress,
                ..
            }
        ));

        git(&repo, &["merge", "--abort"]);
        let invalid_res = git_switch(
            &core.inner,
            WorkspaceGitSwitchParams {
                cwd,
                branch: "bad..name".into(),
                create: true,
                remote: false,
                dirty: GitSwitchDirtyPolicy::Check,
            },
        )
        .await;
        assert!(matches!(
            invalid_res,
            WorkspaceGitSwitchResult::Blocked {
                reason: GitSwitchBlockReason::InvalidBranchName,
                ..
            }
        ));
    }
}

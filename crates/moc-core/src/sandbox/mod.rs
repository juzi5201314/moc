//! 会话沙盒的模式、路径策略、会话授权与跨平台 runner 选择。
//!
//! 这里只做“允许/询问/拒绝”的决策与命令包装；具体的内核 profile 在
//! `linux` / `macos` 子模块里生成。内置文件工具与 bash 共用同一份策略，
//! 避免两边边界漂移。

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use moc_protocol::SessionPermission;
use tempfile::TempDir;

use crate::config::{OnDenied, ResolvedSandboxConfig};
use crate::shell::ShellChoice;
use crate::tools::ToolError;

mod paths;
mod scratch;

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;

pub(crate) use paths::{is_within, resolve_access_path, resolve_landing};
pub(crate) use scratch::SandboxScratch;

/// 实际执行约束的平台后端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxRunner {
    Bubblewrap,
    Seatbelt,
}

impl SandboxRunner {
    pub const fn name(self) -> &'static str {
        match self {
            Self::Bubblewrap => "bubblewrap",
            Self::Seatbelt => "sandbox-exec",
        }
    }
}

/// 当前平台的可用能力；由 `Core::new` 探测一次后缓存。
#[derive(Debug, Clone)]
pub struct SandboxSupport {
    pub runner: Option<SandboxRunner>,
    /// 始终包含 [`SessionPermission::Full`]。
    pub modes: Vec<SessionPermission>,
    /// Linux 下 bwrap 是否支持 `--overlay` 且探测通过。
    pub home_overlay: bool,
}

impl SandboxSupport {
    pub fn full_only() -> Self {
        Self {
            runner: None,
            modes: vec![SessionPermission::Full],
            home_overlay: false,
        }
    }

    pub fn supports(&self, mode: SessionPermission) -> bool {
        self.modes.contains(&mode)
    }
}

/// 进程内只探测一次；测试/嵌入方可用 `Core::new_with_sandbox_support` 覆盖。
pub fn detect() -> SandboxSupport {
    static SUPPORT: OnceLock<SandboxSupport> = OnceLock::new();
    SUPPORT.get_or_init(detect_uncached).clone()
}

fn detect_uncached() -> SandboxSupport {
    #[cfg(target_os = "linux")]
    {
        linux::support()
    }
    #[cfg(target_os = "macos")]
    {
        macos::support()
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        SandboxSupport::full_only()
    }
}

/// 一次访问的语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    Read,
    Write,
}

/// 策略判定；`Ask` 由调用方按 `on_denied` 转成权限提示或拒绝。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Decision {
    Allow,
    Ask,
    Deny,
}

/// 会话内累积的授权；只存在内存里，随模式切换或会话卸载消失。
#[derive(Debug, Clone, Default)]
pub(crate) struct SandboxGrants {
    pub read_paths: Vec<PathBuf>,
    pub write_paths: Vec<PathBuf>,
    pub commands: Vec<CommandGrant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommandGrant {
    /// 完整命令或命令前缀。
    pub text: String,
    pub prefix: bool,
    pub mode: SessionPermission,
}

impl SandboxGrants {
    pub fn clear(&mut self) {
        self.read_paths.clear();
        self.write_paths.clear();
        self.commands.clear();
    }

    /// 记录一条路径授权；写入按真实落点保存。
    pub fn grant_path(&mut self, path: &Path, access: Access) {
        let Some(path) = resolve_access_path(path) else {
            return;
        };
        let paths = match access {
            Access::Read => &mut self.read_paths,
            Access::Write => &mut self.write_paths,
        };
        if !paths.contains(&path) {
            paths.push(path);
        }
    }

    /// 记录一条命令授权；同一规则只保留更宽的模式。
    pub fn grant_command(&mut self, text: &str, prefix: bool, mode: SessionPermission) {
        let text = text.trim_start();
        if let Some(existing) = self
            .commands
            .iter_mut()
            .find(|grant| grant.text == text && grant.prefix == prefix)
        {
            if mode.width() > existing.mode.width() {
                existing.mode = mode;
            }
            return;
        }
        self.commands.push(CommandGrant {
            text: text.to_owned(),
            prefix,
            mode,
        });
    }

    /// 返回覆盖该命令的最宽会话授权。
    pub fn mode_for_command(&self, command: &str) -> Option<SessionPermission> {
        let command = command.trim_start();
        self.commands
            .iter()
            .filter(|grant| {
                if grant.prefix {
                    command == grant.text
                        || command
                            .strip_prefix(grant.text.as_str())
                            .is_some_and(|rest| {
                                rest.chars().next().is_some_and(char::is_whitespace)
                            })
                } else {
                    command == grant.text
                }
            })
            .map(|grant| grant.mode)
            .max_by_key(|mode| mode.width())
    }
}

/// 从一条简单命令提取可展示的命令前缀；复杂命令不提供前缀授权。
pub(crate) fn command_prefix(command: &str) -> Option<String> {
    const WRAPPERS: &[&str] = &[
        "sudo", "doas", "env", "xargs", "command", "nohup", "timeout", "watch", "sh", "bash",
        "zsh", "eval", "exec", "setsid", "stdbuf",
    ];
    let scan = crate::tools::bash::syntax::scan(command)?;
    let [segment] = scan.commands.as_slice() else {
        return None;
    };
    if segment.unsafe_syntax || segment.asynchronous {
        return None;
    }
    let first_token = command.split_whitespace().next()?;
    if first_token.contains('=') {
        return None;
    }
    let program = segment.argv.first()?;
    if program.contains('=') || program.contains('$') || program.contains('`') {
        return None;
    }
    let name = Path::new(program).file_name()?.to_str()?;
    if WRAPPERS.contains(&name) {
        return None;
    }
    Some(name.to_owned())
}

/// 一次命令/工具调用的完整策略。
#[derive(Debug, Clone)]
pub(crate) struct SandboxPolicy {
    pub mode: SessionPermission,
    pub workspace: PathBuf,
    pub network: bool,
    pub on_denied: OnDenied,
    pub overlay_home: bool,
    pub extra_read: Vec<PathBuf>,
    pub extra_write: Vec<PathBuf>,
    pub deny_read: Vec<PathBuf>,
    pub deny_write: Vec<PathBuf>,
    pub grant_read: Vec<PathBuf>,
    pub grant_write: Vec<PathBuf>,
    pub scratch: Option<Arc<SandboxScratch>>,
}

impl SandboxPolicy {
    /// 用户手动命令（`!`）的策略：输入本身即授权，固定按 full 执行。
    pub(crate) fn full(workspace: PathBuf) -> Self {
        Self {
            mode: SessionPermission::Full,
            workspace,
            network: true,
            on_denied: OnDenied::Ask,
            overlay_home: false,
            extra_read: Vec::new(),
            extra_write: Vec::new(),
            deny_read: Vec::new(),
            deny_write: Vec::new(),
            grant_read: Vec::new(),
            grant_write: Vec::new(),
            scratch: None,
        }
    }

    pub(crate) fn new(
        mode: SessionPermission,
        workspace: PathBuf,
        config: &ResolvedSandboxConfig,
        session: &SandboxGrants,
        call: &SandboxGrants,
        scratch: Option<Arc<SandboxScratch>>,
    ) -> Self {
        Self {
            mode,
            workspace,
            network: config.network,
            on_denied: config.on_denied,
            overlay_home: config.overlay_home,
            extra_read: config.extra_read_paths.clone(),
            extra_write: config.extra_write_paths.clone(),
            deny_read: config.deny_read_paths.clone(),
            deny_write: config.deny_write_paths.clone(),
            grant_read: merge_paths(&session.read_paths, &call.read_paths),
            grant_write: merge_paths(&session.write_paths, &call.write_paths),
            scratch,
        }
    }

    pub(crate) fn scratch(&self) -> Result<Arc<SandboxScratch>, ToolError> {
        self.scratch.clone().ok_or_else(|| {
            ToolError::new(
                "E_SANDBOX_UNAVAILABLE",
                "sandbox scratch is not available for this session",
            )
        })
    }

    /// 显式拒读不因派生资源换了物理路径而失效；full 授权仍按原策略放行。
    pub(crate) fn denies_read(&self, path: &Path) -> bool {
        self.mode != SessionPermission::Full
            && resolve_access_path(path)
                .is_some_and(|path| self.deny_read.iter().any(|root| is_within(&path, root)))
    }

    pub(crate) fn inherit_read_denials(&mut self, source: &Path, derived: &Path) {
        if self.denies_read(source) && !self.deny_read.iter().any(|path| path == derived) {
            self.deny_read.push(derived.to_owned());
        }
    }

    /// 判定路径访问；`path` 应是已解析或待解析的真实路径。
    pub(crate) fn check(&self, path: &Path, access: Access) -> Decision {
        if self.mode == SessionPermission::Full {
            return Decision::Allow;
        }
        let Some(path) = resolve_access_path(path) else {
            return self.outside_decision();
        };
        let denied = match access {
            Access::Read => &self.deny_read,
            Access::Write => &self.deny_write,
        };
        if denied.iter().any(|root| is_within(&path, root)) {
            return Decision::Deny;
        }
        if access == Access::Write {
            let allowed = self.write_roots().any(|root| is_within(&path, root));
            return if allowed {
                Decision::Allow
            } else {
                self.outside_decision()
            };
        }
        if self.mode == SessionPermission::Workspace {
            return Decision::Allow;
        }
        let allowed = self.read_roots().any(|root| is_within(&path, root));
        if allowed {
            Decision::Allow
        } else {
            self.outside_decision()
        }
    }

    fn outside_decision(&self) -> Decision {
        if self.on_denied == OnDenied::Ask {
            Decision::Ask
        } else {
            Decision::Deny
        }
    }

    fn write_roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.workspace.as_path())
            .chain(self.extra_write.iter().map(PathBuf::as_path))
            .chain(self.grant_write.iter().map(PathBuf::as_path))
            .chain(self.scratch_roots())
    }

    fn read_roots(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.workspace.as_path())
            .chain(self.extra_read.iter().map(PathBuf::as_path))
            .chain(self.extra_write.iter().map(PathBuf::as_path))
            .chain(self.grant_read.iter().map(PathBuf::as_path))
            .chain(self.grant_write.iter().map(PathBuf::as_path))
            .chain(self.scratch_roots())
            .chain(runtime_read_roots().iter().copied().map(Path::new))
    }

    fn scratch_roots(&self) -> impl Iterator<Item = &Path> {
        self.scratch.iter().flat_map(|scratch| {
            [
                scratch.tmp.as_path(),
                scratch.var_tmp.as_path(),
                scratch.home.as_path(),
            ]
        })
    }
}

fn merge_paths(left: &[PathBuf], right: &[PathBuf]) -> Vec<PathBuf> {
    let mut merged = left.to_vec();
    for path in right {
        if !merged.contains(path) {
            merged.push(path.clone());
        }
    }
    merged
}

/// 运行程序所需的最小只读系统路径；不包含宿主 HOME、`/var`、`/proc`，`/run` 只含 resolver。
pub(crate) fn runtime_read_roots() -> &'static [&'static str] {
    #[cfg(target_os = "linux")]
    {
        &[
            "/usr",
            "/bin",
            "/sbin",
            "/lib",
            "/lib64",
            "/opt",
            "/nix",
            "/etc",
            "/dev",
            "/run/systemd/resolve",
        ]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            "/System",
            "/Library",
            "/usr",
            "/bin",
            "/sbin",
            "/dev",
            "/private/etc",
            "/private/var/db/timezone",
            "/private/var/run/mDNSResponder",
        ]
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        &[]
    }
}

/// 从 stderr 尾部识别沙盒拒绝；只用于给模型追加提示，不改变退出码。
pub(crate) fn denial_in(text: &str) -> bool {
    const MARKERS: &[&str] = &[
        "read-only file system",
        "operation not permitted",
        "permission denied",
        "network is unreachable",
        "temporary failure in name resolution",
        "could not resolve host",
    ];
    let text = text.to_ascii_lowercase();
    MARKERS.iter().any(|marker| text.contains(marker))
}

/// runner 包装后的命令；`Process` 持有 scratch/workdir，保证后台任务期间目录不消失。
pub(crate) struct WrappedCommand {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub envs: Vec<(OsString, OsString)>,
    pub scratch: Option<Arc<SandboxScratch>>,
    pub overlay_work: Option<TempDir>,
}

/// 把命令包装成沙盒执行；`full` 直接返回原 shell 命令。
pub(crate) fn wrap_command(
    shell: &ShellChoice,
    command: &str,
    cwd: &Path,
    policy: &SandboxPolicy,
) -> Result<WrappedCommand, ToolError> {
    if policy.mode == SessionPermission::Full {
        let (program, args) = crate::shell::command_line(shell, command);
        return Ok(WrappedCommand {
            program: program.into_os_string(),
            args,
            envs: Vec::new(),
            scratch: None,
            overlay_work: None,
        });
    }

    #[cfg(target_os = "linux")]
    {
        linux::wrap(shell, command, cwd, policy)
    }
    #[cfg(target_os = "macos")]
    {
        let _ = cwd;
        macos::wrap(shell, command, policy)
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (shell, command, cwd);
        Err(ToolError::new(
            "E_SANDBOX_UNAVAILABLE",
            "this platform has no OS sandbox runner",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> ResolvedSandboxConfig {
        ResolvedSandboxConfig {
            default_mode: SessionPermission::Workspace,
            network: true,
            on_denied: OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home: true,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            deny_write_paths: Vec::new(),
        }
    }

    fn scratch() -> Arc<SandboxScratch> {
        Arc::new(SandboxScratch::new().expect("scratch"))
    }

    #[test]
    fn full_allows_everything() {
        let policy = SandboxPolicy::new(
            SessionPermission::Full,
            PathBuf::from("/work"),
            &config(),
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            None,
        );
        assert_eq!(
            policy.check(Path::new("/etc/passwd"), Access::Write),
            Decision::Allow
        );
    }

    #[test]
    fn workspace_reads_outside_but_asks_for_writes() {
        let policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            PathBuf::from("/work"),
            &config(),
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(scratch()),
        );
        assert_eq!(
            policy.check(Path::new("/etc/passwd"), Access::Read),
            Decision::Allow
        );
        assert_eq!(
            policy.check(Path::new("/etc/passwd"), Access::Write),
            Decision::Ask
        );
    }

    #[test]
    fn workspace_only_asks_for_reads_and_writes_outside() {
        let policy = SandboxPolicy::new(
            SessionPermission::WorkspaceOnly,
            PathBuf::from("/work"),
            &config(),
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(scratch()),
        );
        assert_eq!(
            policy.check(Path::new("/home/user/.ssh/config"), Access::Read),
            Decision::Ask
        );
        assert_eq!(
            policy.check(Path::new("/etc/hosts"), Access::Read),
            Decision::Allow
        );
        assert_eq!(
            policy.check(Path::new("/home/user/.ssh/config"), Access::Write),
            Decision::Ask
        );
    }

    #[test]
    fn deny_wins_over_allow_roots() {
        let mut config = config();
        config.deny_write_paths = vec![PathBuf::from("/work/.env")];
        let policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            PathBuf::from("/work"),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(scratch()),
        );
        assert_eq!(
            policy.check(Path::new("/work/.env"), Access::Write),
            Decision::Deny
        );
    }

    #[test]
    fn history_mirror_inherits_source_denials_even_when_already_cached() {
        let directory = tempfile::tempdir().expect("临时目录");
        let source = directory.path().join("private.jsonl.zst");
        let mirror = directory.path().join("sidecar/history/transcript.jsonl");
        std::fs::write(&source, "压缩源").unwrap();
        std::fs::create_dir_all(mirror.parent().unwrap()).unwrap();
        std::fs::write(&mirror, "旧缓存").unwrap();
        let mut config = config();
        config.deny_read_paths = vec![source.clone()];
        let mut policy = SandboxPolicy::new(
            SessionPermission::Workspace,
            directory.path().to_owned(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            None,
        );
        policy.inherit_read_denials(&source, &mirror);
        assert_eq!(policy.check(&mirror, Access::Read), Decision::Deny);
        assert!(
            policy.deny_read.contains(&mirror),
            "内核沙盒也必须遮蔽已存在的镜像"
        );
        policy.mode = SessionPermission::Full;
        assert_eq!(
            policy.check(&mirror, Access::Read),
            Decision::Allow,
            "已授权的 full 仍可读取"
        );
        policy.mode = SessionPermission::Workspace;
        policy.deny_read = vec![directory.path().to_owned()];
        policy.inherit_read_denials(&source, &mirror);
        assert!(policy.denies_read(&mirror), "父目录拒读也要覆盖镜像");
        std::fs::remove_file(&mirror).unwrap();
        policy.deny_read = vec![source.clone()];
        policy.inherit_read_denials(&source, &mirror);
        assert!(policy.denies_read(&mirror), "镜像未生成时也要继承拒读");
    }

    #[test]
    fn session_write_grant_widens_read_and_write() {
        let mut grants = SandboxGrants::default();
        grants.grant_path(Path::new("/tmp"), Access::Write);
        let policy = SandboxPolicy::new(
            SessionPermission::WorkspaceOnly,
            PathBuf::from("/work"),
            &config(),
            &grants,
            &SandboxGrants::default(),
            Some(scratch()),
        );
        assert_eq!(
            policy.check(Path::new("/tmp/a"), Access::Write),
            Decision::Allow
        );
        assert_eq!(
            policy.check(Path::new("/tmp/a"), Access::Read),
            Decision::Allow
        );
    }

    #[test]
    fn command_grants_match_exact_and_prefix() {
        let mut grants = SandboxGrants::default();
        grants.grant_command("cargo", true, SessionPermission::Full);
        grants.grant_command("git push", false, SessionPermission::Workspace);
        assert_eq!(
            grants.mode_for_command("cargo build"),
            Some(SessionPermission::Full)
        );
        assert_eq!(
            grants.mode_for_command("cargo"),
            Some(SessionPermission::Full)
        );
        assert_eq!(
            grants.mode_for_command("git push origin"),
            None,
            "exact grant 不匹配更长命令"
        );
        assert_eq!(
            grants.mode_for_command("git push"),
            Some(SessionPermission::Workspace)
        );
    }

    #[test]
    fn prefix_is_only_offered_for_simple_commands() {
        assert_eq!(
            command_prefix("cargo build --release"),
            Some("cargo".into())
        );
        assert_eq!(command_prefix("sudo cargo build"), None);
        assert_eq!(command_prefix("cargo build && rm -rf /"), None);
        assert_eq!(command_prefix("FOO=bar cargo build"), None);
    }
}

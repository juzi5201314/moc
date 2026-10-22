//! 管道和 PTY 进程共用的完成句柄。转为后台任务只转移所有权，不会重置 monitored 超时。

use super::{BashArgs, JobLog, ProcessGroup, ToolError};
use crate::limits::BashLimits;
use crate::sandbox::{SandboxPolicy, WrappedCommand, wrap_command};
use futures_util::future::BoxFuture;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::sync::CancellationToken;

/// 子进程退出后等管道读完的时限。超时就放弃读取并回收任务，免得逃出进程组的子孙进程
/// 一直占着 stdout，导致并发名额无法释放。
const PIPE_DRAIN_GRACE: Duration = Duration::from_secs(2);

pub(crate) struct ProcessExit {
    pub code: Option<i32>,
    pub timed_out: bool,
    pub output_truncated: bool,
    /// stderr 命中沙盒拒绝签名；仅作提示，不改变退出码。
    pub sandbox_denied: bool,
    /// 资源范围没能建立（命令实际在无限制下执行）；仅作提示。
    pub limits_failed: bool,
}

pub(crate) struct Process {
    pub pid: Option<u32>,
    pub group: ProcessGroup,
    pub started_at: std::time::Instant,
    pub completion: BoxFuture<'static, Result<ProcessExit, ToolError>>,
}

impl Process {
    pub(super) async fn spawn(
        args: BashArgs,
        cwd: PathBuf,
        timeout: Option<Duration>,
        log: JobLog,
        policy: SandboxPolicy,
        limits: Option<BashLimits>,
    ) -> Result<Self, ToolError> {
        if policy.mode != moc_protocol::SessionPermission::Full && cfg!(not(unix)) {
            return Err(ToolError::new(
                "E_SANDBOX_UNAVAILABLE",
                "sandboxed permissions require a Unix sandbox runner",
            ));
        }
        if timeout.is_some()
            && log.mode() == super::JobLogMode::Monitored
            && log.path().as_os_str().is_empty()
        {
            return Err(ToolError::new(
                "E_BASH_CONFIG",
                "monitored jobs require a log",
            ));
        }
        if args.pty.unwrap_or(false) {
            Self::pty(args, cwd, timeout, log, policy, limits).await
        } else {
            Self::pipe(args, cwd, timeout, log, policy, limits)
        }
    }

    fn pipe(
        args: BashArgs,
        cwd: PathBuf,
        timeout: Option<Duration>,
        log: JobLog,
        policy: SandboxPolicy,
        limits: Option<BashLimits>,
    ) -> Result<Self, ToolError> {
        let shell = crate::shell::detect().ok_or_else(|| {
            ToolError::new("E_BASH_SHELL", "no supported shell found on this platform")
        })?;
        let wrapped = wrap_command(shell, &args.command, &cwd, &policy)?;
        let WrappedCommand {
            program,
            args: sandbox_args,
            envs: sandbox_envs,
            scratch,
            overlay_work,
        } = wrapped;
        // 资源限制只改启动方式，不动沙盒、环境与管道。
        let (program, sandbox_args) = crate::limits::wrap(program, sandbox_args, limits);
        let keepalive = (scratch, overlay_work);
        let mut command = Command::new(program);
        command.args(&sandbox_args);
        command
            .current_dir(cwd)
            .envs(args.env.unwrap_or_default())
            .envs(sandbox_envs)
            // 非交互 bash/sh 仍会读取 BASH_ENV/ENV；清掉它们，避免用户脚本改变行为。
            .env_remove("BASH_ENV")
            .env_remove("ENV")
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(windows)]
        let group = ProcessGroup::for_suspended_spawn(&mut command, limits)?;
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.as_std_mut().process_group(0);
        }
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::new("E_BASH_SPAWN", error.to_string()))?;
        let pid = child.id();
        #[cfg(windows)]
        {
            let handle = child.raw_handle().ok_or_else(|| {
                ToolError::new(
                    "E_BASH_SPAWN",
                    "cannot obtain the Windows child-process handle",
                )
            })?;
            group.assign_and_resume_suspended_process(handle)?;
        }
        #[cfg(unix)]
        let group = ProcessGroup::new(pid)?;
        let completion_group = group.clone();
        let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
        let input = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout 已配置为管道");
        let stderr = child.stderr.take().expect("stderr 已配置为管道");
        let output_group = completion_group.clone();
        let reader_log = log.clone();
        let mut reader = tokio::spawn(async move {
            let write_input = async move {
                if let (Some(mut input), Some(text)) = (input, args.stdin) {
                    match input.write_all(text.as_bytes()).await {
                        Err(error) if error.kind() != std::io::ErrorKind::BrokenPipe => {
                            return Err(error);
                        }
                        _ => {}
                    }
                }
                Ok::<_, std::io::Error>(())
            };
            let (output, errors, input) = tokio::join!(
                super::super::job::copy_output(stdout, reader_log.clone(), output_group.clone()),
                super::super::job::copy_output(stderr, reader_log, output_group),
                write_input
            );
            let truncated = output.and_then(|first| errors.map(|second| first || second))?;
            input.map(|()| truncated)
        });
        let completion = Box::pin(async move {
            let _keepalive = keepalive;
            let (waited, timed_out) = tokio::select! {
                status = child.wait() => (status, false),
                _ = async {
                    if let Some(deadline) = deadline { tokio::time::sleep_until(deadline).await; }
                    else { std::future::pending::<()>().await; }
                } => {
                    completion_group.kill()?;
                    (child.wait().await, true)
                }
            };
            drop(completion_group);
            let output_truncated = match tokio::time::timeout(PIPE_DRAIN_GRACE, &mut reader).await {
                Ok(joined) => joined
                    .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?
                    .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?,
                Err(_) => {
                    tracing::warn!("子进程已退出但 stdout 仍被占用，停止读取输出");
                    reader.abort();
                    false
                }
            };
            let status =
                waited.map_err(|error| ToolError::new("E_BASH_WAIT", error.to_string()))?;
            log.finish()
                .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
            let tail = log.read_tail(80).map(|(text, _)| text).unwrap_or_default();
            Ok(ProcessExit {
                code: status.code(),
                timed_out,
                output_truncated,
                sandbox_denied: crate::sandbox::denial_in(&tail),
                limits_failed: crate::limits::failure_in(&tail),
            })
        });
        Ok(Self {
            pid,
            group,
            started_at: std::time::Instant::now(),
            completion,
        })
    }

    async fn pty(
        args: BashArgs,
        cwd: PathBuf,
        timeout: Option<Duration>,
        log: JobLog,
        policy: SandboxPolicy,
        limits: Option<BashLimits>,
    ) -> Result<Self, ToolError> {
        let cancel = CancellationToken::new();
        let cancel_on_drop = cancel.clone().drop_guard();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            run_pty(args, cwd, timeout, log, policy, limits, cancel, started)
        });
        let (pid, group) = match ready.await {
            Ok((pid, group)) => (pid, group),
            Err(_) => {
                return match worker.await {
                    Ok(Err(error)) => Err(ToolError::new("E_PTY", error.to_string())),
                    Err(error) => Err(ToolError::new("E_PTY", error.to_string())),
                    Ok(Ok(_)) => Err(ToolError::new("E_PTY", "PTY did not report startup status")),
                };
            }
        };
        let completion_group = group.clone();
        let completion = Box::pin(async move {
            let _cancel_on_drop = cancel_on_drop;
            let result = worker
                .await
                .map_err(|error| ToolError::new("E_PTY", error.to_string()))?
                .map_err(|error| ToolError::new("E_PTY", error.to_string()));
            drop(completion_group);
            result
        });
        Ok(Self {
            pid,
            group,
            started_at: std::time::Instant::now(),
            completion,
        })
    }
}

fn run_pty(
    args: BashArgs,
    cwd: PathBuf,
    timeout: Option<Duration>,
    log: JobLog,
    policy: SandboxPolicy,
    limits: Option<BashLimits>,
    cancel: CancellationToken,
    started: tokio::sync::oneshot::Sender<(Option<u32>, ProcessGroup)>,
) -> std::io::Result<ProcessExit> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let shell = crate::shell::detect()
        .ok_or_else(|| std::io::Error::other("no supported shell found on this platform"))?;
    let user_env = args.env.clone().unwrap_or_default();
    #[cfg(unix)]
    let (program, arguments, sandbox_envs, _pty_keepalive) = {
        let wrapped = wrap_command(shell, &args.command, &cwd, &policy)
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        let WrappedCommand {
            program,
            args,
            envs,
            scratch,
            overlay_work,
        } = wrapped;
        let (program, args) = crate::limits::wrap(program, args, limits);
        (program, args, envs, (scratch, overlay_work))
    };
    #[cfg(windows)]
    let (program, arguments, sandbox_envs): (
        std::ffi::OsString,
        Vec<std::ffi::OsString>,
        Vec<(std::ffi::OsString, std::ffi::OsString)>,
    ) = {
        let (program, arguments) = crate::shell::pty_idle_command(shell);
        (program.into_os_string(), arguments, Vec::new())
    };
    let mut builder = CommandBuilder::new(program);
    for argument in arguments {
        builder.arg(argument);
    }
    builder.cwd(cwd);
    #[cfg(windows)]
    if matches!(
        shell.kind,
        crate::shell::ShellKind::Bash | crate::shell::ShellKind::GitBash
    ) {
        builder.env("PS1", "");
    }
    for (key, value) in user_env {
        builder.env(key, value);
    }
    for (key, value) in sandbox_envs {
        builder.env(key, value);
    }
    builder.env_remove("BASH_ENV");
    builder.env_remove("ENV");
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    #[cfg(windows)]
    let group = ProcessGroup::new_windows(limits)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let pid = child.process_id();
    #[cfg(windows)]
    {
        let Some(handle) = portable_pty::Child::as_raw_handle(&*child) else {
            let _ = child.kill();
            return Err(std::io::Error::other(
                "cannot obtain the Windows PTY child-process handle",
            ));
        };
        if let Err(error) = group.assign_running_process(handle) {
            let _ = child.kill();
            return Err(std::io::Error::other(error.to_string()));
        }
    }
    #[cfg(unix)]
    let group = ProcessGroup::new(pid).map_err(|error| std::io::Error::other(error.to_string()))?;
    drop(pair.slave);
    #[cfg(windows)]
    if args.command.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Windows shell commands cannot contain NUL bytes",
        ));
    }
    #[cfg(unix)]
    let input = args.stdin.unwrap_or_default().into_bytes();
    #[cfg(windows)]
    let input = crate::shell::pty_input(shell, &args.command, args.stdin.as_deref());
    let writer_thread = std::thread::spawn(move || writer.write_all(&input));
    let began = std::time::Instant::now();
    let _ = started.send((pid, group.clone()));
    let output_group = group.clone();
    let tail_log = log.clone();
    let reader_thread = std::thread::spawn(move || -> std::io::Result<bool> {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                log.finish()?;
                return Ok(false);
            }
            if log.append(&buffer[..count])? {
                output_group.kill().map_err(std::io::Error::other)?;
                log.finish()?;
                return Ok(true);
            }
        }
    });
    let mut timed_out = false;
    let waited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        timed_out = timeout.is_some_and(|limit| began.elapsed() >= limit);
        if timed_out || cancel.is_cancelled() {
            group.kill().map_err(std::io::Error::other)?;
            break child.wait();
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let _ = writer_thread.join();
    let output_truncated = reader_thread
        .join()
        .map_err(|_| std::io::Error::other("PTY output thread exited unexpectedly"))??;
    let status = waited?;
    let code = status.exit_code().min(i32::MAX as u32) as i32;
    let tail = tail_log
        .read_tail(80)
        .map(|(text, _)| text)
        .unwrap_or_default();
    Ok(ProcessExit {
        code: Some(if status.success() { code } else { code.max(1) }),
        timed_out,
        output_truncated,
        sandbox_denied: crate::sandbox::denial_in(&tail),
        limits_failed: crate::limits::failure_in(&tail),
    })
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use crate::config::{OnDenied, ResolvedSandboxConfig};
    use crate::sandbox::{SandboxGrants, SandboxPolicy, SandboxScratch};
    use std::fs;
    use std::sync::Arc;

    fn bash_args(command: &str) -> BashArgs {
        BashArgs {
            description: Some(format!("Run `{command}`")),
            command: command.to_owned(),
            cwd: None,
            env: None,
            timeout: Some(5),
            lifecycle: Some(crate::tools::bash::BashLifecycle::Monitored),
            pty: Some(false),
            stdin: None,
            asynchronous: false,
            sandbox_permissions: None,
            justification: None,
        }
    }
    async fn test_log(path: PathBuf) -> JobLog {
        JobLog::create(path, super::super::JobLogMode::Monitored, 1024).expect("创建测试日志")
    }

    fn policy(mode: moc_protocol::SessionPermission, workspace: &std::path::Path) -> SandboxPolicy {
        policy_with(mode, workspace, false)
    }

    fn policy_with(
        mode: moc_protocol::SessionPermission,
        workspace: &std::path::Path,
        overlay_home: bool,
    ) -> SandboxPolicy {
        let config = ResolvedSandboxConfig {
            default_mode: mode,
            network: true,
            on_denied: OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            deny_write_paths: Vec::new(),
        };
        let scratch = (mode != moc_protocol::SessionPermission::Full)
            .then(|| Arc::new(SandboxScratch::new().expect("scratch")));
        SandboxPolicy::new(
            mode,
            workspace.to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            scratch,
        )
    }

    #[tokio::test]
    async fn workspace_shell_writes_workspace_but_not_external_paths() {
        if !crate::sandbox::detect().supports(moc_protocol::SessionPermission::Workspace) {
            return;
        }
        let directory = tempfile::tempdir().expect("临时目录");
        let marker = directory.path().join("marker");
        fs::write(&marker, "keep").expect("写入 marker");
        let path = directory.path().join("workspace.log");
        let log = test_log(path).await;
        let process = Process::spawn(
            bash_args("printf x > workspace-marker"),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy(moc_protocol::SessionPermission::Workspace, directory.path()),
            None,
        )
        .await
        .expect("启动 workspace shell");
        let result = process.completion.await.expect("等待 workspace shell");
        assert_eq!(result.code, Some(0));
        assert_eq!(
            fs::read_to_string(directory.path().join("workspace-marker"))
                .expect("读取 workspace marker"),
            "x"
        );

        let external =
            std::path::PathBuf::from(format!("/moc-external-marker-{}", std::process::id()));
        let path = directory.path().join("external.log");
        let log = test_log(path).await;
        let process = Process::spawn(
            bash_args(&format!("printf x > {}", external.display())),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy(moc_protocol::SessionPermission::Workspace, directory.path()),
            None,
        )
        .await
        .expect("启动 external shell");
        let result = process.completion.await.expect("等待 external shell");
        assert_ne!(result.code, Some(0));
        assert!(result.sandbox_denied);
        assert!(!external.exists(), "沙盒不能写入工作区外");
    }

    #[tokio::test]
    async fn denied_file_writes_fail_instead_of_being_discarded() {
        if !crate::sandbox::detect().supports(moc_protocol::SessionPermission::Workspace) {
            return;
        }
        let directory = tempfile::tempdir().expect("临时目录");
        let denied = directory.path().join("denied.txt");
        fs::write(&denied, "keep").expect("写入 denied");
        let config = ResolvedSandboxConfig {
            default_mode: moc_protocol::SessionPermission::Workspace,
            network: true,
            on_denied: OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home: false,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            deny_write_paths: vec![std::fs::canonicalize(&denied).expect("canonical denied")],
        };
        let policy = SandboxPolicy::new(
            moc_protocol::SessionPermission::Workspace,
            directory.path().to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let log = test_log(directory.path().join("denied.log")).await;
        let process = Process::spawn(
            bash_args(&format!("printf overwritten > {}", denied.display())),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy,
            None,
        )
        .await
        .expect("启动 deny shell");
        let result = process.completion.await.expect("等待 deny shell");
        assert_ne!(result.code, Some(0), "deny_write 必须让写入失败");
        assert_eq!(
            fs::read_to_string(&denied).expect("读取 denied"),
            "keep",
            "deny_write 不能把内容写丢"
        );
    }

    #[tokio::test]
    async fn denied_directory_writes_fail() {
        if !crate::sandbox::detect().supports(moc_protocol::SessionPermission::Workspace) {
            return;
        }
        let directory = tempfile::tempdir().expect("临时目录");
        let denied = directory.path().join("denied-dir");
        fs::create_dir(&denied).expect("创建 denied 目录");
        let config = ResolvedSandboxConfig {
            default_mode: moc_protocol::SessionPermission::Workspace,
            network: true,
            on_denied: OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home: false,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: Vec::new(),
            deny_write_paths: vec![std::fs::canonicalize(&denied).expect("canonical denied")],
        };
        let policy = SandboxPolicy::new(
            moc_protocol::SessionPermission::Workspace,
            directory.path().to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let log = test_log(directory.path().join("denied-dir.log")).await;
        let process = Process::spawn(
            bash_args(&format!("printf x > {}/new-file", denied.display())),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy,
            None,
        )
        .await
        .expect("启动 deny dir shell");
        let result = process.completion.await.expect("等待 deny dir shell");
        assert_ne!(result.code, Some(0), "deny_write 目录必须让写入失败");
        assert!(!denied.join("new-file").exists(), "deny 目录不能写丢文件");
    }

    #[tokio::test]
    async fn denied_read_directory_is_empty() {
        if !crate::sandbox::detect().supports(moc_protocol::SessionPermission::Workspace) {
            return;
        }
        let directory = tempfile::tempdir().expect("临时目录");
        let denied = directory.path().join("denied-dir");
        fs::create_dir(&denied).expect("创建 denied 目录");
        fs::write(denied.join("secret"), "secret").expect("写入 secret");
        let config = ResolvedSandboxConfig {
            default_mode: moc_protocol::SessionPermission::Workspace,
            network: true,
            on_denied: OnDenied::Ask,
            allow_unsandboxed: true,
            overlay_home: false,
            extra_read_paths: Vec::new(),
            extra_write_paths: Vec::new(),
            deny_read_paths: vec![std::fs::canonicalize(&denied).expect("canonical denied")],
            deny_write_paths: Vec::new(),
        };
        let policy = SandboxPolicy::new(
            moc_protocol::SessionPermission::Workspace,
            directory.path().to_path_buf(),
            &config,
            &SandboxGrants::default(),
            &SandboxGrants::default(),
            Some(Arc::new(SandboxScratch::new().expect("scratch"))),
        );
        let log = test_log(directory.path().join("denied-read.log")).await;
        let process = Process::spawn(
            bash_args(&format!("test ! -e {}/secret", denied.display())),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy,
            None,
        )
        .await
        .expect("启动 deny read shell");
        let result = process.completion.await.expect("等待 deny read shell");
        assert_eq!(result.code, Some(0), "deny_read 目录必须为空");
    }

    #[tokio::test]
    async fn workspace_overlay_writes_home_in_sandbox_not_host() {
        let support = crate::sandbox::detect();
        if !support.supports(moc_protocol::SessionPermission::Workspace) || !support.home_overlay {
            return;
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        if !home.is_dir() {
            return;
        }
        let name = format!(".moc-overlay-test-{}", std::process::id());
        let host_path = home.join(&name);
        if host_path.exists() {
            return;
        }
        let directory = tempfile::tempdir().expect("临时目录");
        let log = test_log(directory.path().join("overlay.log")).await;
        let command = format!(r#"printf x > "$HOME/{name}" && cat "$HOME/{name}""#);
        let process = Process::spawn(
            bash_args(&command),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy_with(
                moc_protocol::SessionPermission::Workspace,
                directory.path(),
                true,
            ),
            None,
        )
        .await
        .expect("启动 overlay shell");
        let result = process.completion.await.expect("等待 overlay shell");
        let host_exists = host_path.exists();
        if host_exists {
            let _ = fs::remove_file(&host_path);
        }
        assert_eq!(result.code, Some(0));
        assert!(!host_exists, "overlay 写入不能落到宿主 HOME");
    }

    #[tokio::test]
    async fn workspace_only_hides_host_home() {
        if !crate::sandbox::detect().supports(moc_protocol::SessionPermission::WorkspaceOnly) {
            return;
        }
        let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
            return;
        };
        let marker = home.join(format!(".moc-hidden-test-{}", std::process::id()));
        fs::write(&marker, "hidden").expect("写入宿主 marker");
        let directory = tempfile::tempdir().expect("临时目录");
        let log = test_log(directory.path().join("hidden.log")).await;
        let process = Process::spawn(
            bash_args(&format!("cat {}", marker.display())),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy(
                moc_protocol::SessionPermission::WorkspaceOnly,
                directory.path(),
            ),
            None,
        )
        .await
        .expect("启动 workspace-only shell");
        let result = process.completion.await.expect("等待 workspace-only shell");
        let host_marker_still_there = marker.exists();
        let _ = fs::remove_file(&marker);
        assert_ne!(result.code, Some(0), "workspace-only 不能读宿主 HOME");
        assert!(host_marker_still_there, "宿主文件不能被沙盒删除或改写");
    }

    #[tokio::test]
    async fn lingering_grandchild_does_not_stall_completion() {
        let directory = tempfile::tempdir().expect("临时目录");
        let log = test_log(directory.path().join("drain.log")).await;
        let process = Process::spawn(
            bash_args("sleep 20 & echo done"),
            directory.path().to_owned(),
            Some(Duration::from_secs(60)),
            log,
            policy(moc_protocol::SessionPermission::Full, directory.path()),
            None,
        )
        .await
        .expect("启动 shell");
        let result = tokio::time::timeout(Duration::from_secs(6), process.completion)
            .await
            .expect("占着 stdout 的子孙进程不能让任务永远无法回收")
            .expect("等待 shell");
        assert_eq!(result.code, Some(0));
    }

    #[tokio::test]
    async fn bash_env_is_not_sourced() {
        let directory = tempfile::tempdir().expect("临时目录");
        let marker = directory.path().join("bash-env-ran");
        let script = directory.path().join("env.sh");
        std::fs::write(&script, format!("touch {}\n", marker.display())).expect("写入 BASH_ENV");
        let mut args = bash_args("printf ok");
        args.env = Some(std::collections::BTreeMap::from([(
            "BASH_ENV".to_owned(),
            script.display().to_string(),
        )]));
        let log = test_log(directory.path().join("env.log")).await;
        let process = Process::spawn(
            args,
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            policy(moc_protocol::SessionPermission::Full, directory.path()),
            None,
        )
        .await
        .expect("启动 shell");
        let result = process.completion.await.expect("等待 shell");
        assert_eq!(result.code, Some(0));
        assert!(!marker.exists(), "BASH_ENV 不应被执行");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cpu_limit_reaches_the_shell() {
        if !crate::limits::support().available {
            return;
        }
        let directory = tempfile::tempdir().expect("临时目录");
        let log = test_log(directory.path().join("limits.log")).await;
        let limits = crate::limits::BashLimits {
            cpu_cores: 1,
            memory_high_bytes: 0,
            memory_max_bytes: 0,
            memory_swap_bytes: 0,
        };
        let process = Process::spawn(
            bash_args("printf 'CORE_COUNT=%s' \"$(nproc)\""),
            directory.path().to_owned(),
            Some(Duration::from_secs(10)),
            log.clone(),
            policy(moc_protocol::SessionPermission::Full, directory.path()),
            Some(limits),
        )
        .await
        .expect("启动受限 shell");
        let result = process.completion.await.expect("等待受限 shell");
        let (text, _) = log.read_tail(10).expect("读取输出");
        assert_eq!(result.code, Some(0), "log: {text}");
        assert!(text.contains("CORE_COUNT=1"), "实际输出：{text}");
    }
}

//! 管道和 PTY 进程共用的完成句柄。转为后台任务只转移所有权，不会重置 monitored 超时。

use super::{BashArgs, JobLog, ProcessGroup, ToolError};
use futures_util::future::BoxFuture;
use moc_protocol::SessionPermission;
use portable_pty::{CommandBuilder, PtySize, native_pty_system};
use std::{
    ffi::OsString,
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
        workspace: PathBuf,
        timeout: Option<Duration>,
        log: JobLog,
        permission: SessionPermission,
    ) -> Result<Self, ToolError> {
        if cfg!(not(unix)) && permission == SessionPermission::Workspace {
            return Err(ToolError::new(
                "E_SANDBOX_UNAVAILABLE",
                "workspace permissions require a Unix bwrap environment",
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
            Self::pty(args, cwd, workspace, timeout, log, permission).await
        } else {
            Self::pipe(args, cwd, workspace, timeout, log, permission)
        }
    }

    fn pipe(
        args: BashArgs,
        cwd: PathBuf,
        workspace: PathBuf,
        timeout: Option<Duration>,
        log: JobLog,
        permission: SessionPermission,
    ) -> Result<Self, ToolError> {
        let shell = crate::shell::detect().ok_or_else(|| {
            ToolError::new("E_BASH_SHELL", "no supported shell found on this platform")
        })?;
        let mut command = match permission {
            SessionPermission::Full => Command::new(&shell.program),
            SessionPermission::Workspace => {
                let mut command = Command::new("bwrap");
                for argument in sandbox_arguments(&cwd, &workspace) {
                    command.arg(argument);
                }
                command.arg("--").arg(&shell.program);
                command
            }
        };
        crate::shell::apply_arguments(command.as_std_mut(), shell, &args.command);
        command
            .current_dir(cwd)
            .envs(args.env.unwrap_or_default())
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
        let group = ProcessGroup::for_suspended_spawn(&mut command)?;
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
            Ok(ProcessExit {
                code: status.code(),
                timed_out,
                output_truncated,
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
        workspace: PathBuf,
        timeout: Option<Duration>,
        log: JobLog,
        permission: SessionPermission,
    ) -> Result<Self, ToolError> {
        if cfg!(not(unix)) && permission == SessionPermission::Workspace {
            return Err(ToolError::new(
                "E_SANDBOX_UNAVAILABLE",
                "workspace permissions require a Unix bwrap environment",
            ));
        }
        let cancel = CancellationToken::new();
        let cancel_on_drop = cancel.clone().drop_guard();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            run_pty(
                args, cwd, workspace, timeout, log, permission, cancel, started,
            )
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

#[cfg(unix)]
fn sandbox_arguments(cwd: &std::path::Path, workspace: &std::path::Path) -> Vec<OsString> {
    vec![
        "--die-with-parent".into(),
        "--new-session".into(),
        "--unshare-user".into(),
        "--unshare-pid".into(),
        "--unshare-ipc".into(),
        "--unshare-uts".into(),
        "--disable-userns".into(),
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--bind".into(),
        workspace.as_os_str().to_owned(),
        workspace.as_os_str().to_owned(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--chdir".into(),
        cwd.as_os_str().to_owned(),
    ]
}

#[cfg(not(unix))]
fn sandbox_arguments(_cwd: &std::path::Path, _workspace: &std::path::Path) -> Vec<OsString> {
    Vec::new()
}

fn run_pty(
    args: BashArgs,
    cwd: PathBuf,
    workspace: PathBuf,
    timeout: Option<Duration>,
    log: JobLog,
    permission: SessionPermission,
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
    #[cfg(unix)]
    let (program, arguments) = crate::shell::command_line(shell, &args.command);
    #[cfg(windows)]
    let (program, arguments) = crate::shell::pty_idle_command(shell);
    let mut builder = CommandBuilder::new(program.clone().into_os_string());
    if permission == SessionPermission::Workspace {
        for argument in sandbox_arguments(&cwd, &workspace) {
            builder.arg(argument);
        }
        builder.arg("--");
        builder.arg(&program);
    }
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
    for (key, value) in args.env.unwrap_or_default() {
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
    let group =
        ProcessGroup::new_windows().map_err(|error| std::io::Error::other(error.to_string()))?;
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
    Ok(ProcessExit {
        code: Some(if status.success() { code } else { code.max(1) }),
        timed_out,
        output_truncated,
    })
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::fs;

    fn bash_args(command: &str) -> BashArgs {
        BashArgs {
            command: command.to_owned(),
            cwd: None,
            env: None,
            timeout: Some(5),
            lifecycle: Some(crate::tools::bash::BashLifecycle::Monitored),
            pty: Some(false),
            stdin: None,
            asynchronous: false,
        }
    }
    async fn test_log(path: PathBuf) -> JobLog {
        JobLog::create(path, super::super::JobLogMode::Monitored, 1024).expect("创建测试日志")
    }

    #[test]
    fn workspace_arguments_allow_workspace_bind_and_isolate_namespaces() {
        let cwd = std::path::Path::new("/tmp/workspace/src");
        let workspace = std::path::Path::new("/tmp/workspace");
        let arguments = sandbox_arguments(cwd, workspace);
        let values: Vec<_> = arguments
            .iter()
            .map(|argument| argument.to_string_lossy())
            .collect();
        assert!(values.iter().any(|value| value == "--ro-bind"));
        assert!(values.iter().any(|value| value == "--bind"));
        assert!(values.iter().any(|value| value == "--unshare-user"));
        assert!(values.iter().any(|value| value == "--unshare-pid"));
        assert!(values.iter().any(|value| value == "--unshare-ipc"));
        assert!(values.iter().any(|value| value == "--unshare-uts"));
        assert!(values.iter().any(|value| value == "--disable-userns"));
        assert!(values.iter().any(|value| value == "--chdir"));
        assert!(!values.iter().any(|value| value == "--unshare-net"));
    }

    #[tokio::test]
    async fn workspace_shell_writes_workspace_but_not_external_paths() {
        let directory = tempfile::tempdir().expect("临时目录");
        let marker = directory.path().join("marker");
        fs::write(&marker, "keep").expect("写入 marker");
        let path = directory.path().join("workspace.log");
        let log = test_log(path).await;
        let process = Process::spawn(
            bash_args("printf x > workspace-marker"),
            directory.path().to_owned(),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            SessionPermission::Workspace,
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

        let external = directory.path().join("..").join("moc-external-marker");
        let path = directory.path().join("external.log");
        let log = test_log(path).await;
        let process = Process::spawn(
            bash_args(&format!("printf x > {}", external.display())),
            directory.path().to_owned(),
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            SessionPermission::Workspace,
        )
        .await
        .expect("启动 external shell");
        let result = process.completion.await.expect("等待 external shell");
        assert_ne!(result.code, Some(0));
        assert!(!external.exists(), "沙盒不能写入工作区外");
    }

    #[tokio::test]
    async fn lingering_grandchild_does_not_stall_completion() {
        let directory = tempfile::tempdir().expect("临时目录");
        let log = test_log(directory.path().join("drain.log")).await;
        let process = Process::spawn(
            bash_args("sleep 20 & echo done"),
            directory.path().to_owned(),
            directory.path().to_owned(),
            Some(Duration::from_secs(60)),
            log,
            SessionPermission::Full,
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
            directory.path().to_owned(),
            Some(Duration::from_secs(5)),
            log,
            SessionPermission::Full,
        )
        .await
        .expect("启动 shell");
        let result = process.completion.await.expect("等待 shell");
        assert_eq!(result.code, Some(0));
        assert!(!marker.exists(), "BASH_ENV 不应被执行");
    }
}

//! 管道与 PTY 共用的完成句柄；后台化只转移所有权，不重置超时。

use super::{BashArgs, ProcessGroup, ToolError};
use futures_util::future::BoxFuture;
use portable_pty::{ChildKiller, CommandBuilder, PtySize, native_pty_system};
use std::{
    ffi::OsString,
    io::{Read, Write},
    path::PathBuf,
    process::Stdio,
    time::Duration,
};
use tokio::{io::AsyncWriteExt, process::Command};
use tokio_util::sync::CancellationToken;

pub(crate) struct ProcessExit {
    pub code: Option<i32>,
    pub timed_out: bool,
}

pub(crate) struct Process {
    pub pid: Option<u32>,
    pub group: ProcessGroup,
    pub completion: BoxFuture<'static, Result<ProcessExit, ToolError>>,
}

impl Process {
    pub(super) async fn spawn(
        args: BashArgs,
        cwd: PathBuf,
        timeout: u64,
        path: PathBuf,
    ) -> Result<Self, ToolError> {
        let duration = (timeout > 0).then_some(Duration::from_secs(timeout));
        if args.pty.unwrap_or(false) {
            Self::pty(args, cwd, duration, path).await
        } else {
            Self::pipe(args, cwd, duration, path)
        }
    }

    fn pipe(
        args: BashArgs,
        cwd: PathBuf,
        timeout: Option<Duration>,
        path: PathBuf,
    ) -> Result<Self, ToolError> {
        let mut command = Command::new(shell_program());
        for argument in shell_arguments(&args.command) {
            command.arg(argument);
        }
        command
            .current_dir(cwd)
            .envs(args.env.unwrap_or_default())
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
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
            let handle = child
                .raw_handle()
                .ok_or_else(|| ToolError::new("E_BASH_SPAWN", "无法取得 Windows 子进程句柄"))?;
            group.assign_and_resume_suspended_process(handle)?;
        }
        #[cfg(unix)]
        let group = ProcessGroup::new(pid)?;
        let completion_group = group.clone();
        let deadline = timeout.map(|duration| tokio::time::Instant::now() + duration);
        let input = child.stdin.take();
        let stdout = child.stdout.take().expect("stdout 已配置为管道");
        let reader = tokio::spawn(async move {
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
            let (output, input) =
                tokio::join!(super::super::job::copy_output(stdout, path), write_input);
            output.and(input)
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
            reader
                .await
                .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?
                .map_err(|error| ToolError::new("E_BASH_LOG", error.to_string()))?;
            let status =
                waited.map_err(|error| ToolError::new("E_BASH_WAIT", error.to_string()))?;
            Ok(ProcessExit {
                code: status.code(),
                timed_out,
            })
        });
        Ok(Self {
            pid,
            group,
            completion,
        })
    }

    async fn pty(
        args: BashArgs,
        cwd: PathBuf,
        timeout: Option<Duration>,
        path: PathBuf,
    ) -> Result<Self, ToolError> {
        let cancel = CancellationToken::new();
        let cancel_on_drop = cancel.clone().drop_guard();
        let (started, ready) = tokio::sync::oneshot::channel();
        let worker =
            tokio::task::spawn_blocking(move || run_pty(args, cwd, timeout, path, cancel, started));
        let (pid, group) = match ready.await {
            Ok((pid, group)) => (pid, group),
            Err(_) => {
                return match worker.await {
                    Ok(Err(error)) => Err(ToolError::new("E_PTY", error.to_string())),
                    Err(error) => Err(ToolError::new("E_PTY", error.to_string())),
                    Ok(Ok(_)) => Err(ToolError::new("E_PTY", "PTY 未报告启动状态")),
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
            completion,
        })
    }
}

#[cfg(unix)]
fn shell_program() -> OsString {
    std::env::var_os("SHELL").unwrap_or_else(|| "/bin/bash".into())
}

#[cfg(windows)]
fn shell_program() -> OsString {
    std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())
}

#[cfg(unix)]
fn shell_arguments(command: &str) -> Vec<String> {
    vec!["-lc".into(), format!("exec 2>&1\n{command}")]
}

#[cfg(windows)]
fn shell_arguments(command: &str) -> Vec<String> {
    vec![
        "/D".into(),
        "/S".into(),
        "/C".into(),
        format!("chcp 65001>nul & ({command}) 2>&1"),
    ]
}

#[cfg(windows)]
// portable-pty 不支持暂停创建；先启动关闭 AutoRun 的空闲 cmd，再在入 Job 后发送命令。
fn pty_shell_arguments(_command: &str) -> Vec<String> {
    vec![
        "/D".into(),
        "/Q".into(),
        "/K".into(),
        "chcp 65001>nul & prompt $S".into(),
    ]
}

#[cfg(unix)]
fn pty_shell_arguments(command: &str) -> Vec<String> {
    shell_arguments(command)
}

#[cfg(windows)]
fn pty_input(command: &str, stdin: Option<String>) -> std::io::Result<Vec<u8>> {
    if command.contains('\0') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "Windows shell 命令不能包含 NUL 字节",
        ));
    }
    let command = if command.trim().is_empty() {
        "rem"
    } else {
        command
    };
    let command = command
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\n', "\r");
    // exit 与用户命令同属一行，避免未读取的 stdin 被外层 cmd 当作命令。
    Ok(format!("({command}) 2>&1 & exit\r{}", stdin.unwrap_or_default()).into_bytes())
}

#[cfg(unix)]
fn pty_input(_command: &str, stdin: Option<String>) -> std::io::Result<Vec<u8>> {
    Ok(stdin.unwrap_or_default().into_bytes())
}

fn run_pty(
    args: BashArgs,
    cwd: PathBuf,
    timeout: Option<Duration>,
    path: PathBuf,
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
    let mut builder = CommandBuilder::new(shell_program());
    for argument in pty_shell_arguments(&args.command) {
        builder.arg(argument);
    }
    builder.cwd(cwd);
    for (key, value) in args.env.unwrap_or_default() {
        builder.env(key, value);
    }
    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut writer = pair
        .master
        .take_writer()
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut log = std::fs::File::create(path)?;
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
            return Err(std::io::Error::other("无法取得 Windows PTY 子进程句柄"));
        };
        if let Err(error) = group.assign_running_process(handle) {
            let _ = child.kill();
            return Err(std::io::Error::other(error.to_string()));
        }
    }
    #[cfg(unix)]
    let group = ProcessGroup::new(pid).map_err(|error| std::io::Error::other(error.to_string()))?;
    drop(pair.slave);
    let input = pty_input(&args.command, args.stdin)?;
    let writer_thread = std::thread::spawn(move || writer.write_all(&input));
    let reader_thread = std::thread::spawn(move || -> std::io::Result<()> {
        let mut buffer = [0u8; 16 * 1024];
        loop {
            let count = reader.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            log.write_all(&buffer[..count])?;
        }
        log.sync_all()
    });
    let began = std::time::Instant::now();
    let _ = started.send((pid, group.clone()));
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
    drop(group);
    let _ = writer_thread.join();
    reader_thread
        .join()
        .map_err(|_| std::io::Error::other("PTY 输出线程异常退出"))??;
    let status = waited?;
    let code = status.exit_code().min(i32::MAX as u32) as i32;
    Ok(ProcessExit {
        code: Some(if status.success() { code } else { code.max(1) }),
        timed_out,
    })
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{pty_input, pty_shell_arguments};

    #[test]
    fn pty_defers_the_user_command_until_the_shell_is_ready() {
        assert_eq!(
            pty_shell_arguments("echo ready"),
            vec![
                String::from("/D"),
                String::from("/Q"),
                String::from("/K"),
                String::from("chcp 65001>nul & prompt $S"),
            ],
        );
        assert_eq!(
            String::from_utf8(pty_input("echo ready", Some("stdin-data".into())).unwrap()).unwrap(),
            "(echo ready) 2>&1 & exit\rstdin-data",
        );
    }
}

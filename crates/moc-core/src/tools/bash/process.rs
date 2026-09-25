//! 管道与 PTY 共用的完成句柄；后台化只转移所有权，不重置超时。

use super::{BashArgs, ProcessGroup, ToolError, kill_process_group};
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

pub(crate) struct ProcessExit {
    pub code: Option<i32>,
    pub timed_out: bool,
}

pub(crate) struct Process {
    pub pid: Option<u32>,
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
        let mut command =
            Command::new(std::env::var_os("SHELL").unwrap_or_else(|| "/bin/bash".into()));
        command
            .arg("-lc")
            .arg(format!("exec 2>&1\n{}", args.command))
            .current_dir(cwd)
            .envs(args.env.unwrap_or_default())
            .stdin(if args.stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .kill_on_drop(true);
        use std::os::unix::process::CommandExt;
        command.as_std_mut().process_group(0);
        let mut child = command
            .spawn()
            .map_err(|error| ToolError::new("E_BASH_SPAWN", error.to_string()))?;
        let pid = child.id();
        let group = ProcessGroup::new(pid);
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
                    kill_process_group(pid)?;
                    (child.wait().await, true)
                }
            };
            drop(group);
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
        Ok(Self { pid, completion })
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
        let pid = match ready.await {
            Ok(pid) => pid,
            Err(_) => {
                return match worker.await {
                    Ok(Err(error)) => Err(ToolError::new("E_PTY", error.to_string())),
                    Err(error) => Err(ToolError::new("E_PTY", error.to_string())),
                    Ok(Ok(_)) => Err(ToolError::new("E_PTY", "PTY 未报告启动状态")),
                };
            }
        };
        let completion = Box::pin(async move {
            let _cancel_on_drop = cancel_on_drop;
            worker
                .await
                .map_err(|error| ToolError::new("E_PTY", error.to_string()))?
                .map_err(|error| ToolError::new("E_PTY", error.to_string()))
        });
        Ok(Self { pid, completion })
    }
}

fn run_pty(
    args: BashArgs,
    cwd: PathBuf,
    timeout: Option<Duration>,
    path: PathBuf,
    cancel: CancellationToken,
    started: tokio::sync::oneshot::Sender<Option<u32>>,
) -> std::io::Result<ProcessExit> {
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let mut builder =
        CommandBuilder::new(std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into()));
    builder.arg("-lc");
    builder.arg(format!("exec 2>&1\n{}", args.command));
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
    let mut child = pair
        .slave
        .spawn_command(builder)
        .map_err(|error| std::io::Error::other(error.to_string()))?;
    let pid = child.process_id();
    let group = ProcessGroup::new(pid);
    drop(pair.slave);
    let input = args.stdin.unwrap_or_default();
    let writer_thread = std::thread::spawn(move || -> std::io::Result<()> {
        writer.write_all(input.as_bytes())?;
        writer.write_all(b"\x04\x04")
    });
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
    let _ = started.send(pid);
    let mut timed_out = false;
    let waited = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Err(error) => break Err(error),
            Ok(None) => {}
        }
        timed_out = timeout.is_some_and(|limit| began.elapsed() >= limit);
        if timed_out || cancel.is_cancelled() {
            kill_process_group(pid).map_err(std::io::Error::other)?;
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

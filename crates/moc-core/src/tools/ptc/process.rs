//! 独立 QuickJS worker 进程的生命周期与一行一条 JSON 的传输。

use super::ToolError;
use crate::PtcWorkerCommand;
use moc_ptc_protocol::wire::GuestStep;
use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, Lines},
    process::{Child, ChildStdin, ChildStdout},
};

/// worker 的 stderr 只用于诊断；保留尾部，避免崩溃时没有任何线索。
const STDERR_TAIL_BYTES: usize = 4096;
const SHUTDOWN_GRACE: Duration = Duration::from_secs(2);

pub(super) struct Guest {
    child: Child,
    stdin: ChildStdin,
    stdout: Lines<BufReader<ChildStdout>>,
    stderr: Arc<Mutex<String>>,
}

impl Guest {
    pub(super) fn spawn(command: &PtcWorkerCommand) -> Result<Self, ToolError> {
        let mut child = tokio::process::Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| {
                ToolError::new(
                    "E_PTC_WORKER",
                    format!("无法启动 {}: {error}", command.program.display()),
                )
            })?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ToolError::new("E_PTC_WORKER", "worker 的 stdin 不可用"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ToolError::new("E_PTC_WORKER", "worker 的 stdout 不可用"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| ToolError::new("E_PTC_WORKER", "worker 的 stderr 不可用"))?;
        let tail = Arc::new(Mutex::new(String::new()));
        let drain = Arc::clone(&tail);
        tokio::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(mut tail) = drain.lock() else {
                    return;
                };
                tail.push_str(&line);
                tail.push('\n');
                if tail.len() > STDERR_TAIL_BYTES {
                    let keep_from = tail.len() - STDERR_TAIL_BYTES;
                    tail.drain(..keep_from);
                }
            }
        });
        Ok(Self {
            child,
            stdin,
            stdout: BufReader::new(stdout).lines(),
            stderr: tail,
        })
    }

    /// 发送一个 step 的输入并读取一个 `GuestStep`。协议只有"一行进、一行出"。
    pub(super) async fn step(
        &mut self,
        input: &[u8],
        _first: bool,
        max_output: usize,
    ) -> Result<GuestStep, ToolError> {
        if let Err(error) = self.stdin.write_all(input).await {
            return Err(worker_error(
                &self.stderr,
                format!("写入 worker 输入失败：{error}"),
            ));
        }
        if let Err(error) = self.stdin.write_all(b"\n").await {
            return Err(worker_error(
                &self.stderr,
                format!("写入 worker 输入失败：{error}"),
            ));
        }
        if let Err(error) = self.stdin.flush().await {
            return Err(worker_error(
                &self.stderr,
                format!("刷新 worker 输入失败：{error}"),
            ));
        }

        let line = match self.stdout.next_line().await {
            Ok(line) => line,
            Err(error) => {
                return Err(worker_error(
                    &self.stderr,
                    format!("读取 worker 输出失败：{error}"),
                ));
            }
        };
        let Some(line) = line else {
            return Err(worker_error(&self.stderr, "worker 提前退出"));
        };
        if line.len() > max_output.saturating_add(4096) {
            return Err(ToolError::new("E_OUTPUT_LIMIT", "worker 输出超过配置上限"));
        }
        serde_json::from_str(&line)
            .map_err(|error| ToolError::new("E_PTC_PROTOCOL", format!("worker 消息无效：{error}")))
    }

    pub(super) async fn shutdown(&mut self) {
        let _ = self.child.start_kill();
        let _ = tokio::time::timeout(SHUTDOWN_GRACE, self.child.wait()).await;
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        let _ = self.child.start_kill();
    }
}

fn worker_error(stderr: &Arc<Mutex<String>>, message: impl Into<String>) -> ToolError {
    let message = message.into();
    let tail = stderr
        .lock()
        .map(|tail| tail.trim().to_owned())
        .unwrap_or_default();
    if tail.is_empty() {
        ToolError::new("E_PTC_WORKER", message)
    } else {
        ToolError::new("E_PTC_WORKER", format!("{message}\nworker stderr:\n{tail}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[tokio::test]
    async fn reports_worker_exit_without_panicking() {
        let command = crate::PtcWorkerCommand {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "exit 7".into()],
        };
        let mut guest = Guest::spawn(&command).expect("spawn");
        let error = guest
            .step(b"{}", true, 1024)
            .await
            .expect_err("worker exit");
        assert_eq!(error.code, "E_PTC_WORKER");
        guest.shutdown().await;
    }
}

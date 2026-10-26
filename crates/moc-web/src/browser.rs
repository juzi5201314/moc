//! 启动后用系统默认浏览器打开 WebUI。失败只告警，不影响服务。
//!
//! 不引入额外 crate：各平台调用自带的打开命令。Linux 上没有图形会话
//! （`DISPLAY` / `WAYLAND_DISPLAY` 都缺失，典型是 SSH）时直接跳过。

use std::io;
use std::process::Stdio;

use tokio::process::Command;

/// 尝试打开 `url`；返回 `false` 表示当前环境不适合自动打开（已跳过）。
pub fn open(url: &str) -> io::Result<bool> {
    let Some(mut command) = command_for(url) else {
        return Ok(false);
    };
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // 回收子进程，避免留下僵尸进程。
    tokio::spawn(async move {
        if let Err(error) = child.wait().await {
            tracing::debug!(%error, "浏览器打开命令等待失败");
        }
    });
    Ok(true)
}

#[cfg(target_os = "macos")]
fn command_for(url: &str) -> Option<Command> {
    let mut command = Command::new("open");
    command.arg(url);
    Some(command)
}

#[cfg(windows)]
fn command_for(url: &str) -> Option<Command> {
    // 不走 `cmd /c start`：URL 里的 `&` 等字符会被 cmd 解释。
    let mut command = Command::new("rundll32");
    command.args(["url.dll,FileProtocolHandler", url]);
    Some(command)
}

#[cfg(all(unix, not(target_os = "macos")))]
fn command_for(url: &str) -> Option<Command> {
    let graphical = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|value| !value.is_empty()));
    if !graphical {
        return None;
    }
    let mut command = Command::new("xdg-open");
    command.arg(url);
    Some(command)
}

#[cfg(not(any(unix, windows)))]
fn command_for(_url: &str) -> Option<Command> {
    None
}

//! 跨平台在系统文件管理器中定位并显示本地图片或目录。

use std::path::Path;
use std::process::Stdio;

use tokio::process::Command;

/// 在系统文件管理器中打开并高亮选中目标文件（若为目录则直接打开目录）。
pub async fn reveal_in_file_manager(path: &Path) -> anyhow::Result<()> {
    if !path.exists() {
        anyhow::bail!("文件不存在：{}", path.display());
    }
    let canonical = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    reveal_impl(&canonical).await
}

#[cfg(target_os = "macos")]
async fn reveal_impl(path: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("open");
    if path.is_file() {
        cmd.arg("-R");
    }
    cmd.arg(path);
    spawn_detached(cmd)
}

#[cfg(windows)]
async fn reveal_impl(path: &Path) -> anyhow::Result<()> {
    let mut cmd = Command::new("explorer.exe");
    if path.is_file() {
        cmd.arg(format!("/select,{}", path.display()));
    } else {
        cmd.arg(path);
    }
    spawn_detached(cmd)
}

#[cfg(all(unix, not(target_os = "macos")))]
async fn reveal_impl(path: &Path) -> anyhow::Result<()> {
    let graphical = ["DISPLAY", "WAYLAND_DISPLAY"]
        .iter()
        .any(|key| std::env::var_os(key).is_some_and(|v| !v.is_empty()));
    if !graphical {
        anyhow::bail!("当前环境没有图形桌面会话，无法打开系统文件管理器");
    }
    // 优先通过 freedesktop FileManager1 DBus 接口高亮选中目标文件。
    if path.is_file() {
        let uri = file_uri(path);
        let dbus_ok = Command::new("dbus-send")
            .args([
                "--session",
                "--dest=org.freedesktop.FileManager1",
                "--type=method_call",
                "/org/freedesktop/FileManager1",
                "org.freedesktop.FileManager1.ShowItems",
                &format!("array:string:{uri}"),
                "string:",
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await
            .is_ok_and(|status| status.success());
        if dbus_ok {
            return Ok(());
        }
    }
    let target = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    let mut cmd = Command::new("xdg-open");
    cmd.arg(target);
    spawn_detached(cmd)
}

#[cfg(not(any(unix, windows)))]
async fn reveal_impl(_path: &Path) -> anyhow::Result<()> {
    anyhow::bail!("当前平台不支持打开系统文件管理器")
}

#[cfg(all(unix, not(target_os = "macos")))]
fn file_uri(path: &Path) -> String {
    let mut out = String::from("file://");
    for byte in path.as_os_str().as_encoded_bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(byte) {
            out.push(*byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

fn spawn_detached(mut cmd: Command) -> anyhow::Result<()> {
    let mut child = cmd
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    tokio::spawn(async move {
        let _ = child.wait().await;
    });
    Ok(())
}

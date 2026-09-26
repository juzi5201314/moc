//! 剪贴板图片读取（Linux）。
//!
//! 终端没有统一的「读图片」协议：Wayland 走 `wl-paste`，X11 走 `xclip`，
//! 二者按 MIME 探测。子进程在 TTY 之外执行（tokio 的进程组隔离），失败
//! 一律返回 `None`，调用方提示用户即可——图片粘贴是可选增强。

use anyhow::Context;
use tokio::process::Command;

/// 一次成功的图片读取：MIME + 原始字节。
pub struct ClipboardImage {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// 尝试从系统剪贴板读取一张图片。
pub async fn read_clipboard_image() -> Option<ClipboardImage> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-paste") {
        if let Some(image) = try_read("wl-paste", &["-t", "image/png"]).await {
            return Some(image);
        }
        return try_read("wl-paste", &["-t", "image/jpeg"]).await;
    }
    if std::env::var_os("DISPLAY").is_some() && which("xclip") {
        if let Some(image) = try_read(
            "xclip",
            &["-selection", "clipboard", "-t", "image/png", "-o"],
        )
        .await
        {
            return Some(image);
        }
        return try_read(
            "xclip",
            &["-selection", "clipboard", "-t", "image/jpeg", "-o"],
        )
        .await;
    }
    None
}

async fn try_read(program: &str, args: &[&str]) -> Option<ClipboardImage> {
    let output = Command::new(program).args(args).output().await.ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let media_type = if args.contains(&"image/jpeg") {
        "image/jpeg"
    } else {
        "image/png"
    };
    Some(ClipboardImage {
        media_type: media_type.to_owned(),
        bytes: output.stdout,
    })
}

/// 探测可执行文件是否在 PATH 上（不 spawn、只查目录）。
fn which(program: &str) -> bool {
    let Ok(path) = std::env::var("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(program).is_file())
}

/// 图片字节 → base64（发送给 LLM 的载荷）。
pub fn base64_encode(data: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(data)
}

/// 复制文本到系统剪贴板（选择的复制通道；OSC52 由终端层另发）。
pub async fn copy_text_to_clipboard(text: &str) -> anyhow::Result<()> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-copy") {
        let mut child = Command::new("wl-copy")
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("启动 wl-copy 失败")?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            stdin.write_all(text.as_bytes()).await.ok();
            stdin.shutdown().await.ok();
        }
        child.wait().await?;
        return Ok(());
    }
    if std::env::var_os("DISPLAY").is_some() && which("xclip") {
        let mut child = Command::new("xclip")
            .args(["-selection", "clipboard"])
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("启动 xclip 失败")?;
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt as _;
            stdin.write_all(text.as_bytes()).await.ok();
            stdin.shutdown().await.ok();
        }
        child.wait().await?;
        return Ok(());
    }
    anyhow::bail!("未找到 wl-copy / xclip，无法访问系统剪贴板")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        let encoded = base64_encode(b"hello");
        assert_eq!(encoded, "aGVsbG8=");
    }
}

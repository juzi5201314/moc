//! 剪贴板图片与文本访问（Linux / WSL / macOS）。
//!
//! 终端没有统一的「读图片」协议：WSL 走 Windows 剪贴板（powershell.exe，
//! WSLg 默认不带 wl-clipboard，Wayland 通道通常不可用），Wayland 走
//! `wl-paste`，X11 走 `xclip`，macOS 优先 `pngpaste`，否则走系统 `osascript`。
//! 子进程在 TTY 之外执行（tokio 的进程组隔离），
//! 失败一律返回 `None`／报错，调用方提示用户即可——剪贴板是可选增强。

use std::time::Duration;

use anyhow::Context;
use base64::Engine as _;
use rust_i18n::t;
use tokio::process::Command;

/// 一次成功的图片读取：MIME + 原始字节。
pub struct ClipboardImage {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

/// 图片 MIME 探测顺序：优先无损且体积小的格式。
const IMAGE_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// PowerShell 冷启动与程序集加载可达数秒，给足余量。
const WINDOWS_TIMEOUT: Duration = Duration::from_secs(10);
/// 本机剪贴板工具：剪贴板所有者无响应时会挂住，必须设上限。
const LOCAL_TIMEOUT: Duration = Duration::from_secs(3);

/// 读取 Windows 剪贴板图片：`Get-Clipboard -Format Image` 走 Windows
/// PowerShell 自带的剪贴板通道，比显式加载 Windows Forms 省约一半启动时间。
const WINDOWS_IMAGE_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$image = Get-Clipboard -Format Image
if ($null -ne $image) {
    $stream = New-Object System.IO.MemoryStream
    $image.Save($stream, [System.Drawing.Imaging.ImageFormat]::Png)
    [Console]::Out.Write([Convert]::ToBase64String($stream.ToArray()))
}
"#;

/// 读取 Windows 剪贴板纯文本；显式 UTF-8 输出，避免中文按 GBK 编码。
const WINDOWS_TEXT_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
[Console]::OutputEncoding = [Text.Encoding]::UTF8
[Console]::Out.Write([string](Get-Clipboard -Raw))
"#;

/// 写入 Windows 剪贴板；文本经 base64 走 stdin，绕开命令行长度与编码问题。
const WINDOWS_COPY_SCRIPT: &str = r#"
$ErrorActionPreference = 'Stop'
$data = [Console]::In.ReadToEnd()
$text = [Text.Encoding]::UTF8.GetString([Convert]::FromBase64String($data.Trim()))
Set-Clipboard -Value $text
"#;

#[cfg(target_os = "macos")]
const MACOS_IMAGE_SCRIPT: &str = r#"
on run argv
    set imageData to the clipboard as «class PNGf»
    set outputFile to open for access POSIX file (item 1 of argv) with write permission
    try
        set eof outputFile to 0
        write imageData to outputFile
        close access outputFile
    on error errorMessage number errorNumber
        try
            close access outputFile
        end try
        error errorMessage number errorNumber
    end try
end run
"#;

/// 测试与自动化环境的逃生口：置位后所有剪贴板通道视为不可用，不探测、
/// 不启动任何外部工具（避免 CI 上为不存在的工具白付子进程启动成本）。
fn disabled() -> bool {
    std::env::var_os("MOC_CLIPBOARD_DISABLE").is_some()
}

/// 尝试从系统剪贴板读取一张图片。
pub async fn read_clipboard_image() -> Option<ClipboardImage> {
    if disabled() {
        return None;
    }
    if is_wsl()
        && which("powershell.exe")
        && let Some(image) = read_windows_image().await
    {
        return Some(image);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some()
        && which("wl-paste")
        && let Some(image) = read_wayland_image().await
    {
        return Some(image);
    }
    if std::env::var_os("DISPLAY").is_some() && which("xclip") {
        return read_x11_image().await;
    }
    if which("pngpaste") {
        let output = run("pngpaste", &["-"], None, LOCAL_TIMEOUT).await?;
        if !output.is_empty() {
            return Some(ClipboardImage {
                media_type: "image/png".into(),
                bytes: output,
            });
        }
        return None;
    }
    #[cfg(target_os = "macos")]
    return read_macos_image(MACOS_IMAGE_SCRIPT).await;
    #[cfg(not(target_os = "macos"))]
    None
}

#[cfg(target_os = "macos")]
async fn read_macos_image(script: &str) -> Option<ClipboardImage> {
    let file = tempfile::Builder::new()
        .prefix("moc-clipboard-")
        .suffix(".png")
        .tempfile()
        .ok()?;
    run(
        "/usr/bin/osascript",
        &["-e", script, file.path().to_str()?],
        None,
        LOCAL_TIMEOUT,
    )
    .await?;
    let bytes = tokio::fs::read(file.path()).await.ok()?;
    if !bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return None;
    }
    Some(ClipboardImage {
        media_type: "image/png".into(),
        bytes,
    })
}

/// 尝试从系统剪贴板读取纯文本；空内容返回 `None`。
pub async fn read_clipboard_text() -> Option<String> {
    if disabled() {
        return None;
    }
    if is_wsl() && which("powershell.exe") {
        let output = run(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_TEXT_SCRIPT,
            ],
            None,
            WINDOWS_TIMEOUT,
        )
        .await?;
        let text = String::from_utf8(output).ok()?;
        return (!text.is_empty()).then_some(text);
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-paste") {
        let output = run("wl-paste", &["--no-newline"], None, LOCAL_TIMEOUT).await?;
        let text = String::from_utf8(output).ok()?;
        return (!text.is_empty()).then_some(text);
    }
    if std::env::var_os("DISPLAY").is_some() && which("xclip") {
        let output = run(
            "xclip",
            &["-selection", "clipboard", "-o"],
            None,
            LOCAL_TIMEOUT,
        )
        .await?;
        let text = String::from_utf8(output).ok()?;
        return (!text.is_empty()).then_some(text);
    }
    if which("pbpaste") {
        let output = run("pbpaste", &[], None, LOCAL_TIMEOUT).await?;
        let text = String::from_utf8(output).ok()?;
        return (!text.is_empty()).then_some(text);
    }
    None
}

/// 复制文本到系统剪贴板（选择的复制通道；OSC52 由终端层另发）。
pub async fn copy_text_to_clipboard(text: &str) -> anyhow::Result<()> {
    if disabled() {
        anyhow::bail!("{}", t!("tui.err.no_clipboard_tool"))
    }
    if is_wsl() && which("powershell.exe") {
        run(
            "powershell.exe",
            &[
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                WINDOWS_COPY_SCRIPT,
            ],
            Some(base64_encode(text.as_bytes()).as_bytes()),
            WINDOWS_TIMEOUT,
        )
        .await
        .context(t!("tui.err.clipboard_write_win"))?;
        return Ok(());
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-copy") {
        run("wl-copy", &[], Some(text.as_bytes()), LOCAL_TIMEOUT)
            .await
            .context(t!("tui.err.wl_copy_spawn"))?;
        return Ok(());
    }
    if std::env::var_os("DISPLAY").is_some() && which("xclip") {
        run(
            "xclip",
            &["-selection", "clipboard"],
            Some(text.as_bytes()),
            LOCAL_TIMEOUT,
        )
        .await
        .context(t!("tui.err.xclip_spawn"))?;
        return Ok(());
    }
    if which("pbcopy") {
        run("pbcopy", &[], Some(text.as_bytes()), LOCAL_TIMEOUT)
            .await
            .context(t!("tui.err.pbcopy_spawn"))?;
        return Ok(());
    }
    anyhow::bail!("{}", t!("tui.err.no_clipboard_tool"))
}

/// 文本复制会走到的本地通道（与 [`copy_text_to_clipboard`] 同序判定；
/// OSC52 由终端层永远并写，不算在内）。`/doctor` 报告用。
pub fn local_channel() -> Option<&'static str> {
    if disabled() {
        return None;
    }
    if is_wsl() && which("powershell.exe") {
        return Some("powershell.exe");
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-copy") {
        return Some("wl-copy");
    }
    if std::env::var_os("DISPLAY").is_some() && which("xclip") {
        return Some("xclip");
    }
    if which("pbcopy") {
        return Some("pbcopy");
    }
    None
}

/// 通过终端写系统剪贴板的 OSC52 序列：SSH、容器或没装 wl-clipboard 时
/// 唯一可用的通道。由 TUI 在绘制线程写出（`\x1b]52;c;<base64>\x1b\\`）。
pub fn osc52_sequence(text: &str) -> String {
    format!("\x1b]52;c;{}\x1b\\", base64_encode(text.as_bytes()))
}

/// 读取 WSL 中的 Windows 剪贴板图片（PNG 重编码后 base64 回传）。
async fn read_windows_image() -> Option<ClipboardImage> {
    let output = run(
        "powershell.exe",
        &[
            "-NoProfile",
            "-NonInteractive",
            "-Sta",
            "-Command",
            WINDOWS_IMAGE_SCRIPT,
        ],
        None,
        WINDOWS_TIMEOUT,
    )
    .await?;
    let encoded = String::from_utf8(output).ok()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    if bytes.is_empty() {
        return None;
    }
    Some(ClipboardImage {
        media_type: "image/png".to_owned(),
        bytes,
    })
}

/// Wayland：按 `--list-types` 结果挑选优先级最高的图片 MIME 再取内容。
async fn read_wayland_image() -> Option<ClipboardImage> {
    let types = run("wl-paste", &["--list-types"], None, LOCAL_TIMEOUT).await?;
    let types = String::from_utf8(types).ok()?;
    let media_type = IMAGE_MIME_TYPES
        .iter()
        .find(|candidate| types.lines().any(|line| line.trim() == **candidate))?;
    let bytes = run("wl-paste", &["--type", media_type], None, LOCAL_TIMEOUT).await?;
    if bytes.is_empty() {
        return None;
    }
    Some(ClipboardImage {
        media_type: (*media_type).to_owned(),
        bytes,
    })
}

/// X11：xclip 没有列举目标格式的能力，按优先级逐个尝试。
async fn read_x11_image() -> Option<ClipboardImage> {
    for media_type in IMAGE_MIME_TYPES {
        let args = ["-selection", "clipboard", "-t", media_type, "-o"];
        let Some(bytes) = run("xclip", &args, None, LOCAL_TIMEOUT).await else {
            continue;
        };
        if bytes.is_empty() {
            continue;
        }
        return Some(ClipboardImage {
            media_type: media_type.to_owned(),
            bytes,
        });
    }
    None
}

/// 运行子进程并收集 stdout；非零退出码、超时或未找到程序都返回 `None`。
async fn run(
    program: &str,
    args: &[&str],
    stdin: Option<&[u8]>,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let mut command = Command::new(program);
    command.args(args);
    let mut child = command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        // 无载荷时接 /dev/null：PowerShell 会把重定向的 stdin 当作管道输入。
        .stdin(if stdin.is_some() {
            std::process::Stdio::piped()
        } else {
            std::process::Stdio::null()
        })
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    if let Some(payload) = stdin {
        let mut pipe = child.stdin.take()?;
        use tokio::io::AsyncWriteExt as _;
        let write = async {
            pipe.write_all(payload).await.ok();
            pipe.shutdown().await.ok();
        };
        tokio::time::timeout(timeout, write).await.ok()?;
    }
    let output = tokio::time::timeout(timeout, child.wait_with_output())
        .await
        .ok()?
        .ok()?;
    output.status.success().then_some(output.stdout)
}

/// 是否运行在 WSL：先看 WSL 注入的环境变量，再退回微软内核标识
/// （部分工具链会剥掉这些变量，只留内核）。
fn is_wsl() -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_some() || std::env::var_os("WSL_INTEROP").is_some() {
        return true;
    }
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .is_ok_and(|release| release.to_ascii_lowercase().contains("microsoft"))
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
    base64::engine::general_purpose::STANDARD.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_wraps_base64_payload() {
        assert_eq!(osc52_sequence("hi"), "\x1b]52;c;aGk=\x1b\\");
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_image_script_reads_png_without_changing_clipboard() {
        let mut png = std::io::Cursor::new(Vec::new());
        image::DynamicImage::new_rgba8(1, 1)
            .write_to(&mut png, image::ImageFormat::Png)
            .unwrap();
        let png = png.into_inner();
        let hex = png
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect::<String>();
        let script = MACOS_IMAGE_SCRIPT.replace(
            "the clipboard as «class PNGf»",
            &format!("«data PNGf{hex}»"),
        );
        let image = read_macos_image(&script).await.expect("PNG image");
        assert_eq!(image.media_type, "image/png");
        assert_eq!(image.bytes, png);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn macos_image_script_rejects_empty_invalid_and_failed_reads() {
        assert!(read_macos_image("return").await.is_none());
        let invalid = MACOS_IMAGE_SCRIPT.replace("the clipboard as «class PNGf»", "\"text\"");
        assert!(read_macos_image(&invalid).await.is_none());
        assert!(read_macos_image("error \"no image\"").await.is_none());
    }
}

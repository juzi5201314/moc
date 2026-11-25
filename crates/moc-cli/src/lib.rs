//! moc-cli：命令行入口。
//!
//! 行式与 ACP 前端日志写 stderr，避免污染 stdout；Web 前端落 `~/.moc/web.log`。

#![forbid(unsafe_code)]

pub mod args;
mod commands;
mod print;
mod render;
mod repl;
pub mod update;

pub use args::CliArgs;
pub use args::OutputFormat;

/// 以退出码标识的 `--print` 失败类别（权限拒绝 / 超出轮次等）：
/// 它穿过 anyhow 错误链，main 里 downcast 后按 `code` 退出进程。
/// 一般性错误仍是普通 anyhow 错误，退出码为 1。
#[derive(Debug)]
pub struct CliExit {
    pub code: i32,
    pub message: String,
}

impl std::fmt::Display for CliExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliExit {}

/// 前端形态：`moc`（默认 TUI / REPL / print）、`moc web`、`moc acp`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    Default,
    Web,
    Acp,
}

/// 单个 Web 日志文件的启动轮转上限（8 MiB，与 `moc-tui` 的 `tui.log` 对齐）。
const MAX_WEB_LOG_BYTES: u64 = 8 * 1024 * 1024;

pub async fn run(config: CliArgs, frontend: Frontend) -> anyhow::Result<()> {
    // 进程级资源采样（`/monitor` 的数据源）：所有前端形态共用同一个环形缓冲，
    // worker 子进程走独立入口不经过这里。
    moc_core::procstats::start();
    // TUI 有 moc-tui 自己的文件日志；Web 后台日志落 `~/.moc/web.log` 避免刷屏终端；
    // 行式前端（REPL / print / --list / ACP）保留 stderr 订阅者，否则内核告警会丢失。
    let web_log_path = if frontend == Frontend::Web {
        init_web_logging()
    } else {
        if frontend != Frontend::Default || !repl::use_tui(&config) {
            init_logging();
        }
        None
    };
    repl::run(config, frontend, web_log_path).await
}

/// 给 `moc web` 装文件日志订阅者，避免后台运行时日志刷屏占用终端。
///
/// 路径与 `moc-tui` 的 `tui.log` 对齐（`$MOC_HOME/web.log` 或 `~/.moc/web.log`）；
/// 默认级别 `info`，可用 `RUST_LOG` / `MOC_LOG` 调整。打开文件失败时退回 stderr，
/// 保证告警与错误不丢失。
fn init_web_logging() -> Option<std::path::PathBuf> {
    let path = web_log_path();
    let Ok(file) = open_rotated_log_file(&path, MAX_WEB_LOG_BYTES) else {
        init_logging();
        return None;
    };
    let _ = tracing_subscriber::fmt()
        .with_writer(std::sync::Arc::new(file))
        .with_ansi(false)
        .with_target(true)
        .with_max_level(log_level())
        .try_init();
    Some(path)
}

fn web_log_path() -> std::path::PathBuf {
    let dir = std::env::var_os("MOC_HOME")
        .filter(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| {
            std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .filter(|value| !value.is_empty())
                .map(|home| std::path::PathBuf::from(home).join(".moc"))
        })
        .unwrap_or_else(std::env::temp_dir);
    dir.join("web.log")
}

/// 打开追加写入的日志文件：父目录不存在时自动创建；现有文件达到上限时先清理轮转。
/// Unix 下以 `0600` 权限创建，避免诊断信息对同机其他用户可读。
fn open_rotated_log_file(path: &std::path::Path, max_bytes: u64) -> std::io::Result<std::fs::File> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    if std::fs::metadata(path)
        .ok()
        .is_some_and(|metadata| metadata.len() >= max_bytes)
    {
        let _ = std::fs::remove_file(path);
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.create(true).append(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    opts.open(path)
}

/// 给行式前端装 stderr 订阅者；已经装过（库被其它前端嵌入）就沿用。
///
/// 格式与 moc-tui 的文件日志一致（默认 fmt、带 target），但写 stderr 且默认 `info`：
/// 面向交互终端，DEBUG 会把会话输出淹掉，需要排查时用 `RUST_LOG` / `MOC_LOG` 提级。
fn init_logging() {
    use std::io::IsTerminal as _;

    let _ = tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(true)
        .with_max_level(log_level())
        .try_init();
}

/// 生效的日志级别：`RUST_LOG` / `MOC_LOG` 取首个可解析的值，默认 `info`。
fn log_level() -> tracing::Level {
    ["RUST_LOG", "MOC_LOG"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .find_map(|value| parse_level(&value))
        .unwrap_or(tracing::Level::INFO)
}

/// 解析级别名；无法识别（含空值、`off`）视为未设置。
fn parse_level(value: &str) -> Option<tracing::Level> {
    match value.trim().to_ascii_lowercase().as_str() {
        "trace" => Some(tracing::Level::TRACE),
        "debug" => Some(tracing::Level::DEBUG),
        "info" => Some(tracing::Level::INFO),
        "warn" | "warning" => Some(tracing::Level::WARN),
        "error" => Some(tracing::Level::ERROR),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 级别解析：认识的名字都认，未知值退回「未设置」（由调用方给默认值）。
    #[test]
    fn parses_log_level_names() {
        assert_eq!(parse_level("DEBUG"), Some(tracing::Level::DEBUG));
        assert_eq!(parse_level(" warn "), Some(tracing::Level::WARN));
        assert_eq!(parse_level("Trace"), Some(tracing::Level::TRACE));
        assert_eq!(parse_level("warning"), Some(tracing::Level::WARN));
        assert_eq!(parse_level("error"), Some(tracing::Level::ERROR));
        assert_eq!(parse_level(""), None);
        assert_eq!(parse_level("moc=debug"), None);
    }

    #[test]
    fn rotates_web_log_file_when_reaching_limit() {
        use std::io::Write as _;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("web.log");

        let mut file = open_rotated_log_file(&path, 16).unwrap();
        file.write_all(b"0123456789abcdef").unwrap();
        drop(file);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16);

        // 达到上限后再次打开会先轮转清空，再以追加方式写入新记录。
        let mut rotated = open_rotated_log_file(&path, 16).unwrap();
        rotated.write_all(b"fresh").unwrap();
        drop(rotated);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fresh");
    }
}

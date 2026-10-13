//! moc-cli：命令行入口。
//!
//! 日志只写 stderr：REPL / print 的 stdout 承载应答正文与工具输出，不能被日志污染。

#![forbid(unsafe_code)]

pub mod args;
mod render;
mod repl;

pub use args::CliArgs;

pub async fn run(config: CliArgs) -> anyhow::Result<()> {
    // TUI 有 moc-tui 自己的文件日志；行式前端（REPL / print / --list）此前没有订阅者，
    // 内核的 warn/error（例如 MCP 配置解析失败）会直接消失。
    if !repl::use_tui(&config) {
        init_logging();
    }
    repl::run(config).await
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
}

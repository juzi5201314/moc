use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use moc_protocol::methods::SessionMode;

#[derive(Debug, Parser)]
#[command(name = "moc", version, about = "MOC 编码代理")]
pub struct CliArgs {
    /// 启动时发送的提示词。
    pub prompt: Option<String>,
    /// 覆盖会话模型 selector。
    #[arg(long)]
    pub model: Option<String>,
    /// 新会话的工作模式。
    #[arg(long, value_enum, default_value_t = ModeArg::Normal)]
    pub mode: ModeArg,
    /// 会话工作目录。
    #[arg(long)]
    pub cwd: Option<PathBuf>,
    /// 追加配置覆盖层，可重复。
    #[arg(long = "config", value_name = "FILE", action = clap::ArgAction::Append)]
    pub config_files: Vec<PathBuf>,
    /// 恢复指定会话；不带 ID 时恢复当前目录下最近会话。
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "continue_session")]
    pub resume: Option<String>,
    /// 继续当前目录下最近会话。
    #[arg(long = "continue", conflicts_with = "resume")]
    pub continue_session: bool,
    /// 列出已保存会话。
    #[arg(long, conflicts_with = "models")]
    pub list: bool,
    /// 列出可用模型 selector。
    #[arg(long)]
    pub models: bool,
    /// 单次提示模式：可从 stdin 读取提示词，只打印最终答案。
    #[arg(long = "print")]
    pub print: bool,
    /// 强制使用行式 REPL（默认进入 TUI；无 TTY 时自动降级 REPL）。
    #[arg(long = "repl")]
    pub repl: bool,
    /// 行式／`--print` 输出不打印思维链增量（TUI 用 `--detail`）。
    #[arg(long)]
    pub hide_thinking: bool,
    /// 启动时的显示档位（思考与工具同时生效）。
    #[arg(long, value_enum)]
    pub detail: Option<DetailArg>,
    /// 覆盖会话存储目录。
    #[arg(long)]
    pub session_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum ModeArg {
    Normal,
    Plan,
    Ptc,
    ClmExperimental,
}

/// `--detail` 的三档取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DetailArg {
    Hidden,
    Brief,
    Full,
}

impl From<DetailArg> for moc_tui::DetailLevel {
    fn from(detail: DetailArg) -> Self {
        match detail {
            DetailArg::Hidden => Self::Hidden,
            DetailArg::Brief => Self::Brief,
            DetailArg::Full => Self::Full,
        }
    }
}

impl From<ModeArg> for SessionMode {
    fn from(mode: ModeArg) -> Self {
        match mode {
            ModeArg::Normal => Self::Normal,
            ModeArg::Plan => Self::Plan,
            ModeArg::Ptc => Self::Ptc,
            ModeArg::ClmExperimental => Self::ClmExperimental,
        }
    }
}

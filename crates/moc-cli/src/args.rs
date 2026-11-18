use std::path::PathBuf;

use clap::{Parser, ValueEnum};
use moc_protocol::methods::SessionEngine;
use rust_i18n::t;

#[derive(Debug, Parser)]
#[command(name = "moc", version, about = t!("cli.args.about").into_owned())]
pub struct CliArgs {
    /// 启动时发送的提示词。
    #[arg(help = t!("cli.args.prompt").into_owned())]
    pub prompt: Option<String>,
    /// 覆盖会话模型 selector。
    #[arg(long, help = t!("cli.args.model").into_owned())]
    pub model: Option<String>,
    /// 新会话的引擎（模型与工具的交互形态；跑过首条提示后锁定）。
    #[arg(long, value_enum, default_value_t = EngineArg::Standard, help = t!("cli.args.engine").into_owned())]
    pub engine: EngineArg,
    /// 新会话直接进入 plan 模式（可与任意引擎共存）。
    #[arg(long, help = t!("cli.args.plan").into_owned())]
    pub plan: bool,
    /// 会话工作目录。
    #[arg(long, help = t!("cli.args.cwd").into_owned())]
    pub cwd: Option<PathBuf>,
    /// 追加配置覆盖层，可重复。
    #[arg(long = "config", value_name = "FILE", action = clap::ArgAction::Append, help = t!("cli.args.config").into_owned())]
    pub config_files: Vec<PathBuf>,
    /// 恢复指定会话；不带 ID 时恢复当前目录下最近会话。
    #[arg(long, num_args = 0..=1, default_missing_value = "", conflicts_with = "continue_session", help = t!("cli.args.resume").into_owned())]
    pub resume: Option<String>,
    /// 继续当前目录下最近会话。
    #[arg(long = "continue", conflicts_with = "resume", help = t!("cli.args.continue").into_owned())]
    pub continue_session: bool,
    /// 列出已保存会话。
    #[arg(long, conflicts_with = "models", help = t!("cli.args.list").into_owned())]
    pub list: bool,
    /// 列出可用模型 selector。
    #[arg(long, help = t!("cli.args.models").into_owned())]
    pub models: bool,
    /// 单次提示模式：可从 stdin 读取提示词，只打印最终答案。
    #[arg(long = "print", help = t!("cli.args.print").into_owned())]
    pub print: bool,
    /// `--print` 的输出格式：text 只打印最终答案；json 结束时输出一个汇总对象；
    /// stream-json 把每条会话更新按行输出为 NDJSON，末行是汇总对象。
    #[arg(long, value_enum, default_value_t = OutputFormat::Text, requires = "print", help = t!("cli.args.output_format").into_owned())]
    pub output_format: OutputFormat,
    /// `--print` 下单次提示的模型请求轮数上限；超出后取消提示并以退出码 3 结束。
    #[arg(long, requires = "print", help = t!("cli.args.max_turns").into_owned())]
    pub max_turns: Option<u32>,
    /// `--print` 下自动批准这些工具的权限申请（逗号分隔，可重复）；`all` 放行全部。
    #[arg(long = "allow-tools", value_name = "TOOL", value_delimiter = ',', requires = "print", help = t!("cli.args.allow_tools").into_owned())]
    pub allow_tools: Vec<String>,
    /// 强制使用行式 REPL（默认进入 TUI；无 TTY 时自动降级 REPL）。
    #[arg(long = "repl", help = t!("cli.args.repl").into_owned())]
    pub repl: bool,
    /// 行式／`--print` 输出不打印思维链增量（TUI 用 `--detail`）。
    #[arg(long, help = t!("cli.args.hide_thinking").into_owned())]
    pub hide_thinking: bool,
    /// 启动时的显示档位（full 只展开思考，工具仍为简略）。
    #[arg(long, value_enum, help = t!("cli.args.detail").into_owned())]
    pub detail: Option<DetailArg>,
    /// 覆盖会话存储目录。
    #[arg(long, help = t!("cli.args.session_dir").into_owned())]
    pub session_dir: Option<PathBuf>,
    /// `moc web` 的监听端口；缺省优先 7432，被占用时改用随机端口。只绑定 127.0.0.1。
    #[arg(long, help = t!("cli.args.port").into_owned())]
    pub port: Option<u16>,
    /// `moc web` 的访问令牌；缺省随机生成并打印在启动信息里。
    #[arg(long, help = t!("cli.args.token").into_owned())]
    pub token: Option<String>,
    /// `moc web` 启动后不自动打开浏览器。
    #[arg(long, help = t!("cli.args.no_open").into_owned())]
    pub no_open: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum EngineArg {
    Standard,
    Ptc,
    #[value(name = "clm")]
    ClmExperimental,
}

/// `--detail` 的三档取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum DetailArg {
    Hidden,
    Brief,
    Full,
}

/// `--output-format` 的三档取值（仅 `--print` 有效）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// 只打印最终答案（原 `--print` 行为）。
    #[default]
    Text,
    /// 结束时输出一个 JSON 汇总对象。
    Json,
    /// 每条会话更新一行 NDJSON，末行输出与 json 相同的汇总对象。
    StreamJson,
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

impl From<EngineArg> for SessionEngine {
    fn from(engine: EngineArg) -> Self {
        match engine {
            EngineArg::Standard => Self::Standard,
            EngineArg::Ptc => Self::Ptc,
            EngineArg::ClmExperimental => Self::ClmExperimental,
        }
    }
}

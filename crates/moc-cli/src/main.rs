// bin crate 单独持有一份 catalog（t! 宏按 crate root 解析）。
rust_i18n::i18n!("../../locales", fallback = "zh");

use clap::Parser;
use rust_i18n::t;

fn main() -> anyhow::Result<()> {
    // 先用系统语言探测兜底：配置加载前的错误（含配置本身的错误）也能本地化；
    // Core::new 加载配置后再按 language 字段覆盖。
    moc_core::i18n::init("auto");
    let mut argv: Vec<String> = std::env::args().collect();
    // PTC worker 与 CLI 共用同一个可执行文件，但仍在独立子进程中运行。
    if argv.get(1).is_some_and(|arg| arg == "--ptc-worker") {
        return moc_ptc_worker::run_stdio().map_err(anyhow::Error::msg);
    }
    // workflow run 的独立 worker：一个 run 独占一个进程。
    if argv.get(1).is_some_and(|arg| arg == "--workflow-worker") {
        return moc_ptc_worker::run_workflow_stdio().map_err(anyhow::Error::msg);
    }
    if argv.get(1).is_some_and(|arg| arg == "update") {
        if argv.len() > 3 {
            anyhow::bail!("{}", t!("cli.update.usage"));
        }
        let version = argv.get(2).cloned();
        if matches!(version.as_deref(), Some("-h" | "--help")) {
            println!("{}", t!("cli.update.help"));
            return Ok(());
        }
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(moc_cli::update::run(version));
    }
    // 其余参数保持平铺；`moc web` / `moc acp` 只剥掉位置上的子命令名。
    let frontend = match argv.get(1).map(String::as_str) {
        Some("web") => Some(moc_cli::Frontend::Web),
        Some("acp") => Some(moc_cli::Frontend::Acp),
        _ => None,
    };
    if frontend.is_some() {
        argv.remove(1);
    }
    let frontend = frontend.unwrap_or(moc_cli::Frontend::Default);
    // worker 只需要同步 stdio，CLI 路径才创建 Tokio 运行时。
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(moc_cli::run(moc_cli::CliArgs::parse_from(argv), frontend))
}

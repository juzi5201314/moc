use clap::Parser;

fn main() -> anyhow::Result<()> {
    let mut argv: Vec<String> = std::env::args().collect();
    // PTC worker 与 CLI 共用同一个可执行文件，但仍在独立子进程中运行。
    if argv.get(1).is_some_and(|arg| arg == "--ptc-worker") {
        return moc_ptc_worker::run_stdio().map_err(anyhow::Error::msg);
    }
    if argv.get(1).is_some_and(|arg| arg == "update") {
        if argv.len() > 3 {
            anyhow::bail!("用法: moc update [版本]");
        }
        let version = argv.get(2).cloned();
        if matches!(version.as_deref(), Some("-h" | "--help")) {
            println!(
                "用法: moc update [版本]\n\n省略版本时更新到最新正式版；例如: moc update v2.2.1-rc.2"
            );
            return Ok(());
        }
        return tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?
            .block_on(moc_cli::update::run(version));
    }
    // 其余参数保持平铺；`moc web` 只剥掉位置上的子命令名。
    let web = argv.get(1).is_some_and(|arg| arg == "web");
    if web {
        argv.remove(1);
    }
    // worker 只需要同步 stdio，CLI 路径才创建 Tokio 运行时。
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(moc_cli::run(moc_cli::CliArgs::parse_from(argv), web))
}

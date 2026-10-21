use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let mut argv: Vec<String> = std::env::args().collect();
    // 其余参数保持平铺；`moc web` 只剥掉位置上的子命令名。
    let web = argv.get(1).is_some_and(|arg| arg == "web");
    if web {
        argv.remove(1);
    }
    moc_cli::run(moc_cli::CliArgs::parse_from(argv), web).await
}

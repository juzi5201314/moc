use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    if std::env::args().nth(1).as_deref() == Some("--ptc-worker") {
        return moc_core::tools::ptc::run_worker_stdio().map_err(anyhow::Error::msg);
    }
    moc_cli::run(moc_cli::CliArgs::parse()).await
}

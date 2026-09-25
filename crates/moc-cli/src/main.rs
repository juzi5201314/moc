use clap::Parser;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    moc_cli::run(moc_cli::CliArgs::parse()).await
}

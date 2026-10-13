#![forbid(unsafe_code)]

pub mod args;
mod render;
mod repl;

pub use args::CliArgs;

pub async fn run(config: CliArgs) -> anyhow::Result<()> {
    repl::run(config).await
}

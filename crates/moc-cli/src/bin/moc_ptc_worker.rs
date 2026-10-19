fn main() -> anyhow::Result<()> {
    moc_ptc_worker::run_stdio().map_err(anyhow::Error::msg)
}

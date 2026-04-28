use clap::Parser;
use mneme::cli::{Cli, dispatch};
use tracing_subscriber::{EnvFilter, fmt};

fn main() -> anyhow::Result<()> {
    init_logging();
    tracing::debug!("mneme {} starting", env!("CARGO_PKG_VERSION"));
    let cli = Cli::parse();
    dispatch(cli).map_err(Into::into)
}

fn init_logging() {
    let filter =
        EnvFilter::try_from_env("MNEME_LOG").unwrap_or_else(|_| EnvFilter::new("warn,mneme=info"));
    fmt()
        .with_env_filter(filter)
        .with_writer(std::io::stderr)
        .init();
}

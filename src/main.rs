mod cli;
mod config;
mod devcontainer;
mod docker;
mod host;
mod state;
mod ui;
mod workspace;

use anyhow::{Context, Result};

fn main() {
    if let Err(error) = run() {
        ui::error(&format!("{error:#}"));
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("Failed to initialize async runtime")?;

    runtime.block_on(cli::run())
}

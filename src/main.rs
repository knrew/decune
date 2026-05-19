mod cli;
mod config;
mod devcontainer;
mod docker;
mod down;
mod error;
mod host;
mod state;
mod ui;
mod up;
mod workspace;

use anyhow::Result;

use crate::error::ResultExt;

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
        .with_resource_context("initialize async runtime", "tokio runtime")?;

    runtime.block_on(cli::run())
}

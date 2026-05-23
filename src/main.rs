mod cli;
mod config;
mod devcontainer;
mod docker;
mod down;
mod error;
mod host;
mod state;
mod terminal;
mod ui;
mod up;
mod workspace;

use anyhow::Result;

use crate::error::ResultExt;

fn main() {
    let exit_code = match run() {
        Ok(exit_code) => exit_code,
        Err(error) => {
            ui::error(&format!("{error:#}"));
            1
        }
    };
    std::process::exit(exit_code);
}

fn run() -> Result<i32> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .with_resource_context("initialize async runtime", "tokio runtime")?;

    runtime.block_on(cli::run())
}

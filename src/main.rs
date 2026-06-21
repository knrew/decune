mod cli;
mod config;
mod devcontainer;
mod docker;
mod down;
mod error;
mod host;
mod ports;
mod runtime;
mod state;
mod terminal;
mod ui;
mod up;
mod version;
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
    if host::credentials::invoked_as_git_credential_helper() {
        host::credentials::run_git_credential_helper()?;
        return Ok(0);
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .with_resource_context("initialize async runtime", "tokio runtime")?;

    if host::forward::invoked_as_forward_agent() {
        runtime.block_on(host::forward::run_forward_agent())?;
        return Ok(0);
    }

    runtime.block_on(cli::run())
}

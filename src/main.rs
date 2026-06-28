#![allow(
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_possible_truncation,
    clippy::cognitive_complexity,
    clippy::create_dir,
    clippy::expect_used,
    clippy::filetype_is_file,
    clippy::future_not_send,
    clippy::get_unwrap,
    clippy::large_futures,
    clippy::large_stack_frames,
    clippy::mod_module_files,
    clippy::multiple_crate_versions,
    clippy::multiple_unsafe_ops_per_block,
    clippy::non_ascii_literal,
    clippy::panic_in_result_fn,
    clippy::similar_names,
    clippy::string_slice,
    clippy::suspicious_operation_groupings,
    clippy::too_many_lines,
    clippy::undocumented_unsafe_blocks,
    clippy::unwrap_in_result,
    clippy::wildcard_enum_match_arm,
    clippy::zero_sized_map_values,
    reason = "Temporary allow while strict clippy policy is introduced; code fixes will follow separately."
)]

mod clean;
mod cli;
mod config;
mod devcontainer;
mod docker;
mod down;
mod error;
mod hex;
mod host;
mod ports;
mod runtime;
mod state;
mod status;
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

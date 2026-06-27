#![allow(
    clippy::assigning_clones,
    clippy::case_sensitive_file_extension_comparisons,
    clippy::cast_lossless,
    clippy::cast_possible_truncation,
    clippy::clone_on_ref_ptr,
    clippy::cognitive_complexity,
    clippy::collection_is_never_read,
    clippy::create_dir,
    clippy::default_trait_access,
    clippy::elidable_lifetime_names,
    clippy::expect_used,
    clippy::filetype_is_file,
    clippy::fn_params_excessive_bools,
    clippy::format_collect,
    clippy::format_push_string,
    clippy::future_not_send,
    clippy::get_unwrap,
    clippy::if_not_else,
    clippy::implicit_clone,
    clippy::inconsistent_struct_constructor,
    clippy::large_futures,
    clippy::large_stack_frames,
    clippy::let_underscore_must_use,
    clippy::let_underscore_untyped,
    clippy::manual_let_else,
    clippy::manual_string_new,
    clippy::map_err_ignore,
    clippy::map_unwrap_or,
    clippy::match_same_arms,
    clippy::mod_module_files,
    clippy::multiple_crate_versions,
    clippy::multiple_unsafe_ops_per_block,
    clippy::needless_collect,
    clippy::needless_pass_by_value,
    clippy::non_ascii_literal,
    clippy::option_if_let_else,
    clippy::or_fun_call,
    clippy::panic_in_result_fn,
    clippy::redundant_clone,
    clippy::redundant_closure_for_method_calls,
    clippy::redundant_else,
    clippy::ref_option,
    clippy::similar_names,
    clippy::single_match_else,
    clippy::single_option_map,
    clippy::string_slice,
    clippy::struct_excessive_bools,
    clippy::struct_field_names,
    clippy::suspicious_operation_groupings,
    clippy::too_many_lines,
    clippy::trivially_copy_pass_by_ref,
    clippy::undocumented_unsafe_blocks,
    clippy::unnecessary_wraps,
    clippy::unused_async,
    clippy::unused_result_ok,
    clippy::unused_self,
    clippy::unwrap_in_result,
    clippy::use_self,
    clippy::useless_let_if_seq,
    clippy::verbose_file_reads,
    clippy::wildcard_enum_match_arm,
    clippy::wildcard_imports,
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

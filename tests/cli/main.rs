#![allow(
    clippy::tests_outside_test_module,
    clippy::too_many_lines,
    clippy::undocumented_unsafe_blocks,
    reason = "Temporary allow while strict clippy policy is introduced; code fixes will follow separately."
)]

#[path = "../support/mod.rs"]
pub(crate) mod support;

mod build;
mod clean;
mod compose;
mod compose_ci;
mod compose_integration;
mod dotfiles;
mod features;
mod git_credentials;
mod github_cli;
mod harness;
mod help;
mod host_git_config;
mod image_metadata;
mod install_script;
mod lifecycle;
mod mounts;
mod ports;
mod rebuild;
mod remove;
mod ssh_agent;
mod status;

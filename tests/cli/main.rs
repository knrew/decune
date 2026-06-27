#[cfg(test)]
#[path = "../support/support.rs"]
pub(crate) mod support;

#[cfg(test)]
mod build;
#[cfg(test)]
mod clean;
#[cfg(test)]
mod compose;
#[cfg(test)]
mod compose_ci;
#[cfg(test)]
mod compose_integration;
#[cfg(test)]
mod dotfiles;
#[cfg(test)]
mod features;
#[cfg(test)]
mod git_credentials;
#[cfg(test)]
mod github_cli;
#[cfg(test)]
mod harness;
#[cfg(test)]
mod help;
#[cfg(test)]
mod host_git_config;
#[cfg(test)]
mod image_metadata;
#[cfg(test)]
mod install_script;
#[cfg(test)]
mod lifecycle;
#[cfg(test)]
mod mounts;
#[cfg(test)]
mod ports;
#[cfg(test)]
mod rebuild;
#[cfg(test)]
mod remove;
#[cfg(test)]
mod ssh_agent;
#[cfg(test)]
mod status;

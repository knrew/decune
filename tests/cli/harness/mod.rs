use assert_cmd::Command;

pub(crate) use bollard::Docker;
pub(crate) use predicates::prelude::*;
pub(crate) use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
};

pub(crate) use crate::support;

mod docker;
mod features;
mod images;
mod locks;
mod names;

pub(crate) use docker::*;
pub(crate) use features::*;
pub(crate) use images::*;
pub(crate) use names::*;

const DECUNE_DOCKER_RESOURCE_LOCK_ENV: &str = "DECUNE_DOCKER_RESOURCE_LOCK";

pub(crate) fn decune() -> Command {
    let gh_config_dir =
        std::env::temp_dir().join(format!("decune-cli-test-empty-gh-{}", std::process::id()));
    std::fs::create_dir_all(&gh_config_dir).unwrap();

    let mut command = Command::cargo_bin("decune").unwrap();
    command
        .env("GH_CONFIG_DIR", gh_config_dir)
        .env(
            DECUNE_DOCKER_RESOURCE_LOCK_ENV,
            locks::docker_resource_lock_path(),
        )
        .env_remove("GH_TOKEN")
        .env_remove("GITHUB_TOKEN")
        .env_remove("GH_ENTERPRISE_TOKEN")
        .env_remove("GITHUB_ENTERPRISE_TOKEN");
    command
}

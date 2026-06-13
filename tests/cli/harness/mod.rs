use assert_cmd::Command;

pub(crate) use predicates::prelude::*;
pub(crate) use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
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

pub(crate) fn symlink_host_executable_into_path(name: &str, path_dir: &Path) -> PathBuf {
    let source = find_host_executable(name)
        .unwrap_or_else(|| panic!("host executable was not found in PATH: {name}"));
    let target = path_dir.join(name);
    std::os::unix::fs::symlink(&source, &target).unwrap_or_else(|error| {
        panic!(
            "failed to symlink host executable {} to {}: {error}",
            source.display(),
            target.display()
        )
    });
    target
}

fn find_host_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|path| path.join(name))
        .find(|candidate| candidate.is_file())
}

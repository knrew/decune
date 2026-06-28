use std::fmt::{Debug, Display};

use assert_cmd::Command;

pub(crate) use predicates::prelude::*;
pub(crate) use std::{
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::{Path, PathBuf},
};

pub(crate) use crate::support;

mod compose;
mod docker;
mod features;
mod images;
mod locks;
mod names;

pub(crate) use compose::*;
pub(crate) use docker::*;
pub(crate) use features::*;
pub(crate) use images::*;
pub(crate) use names::*;

const DECUNE_DOCKER_RESOURCE_LOCK_ENV: &str = "DECUNE_DOCKER_RESOURCE_LOCK";
const DECUNE_FAKE_COMPOSE_CAPABILITIES_ENV: &str = "DECUNE_FAKE_COMPOSE_CAPABILITIES";

// Clippy's allow-*-in-tests settings do not apply to shared integration-test
// helpers, so these keep setup failures as test failures without local allows.
pub(crate) trait TestUnwrap<T> {
    fn must(self) -> T;
    fn must_msg(self, message: impl Display) -> T;
}

impl<T, E> TestUnwrap<T> for Result<T, E>
where
    E: Debug,
{
    fn must(self) -> T {
        match self {
            Ok(value) => value,
            Err(error) => test_fail(format_args!("test helper failed: {error:?}")),
        }
    }

    fn must_msg(self, message: impl Display) -> T {
        match self {
            Ok(value) => value,
            Err(error) => test_fail(format_args!("{message}: {error:?}")),
        }
    }
}

impl<T> TestUnwrap<T> for Option<T> {
    fn must(self) -> T {
        let Some(value) = self else {
            test_fail("test helper value was missing");
        };
        value
    }

    fn must_msg(self, message: impl Display) -> T {
        let Some(value) = self else {
            test_fail(message);
        };
        value
    }
}

pub(crate) fn test_fail(message: impl Display) -> ! {
    let failed = true;
    assert!(!failed, "{message}");
    std::process::abort();
}

pub(crate) fn decune() -> Command {
    let gh_config_dir =
        std::env::temp_dir().join(format!("decune-cli-test-empty-gh-{}", std::process::id()));
    std::fs::create_dir_all(&gh_config_dir).must();

    let mut command = Command::cargo_bin("decune").must();
    command
        .env("GH_CONFIG_DIR", gh_config_dir)
        .env(
            DECUNE_FAKE_COMPOSE_CAPABILITIES_ENV,
            fake_compose_capabilities_script_path(),
        )
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
    let source = find_host_executable(name).must_msg(format_args!(
        "host executable was not found in PATH: {name}"
    ));
    let target = path_dir.join(name);
    std::os::unix::fs::symlink(&source, &target).must_msg(format_args!(
        "failed to symlink host executable {} to {}",
        source.display(),
        target.display()
    ));
    target
}

fn find_host_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|path| path.join(name))
        .find(|candidate| candidate.is_file())
}

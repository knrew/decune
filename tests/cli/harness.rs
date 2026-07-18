use std::{
    ffi::OsString,
    fmt::{Debug, Display},
    ops::{Deref, DerefMut},
};

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

pub fn acquire_exclusive_docker_resource_lock() -> anyhow::Result<impl Drop> {
    locks::acquire_exclusive_docker_resource_lock()
}

const DECUNE_DOCKER_RESOURCE_LOCK_ENV: &str = "DECUNE_DOCKER_RESOURCE_LOCK";
const DECUNE_FAKE_COMPOSE_CAPABILITIES_ENV: &str = "DECUNE_FAKE_COMPOSE_CAPABILITIES";

pub(crate) struct TestCommand {
    command: Command,
    gh_config: support::TempWorkspace,
}

impl Deref for TestCommand {
    type Target = Command;

    fn deref(&self) -> &Self::Target {
        &self.command
    }
}

impl DerefMut for TestCommand {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.command
    }
}

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

pub(crate) fn decune() -> TestCommand {
    let gh_config = support::TempWorkspace::new().must();
    let mut command = Command::cargo_bin("decune").must();
    command
        .env("GH_CONFIG_DIR", gh_config.path())
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
    TestCommand { command, gh_config }
}

pub fn write_multiple_dotfile_skeleton_sources(workspace: &support::TempWorkspace) {
    for tool in ["tool-a", "tool-b"] {
        workspace.create_dir(format!("dotfiles-repo/{tool}")).must();
        workspace.create_dir(format!("dotfiles-src/{tool}")).must();
        workspace
            .write_file(
                format!("dotfiles-repo/{tool}/{tool}-config.yml"),
                format!("{tool}-config\n"),
            )
            .must();
        workspace
            .write_file(
                format!("dotfiles-src/{tool}/{tool}-local.yml"),
                format!("{tool}-local\n"),
            )
            .must();
        std::os::unix::fs::symlink(
            workspace
                .path()
                .join(format!("dotfiles-repo/{tool}/{tool}-config.yml")),
            workspace
                .path()
                .join(format!("dotfiles-src/{tool}/{tool}-config.yml")),
        )
        .must();
    }
}

#[test]
fn decune_command_removes_gh_config_directory_on_drop() {
    let command = decune();
    let gh_config_path = command.gh_config.path().to_path_buf();
    assert!(gh_config_path.is_dir());

    drop(command);

    assert!(!gh_config_path.exists());
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

pub(crate) fn fake_command_path(
    workspace: &support::TempWorkspace,
    command_name: &str,
    fixture: &str,
) -> OsString {
    fake_path_with_commands(workspace, &[(command_name, fixture)])
}

pub(crate) fn fake_docker_path(workspace: &support::TempWorkspace, fixture: &str) -> OsString {
    fake_command_path(workspace, "docker", fixture)
}

pub(crate) fn fake_gh_path(workspace: &support::TempWorkspace, fixture: &str) -> OsString {
    fake_command_path(workspace, "gh", fixture)
}

pub(crate) fn fake_git_path(workspace: &support::TempWorkspace, fixture: &str) -> OsString {
    fake_command_path(workspace, "git", fixture)
}

pub(crate) fn fake_path_with_commands(
    workspace: &support::TempWorkspace,
    fixtures: &[(&str, &str)],
) -> OsString {
    let bin_dir = workspace.create_dir("bin").must();

    for (command_name, fixture) in fixtures {
        let destination = Path::new("bin").join(command_name);
        workspace
            .write_executable_fixture(destination, fixture)
            .must();
    }

    support::path_with_prepended(bin_dir).must()
}

pub(crate) fn fake_container_tools_bundle(workspace: &support::TempWorkspace) -> PathBuf {
    workspace
        .write_file("container-tools/linux-amd64/decune-forward-agent", b"agent")
        .must();
    workspace
        .write_file(
            "container-tools/linux-amd64/git-credential-decune",
            b"helper",
        )
        .must();
    workspace
        .write_file(
            "container-tools/manifest.json",
            FAKE_CONTAINER_TOOLS_MANIFEST,
        )
        .must();
    workspace.path().join("container-tools")
}

fn find_host_executable(name: &str) -> Option<PathBuf> {
    std::env::var_os("PATH")
        .into_iter()
        .flat_map(|path| std::env::split_paths(&path).collect::<Vec<_>>())
        .map(|path| path.join(name))
        .find(|candidate| candidate.is_file())
}

const FAKE_CONTAINER_TOOLS_MANIFEST: &str = r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"decune-forward-agent","platform":"linux-amd64","path":"linux-amd64/decune-forward-agent","sha256":"d4f0bc5a29de06b510f9aa428f1eedba926012b591fef7a518e776a7c9bd1824"},{"name":"git-credential-decune","platform":"linux-amd64","path":"linux-amd64/git-credential-decune","sha256":"e81d3b0e9d82feaaf5f6e55bdff24731d7eee08632ffa63801e6397290c5d20a"}]}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn support_fixture_template_and_executable_helpers_are_available_to_cli_tests() {
        let workspace = support::TempWorkspace::new().must();
        workspace
            .write_fixture_template(
                "rendered.txt",
                "cli/harness/template.txt",
                &[("__NAME__", "cli")],
            )
            .must();
        workspace
            .write_executable("bin/generated", "#!/bin/sh\nexit 0\n")
            .must();
        let fake_path = fake_command_path(&workspace, "hello", "cli/harness/hello.sh");
        let first_path = std::env::split_paths(&fake_path).next().must();

        assert_eq!(
            fs::read_to_string(workspace.path().join("rendered.txt")).must(),
            "name=cli\n"
        );
        assert_eq!(first_path, workspace.path().join("bin"));
        assert_eq!(
            fs::metadata(workspace.path().join("bin/generated"))
                .must()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            fs::metadata(workspace.path().join("bin/hello"))
                .must()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
    }
}

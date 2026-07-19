use std::{
    ffi::OsString,
    fmt::{Debug, Display, Write as _},
    net::TcpListener,
    ops::{Deref, DerefMut},
};

use assert_cmd::Command;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

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
const FAKE_CONTAINER_TOOL_NAMES: [&str; 3] =
    ["git-credential-decune", "decune-forward-agent", "decune"];
const FAKE_CONTAINER_TOOL_PLATFORMS: [&str; 2] = ["linux-amd64", "linux-arm64"];
const FAKE_CONTAINER_TOOLS_PROTOCOL_VERSION: u32 = 1;
const FAKE_CONTAINER_TOOLS_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
struct FakeContainerToolsManifest {
    schema_version: u32,
    protocol_version: u32,
    tools: Vec<FakeContainerToolsManifestEntry>,
}

#[derive(Debug, Deserialize, Serialize)]
struct FakeContainerToolsManifestEntry {
    name: String,
    platform: String,
    path: String,
    sha256: String,
}

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

pub fn available_localhost_ports<const COUNT: usize>() -> [u16; COUNT] {
    let listeners: [TcpListener; COUNT] =
        std::array::from_fn(|_| TcpListener::bind(("127.0.0.1", 0)).must());
    listeners
        .each_ref()
        .map(|listener| listener.local_addr().must().port())
}

pub fn available_localhost_port() -> u16 {
    let [port] = available_localhost_ports();
    port
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

#[test]
fn available_localhost_ports_are_distinct() {
    let [first, second, third] = available_localhost_ports();
    assert_ne!(first, second);
    assert_ne!(first, third);
    assert_ne!(second, third);
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
    let mut tools =
        Vec::with_capacity(FAKE_CONTAINER_TOOL_NAMES.len() * FAKE_CONTAINER_TOOL_PLATFORMS.len());
    let mut sums = String::new();

    for platform in FAKE_CONTAINER_TOOL_PLATFORMS {
        for name in FAKE_CONTAINER_TOOL_NAMES {
            let contents = format!("fake {name} for {platform}\n");
            let relative_path = PathBuf::from(platform).join(name);
            let sha256 = names::hex_lower(&Sha256::digest(contents.as_bytes()));
            workspace
                .write_executable(
                    Path::new("container-tools").join(&relative_path),
                    contents.as_bytes(),
                )
                .must();
            writeln!(sums, "{sha256}  {}", relative_path.display()).must();
            tools.push(FakeContainerToolsManifestEntry {
                name: name.to_owned(),
                platform: platform.to_owned(),
                path: relative_path.to_string_lossy().into_owned(),
                sha256,
            });
        }
    }

    let manifest = FakeContainerToolsManifest {
        schema_version: FAKE_CONTAINER_TOOLS_SCHEMA_VERSION,
        protocol_version: FAKE_CONTAINER_TOOLS_PROTOCOL_VERSION,
        tools,
    };
    let manifest = serde_json::to_string(&manifest).must();
    workspace
        .write_file("container-tools/manifest.json", format!("{manifest}\n"))
        .must();
    workspace
        .write_file("container-tools/SHA256SUMS", sums)
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

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;

    #[test]
    fn fake_container_tools_bundle_matches_complete_external_contract() {
        let workspace = support::TempWorkspace::new().must();
        let bundle = fake_container_tools_bundle(&workspace);
        let manifest: FakeContainerToolsManifest =
            serde_json::from_slice(&fs::read(bundle.join("manifest.json")).must()).must();
        let sums = fs::read_to_string(bundle.join("SHA256SUMS")).must();
        let sum_lines = sums.lines().count();
        let sums = sums
            .lines()
            .map(|line| {
                let (sha256, path) = line.split_once("  ").must();
                (path.to_owned(), sha256.to_owned())
            })
            .collect::<BTreeMap<_, _>>();
        let expected_tools = FAKE_CONTAINER_TOOL_PLATFORMS
            .into_iter()
            .flat_map(|platform| {
                FAKE_CONTAINER_TOOL_NAMES
                    .into_iter()
                    .map(move |name| (name, platform))
            })
            .collect::<BTreeSet<_>>();
        let actual_tools = manifest
            .tools
            .iter()
            .map(|entry| (entry.name.as_str(), entry.platform.as_str()))
            .collect::<BTreeSet<_>>();

        assert_eq!(manifest.schema_version, FAKE_CONTAINER_TOOLS_SCHEMA_VERSION);
        assert_eq!(
            manifest.protocol_version,
            FAKE_CONTAINER_TOOLS_PROTOCOL_VERSION
        );
        assert_eq!(manifest.tools.len(), expected_tools.len());
        assert_eq!(actual_tools, expected_tools);
        assert_eq!(sum_lines, expected_tools.len());
        assert_eq!(sums.len(), expected_tools.len());

        for entry in manifest.tools {
            let relative_path = PathBuf::from(&entry.platform).join(&entry.name);
            let artifact = bundle.join(&relative_path);
            let metadata = fs::metadata(&artifact).must();
            let contents = fs::read(&artifact).must();
            let sha256 = names::hex_lower(&Sha256::digest(&contents));

            assert_eq!(Path::new(&entry.path), relative_path);
            assert!(metadata.is_file());
            assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
            assert_eq!(entry.sha256, sha256);
            assert_eq!(sums.get(&entry.path), Some(&entry.sha256));
        }
    }

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

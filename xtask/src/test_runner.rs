use std::{path::Path, process::Command};

use anyhow::{Context, Result, bail};

use crate::{
    command::{ChildCommand, cargo_command_with_container_tools, run_command, run_command_spec},
    container_tools::prepare_xtask_container_tools_bundle,
};

const COMPOSE_CAPABILITIES: [ComposeCapabilityRequirement; 8] = [
    ComposeCapabilityRequirement {
        subcommand: "config",
        option: "--format",
        capability: "docker compose config --format json",
    },
    ComposeCapabilityRequirement {
        subcommand: "ps",
        option: "--format",
        capability: "docker compose ps --format json",
    },
    ComposeCapabilityRequirement {
        subcommand: "build",
        option: "--with-dependencies",
        capability: "docker compose build --with-dependencies",
    },
    ComposeCapabilityRequirement {
        subcommand: "pull",
        option: "--policy",
        capability: "docker compose pull --policy always",
    },
    ComposeCapabilityRequirement {
        subcommand: "pull",
        option: "--ignore-buildable",
        capability: "docker compose pull --ignore-buildable",
    },
    ComposeCapabilityRequirement {
        subcommand: "pull",
        option: "--include-deps",
        capability: "docker compose pull --include-deps",
    },
    ComposeCapabilityRequirement {
        subcommand: "up",
        option: "--force-recreate",
        capability: "docker compose up --force-recreate",
    },
    ComposeCapabilityRequirement {
        subcommand: "up",
        option: "--remove-orphans",
        capability: "docker compose up --remove-orphans",
    },
];

#[derive(Debug, Clone, Copy)]
struct ComposeCapabilityRequirement {
    subcommand: &'static str,
    option: &'static str,
    capability: &'static str,
}

pub(crate) fn compose_integration(workspace: &Path, release: bool) -> Result<()> {
    let mut docker_version = Command::new("docker");
    docker_version.arg("version");
    run_command(
        docker_version,
        "Docker CLI is required for Docker Compose integration tests",
    )?;
    compose_integration_preflight()?;

    let bundle_dir = prepare_xtask_container_tools_bundle(workspace, true)?;
    let command = compose_integration_cargo_command(workspace, release, &bundle_dir);

    run_command_spec(command, "Failed to run Docker Compose integration tests")
}

fn compose_integration_preflight() -> Result<()> {
    let version = docker_output_text(&["compose", "version"])
        .context("Docker Compose v2 plugin is required for Docker Compose integration tests")?;
    let version_short = docker_output_text(&["compose", "version", "--short"])
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| version.trim().to_owned());
    eprintln!("Docker Compose version: {version_short}");

    for requirement in COMPOSE_CAPABILITIES {
        let help = docker_output_text(&["compose", requirement.subcommand, "--help"])
            .with_context(|| {
                format!(
                    "Failed to probe Docker Compose capability: {}",
                    requirement.capability
                )
            })?;
        if !help_contains_option(&help, requirement.option) {
            bail!(
                "Docker Compose v2 plugin is missing required capability: {} ({} --help does not list {}). Update Docker Compose v2 plugin to a newer release.",
                requirement.capability,
                requirement.subcommand,
                requirement.option
            );
        }
        eprintln!("Docker Compose capability OK: {}", requirement.capability);
    }

    Ok(())
}

pub(crate) fn workspace_test(workspace: &Path, release: bool) -> Result<()> {
    let bundle_dir = prepare_xtask_container_tools_bundle(workspace, true)?;
    let command = workspace_test_cargo_command(workspace, release, &bundle_dir);

    run_command_spec(command, "Failed to run workspace tests")
}

fn compose_integration_cargo_command(
    workspace: &Path,
    release: bool,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).arg("test");
    if release {
        command = command.arg("--release");
    }
    command.args([
        "--workspace",
        "--all-features",
        "--no-fail-fast",
        "compose_integration",
        "--",
        "--ignored",
        "--test-threads=1",
    ])
}

fn workspace_test_cargo_command(
    workspace: &Path,
    release: bool,
    bundle_dir: &Path,
) -> ChildCommand {
    let mut command = cargo_command_with_container_tools(workspace, bundle_dir).arg("test");
    if release {
        command = command.arg("--release");
    }
    command.args([
        "--workspace",
        "--all-features",
        "--no-fail-fast",
        "--verbose",
    ])
}

fn docker_output_text(args: &[&str]) -> Result<String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .with_context(|| format!("failed to spawn docker {}", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "docker {} exited with {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
    if !output.stderr.is_empty() {
        text.push('\n');
        text.push_str(&String::from_utf8_lossy(&output.stderr));
    }
    Ok(text)
}

fn help_contains_option(help: &str, option: &str) -> bool {
    help.split(|ch: char| {
        ch.is_ascii_whitespace() || matches!(ch, ',' | ';' | '[' | ']' | '(' | ')' | '{' | '}')
    })
    .any(|token| token == option || token.starts_with(&format!("{option}=")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::container_tools::default_xtask_container_tools_bundle_dir;

    #[test]
    fn compose_integration_cargo_command_runs_ignored_tests_with_prepared_bundle() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);

        let command = compose_integration_cargo_command(workspace, false, &bundle_dir);

        assert_eq!(command.program, "cargo");
        assert_eq!(command.current_dir.as_deref(), Some(workspace));
        assert_eq!(
            command.args,
            [
                "test",
                "--workspace",
                "--all-features",
                "--no-fail-fast",
                "compose_integration",
                "--",
                "--ignored",
                "--test-threads=1",
            ]
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&bundle_dir.as_os_str().to_owned())
        );
        assert!(!command.env.contains_key("DECUNE_COMPOSE_INTEGRATION"));
    }

    #[test]
    fn workspace_test_cargo_command_uses_prepared_bundle_dir() {
        let workspace = Path::new("/workspace/decune");
        let bundle_dir = default_xtask_container_tools_bundle_dir(workspace);

        let command = workspace_test_cargo_command(workspace, true, &bundle_dir);

        assert_eq!(
            command.args,
            [
                "test",
                "--release",
                "--workspace",
                "--all-features",
                "--no-fail-fast",
                "--verbose",
            ]
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE"),
            Some(&std::ffi::OsString::from("required"))
        );
        assert_eq!(
            command.env.get("DECUNE_CONTAINER_TOOLS_BUNDLE_DIR"),
            Some(&bundle_dir.as_os_str().to_owned())
        );
        assert!(!command.env.contains_key("DECUNE_COMPOSE_INTEGRATION"));
    }
}

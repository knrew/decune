use std::{collections::BTreeMap, path::Path};

use anyhow::Result;

use crate::{
    docker::exec::ExecCommandSpec,
    host::{
        container_tools::{
            ContainerTool, ContainerToolPlatform, remove_container_tool, stage_container_tool,
        },
        runtime::prepare_private_runtime_dir,
    },
    runtime::docker_cli::DockerCli,
    ui,
};

pub(crate) const CONTAINER_CLI_TARGET: &str = "/run/decune/decune";
pub(crate) const CONTAINER_CLI_SYMLINK: &str = "/usr/local/bin/decune";
const CONTAINER_CLI_SYMLINK_PARENT: &str = "/usr/local/bin";

const RECONCILE_CONTAINER_CLI_SYMLINK_SCRIPT: &str = r#"mode=$1
destination=$2
target=$3
parent=$4

finish() {
    printf '%s\n' "$1"
    exit 0
}

classify_enabled_destination() {
    if [ -L "$destination" ]; then
        current=$(readlink "$destination" 2>/dev/null) || finish inspect_failed
        if [ "$current" = "$target" ]; then
            finish ready
        fi
        finish collision_symlink
    fi
    if [ -f "$destination" ]; then
        finish collision_regular
    fi
    if [ -d "$destination" ]; then
        finish collision_directory
    fi
    if [ -e "$destination" ]; then
        finish collision_other
    fi
    return 1
}

if [ "$mode" = enabled ]; then
    classify_enabled_destination
    if [ ! -d "$parent" ]; then
        if [ -e "$parent" ] || [ -L "$parent" ]; then
            finish parent_not_directory
        fi
        mkdir -p "$parent" 2>/dev/null || finish parent_create_failed
    fi
    classify_enabled_destination
    ln -s "$target" "$destination" 2>/dev/null && finish installed
    classify_enabled_destination
    finish install_failed
fi

if [ -L "$destination" ]; then
    current=$(readlink "$destination" 2>/dev/null) || finish inspect_failed
    if [ "$current" = "$target" ]; then
        rm "$destination" 2>/dev/null && finish removed
        finish remove_failed
    fi
fi
finish unchanged
"#;

pub(crate) fn prepare_container_cli_artifact(
    enabled: bool,
    platform: ContainerToolPlatform,
    runtime_dir: &Path,
) -> Result<()> {
    prepare_private_runtime_dir(runtime_dir, "decune container CLI")?;
    if enabled {
        stage_container_tool(ContainerTool::Decune, platform, runtime_dir)?;
    } else {
        remove_container_tool(ContainerTool::Decune, runtime_dir)?;
    }
    Ok(())
}

pub(crate) async fn reconcile_container_cli_symlink(
    cli: &DockerCli,
    container: &str,
    enabled: bool,
) {
    if let Some(warning) = reconcile_container_cli_symlink_warning(cli, container, enabled).await {
        ui::warn(&warning);
    }
}

async fn reconcile_container_cli_symlink_warning(
    cli: &DockerCli,
    container: &str,
    enabled: bool,
) -> Option<String> {
    let Ok(output) = cli
        .exec_capture(container, &container_cli_symlink_command(enabled))
        .await
    else {
        return Some(container_cli_symlink_warning(enabled, "exec_failed"));
    };
    if output.exit_code != 0 {
        return Some(container_cli_symlink_warning(enabled, "exec_failed"));
    }
    let Ok(status) = std::str::from_utf8(&output.stdout) else {
        return Some(container_cli_symlink_warning(enabled, "invalid_result"));
    };
    match status.trim() {
        "ready" | "installed" | "removed" | "unchanged" => None,
        status => Some(container_cli_symlink_warning(enabled, status)),
    }
}

fn container_cli_symlink_command(enabled: bool) -> ExecCommandSpec {
    ExecCommandSpec {
        command: vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            RECONCILE_CONTAINER_CLI_SYMLINK_SCRIPT.to_owned(),
            "decune-container-cli-symlink".to_owned(),
            if enabled { "enabled" } else { "disabled" }.to_owned(),
            CONTAINER_CLI_SYMLINK.to_owned(),
            CONTAINER_CLI_TARGET.to_owned(),
            CONTAINER_CLI_SYMLINK_PARENT.to_owned(),
        ],
        user: Some("0".to_owned()),
        working_dir: None,
        env: BTreeMap::new(),
        redactions: Vec::new(),
        tty: false,
    }
}

fn container_cli_symlink_warning(enabled: bool, status: &str) -> String {
    let action = if enabled {
        "install or update"
    } else {
        "remove"
    };
    let reason = match status {
        "collision_symlink" => "the destination is another or broken symlink",
        "collision_regular" => "the destination is an existing regular file",
        "collision_directory" => "the destination is an existing directory",
        "collision_other" => "the destination is an existing non-regular filesystem entry",
        "parent_not_directory" => "`/usr/local/bin` is not a directory",
        "parent_create_failed" => "`/usr/local/bin` could not be created",
        "install_failed" => "the container root filesystem did not allow the symlink to be created",
        "remove_failed" => {
            "the container root filesystem did not allow the managed symlink to be removed"
        }
        "inspect_failed" => "the existing symlink target could not be inspected",
        "invalid_result" => "the container setup command returned an invalid result",
        _ => "the container setup command could not be executed",
    };
    format!(
        "Could not {action} {CONTAINER_CLI_SYMLINK} because {reason}. Any existing destination was left unchanged. Direct command: {CONTAINER_CLI_TARGET}"
    )
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
        path::Path,
        process::Command,
        sync::Arc,
    };

    use tempfile::TempDir;

    use super::{
        CONTAINER_CLI_TARGET, RECONCILE_CONTAINER_CLI_SYMLINK_SCRIPT,
        container_cli_symlink_warning, prepare_container_cli_artifact,
        reconcile_container_cli_symlink_warning,
    };
    use crate::{
        host::container_tools::ContainerToolPlatform,
        runtime::{
            command::{FakeRuntimeCommand, RuntimeOutput},
            docker_cli::DockerCli,
        },
    };

    #[test]
    fn container_cli_artifact_is_staged_persistently_and_removed_when_disabled() {
        let temp = TempDir::new().unwrap();
        let runtime_dir = temp.path().join("runtime");
        prepare_container_cli_artifact(true, ContainerToolPlatform::LinuxAmd64, &runtime_dir)
            .unwrap();
        let metadata = fs::metadata(runtime_dir.join("decune")).unwrap();
        assert!(metadata.len() > 0);
        assert_eq!(metadata.permissions().mode() & 0o777, 0o755);
        assert!(runtime_dir.join("decune").is_file());

        prepare_container_cli_artifact(false, ContainerToolPlatform::LinuxAmd64, &runtime_dir)
            .unwrap();

        assert!(runtime_dir.join("decune").symlink_metadata().is_err());
    }

    #[test]
    fn enabled_reconciliation_accepts_exact_target_symlink_when_target_is_missing() {
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("usr/local/bin");
        let destination = parent.join("decune");
        let target = temp.path().join("run/decune/decune");
        fs::create_dir_all(&parent).unwrap();
        symlink(&target, &destination).unwrap();

        let status = run_reconciliation_script(true, &destination, &target, &parent);

        assert_eq!(status, "ready");
        assert_eq!(fs::read_link(destination).unwrap(), target);
    }

    #[test]
    fn enabled_symlink_reconciliation_preserves_collision_matrix() {
        for case in [
            "missing",
            "correct",
            "wrong-symlink",
            "broken-symlink",
            "regular",
            "directory",
        ] {
            let temp = TempDir::new().unwrap();
            let parent = temp.path().join("usr/local/bin");
            let destination = parent.join("decune");
            let target = temp.path().join("run/decune/decune");
            fs::create_dir_all(&parent).unwrap();
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, b"cli").unwrap();
            match case {
                "missing" => {}
                "correct" => symlink(&target, &destination).unwrap(),
                "wrong-symlink" => {
                    let other = temp.path().join("other");
                    fs::write(&other, b"other").unwrap();
                    symlink(&other, &destination).unwrap();
                }
                "broken-symlink" => {
                    symlink(temp.path().join("missing"), &destination).unwrap();
                }
                "regular" => fs::write(&destination, b"existing").unwrap(),
                "directory" => fs::create_dir_all(&destination).unwrap(),
                _ => unreachable!(),
            }

            let status = run_reconciliation_script(true, &destination, &target, &parent);

            match case {
                "missing" => {
                    assert_eq!(status, "installed");
                    assert_eq!(fs::read_link(&destination).unwrap(), target);
                }
                "correct" => assert_eq!(status, "ready"),
                "wrong-symlink" | "broken-symlink" => {
                    assert_eq!(status, "collision_symlink");
                }
                "regular" => {
                    assert_eq!(status, "collision_regular");
                    assert_eq!(fs::read(&destination).unwrap(), b"existing");
                }
                "directory" => {
                    assert_eq!(status, "collision_directory");
                    assert!(destination.is_dir());
                }
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn disabled_symlink_reconciliation_removes_only_exact_target() {
        for case in [
            "exact",
            "wrong-symlink",
            "broken-symlink",
            "regular",
            "directory",
        ] {
            let temp = TempDir::new().unwrap();
            let parent = temp.path().join("usr/local/bin");
            let destination = parent.join("decune");
            let target = temp.path().join("run/decune/decune");
            fs::create_dir_all(&parent).unwrap();
            match case {
                "exact" => symlink(&target, &destination).unwrap(),
                "wrong-symlink" => {
                    let other = temp.path().join("other");
                    fs::write(&other, b"other").unwrap();
                    symlink(&other, &destination).unwrap();
                }
                "broken-symlink" => {
                    symlink(temp.path().join("missing"), &destination).unwrap();
                }
                "regular" => fs::write(&destination, b"existing").unwrap(),
                "directory" => fs::create_dir_all(&destination).unwrap(),
                _ => unreachable!(),
            }

            let status = run_reconciliation_script(false, &destination, &target, &parent);

            if case == "exact" {
                assert_eq!(status, "removed");
                assert!(destination.symlink_metadata().is_err());
            } else {
                assert_eq!(status, "unchanged");
                assert!(destination.symlink_metadata().is_ok());
            }
        }
    }

    #[test]
    fn enabled_reconciliation_creates_missing_parent() {
        let temp = TempDir::new().unwrap();
        let parent = temp.path().join("usr/local/bin");
        let destination = parent.join("decune");
        let target = temp.path().join("run/decune/decune");

        let status = run_reconciliation_script(true, &destination, &target, &parent);

        assert_eq!(status, "installed");
        assert_eq!(fs::read_link(destination).unwrap(), target);
    }

    #[test]
    fn fake_exec_degrades_collisions_read_only_and_write_failures_to_sanitized_warnings() {
        for (enabled, status, expected) in [
            (true, "collision_regular", "existing regular file"),
            (true, "parent_create_failed", "could not be created"),
            (true, "install_failed", "root filesystem"),
            (false, "remove_failed", "managed symlink"),
        ] {
            let runner = FakeRuntimeCommand::new(vec![Ok(RuntimeOutput {
                stdout: format!("{status}\n").into_bytes(),
                stderr: b"ignored host path: /private/workspace".to_vec(),
                exit_code: 0,
            })]);
            let cli = DockerCli::new(Arc::new(runner.clone()));
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();

            let warning = runtime
                .block_on(reconcile_container_cli_symlink_warning(
                    &cli, "primary", enabled,
                ))
                .unwrap();

            assert!(warning.contains(expected), "{warning}");
            assert!(warning.contains("Any existing destination was left unchanged"));
            assert!(warning.contains(CONTAINER_CLI_TARGET));
            assert!(!warning.contains("/private/workspace"));
            let commands = runner.commands();
            assert_eq!(commands.len(), 1);
            assert_eq!(commands[0].program(), "docker");
            assert!(
                commands[0]
                    .args_vec()
                    .windows(2)
                    .any(|args| { args == ["--user", "0"] })
            );
        }

        let warning = container_cli_symlink_warning(true, "unknown");
        assert!(warning.contains("could not be executed"));
    }

    fn run_reconciliation_script(
        enabled: bool,
        destination: &Path,
        target: &Path,
        parent: &Path,
    ) -> String {
        let output = Command::new("/bin/sh")
            .args([
                "-c",
                RECONCILE_CONTAINER_CLI_SYMLINK_SCRIPT,
                "decune-container-cli-symlink",
                if enabled { "enabled" } else { "disabled" },
            ])
            .arg(destination)
            .arg(target)
            .arg(parent)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    }
}

use assert_cmd::Command;
use bollard::{
    Docker,
    models::ContainerSummary,
    query_parameters::{ListContainersOptionsBuilder, RemoveContainerOptionsBuilder},
};
use predicates::prelude::*;
use std::{collections::HashMap, path::Path};

mod support;

fn decune() -> Command {
    Command::cargo_bin("decune").unwrap()
}

#[test]
fn root_help_is_displayed() {
    decune()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Run dev containers from the command line.",
        ))
        .stdout(predicate::str::contains("up"))
        .stdout(predicate::str::contains("down"))
        .stdout(predicate::str::contains("clean"))
        .stdout(predicate::str::contains("rebuild"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn command_help_is_displayed() {
    for command in ["up", "down", "clean", "rebuild"] {
        decune()
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Workspace directory"))
            .stdout(predicate::str::contains("WORKSPACE"))
            .stderr(predicate::str::is_empty());
    }
}

#[test]
fn commands_fail_with_not_implemented_error() {
    for command in ["down", "clean", "rebuild"] {
        decune()
            .arg(command)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Error:"))
            .stderr(predicate::str::contains("not implemented"));
    }
}

#[test]
fn up_without_detach_reports_shell_attach_not_implemented() {
    decune()
        .arg("up")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Shell attach is not implemented yet",
        ))
        .stderr(predicate::str::contains("--detach"));
}

#[test]
fn up_detach_creates_and_reuses_image_container_when_docker_tests_are_enabled() {
    if support::skip_unless_docker_tests_enabled() {
        return;
    }

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20"
            }
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state.to_string() == "running")
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

async fn workspace_containers(workspace_root: &Path) -> anyhow::Result<Vec<ContainerSummary>> {
    let docker = Docker::connect_with_defaults()?;
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![
            "decune.managed=true".to_owned(),
            format!("decune.workspace={}", workspace_root.display()),
        ],
    );
    let options = ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build();

    Ok(docker.list_containers(Some(options)).await?)
}

async fn cleanup_workspace_containers(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let containers = workspace_containers(workspace_root).await?;
    let options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();

    for container in containers {
        if let Some(id) = container.id {
            docker.remove_container(&id, Some(options.clone())).await?;
        }
    }

    Ok(())
}

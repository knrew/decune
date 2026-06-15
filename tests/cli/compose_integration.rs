use std::{path::Path, process::Command};

use serde::Deserialize;

use crate::harness::*;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ComposeIntegrationDecision {
    Run,
    Error(String),
}

#[derive(Debug, Deserialize)]
struct ComposeContainer {
    #[serde(rename = "ID")]
    id: String,
    #[serde(rename = "State")]
    state: String,
    #[serde(rename = "Labels")]
    labels: String,
}

struct ComposeFixtureWorkspace {
    workspace: support::TempWorkspace,
}

impl ComposeFixtureWorkspace {
    fn path(&self) -> &Path {
        self.workspace.path()
    }
}

impl Drop for ComposeFixtureWorkspace {
    fn drop(&mut self) {
        cleanup_compose_workspace(self.workspace.path());
    }
}

struct UnrelatedComposeFixture {
    container_name: String,
    image: String,
}

impl Drop for UnrelatedComposeFixture {
    fn drop(&mut self) {
        let _ = docker_status(["rm", "--force", "--volumes", &self.container_name]);
        let _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
    }
}

#[test]
fn compose_integration_plugin_detection_runs_when_tools_are_available() {
    assert_eq!(
        compose_integration_decision(Ok(()), Ok(())),
        ComposeIntegrationDecision::Run
    );
}

#[test]
fn compose_integration_plugin_detection_errors_when_docker_is_missing() {
    assert_eq!(
        compose_integration_decision(Err("docker executable was not found".to_owned()), Ok(())),
        ComposeIntegrationDecision::Error(
            "Docker Compose integration tests require Docker CLI: docker executable was not found"
                .to_owned()
        )
    );
}

#[test]
fn compose_integration_plugin_detection_errors_when_compose_v2_plugin_is_missing() {
    assert_eq!(
        compose_integration_decision(
            Ok(()),
            Err("docker compose version exited with 1".to_owned())
        ),
        ComposeIntegrationDecision::Error(
            "Docker Compose integration tests require Docker Compose v2 plugin: docker compose version exited with 1"
                .to_owned()
        )
    );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_minimal_single_service_up_detach() {
    let workspace = compose_fixture_workspace("minimal");

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app"]);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_multi_file_merge_applies_override() {
    let workspace = compose_fixture_workspace("multi-file");

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app"]);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_run_services_starts_subset_and_primary_service() {
    let workspace = compose_fixture_workspace("run-services");

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app", "worker"]);
    assert!(!containers.iter().any(|container| {
        compose_label(&container.labels, "com.docker.compose.service") == Some("idle")
    }));
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_profiles_start_profile_service_when_enabled_by_host_env() {
    let workspace = compose_fixture_workspace("profiles");

    run_decune_up_detach(workspace.path(), &[("COMPOSE_PROFILES", "debug")]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app", "debug"]);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_primary_build_service_runs_lifecycle_assertion() {
    let workspace = compose_fixture_workspace("primary-build");

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app"]);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_features_apply_to_primary_service() {
    let workspace = compose_fixture_workspace("features");

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app", "sidecar"]);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_credentials_disabled_fixture_runs_without_forwarding_setup() {
    let workspace = compose_fixture_workspace("credentials-disabled");

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app"]);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_lifecycle_command_runs_in_primary_service() {
    let workspace = compose_fixture_workspace("lifecycle");

    run_decune_up_detach(workspace.path(), &[]);

    assert_eq!(
        fs::read_to_string(workspace.path().join("lifecycle-marker.txt")).unwrap(),
        "started"
    );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_cleanup_safety_keeps_unrelated_project_and_user_image() {
    let workspace = compose_fixture_workspace("cleanup-safety");
    let unrelated = create_unrelated_compose_fixture(workspace.path());

    run_decune_up_detach(workspace.path(), &[]);

    decune()
        .args(["clean", "--force"])
        .arg(workspace.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed Docker Compose project"));

    assert!(
        docker_status(["image", "inspect", &unrelated.image]).is_ok(),
        "decune clean must not remove user images"
    );
    assert!(
        docker_status(["container", "inspect", &unrelated.container_name]).is_ok(),
        "decune clean must not remove unrelated Compose project containers"
    );
}

fn compose_fixture_workspace(name: &str) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => panic!("{message}"),
    }

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.copy_dir_from(compose_fixture_path(name)).unwrap();
    ComposeFixtureWorkspace { workspace }
}

fn compose_fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compose")
        .join(name)
}

fn compose_integration_readiness() -> ComposeIntegrationDecision {
    compose_integration_decision(command_ok(["version"]), command_ok(["compose", "version"]))
}

fn compose_integration_decision(
    docker: Result<(), String>,
    compose: Result<(), String>,
) -> ComposeIntegrationDecision {
    if let Err(reason) = docker {
        return ComposeIntegrationDecision::Error(format!(
            "Docker Compose integration tests require Docker CLI: {reason}"
        ));
    }

    if let Err(reason) = compose {
        return ComposeIntegrationDecision::Error(format!(
            "Docker Compose integration tests require Docker Compose v2 plugin: {reason}"
        ));
    }

    ComposeIntegrationDecision::Run
}

fn command_ok<const N: usize>(args: [&str; N]) -> Result<(), String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| format!("failed to spawn docker: {error}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "docker {} exited with {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn run_decune_up_detach(workspace: &Path, envs: &[(&str, &str)]) {
    let mut command = decune();
    command.args(["up", "--detach"]).arg(workspace);
    for (key, value) in envs {
        command.env(key, value);
    }
    command
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));
}

fn compose_project_containers(workspace: &Path) -> anyhow::Result<Vec<ComposeContainer>> {
    let project = compose_project_name(workspace);
    let output = docker_output([
        "ps",
        "--all",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
        "--format",
        "json",
    ])?;
    output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| Ok(serde_json::from_str(line)?))
        .collect()
}

fn running_services(containers: &[ComposeContainer]) -> Vec<&str> {
    let mut services = containers
        .iter()
        .filter(|container| container.state == "running")
        .filter_map(|container| compose_label(&container.labels, "com.docker.compose.service"))
        .collect::<Vec<_>>();
    services.sort();
    services
}

fn compose_label<'a>(labels: &'a str, key: &str) -> Option<&'a str> {
    labels.split(',').find_map(|entry| {
        let (candidate, value) = entry.split_once('=')?;
        (candidate == key).then_some(value)
    })
}

fn compose_project_name(workspace: &Path) -> String {
    let basename = workspace
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");
    format!(
        "decune-{}-{}",
        safe_workspace_slug(basename),
        workspace_id(workspace)
    )
}

fn cleanup_compose_workspace(workspace: &Path) {
    let _ = decune().args(["clean", "--force"]).arg(workspace).assert();
    let project = compose_project_name(workspace);
    if let Ok(containers) = compose_project_containers(workspace) {
        for container in containers {
            let _ = docker_status(["rm", "--force", "--volumes", &container.id]);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let _ = cleanup_workspace_images(workspace).await;
    });
    let _ = docker_status([
        "network",
        "prune",
        "--force",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
    ]);
    let _ = docker_status([
        "volume",
        "prune",
        "--force",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
    ]);
}

fn create_unrelated_compose_fixture(workspace: &Path) -> UnrelatedComposeFixture {
    let unrelated_project = format!(
        "decune-compose-integration-unrelated-{}",
        workspace_id(workspace)
    );
    let image = format!(
        "decune-compose-integration-user-image-{}:latest",
        workspace_id(workspace)
    );
    let container_name = format!("{unrelated_project}-app-1");

    docker_status(["image", "inspect", "alpine:3.20"])
        .or_else(|_| docker_status(["pull", "alpine:3.20"]))
        .unwrap();
    docker_status(["image", "tag", "alpine:3.20", &image]).unwrap();
    docker_status([
        "run",
        "--detach",
        "--name",
        &container_name,
        "--label",
        &format!("com.docker.compose.project={unrelated_project}"),
        "--label",
        "com.docker.compose.service=app",
        "alpine:3.20",
        "sleep",
        "infinity",
    ])
    .unwrap();

    UnrelatedComposeFixture {
        container_name,
        image,
    }
}

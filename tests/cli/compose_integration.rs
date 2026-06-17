use std::{fs, io, path::Path, process::Command, thread, time::Duration};

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

struct ComposeImageCleanup {
    image: String,
}

struct ComposeRegistryFixture {
    container_name: String,
    image: String,
}

impl Drop for UnrelatedComposeFixture {
    fn drop(&mut self) {
        let _ = docker_status(["rm", "--force", "--volumes", &self.container_name]);
        let _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
    }
}

impl Drop for ComposeImageCleanup {
    fn drop(&mut self) {
        let _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
    }
}

impl Drop for ComposeRegistryFixture {
    fn drop(&mut self) {
        let _ = docker_status(["rm", "--force", "--volumes", &self.container_name]);
        let _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
    }
}

#[test]
fn compose_integration_plugin_detection_runs_when_tools_are_available() {
    assert_eq!(
        compose_integration_decision(Ok(()), Ok(()), Ok(())),
        ComposeIntegrationDecision::Run
    );
}

#[test]
fn compose_integration_plugin_detection_errors_when_docker_is_missing() {
    assert_eq!(
        compose_integration_decision(
            Err("docker executable was not found".to_owned()),
            Ok(()),
            Ok(())
        ),
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
            Err("docker compose version exited with 1".to_owned()),
            Ok(())
        ),
        ComposeIntegrationDecision::Error(
            "Docker Compose integration tests require Docker Compose v2 plugin: docker compose version exited with 1"
                .to_owned()
        )
    );
}

#[test]
fn compose_integration_plugin_detection_errors_when_capability_is_missing() {
    assert_eq!(
        compose_integration_decision(
            Ok(()),
            Ok(()),
            Err("missing docker compose build --with-dependencies".to_owned())
        ),
        ComposeIntegrationDecision::Error(
            "Docker Compose integration tests require newer Docker Compose v2 plugin capabilities: missing docker compose build --with-dependencies"
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
fn compose_integration_generated_override_is_valid_final_compose_config() {
    let workspace = compose_fixture_workspace("minimal");
    let state_home = support::TempWorkspace::new().unwrap();
    let state_home_value = state_home.path().to_string_lossy().into_owned();

    run_decune_up_detach(workspace.path(), &[("XDG_STATE_HOME", &state_home_value)]);

    let devcontainer_dir = workspace.path().join(".devcontainer");
    let generated_override = state_home
        .path()
        .join("decune")
        .join(workspace_id(workspace.path()))
        .join("compose.override.yaml");
    assert!(
        generated_override.is_file(),
        "generated Compose override was not written at {}",
        generated_override.display()
    );

    let output = docker_output(vec![
        "compose".to_owned(),
        "--project-name".to_owned(),
        compose_project_name(workspace.path()),
        "--project-directory".to_owned(),
        devcontainer_dir.display().to_string(),
        "-f".to_owned(),
        devcontainer_dir.join("compose.yaml").display().to_string(),
        "-f".to_owned(),
        generated_override.display().to_string(),
        "config".to_owned(),
        "--format".to_owned(),
        "json".to_owned(),
    ])
    .unwrap();

    assert!(output.contains("\"services\""));
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
fn compose_integration_localenv_container_env_is_expanded() {
    let workspace = compose_fixture_workspace("localenv-container-env");
    let state_home = support::TempWorkspace::new().unwrap();
    let state_home_value = state_home.path().to_string_lossy().into_owned();

    let assert = decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .env("NPM_TOKEN", "secret-token")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr);

    let npm_token = compose_primary_container_output(workspace.path(), ["printenv", "NPM_TOKEN"]);
    assert_eq!(npm_token.trim(), "secret-token");
    assert_ne!(npm_token.trim(), "${DECUNE_CONTAINER_ENV_NPM_TOKEN}");

    let generated_override = state_home
        .path()
        .join("decune")
        .join(workspace_id(workspace.path()))
        .join("compose.override.yaml");
    let generated_override = fs::read_to_string(&generated_override).unwrap();
    assert!(generated_override.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
    assert!(!generated_override.contains("secret-token"));

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert!(
        !containers
            .iter()
            .any(|container| container.labels.contains("secret-token"))
    );
    assert!(!stderr.contains("secret-token"));
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
fn compose_integration_run_services_builds_dependencies_for_selected_service() {
    let workspace = compose_fixture_workspace("build-dependencies");
    let base_image = format!(
        "decune-compose-build-dependencies-base-{}:latest",
        workspace_id(workspace.path())
    );
    let _cleanup = ComposeImageCleanup {
        image: base_image.clone(),
    };
    let _ = docker_status(["image", "rm", "--force", "--no-prune", &base_image]);

    run_decune_up_detach(
        workspace.path(),
        &[("DECUNE_BUILD_DEPS_BASE_IMAGE", &base_image)],
    );

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app"]);
    let dependency_label = docker_output([
        "image",
        "inspect",
        "--format",
        "{{ index .Config.Labels \"org.decune.test.build-dependency\" }}",
        &base_image,
    ])
    .unwrap();
    assert_eq!(dependency_label.trim(), "true");
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

#[test]
#[ignore = "requires Docker daemon, Docker Compose v2 plugin, and local registry image"]
fn compose_integration_up_pull_recreates_image_only_service_for_updated_tag() {
    let workspace = compose_pull_registry_workspace();
    let registry = create_compose_registry_fixture(workspace.path());

    build_and_push_compose_registry_image(&registry.image, "v1");
    run_decune_up_detach(workspace.path(), &[]);
    assert_eq!(
        compose_primary_container_output(workspace.path(), ["cat", "/decune-version"]).trim(),
        "v1"
    );

    build_and_push_compose_registry_image(&registry.image, "v2");
    decune()
        .args(["up", "--pull", "--detach"])
        .arg(workspace.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    assert_eq!(
        compose_primary_container_output(workspace.path(), ["cat", "/decune-version"]).trim(),
        "v2"
    );
}

fn compose_fixture_workspace(name: &str) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => panic!("{message}"),
    }

    let workspace = support::TempWorkspace::new().unwrap();
    copy_dir_contents(&compose_fixture_path(name), workspace.path()).unwrap();
    ComposeFixtureWorkspace { workspace }
}

fn copy_dir_contents(source: &Path, destination: &Path) -> io::Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        let file_type = entry.file_type()?;

        if file_type.is_dir() {
            fs::create_dir_all(&destination_path)?;
            copy_dir_contents(&source_path, &destination_path)?;
        } else if file_type.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::copy(&source_path, &destination_path)?;
        } else if file_type.is_symlink() {
            let target = fs::read_link(&source_path)?;
            std::os::unix::fs::symlink(target, &destination_path)?;
        }
    }

    Ok(())
}

fn compose_pull_registry_workspace() -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => panic!("{message}"),
    }

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "updateRemoteUserUID": false
            }
            "#,
        )
        .unwrap();
    let image = format!(
        "127.0.0.1:5000/decune-placeholder-{}:latest",
        workspace_id(workspace.path())
    );
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: "{image}"
            "#
            ),
        )
        .unwrap();

    ComposeFixtureWorkspace { workspace }
}

fn compose_fixture_path(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/compose")
        .join(name)
}

fn compose_integration_readiness() -> ComposeIntegrationDecision {
    compose_integration_decision(
        command_ok(["version"]),
        command_ok(["compose", "version"]),
        compose_capabilities_ok(),
    )
}

fn compose_integration_decision(
    docker: Result<(), String>,
    compose: Result<(), String>,
    capabilities: Result<(), String>,
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

    if let Err(reason) = capabilities {
        return ComposeIntegrationDecision::Error(format!(
            "Docker Compose integration tests require newer Docker Compose v2 plugin capabilities: {reason}"
        ));
    }

    ComposeIntegrationDecision::Run
}

fn compose_capabilities_ok() -> Result<(), String> {
    let requirements = [
        ("config", "--format", "docker compose config --format json"),
        ("ps", "--format", "docker compose ps --format json"),
        (
            "build",
            "--with-dependencies",
            "docker compose build --with-dependencies",
        ),
        ("pull", "--policy", "docker compose pull --policy always"),
        (
            "pull",
            "--ignore-buildable",
            "docker compose pull --ignore-buildable",
        ),
        (
            "up",
            "--force-recreate",
            "docker compose up --force-recreate",
        ),
        (
            "up",
            "--remove-orphans",
            "docker compose up --remove-orphans",
        ),
    ];
    for (subcommand, option, capability) in requirements {
        let help = command_output(["compose", subcommand, "--help"])?;
        if !help_contains_option(&help, option) {
            return Err(format!("missing {capability}"));
        }
    }
    Ok(())
}

fn command_ok<const N: usize>(args: [&str; N]) -> Result<(), String> {
    command_output(args).map(|_| ())
}

fn command_output<const N: usize>(args: [&str; N]) -> Result<String, String> {
    let output = Command::new("docker")
        .args(args)
        .output()
        .map_err(|error| format!("failed to spawn docker: {error}"))?;
    if output.status.success() {
        let mut text = String::from_utf8_lossy(&output.stdout).into_owned();
        if !output.stderr.is_empty() {
            text.push('\n');
            text.push_str(&String::from_utf8_lossy(&output.stderr));
        }
        Ok(text)
    } else {
        Err(format!(
            "docker {} exited with {}: {}",
            args.join(" "),
            output.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

fn help_contains_option(help: &str, option: &str) -> bool {
    help.split(|ch: char| {
        ch.is_ascii_whitespace() || matches!(ch, ',' | ';' | '[' | ']' | '(' | ')' | '{' | '}')
    })
    .any(|token| token == option || token.starts_with(&format!("{option}=")))
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

fn compose_primary_container_output<const N: usize>(
    workspace: &Path,
    command: [&str; N],
) -> String {
    let containers = compose_project_containers(workspace).unwrap();
    let container_id = containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .map(|container| container.id.as_str())
        .expect("primary Compose container was not found");
    let mut args = vec!["exec", container_id];
    args.extend(command);
    docker_output(args).unwrap()
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

fn create_compose_registry_fixture(workspace: &Path) -> ComposeRegistryFixture {
    let container_name = format!("decune-compose-registry-{}", workspace_id(workspace));
    let _ = docker_status(["rm", "--force", "--volumes", &container_name]);
    docker_status(["image", "inspect", "registry:2"])
        .or_else(|_| docker_status(["pull", "registry:2"]))
        .unwrap();
    docker_status([
        "run",
        "--detach",
        "--name",
        &container_name,
        "--publish",
        "127.0.0.1::5000",
        "registry:2",
    ])
    .unwrap();
    let port = docker_output(["port", &container_name, "5000/tcp"]).unwrap();
    let port = port
        .trim()
        .rsplit(':')
        .next()
        .expect("registry port output was empty");
    let image = format!(
        "127.0.0.1:{port}/decune-compose-pull-{}:latest",
        workspace_id(workspace)
    );
    rewrite_compose_image(workspace, &image);

    ComposeRegistryFixture {
        container_name,
        image,
    }
}

fn rewrite_compose_image(workspace: &Path, image: &str) {
    fs::write(
        workspace.join(".devcontainer/compose.yaml"),
        format!(
            r#"
services:
  app:
    image: "{image}"
"#
        ),
    )
    .unwrap();
}

fn build_and_push_compose_registry_image(image: &str, version: &str) {
    let context = tempfile::tempdir().unwrap();
    fs::write(
        context.path().join("Dockerfile"),
        format!(
            r#"FROM alpine:3.20
RUN printf '%s\n' '{version}' >/decune-version
"#
        ),
    )
    .unwrap();
    docker_status([
        "build",
        "--tag",
        image,
        context.path().to_string_lossy().as_ref(),
    ])
    .unwrap();
    push_image_with_retry(image);
    docker_status(["image", "rm", "--force", "--no-prune", image]).unwrap();
}

fn push_image_with_retry(image: &str) {
    let mut last_error = None;
    for _ in 0..20 {
        match docker_status(["push", image]) {
            Ok(()) => return,
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(100));
            }
        }
    }

    panic!(
        "failed to push test image to local registry: {}",
        last_error.unwrap()
    );
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

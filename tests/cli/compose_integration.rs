use std::{fs, net::TcpListener, path::Path, process::Command, thread, time::Duration};

use serde::Deserialize;
use serde_json::Value;

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
    extra_images: Vec<String>,
}

impl Drop for UnrelatedComposeFixture {
    fn drop(&mut self) {
        _ = docker_status(["rm", "--force", "--volumes", &self.container_name]);
        _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
    }
}

impl Drop for ComposeImageCleanup {
    fn drop(&mut self) {
        _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
    }
}

impl Drop for ComposeRegistryFixture {
    fn drop(&mut self) {
        _ = docker_status(["rm", "--force", "--volumes", &self.container_name]);
        _ = docker_status(["image", "rm", "--force", "--no-prune", &self.image]);
        for image in &self.extra_images {
            _ = docker_status(["image", "rm", "--force", "--no-prune", image]);
        }
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
fn compose_integration_reuses_running_compose_project_with_published_port_relocation_at_max_port() {
    let Ok(listener) = TcpListener::bind(("127.0.0.1", u16::MAX)) else {
        return;
    };
    drop(listener);

    let workspace = compose_published_primary_workspace(u16::MAX);
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let first_containers = compose_project_containers(workspace.path()).unwrap();
    let first_primary = first_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    let first_id = first_primary.id.clone();

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Reusing running dev container"));

    let second_containers = compose_project_containers(workspace.path()).unwrap();
    let second_primary = second_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    assert_eq!(second_primary.id, first_id);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_default_published_port_collision_fails_without_relocation() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    drop(requested_listener);
    let first = compose_published_primary_workspace(requested_port);
    let second = compose_published_primary_workspace(requested_port);
    let first_container_tools_dir = fake_container_tools_bundle(&first.workspace);
    let second_container_tools_dir = fake_container_tools_bundle(&second.workspace);

    decune()
        .args(["up", "--detach"])
        .arg(first.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &first_container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    decune()
        .args(["up", "--detach"])
        .arg(second.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &second_container_tools_dir)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_published_port_collision")
                .and(predicate::str::contains("service: `app`"))
                .and(predicate::str::contains(format!(
                    "127.0.0.1:{requested_port}"
                )))
                .and(predicate::str::contains("app:3000/tcp"))
                .and(predicate::str::contains(
                    "decune automatic forwarding does not replace",
                )),
        );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_fixed_subnet_overlap_fails_before_compose_up() {
    let subnet = "172.31.240.0/24";
    let first = compose_fixed_subnet_workspace(subnet);
    let second = compose_fixed_subnet_workspace(subnet);
    let first_container_tools_dir = fake_container_tools_bundle(&first.workspace);
    let second_container_tools_dir = fake_container_tools_bundle(&second.workspace);

    decune()
        .args(["up", "--detach"])
        .arg(first.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &first_container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let first_network = format!("{}_grpc", compose_project_name(first.path()));

    decune()
        .args(["up", "--detach"])
        .arg(second.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &second_container_tools_dir)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_network_subnet_overlap")
                .and(predicate::str::contains("network: `grpc`"))
                .and(predicate::str::contains(format!(
                    "requested subnet: {subnet}"
                )))
                .and(predicate::str::contains(format!(
                    "existing network: `{first_network}`"
                )))
                .and(predicate::str::contains(format!(
                    "existing subnet: {subnet}"
                ))),
        );

    let second_containers = compose_project_containers(second.path()).unwrap();
    assert!(
        second_containers.is_empty(),
        "second project should fail before docker compose up creates containers: {second_containers:?}"
    );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_published_port_relocation_starts_second_workspace_and_reports_ports() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    drop(requested_listener);
    let first = compose_published_primary_workspace(requested_port);
    let second = compose_published_primary_workspace(requested_port);
    let first_container_tools_dir = fake_container_tools_bundle(&first.workspace);
    let second_container_tools_dir = fake_container_tools_bundle(&second.workspace);

    decune()
        .args(["up", "--detach"])
        .arg(first.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &first_container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    decune()
        .args([
            "up",
            "--detach",
            "--no-auto-forward",
            "--published-port-relocation",
        ])
        .arg(second.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &second_container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let published_ports = compose_service_published_host_ports(second.path(), "app", "3000/tcp");
    assert!(
        !published_ports.contains(&requested_port),
        "relocation must not keep the original requested port active"
    );
    let planned_port = *published_ports
        .iter()
        .find(|port| **port > requested_port)
        .unwrap_or_else(|| {
            panic!(
                "expected relocated host port greater than {requested_port}: {published_ports:?}"
            )
        });

    decune()
        .args(["ports"])
        .arg(second.path())
        .assert()
        .success()
        .stdout(
            predicate::str::contains("published")
                .and(predicate::str::contains("compose"))
                .and(predicate::str::contains("app:3000/tcp"))
                .and(predicate::str::contains(format!(
                    "127.0.0.1:{planned_port}"
                )))
                .and(predicate::str::contains(format!(
                    "127.0.0.1:{requested_port}"
                )))
                .and(predicate::str::contains("relocated")),
        )
        .stderr(predicate::str::is_empty());

    let ports = decune_ports_json(second.path());
    let published = ports
        .iter()
        .find(|port| port["type"] == "published" && port["service"] == "app")
        .unwrap_or_else(|| panic!("published app port was not reported: {ports:#?}"));
    assert_eq!(published["source"], "compose");
    assert_eq!(published["container_port"].as_u64(), Some(3000));
    assert_eq!(published["requested"]["host_ip"], "127.0.0.1");
    assert_eq!(
        published["requested"]["host_port"].as_u64(),
        Some(u64::from(requested_port))
    );
    assert_eq!(published["planned"]["host_ip"], "127.0.0.1");
    assert_eq!(
        published["planned"]["host_port"].as_u64(),
        Some(u64::from(planned_port))
    );
    assert_eq!(published["relocated"], true);
    assert!(
        published["actual_bindings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|binding| binding["host_port"].as_u64() == Some(u64::from(planned_port))),
        "actual Docker binding should include relocated port: {published:#?}"
    );
    assert!(
        ports.iter().all(|port| port["type"] != "forwarded"),
        "--no-auto-forward plus relocation must not create forwarding listeners: {ports:#?}"
    );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_published_port_relocation_recreates_stopped_project_when_binding_must_move()
{
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    drop(requested_listener);
    let workspace = compose_published_primary_workspace(requested_port);
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let first_containers = compose_project_containers(workspace.path()).unwrap();
    let first_primary = first_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    let first_id = first_primary.id.clone();

    decune()
        .arg("down")
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success();
    let blocker = TcpListener::bind(("127.0.0.1", requested_port)).unwrap();

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("Compose published port relocation will recreate")
                .and(predicate::str::contains("service `app`"))
                .and(predicate::str::contains("Started dev container")),
        );

    let second_containers = compose_project_containers(workspace.path()).unwrap();
    let second_primary = second_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    assert_ne!(second_primary.id, first_id);

    let published_ports = compose_service_published_host_ports(workspace.path(), "app", "3000/tcp");
    assert!(
        !published_ports.contains(&requested_port),
        "relocation must not keep the blocked requested port active"
    );
    assert!(
        published_ports.iter().any(|port| *port > requested_port),
        "expected relocated host port greater than {requested_port}: {published_ports:?}"
    );
    drop(blocker);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_published_port_relocation_keeps_running_binding_after_blocker_disappears() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    let workspace = compose_published_primary_workspace(requested_port);
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let published_ports = compose_service_published_host_ports(workspace.path(), "app", "3000/tcp");
    assert!(
        !published_ports.contains(&requested_port),
        "initial relocation must not keep the blocked requested port active"
    );
    let planned_port = *published_ports
        .iter()
        .find(|port| **port > requested_port)
        .unwrap_or_else(|| {
            panic!(
                "expected relocated host port greater than {requested_port}: {published_ports:?}"
            )
        });
    let first_containers = compose_project_containers(workspace.path()).unwrap();
    let first_primary = first_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    let first_id = first_primary.id.clone();
    drop(requested_listener);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Reusing running dev container"));

    let second_containers = compose_project_containers(workspace.path()).unwrap();
    let second_primary = second_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    assert_eq!(second_primary.id, first_id);

    let current_ports = compose_service_published_host_ports(workspace.path(), "app", "3000/tcp");
    assert!(current_ports.contains(&planned_port));
    assert!(
        !current_ports.contains(&requested_port),
        "relocated binding must not move back to requested port until rebuild"
    );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_published_port_relocation_returns_to_requested_on_rebuild() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    let workspace = compose_published_primary_workspace(requested_port);
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let relocated_ports = compose_service_published_host_ports(workspace.path(), "app", "3000/tcp");
    assert!(
        !relocated_ports.contains(&requested_port),
        "initial relocation must not keep the blocked requested port active"
    );
    drop(requested_listener);

    decune()
        .args(["rebuild", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let rebuilt_ports = compose_service_published_host_ports(workspace.path(), "app", "3000/tcp");
    assert!(
        rebuilt_ports.contains(&requested_port),
        "rebuild should return relocated binding to requested port {requested_port}: {rebuilt_ports:?}"
    );
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_published_port_relocation_replaces_original_binding() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    let workspace = compose_published_primary_workspace(requested_port);
    let compose_file = workspace.path().join(".devcontainer/compose.yaml");
    let devcontainer_file = workspace.path().join(".devcontainer/devcontainer.json");
    let original_compose = fs::read_to_string(&compose_file).unwrap();
    let original_devcontainer = fs::read_to_string(&devcontainer_file).unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let published_ports = compose_service_published_host_ports(workspace.path(), "app", "3000/tcp");

    assert!(
        !published_ports.contains(&requested_port),
        "relocation must replace the original requested port instead of leaving it active"
    );
    assert!(
        published_ports.iter().any(|port| *port > requested_port),
        "expected a relocated published port greater than {requested_port}, got {published_ports:?}"
    );
    assert_eq!(fs::read_to_string(compose_file).unwrap(), original_compose);
    assert_eq!(
        fs::read_to_string(devcontainer_file).unwrap(),
        original_devcontainer
    );
    drop(requested_listener);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_published_port_relocation_preserves_host_ip_and_long_syntax() {
    let Some(requested_listeners) = reserved_localhost_port_block_with_room(3, 16) else {
        return;
    };
    let requested_port = requested_listeners[0].local_addr().unwrap().port();
    let workspace = compose_published_host_ip_workspace(requested_port);
    let state_home = support::TempWorkspace::new().unwrap();
    let state_home_value = state_home.path().to_string_lossy().into_owned();
    let compose_file = workspace.path().join(".devcontainer/compose.yaml");
    let devcontainer_file = workspace.path().join(".devcontainer/devcontainer.json");
    let original_compose = fs::read_to_string(&compose_file).unwrap();
    let original_devcontainer = fs::read_to_string(&devcontainer_file).unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let config = final_compose_config_json(workspace.path(), state_home.path());
    let ports = compose_config_service_ports(&config, "app");
    let omitted = compose_config_port_for_target(ports, 3000);
    let loopback = compose_config_port_for_target(ports, 3001);
    let wildcard = compose_config_port_for_target(ports, 3002);

    assert_eq!(omitted.get("host_ip"), None);
    assert!(compose_config_port_value(omitted, "published").unwrap() > requested_port);
    assert_eq!(loopback["host_ip"], "127.0.0.1");
    assert!(compose_config_port_value(loopback, "published").unwrap() > requested_port);
    assert_eq!(loopback["protocol"], "tcp");
    assert_eq!(loopback["name"], "loopback");
    assert_eq!(loopback["mode"], "host");
    assert_eq!(wildcard["host_ip"], "0.0.0.0");
    assert!(compose_config_port_value(wildcard, "published").unwrap() > requested_port);

    let ports = decune_ports_json_with_state_home(workspace.path(), state_home.path());
    let omitted_json = ports
        .iter()
        .find(|port| port["type"] == "published" && port["container_port"].as_u64() == Some(3000))
        .unwrap_or_else(|| panic!("omitted host IP published port was not reported: {ports:#?}"));
    assert_eq!(omitted_json["requested"]["host_ip"], Value::Null);
    assert_eq!(omitted_json["planned"]["host_ip"], Value::Null);
    assert_eq!(omitted_json["requested_host_ip_kind"], "omitted");
    assert_eq!(omitted_json["planned_host_ip_kind"], "omitted");

    assert_eq!(fs::read_to_string(compose_file).unwrap(), original_compose);
    assert_eq!(
        fs::read_to_string(devcontainer_file).unwrap(),
        original_devcontainer
    );
    drop(requested_listeners);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_sidecar_published_port_relocation_uses_docker_binding() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    let workspace = compose_published_sidecar_workspace(requested_port);
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app", "db"]);
    let published_ports = compose_service_published_host_ports(workspace.path(), "db", "5432/tcp");
    assert!(
        !published_ports.contains(&requested_port),
        "sidecar relocation must replace the original requested port"
    );
    assert!(
        published_ports.iter().any(|port| *port > requested_port),
        "expected relocated sidecar host port greater than {requested_port}: {published_ports:?}"
    );
    let ports = decune_ports_json(workspace.path());
    assert!(
        ports.iter().any(|port| {
            port["type"] == "published"
                && port["source"] == "compose"
                && port["service"] == "db"
                && port["container_port"].as_u64() == Some(5432)
                && port["relocated"] == true
        }),
        "ports output did not report relocated sidecar published port: {ports:#?}"
    );
    drop(requested_listener);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_dependency_published_port_relocation_uses_compose_active_service_set() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    let workspace = compose_published_dependency_workspace(requested_port);
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(workspace.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app", "db"]);
    let published_ports = compose_service_published_host_ports(workspace.path(), "db", "5432/tcp");
    assert!(
        !published_ports.contains(&requested_port),
        "dependency relocation must replace the original requested port"
    );
    assert!(
        published_ports.iter().any(|port| *port > requested_port),
        "expected relocated dependency host port greater than {requested_port}: {published_ports:?}"
    );
    let ports = decune_ports_json(workspace.path());
    assert!(
        ports.iter().any(|port| {
            port["type"] == "published"
                && port["source"] == "compose"
                && port["service"] == "db"
                && port["container_port"].as_u64() == Some(5432)
                && port["relocated"] == true
        }),
        "ports output did not report relocated dependency published port: {ports:#?}"
    );
    drop(requested_listener);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2.24.4 plugin"]
fn compose_integration_profile_published_port_relocation_follows_active_service_set() {
    let Some(requested_listener) = reserved_localhost_port_with_room_for_relocation() else {
        return;
    };
    let requested_port = requested_listener.local_addr().unwrap().port();
    let inactive = compose_profile_published_workspace(requested_port, false);
    let active = compose_profile_published_workspace(requested_port, true);
    let inactive_container_tools_dir = fake_container_tools_bundle(&inactive.workspace);
    let active_container_tools_dir = fake_container_tools_bundle(&active.workspace);

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(inactive.path())
        .env("DECUNE_CONTAINER_TOOLS_DIR", &inactive_container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let inactive_containers = compose_project_containers(inactive.path()).unwrap();
    assert_eq!(running_services(&inactive_containers), vec!["app"]);
    assert!(
        decune_ports_json(inactive.path()).is_empty(),
        "inactive profile service must not publish or relocate ports"
    );

    decune()
        .args(["up", "--detach", "--published-port-relocation"])
        .arg(active.path())
        .env("COMPOSE_PROFILES", "debug")
        .env("DECUNE_CONTAINER_TOOLS_DIR", &active_container_tools_dir)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let active_containers = compose_project_containers(active.path()).unwrap();
    assert_eq!(running_services(&active_containers), vec!["app", "debug"]);
    let published_ports = compose_service_published_host_ports(active.path(), "debug", "9229/tcp");
    assert!(
        !published_ports.contains(&requested_port),
        "active profile service should relocate away from requested port"
    );
    assert!(
        published_ports.iter().any(|port| *port > requested_port),
        "expected relocated profile host port greater than {requested_port}: {published_ports:?}"
    );
    drop(requested_listener);
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_ports_reports_published_port_from_non_primary_service() {
    let host_port = available_localhost_port();
    let workspace = compose_published_sidecar_workspace(host_port);

    run_decune_up_detach(workspace.path(), &[]);

    let containers = compose_project_containers(workspace.path()).unwrap();
    assert_eq!(running_services(&containers), vec!["app", "db"]);
    let db = containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("db")
        })
        .expect("db service container should exist");
    assert_ne!(
        compose_label(&db.labels, "decune.managed"),
        Some("true"),
        "db sidecar should not rely on decune-managed labels"
    );

    let output = decune()
        .args(["ports", "--json"])
        .arg(workspace.path())
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let ports: Value = serde_json::from_slice(&output).unwrap();
    let ports = ports.as_array().unwrap();

    assert!(
        ports.iter().any(|port| {
            port["type"] == "published"
                && port["source"] == "compose"
                && port["service"] == "db"
                && port["container_port"].as_u64() == Some(5432)
                && port["host_port"].as_u64() == Some(u64::from(host_port))
        }),
        "ports output did not include db published port: {ports:#?}"
    );
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

#[cfg(unix)]
#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_reuses_running_container_without_refreshing_dotfile_skeleton_mounts() {
    use std::os::unix::fs as unix_fs;

    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => panic!("{message}"),
    }

    let workspace = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("actual-nvim").unwrap();
    workspace.create_dir("nvim-source").unwrap();
    workspace
        .write_file("actual-nvim/init.lua", "return {}\n")
        .unwrap();
    workspace
        .write_file("actual-nvim/extra.lua", "not mounted\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("actual-nvim/init.lua"),
        workspace.path().join("nvim-source/init.lua"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            r#"
            services:
              app:
                image: "alpine:3.20"
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.github]
            enabled = false

            [[dotfiles]]
            source = "nvim-source"
            target = ".config/nvim"
            read_only = true
            "#,
        )
        .unwrap();
    let workspace = ComposeFixtureWorkspace { workspace };
    let state_home_value = state_home.path().to_string_lossy().into_owned();

    decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let first_containers = compose_project_containers(workspace.path()).unwrap();
    let first_primary = first_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    let first_id = first_primary.id.clone();

    decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Reusing running dev container"));

    let second_containers = compose_project_containers(workspace.path()).unwrap();
    let second_primary = second_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    assert_eq!(second_primary.id, first_id);

    let output =
        compose_primary_container_output(workspace.path(), ["cat", "/root/.config/nvim/init.lua"]);
    assert_eq!(output, "return {}\n");
}

#[test]
#[ignore = "requires Docker daemon and Docker Compose v2 plugin"]
fn compose_integration_rejects_reuse_when_compose_env_interpolation_changes() {
    let workspace = compose_fixture_workspace("env-interpolation");
    let state_home = support::TempWorkspace::new().unwrap();
    let state_home_value = state_home.path().to_string_lossy().into_owned();
    let container_tools_dir = fake_container_tools_bundle(&workspace.workspace);

    let first = decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .env("DECUNE_TEST_COMPOSE_ENV_TOKEN", "first-secret")
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"))
        .stderr(predicate::str::contains("first-secret").not());
    let first_stderr = String::from_utf8_lossy(&first.get_output().stderr);
    assert!(!first_stderr.contains("first-secret"));

    let first_containers = compose_project_containers(workspace.path()).unwrap();
    let first_container = first_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should exist");
    let first_id = first_container.id.clone();
    let first_hash = compose_label(&first_container.labels, "decune.config_hash")
        .expect("primary Compose service should have decune.config_hash label")
        .to_owned();
    assert!(!first_container.labels.contains("first-secret"));
    assert_eq!(
        compose_primary_container_output(workspace.path(), ["printenv", "APP_TOKEN"]).trim(),
        "first-secret"
    );

    let state_file = state_home
        .path()
        .join("decune")
        .join(workspace_id(workspace.path()))
        .join("state.toml");
    let first_state = fs::read_to_string(&state_file).unwrap();
    assert!(!first_state.contains("first-secret"));

    decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
        .env("DECUNE_TEST_COMPOSE_ENV_TOKEN", "second-secret")
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Dev container configuration changed. Run decune rebuild to recreate it.",
        ))
        .stderr(predicate::str::contains("second-secret").not());

    let unchanged_containers = compose_project_containers(workspace.path()).unwrap();
    let unchanged_container = unchanged_containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some("app")
        })
        .expect("primary Compose service container should still exist");
    assert_eq!(unchanged_container.id, first_id);
    assert_eq!(
        compose_label(&unchanged_container.labels, "decune.config_hash"),
        Some(first_hash.as_str())
    );
    assert!(!unchanged_container.labels.contains("first-secret"));
    assert!(!unchanged_container.labels.contains("second-secret"));
    assert_eq!(
        compose_primary_container_output(workspace.path(), ["printenv", "APP_TOKEN"]).trim(),
        "first-secret"
    );

    let unchanged_state = fs::read_to_string(&state_file).unwrap();
    assert!(!unchanged_state.contains("first-secret"));
    assert!(!unchanged_state.contains("second-secret"));
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
    _ = docker_status(["image", "rm", "--force", "--no-prune", &base_image]);

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
fn compose_integration_reuses_running_feature_container_without_rebuilding() {
    let workspace = compose_fixture_workspace("features");
    let state_home = support::TempWorkspace::new().unwrap();
    let state_home_value = state_home.path().to_string_lossy().into_owned();
    let config_home = support::TempWorkspace::new().unwrap();
    let config_home_value = config_home.path().to_string_lossy().into_owned();
    workspace.workspace.create_dir(".decune").unwrap();
    workspace
        .workspace
        .write_file(
            ".decune/config.toml",
            b"version = 1\n[credentials.git]\nenabled = false\n[credentials.github]\nenabled = false\n",
        )
        .unwrap();

    decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .env("XDG_CONFIG_HOME", &config_home_value)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Building Docker image"))
        .stderr(predicate::str::contains("Started dev container"));

    decune()
        .args(["up", "--detach"])
        .arg(workspace.path())
        .env("XDG_STATE_HOME", &state_home_value)
        .env("XDG_CONFIG_HOME", &config_home_value)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Reusing running dev container"))
        .stderr(predicate::str::contains("Building Docker image").not());
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
        .args(["remove", "--no-confirm"])
        .arg(workspace.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed Docker Compose project"));

    assert!(
        docker_status(["image", "inspect", &unrelated.image]).is_ok(),
        "decune remove must not remove user images"
    );
    assert!(
        docker_status(["container", "inspect", &unrelated.container_name]).is_ok(),
        "decune remove must not remove unrelated Compose project containers"
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

#[test]
#[ignore = "requires Docker daemon, Docker Compose v2 plugin, and local registry image"]
fn compose_integration_up_pull_updates_dependency_service_image() {
    let workspace = compose_pull_dependency_registry_workspace();
    let registry = create_compose_dependency_registry_fixture(workspace.path());
    let db_image = registry
        .extra_images
        .first()
        .expect("dependency registry fixture must include a db image");

    build_and_push_compose_registry_image(&registry.image, "v1");
    build_and_push_compose_registry_image(db_image, "v1");
    run_decune_up_detach(workspace.path(), &[]);
    assert_eq!(
        compose_service_container_output(workspace.path(), "app", ["cat", "/decune-version"])
            .trim(),
        "v1"
    );
    assert_eq!(
        compose_service_container_output(workspace.path(), "db", ["cat", "/decune-version"]).trim(),
        "v1"
    );

    build_and_push_compose_registry_image(&registry.image, "v2");
    build_and_push_compose_registry_image(db_image, "v2");
    decune()
        .args(["up", "--pull", "--detach"])
        .arg(workspace.path())
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    assert_eq!(
        compose_service_container_output(workspace.path(), "app", ["cat", "/decune-version"])
            .trim(),
        "v2"
    );
    assert_eq!(
        compose_service_container_output(workspace.path(), "db", ["cat", "/decune-version"]).trim(),
        "v2"
    );
}

fn compose_fixture_workspace(name: &str) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace
        .copy_fixture_dir(Path::new("compose").join(name))
        .must();
    ComposeFixtureWorkspace { workspace }
}

fn compose_published_sidecar_workspace(host_port: u16) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "name": "compose-sidecar-published-port",
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "runServices": ["app", "db"],
              "workspaceFolder": "/workspace",
              "updateRemoteUserUID": false
            }
            "#,
        )
        .must();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                command: sleep infinity
                volumes:
                  - ..:/workspace
              db:
                image: alpine:3.20
                command: sleep infinity
                ports:
                  - "127.0.0.1:{host_port}:5432"
            "#
            ),
        )
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn compose_published_dependency_workspace(host_port: u16) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "name": "compose-dependency-published-port",
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "workspaceFolder": "/workspace",
              "updateRemoteUserUID": false
            }
            "#,
        )
        .must();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                command: sleep infinity
                depends_on:
                  - db
                volumes:
                  - ..:/workspace
              db:
                image: alpine:3.20
                command: sleep infinity
                ports:
                  - "127.0.0.1:{host_port}:5432"
            "#
            ),
        )
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn compose_published_primary_workspace(host_port: u16) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "name": "compose-primary-published-port",
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "workspaceFolder": "/workspace",
              "updateRemoteUserUID": false
            }
            "#,
        )
        .must();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                command: sleep infinity
                volumes:
                  - ..:/workspace
                ports:
                  - "127.0.0.1:{host_port}:3000"
            "#
            ),
        )
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn compose_fixed_subnet_workspace(subnet: &str) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "name": "compose-fixed-subnet",
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "workspaceFolder": "/workspace",
              "updateRemoteUserUID": false
            }
            "#,
        )
        .must();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                command: sleep infinity
                volumes:
                  - ..:/workspace
                networks:
                  - grpc
            networks:
              grpc:
                ipam:
                  config:
                    - subnet: "{subnet}"
            "#
            ),
        )
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn compose_published_host_ip_workspace(base_host_port: u16) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "name": "compose-published-host-ip",
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "workspaceFolder": "/workspace",
              "updateRemoteUserUID": false
            }
            "#,
        )
        .must();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                command: sleep infinity
                volumes:
                  - ..:/workspace
                ports:
                  - "{base_host_port}:3000"
                  - target: 3001
                    published: "{}"
                    host_ip: "127.0.0.1"
                    protocol: tcp
                    name: loopback
                    mode: host
                  - target: 3002
                    published: "{}"
                    host_ip: "0.0.0.0"
                    protocol: tcp
            "#,
                base_host_port + 1,
                base_host_port + 2,
            ),
        )
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn compose_profile_published_workspace(host_port: u16, run_debug: bool) -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    let devcontainer_json = if run_debug {
        r#"
        {
          "name": "compose-profile-published-port",
          "dockerComposeFile": "compose.yaml",
          "service": "app",
          "runServices": ["debug"],
          "workspaceFolder": "/workspace",
          "updateRemoteUserUID": false
        }
        "#
    } else {
        r#"
        {
          "name": "compose-profile-published-port",
          "dockerComposeFile": "compose.yaml",
          "service": "app",
          "workspaceFolder": "/workspace",
          "updateRemoteUserUID": false
        }
        "#
    };
    workspace
        .write_file(".devcontainer/devcontainer.json", devcontainer_json)
        .must();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                command: sleep infinity
                volumes:
                  - ..:/workspace
              debug:
                image: alpine:3.20
                command: sleep infinity
                profiles:
                  - debug
                ports:
                  - "127.0.0.1:{host_port}:9229"
            "#
            ),
        )
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn available_localhost_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).must();
    listener.local_addr().must().port()
}

fn reserved_localhost_port_with_room_for_relocation() -> Option<TcpListener> {
    (0..16).find_map(|_| {
        let listener = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        (listener.local_addr().ok()?.port() < u16::MAX - 16).then_some(listener)
    })
}

fn reserved_localhost_port_block_with_room(
    count: u16,
    relocation_room: u16,
) -> Option<Vec<TcpListener>> {
    (0..64).find_map(|_| {
        let first = TcpListener::bind(("127.0.0.1", 0)).ok()?;
        let base = first.local_addr().ok()?.port();
        if base > u16::MAX - count - relocation_room {
            return None;
        }
        let mut listeners = vec![first];
        for offset in 1..count {
            let listener = TcpListener::bind(("127.0.0.1", base + offset)).ok()?;
            listeners.push(listener);
        }
        Some(listeners)
    })
}

fn compose_pull_registry_workspace() -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
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
        .must();
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
        .must();

    ComposeFixtureWorkspace { workspace }
}

fn compose_pull_dependency_registry_workspace() -> ComposeFixtureWorkspace {
    match compose_integration_readiness() {
        ComposeIntegrationDecision::Run => {}
        ComposeIntegrationDecision::Error(message) => test_fail(message),
    }

    let workspace = support::TempWorkspace::new().must();
    workspace.create_dir(".devcontainer").must();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "runServices": ["app"],
              "overrideCommand": true,
              "updateRemoteUserUID": false
            }
            "#,
        )
        .must();
    let app_image = format!(
        "127.0.0.1:5000/decune-placeholder-app-{}:latest",
        workspace_id(workspace.path())
    );
    let db_image = format!(
        "127.0.0.1:5000/decune-placeholder-db-{}:latest",
        workspace_id(workspace.path())
    );
    rewrite_compose_dependency_images(workspace.path(), &app_image, &db_image);

    ComposeFixtureWorkspace { workspace }
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
            "pull",
            "--include-deps",
            "docker compose pull --include-deps",
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
    compose_service_container_output(workspace, "app", command)
}

fn compose_service_container_output<const N: usize>(
    workspace: &Path,
    service: &str,
    command: [&str; N],
) -> String {
    let containers = compose_project_containers(workspace).must();
    let container_id = containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some(service)
        })
        .must_msg(format_args!(
            "Compose service container was not found: {service}"
        ))
        .id
        .as_str();
    let mut args = vec!["exec", container_id];
    args.extend(command);
    docker_output(args).must()
}

fn compose_service_published_host_ports(
    workspace: &Path,
    service: &str,
    container_port_key: &str,
) -> Vec<u16> {
    let containers = compose_project_containers(workspace).must();
    let container_id = containers
        .iter()
        .find(|container| {
            compose_label(&container.labels, "com.docker.compose.service") == Some(service)
        })
        .must_msg(format_args!(
            "Compose service container was not found: {service}"
        ))
        .id
        .as_str();
    let output = docker_output(["container", "inspect", container_id]).must();
    let inspect = serde_json::from_str::<Vec<Value>>(&output).must();
    let ports = inspect
        .first()
        .and_then(|container| container.pointer("/NetworkSettings/Ports"))
        .and_then(Value::as_object)
        .and_then(|ports| ports.get(container_port_key))
        .and_then(Value::as_array)
        .must_msg(format_args!(
            "published port binding was not found: {container_port_key}"
        ));

    let mut host_ports = ports
        .iter()
        .filter_map(|binding| binding.get("HostPort"))
        .filter_map(Value::as_str)
        .map(|port| port.parse::<u16>().must())
        .collect::<Vec<_>>();
    host_ports.sort_unstable();
    host_ports.dedup();
    host_ports
}

fn decune_ports_json(workspace: &Path) -> Vec<Value> {
    let mut command = decune();
    command.args(["ports", "--json"]).arg(workspace);
    decune_ports_json_from_command(command)
}

fn decune_ports_json_with_state_home(workspace: &Path, state_home: &Path) -> Vec<Value> {
    let mut command = decune();
    command
        .args(["ports", "--json"])
        .arg(workspace)
        .env("XDG_STATE_HOME", state_home);
    decune_ports_json_from_command(command)
}

fn decune_ports_json_from_command(mut command: assert_cmd::Command) -> Vec<Value> {
    let output = command
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice::<Vec<Value>>(&output).must()
}

fn final_compose_config_json(workspace: &Path, state_home: &Path) -> Value {
    let devcontainer_dir = workspace.join(".devcontainer");
    let generated_override = state_home
        .join("decune")
        .join(workspace_id(workspace))
        .join("compose.override.yaml");
    assert!(
        generated_override.is_file(),
        "generated Compose override was not written at {}",
        generated_override.display()
    );

    let output = docker_output(vec![
        "compose".to_owned(),
        "--project-name".to_owned(),
        compose_project_name(workspace),
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
    .must();
    serde_json::from_str(&output).must()
}

fn compose_config_service_ports<'a>(config: &'a Value, service: &str) -> &'a [Value] {
    config
        .pointer(&format!("/services/{service}/ports"))
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .must_msg(format_args!(
            "Compose config did not contain service ports: {config:#?}"
        ))
}

fn compose_config_port_for_target(ports: &[Value], target: u16) -> &Value {
    ports
        .iter()
        .find(|port| compose_config_port_value(port, "target") == Some(target))
        .must_msg(format_args!(
            "Compose config did not contain target port {target}: {ports:#?}"
        ))
}

fn compose_config_port_value(port: &Value, key: &str) -> Option<u16> {
    match port.get(key)? {
        Value::Number(number) => number.as_u64().and_then(|value| u16::try_from(value).ok()),
        Value::String(value) => value.parse().ok(),
        Value::Null | Value::Bool(_) | Value::Array(_) | Value::Object(_) => None,
    }
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
    services.sort_unstable();
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
    _ = decune()
        .args(["remove", "--no-confirm"])
        .arg(workspace)
        .assert();
    let project = compose_project_name(workspace);
    if let Ok(containers) = compose_project_containers(workspace) {
        for container in containers {
            _ = docker_status(["rm", "--force", "--volumes", &container.id]);
        }
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .must();
    runtime.block_on(async {
        _ = cleanup_workspace_images(workspace);
    });
    _ = docker_status([
        "network",
        "prune",
        "--force",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
    ]);
    _ = docker_status([
        "volume",
        "prune",
        "--force",
        "--filter",
        &format!("label=com.docker.compose.project={project}"),
    ]);
}

fn create_compose_registry_fixture(workspace: &Path) -> ComposeRegistryFixture {
    let (container_name, port) = start_compose_registry(workspace);
    let image = format!(
        "127.0.0.1:{port}/decune-compose-pull-{}:latest",
        workspace_id(workspace)
    );
    rewrite_compose_image(workspace, &image);

    ComposeRegistryFixture {
        container_name,
        image,
        extra_images: Vec::new(),
    }
}

fn create_compose_dependency_registry_fixture(workspace: &Path) -> ComposeRegistryFixture {
    let (container_name, port) = start_compose_registry(workspace);
    let image = format!(
        "127.0.0.1:{port}/decune-compose-pull-app-{}:latest",
        workspace_id(workspace)
    );
    let db_image = format!(
        "127.0.0.1:{port}/decune-compose-pull-db-{}:latest",
        workspace_id(workspace)
    );
    rewrite_compose_dependency_images(workspace, &image, &db_image);

    ComposeRegistryFixture {
        container_name,
        image,
        extra_images: vec![db_image],
    }
}

fn start_compose_registry(workspace: &Path) -> (String, String) {
    let container_name = format!("decune-compose-registry-{}", workspace_id(workspace));
    _ = docker_status(["rm", "--force", "--volumes", &container_name]);
    docker_status(["image", "inspect", "registry:2"])
        .or_else(|_| docker_status(["pull", "registry:2"]))
        .must();
    docker_status([
        "run",
        "--detach",
        "--name",
        &container_name,
        "--publish",
        "127.0.0.1::5000",
        "registry:2",
    ])
    .must();
    let port = docker_output(["port", &container_name, "5000/tcp"]).must();
    let port = port
        .trim()
        .rsplit(':')
        .next()
        .must_msg("registry port output was empty")
        .to_owned();
    (container_name, port)
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
    .must();
}

fn rewrite_compose_dependency_images(workspace: &Path, app_image: &str, db_image: &str) {
    fs::write(
        workspace.join(".devcontainer/compose.yaml"),
        format!(
            r#"
services:
  app:
    image: "{app_image}"
    depends_on:
      - db
  db:
    image: "{db_image}"
    command: ["sleep", "infinity"]
"#
        ),
    )
    .must();
}

fn build_and_push_compose_registry_image(image: &str, version: &str) {
    let context = tempfile::tempdir().must();
    fs::write(
        context.path().join("Dockerfile"),
        format!(
            r"FROM alpine:3.20
RUN printf '%s\n' '{version}' >/decune-version
"
        ),
    )
    .must();
    docker_status([
        "build",
        "--tag",
        image,
        context.path().to_string_lossy().as_ref(),
    ])
    .must();
    push_image_with_retry(image);
    docker_status(["image", "rm", "--force", "--no-prune", image]).must();
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

    test_fail(format_args!(
        "failed to push test image to local registry: {}",
        last_error.must()
    ));
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
        .must();
    docker_status(["image", "tag", "alpine:3.20", &image]).must();
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
    .must();

    UnrelatedComposeFixture {
        container_name,
        image,
    }
}

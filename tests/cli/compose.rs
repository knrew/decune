use crate::harness::*;
use sha2::{Digest, Sha256};
use std::net::TcpListener;

#[test]
fn compose_multi_replica_fixed_published_port_reports_diagnostic_code() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
                image: alpine:3.20
                scale: 2
                ports:
                  - "3000:3000"
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-multi-replica-fixed-published-port-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .args(["up", "--detach", "--automatic-published-port-relocation"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_published_port_multi_replica_unsupported")
                .and(predicate::str::contains("service `app`"))
                .and(predicate::str::contains("2 replicas"))
                .and(predicate::str::contains("<host_ip omitted>:3000"))
                .and(predicate::str::contains("app:3000/tcp")),
        );
}

#[test]
fn compose_invalid_published_port_config_reports_diagnostic_code() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
                image: alpine:3.20
                ports:
                  - "999.999.999.999:3000:3000"
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-invalid-published-port-config-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_published_port_invalid")
                .and(predicate::str::contains("invalid IP address")),
        );
}

#[test]
fn compose_up_default_published_port_collision_reports_diagnostic_code() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
                image: alpine:3.20
                ports:
                  - "3000:3000"
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-default-published-port-collision-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_published_port_collision")
                .and(predicate::str::contains("service: `app`"))
                .and(predicate::str::contains("<host_ip omitted>:3000"))
                .and(predicate::str::contains("app:3000/tcp"))
                .and(predicate::str::contains("Failed to start Docker Compose project").not()),
        );
}

#[test]
fn compose_fixed_subnet_overlap_reports_diagnostic_code_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
                networks:
                  - grpc
            networks:
              grpc:
                ipam:
                  config:
                    - subnet: 172.28.0.0/16
                      gateway: 172.28.0.1
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-subnet-overlap-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_network_subnet_overlap")
                .and(predicate::str::contains("network: `grpc`"))
                .and(predicate::str::contains("requested subnet: 172.28.0.0/16"))
                .and(predicate::str::contains("existing network: `other_grpc`"))
                .and(predicate::str::contains("existing subnet: 172.28.10.0/24"))
                .and(predicate::str::contains(
                    "existing compose project: other-project",
                ))
                .and(predicate::str::contains("compose up should not run").not()),
        );
}

#[test]
fn compose_fixed_container_name_conflict_reports_diagnostic_code_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
                container_name: fixed-app
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-container-name-conflict-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_fixed_name_conflict")
                .and(predicate::str::contains("service container `app`"))
                .and(predicate::str::contains("requested name: `fixed-app`"))
                .and(predicate::str::contains(
                    "existing resource: container `fixed-app`",
                ))
                .and(predicate::str::contains(
                    "existing compose project: other-project",
                ))
                .and(predicate::str::contains("compose up should not run").not()),
        );
}

#[test]
fn compose_fixed_volume_name_conflict_reports_diagnostic_code_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
                volumes:
                  - cache:/cache
            volumes:
              cache:
                name: fixed-cache
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-volume-name-conflict-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_fixed_name_conflict")
                .and(predicate::str::contains("volume `cache`"))
                .and(predicate::str::contains("requested name: `fixed-cache`"))
                .and(predicate::str::contains(
                    "existing resource: volume `fixed-cache`",
                ))
                .and(predicate::str::contains(
                    "existing compose project: other-project",
                ))
                .and(predicate::str::contains("compose up should not run").not()),
        );
}

#[test]
fn compose_clone_isolation_opt_in_rewrites_fixed_names_in_generated_override() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
                container_name: fixed-app
                networks: [default]
                volumes: [cache:/cache]
              sidecar:
                image: alpine:3.20
                network_mode: container:fixed-app
                ipc: container:fixed-app
                pid: container:fixed-app
                volumes_from: [container:fixed-app:ro]
                external_links: [fixed-app:app-alias]
            volumes:
              cache:
                name: fixed-cache
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [compose.clone_isolation]
            enabled = true
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let rewritten_container = format!("fixed-app-{workspace_id}");
    let rewritten_volume = format!("fixed-cache-{workspace_id}");

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", state_home.path())
        .env("DECUNE_FAKE_EXPECTED_CONTAINER_NAME", &rewritten_container)
        .env("DECUNE_FAKE_EXPECTED_VOLUME_NAME", &rewritten_volume)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let override_file = state_home
        .path()
        .join("decune")
        .join(&workspace_id)
        .join("compose.override.yaml");
    let generated = fs::read_to_string(override_file).unwrap();
    assert!(generated.contains(&format!("container_name: '{rewritten_container}'")));
    assert!(generated.contains("'default':\n        aliases:\n          - 'fixed-app'"));
    assert!(generated.contains(&format!(
        "volumes:\n  'cache':\n    name: '{rewritten_volume}'"
    )));
    assert!(generated.contains(&format!("network_mode: 'container:{rewritten_container}'")));
    assert!(generated.contains(&format!("ipc: 'container:{rewritten_container}'")));
    assert!(generated.contains(&format!("pid: 'container:{rewritten_container}'")));
    assert!(generated.contains(&format!(
        "volumes_from: !override\n      - 'container:{rewritten_container}:ro'"
    )));
    assert!(generated.contains(&format!(
        "external_links: !override\n      - '{rewritten_container}:app-alias'"
    )));
}

#[test]
fn compose_clone_isolation_without_opt_in_does_not_rewrite_fixed_names() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
                container_name: fixed-app
                networks: [default]
                volumes: [cache:/cache]
              sidecar:
                image: alpine:3.20
                network_mode: container:fixed-app
                ipc: container:fixed-app
                pid: container:fixed-app
                volumes_from: [container:fixed-app:ro]
                external_links: [fixed-app:app-alias]
            volumes:
              cache:
                name: fixed-cache
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [compose.clone_isolation]
            enabled = false
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", state_home.path())
        .env("DECUNE_FAKE_EXPECTED_CONTAINER_NAME", "fixed-app")
        .env("DECUNE_FAKE_EXPECTED_VOLUME_NAME", "fixed-cache")
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let override_file = state_home
        .path()
        .join("decune")
        .join(workspace_id)
        .join("compose.override.yaml");
    let generated = fs::read_to_string(override_file).unwrap();
    assert!(!generated.contains("container_name:"));
    assert!(!generated.contains("volumes:\n  'cache':\n    name:"));
    assert!(!generated.contains("network_mode:"));
    assert!(!generated.contains("volumes_from:"));
    assert!(!generated.contains("external_links:"));
}

#[test]
fn compose_clone_isolation_list_reference_rewrite_requires_compose_v2_24_4() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            r"
            services:
              app:
                image: alpine:3.20
                container_name: fixed-app
              sidecar:
                image: alpine:3.20
                volumes_from: [container:fixed-app:ro]
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [compose.clone_isolation]
            enabled = true
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let rewritten_container = format!("fixed-app-{}", workspace_id(&workspace_root));

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMPOSE_VERSION_SHORT", "2.24.3")
        .env("DECUNE_FAKE_COMPOSE_UP_MUST_NOT_RUN", "1")
        .env("DECUNE_FAKE_EXPECTED_CONTAINER_NAME", &rewritten_container)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_clone_isolation_unsupported")
                .and(predicate::str::contains(
                    "Compose clone isolation container-reference list rewrite requires Docker Compose v2.24.4 or newer",
                ))
                .and(predicate::str::contains("docker compose up must not run").not()),
        );
}

#[test]
fn compose_clone_isolation_relocates_fixed_subnet_after_occupied_initial_slot() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/24"
            subnet_prefix = 25

            [credentials.git]
            enabled = false

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let (occupied, expected) = two_slot_subnets(&workspace_id, "grpc");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", state_home.path())
        .env("XDG_RUNTIME_DIR", host_tools.path())
        .env("DECUNE_FAKE_SUBNET_RELOCATION", "1")
        .env("DECUNE_FAKE_OCCUPIED_SUBNET", &occupied)
        .args(["up", "--detach", "--no-global-config"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let state_dir = state_home.path().join("decune").join(&workspace_id);
    let generated = fs::read_to_string(state_dir.join("compose.override.yaml")).unwrap();
    assert!(generated.contains("config: !override"));
    assert!(generated.contains(&format!("subnet: '{expected}'")));
    assert!(generated.contains(&format!(
        "ip_range: '{}/26'",
        address_at_offset(&expected, 64)
    )));
    assert!(generated.contains(&format!(
        "'reserved': '{}'",
        address_at_offset(&expected, 10)
    )));
    assert!(!generated.contains(&format!("subnet: '{occupied}'")));
    let state = fs::read_to_string(state_dir.join("state.toml")).unwrap();
    assert!(state.contains("[[clone_isolation.networks]]"));
    assert!(state.contains(&format!("planned_subnet = \"{expected}\"")));
}

#[test]
fn compose_clone_isolation_rejects_unknown_ipam_fields_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/24"
            subnet_prefix = 25
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_UNKNOWN_IPAM_FIELD", "1")
        .args(["up", "--detach", "--no-global-config"])
        .arg(workspace.path())
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_clone_isolation_unsupported")
                .and(predicate::str::contains("network `grpc`"))
                .and(predicate::str::contains(
                    "fields `alpha_field`, `future_field`",
                ))
                .and(predicate::str::contains("alpha-field-value-must-not-leak").not())
                .and(predicate::str::contains("future-field-value-must-not-leak").not())
                .and(predicate::str::contains("docker compose up must not run").not()),
        );
}

#[test]
fn compose_clone_isolation_rejects_subnetless_ipam_config_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/24"
            subnet_prefix = 25
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_SUBNETLESS_IPAM_CONFIG", "1")
        .args(["up", "--detach", "--no-global-config"])
        .arg(workspace.path())
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_clone_isolation_unsupported")
                .and(predicate::str::contains("network `grpc`"))
                .and(predicate::str::contains(
                    "IPAM config entries without subnet",
                ))
                .and(predicate::str::contains("sensitive-range-value").not())
                .and(predicate::str::contains("sensitive-address-value").not())
                .and(predicate::str::contains("docker compose up must not run").not()),
        );
}

#[test]
fn compose_clone_isolation_rejects_undeclared_stale_endpoint_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/24"
            subnet_prefix = 25
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let (occupied, _) = two_slot_subnets(&workspace_id, "grpc");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_ENDPOINT_RELOCATION", "1")
        .env("DECUNE_FAKE_OCCUPIED_SUBNET", &occupied)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_clone_isolation_endpoint_unsafe")
                .and(predicate::str::contains("service: `app`"))
                .and(predicate::str::contains(
                    "environment variable: `HOST_AGENT_ENDPOINT`",
                ))
                .and(predicate::str::contains("network: `grpc`"))
                .and(predicate::str::contains("original address: 10.99.0.1"))
                .and(predicate::str::contains("endpoint-plaintext-must-not-leak").not()),
        );
}

#[test]
fn compose_clone_isolation_rejects_stale_address_left_by_multi_network_endpoint() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/16"
            subnet_prefix = 25

            [[compose.clone_isolation.endpoints]]
            service = "app"
            env = "HOST_AGENT_ENDPOINT"
            value = "grpc://${decune.network.grpc.gateway}:50051/failover/10.100.0.1"
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_MULTI_ENDPOINT_RELOCATION", "1")
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_clone_isolation_endpoint_unsafe")
                .and(predicate::str::contains("service: `app`"))
                .and(predicate::str::contains("network: `metrics`"))
                .and(predicate::str::contains("original address: 10.100.0.1")),
        );
}

#[test]
fn compose_clone_isolation_renders_declared_endpoint_in_generated_override() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/24"
            subnet_prefix = 25

            [[compose.clone_isolation.endpoints]]
            service = "app"
            env = "HOST_AGENT_ENDPOINT"
            value = "grpc://${decune.network.grpc.gateway}:50051"

            [credentials.git]
            enabled = false

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let (occupied, expected_subnet) = two_slot_subnets(&workspace_id, "grpc");
    let expected_gateway = first_host_address(&expected_subnet);
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", state_home.path())
        .env("XDG_RUNTIME_DIR", host_tools.path())
        .env("DECUNE_FAKE_ENDPOINT_RELOCATION", "1")
        .env("DECUNE_FAKE_OCCUPIED_SUBNET", &occupied)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let state_dir = state_home.path().join("decune").join(workspace_id);
    let generated = fs::read_to_string(state_dir.join("compose.override.yaml")).unwrap();
    assert!(generated.contains(&format!(
        "'HOST_AGENT_ENDPOINT': 'grpc://{expected_gateway}:50051'"
    )));
    let state = fs::read_to_string(state_dir.join("state.toml")).unwrap();
    assert!(!state.contains("grpc://"));
    assert!(!state.contains("endpoint-plaintext-must-not-leak"));
}

#[test]
fn compose_clone_isolation_endpoint_reports_disabled_network_relocation() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = true

            [compose.clone_isolation.networks]
            relocation = false

            [[compose.clone_isolation.endpoints]]
            service = "app"
            env = "HOST_AGENT_ENDPOINT"
            value = "grpc://${decune.network.grpc.gateway}:50051"
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_ENDPOINT_RELOCATION", "1")
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_clone_isolation_invalid")
                .and(predicate::str::contains("network relocation is disabled"))
                .and(predicate::str::contains(
                    "compose.clone_isolation.networks.relocation = true",
                )),
        );
}

#[test]
fn disabled_compose_clone_isolation_warns_for_endpoint_and_does_not_override_fixed_subnet() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app","overrideCommand":true}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [compose.clone_isolation]
            enabled = false

            [compose.clone_isolation.networks]
            relocation = true
            subnet_pool = "10.200.0.0/24"
            subnet_prefix = 25

            [[compose.clone_isolation.endpoints]]
            service = "app"
            env = "HOST_AGENT_ENDPOINT"
            value = "grpc://${decune.network.grpc.gateway}:50051"

            [credentials.git]
            enabled = false

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-fixed-name-rewrite-generated-override.sh",
    );

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", state_home.path())
        .env("XDG_RUNTIME_DIR", host_tools.path())
        .env("DECUNE_FAKE_SUBNET_RELOCATION", "1")
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "compose.clone_isolation.endpoints is ignored because compose.clone_isolation.enabled is false",
        ));

    let generated = fs::read_to_string(
        state_home
            .path()
            .join("decune")
            .join(workspace_id)
            .join("compose.override.yaml"),
    )
    .unwrap();
    assert!(!generated.contains("config: !override"));
}

fn two_slot_subnets(workspace_id: &str, network: &str) -> (String, String) {
    let input = format!("decune-clone-isolation-subnet-v1:{workspace_id}:{network}");
    let digest = Sha256::digest(input.as_bytes());
    let slot = u64::from_be_bytes([
        digest[0], digest[1], digest[2], digest[3], digest[4], digest[5], digest[6], digest[7],
    ]) % 2;
    if slot == 0 {
        ("10.200.0.0/25".to_owned(), "10.200.0.128/25".to_owned())
    } else {
        ("10.200.0.128/25".to_owned(), "10.200.0.0/25".to_owned())
    }
}

fn first_host_address(subnet: &str) -> String {
    address_at_offset(subnet, 1)
}

fn address_at_offset(subnet: &str, offset: u32) -> String {
    let (network, _) = subnet.split_once('/').unwrap();
    let network = network.parse::<std::net::Ipv4Addr>().unwrap();
    std::net::Ipv4Addr::from(u32::from(network) + offset).to_string()
}

#[test]
fn compose_reports_all_fixed_resource_name_conflicts_before_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
                networks: [app]
                configs: [app]
                secrets: [app]
            networks:
              app:
                name: fixed-network
            configs:
              app:
                name: fixed-config
            secrets:
              app:
                name: fixed-secret
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-reports-all-fixed-resource-name-conflicts.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains(
                "Docker Compose clone isolation preflight detected 3 conflicts:",
            )
            .and(predicate::str::contains("network `app`"))
            .and(predicate::str::contains("requested name: `fixed-network`"))
            .and(predicate::str::contains("config `app`"))
            .and(predicate::str::contains("requested name: `fixed-config`"))
            .and(predicate::str::contains("secret `app`"))
            .and(predicate::str::contains("requested name: `fixed-secret`"))
            .and(predicate::str::contains("compose up should not run").not()),
        );
}

#[test]
fn compose_without_clone_sensitive_config_does_not_list_networks() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
            r"
            services:
              app:
                image: alpine:3.20
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-without-clone-sensitive-config-does-not-list-networks.sh",
    );
    let command_log = host_tools.path().join("commands.log");
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let commands = fs::read_to_string(command_log).unwrap();
    // One config validates the preliminary plan and one prepares startup context. The latter
    // must not be repeated when the lifecycle targets the whole Compose project.
    assert_eq!(
        commands
            .lines()
            .filter(|command| command.contains(" config --format json"))
            .count(),
        2
    );
}

#[test]
fn compose_isolation_preflight_ignores_unselected_services_and_resources() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "runServices": ["app"],
              "overrideCommand": true
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            r"
            services:
              app:
                image: alpine:3.20
              unused:
                image: alpine:3.20
                container_name: fixed-unused
                networks: [unused-network]
                volumes: [unused-volume:/data]
            networks:
              unused-network:
                ipam:
                  config:
                    - subnet: 172.28.0.0/16
            volumes:
              unused-volume:
                name: fixed-unused-volume
            ",
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-ignores-unselected-isolation-resources.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_RUNTIME_DIR", host_tools.path())
        .env("XDG_STATE_HOME", host_tools.path())
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "selected Compose up reached after isolation preflight",
        ));
}

#[test]
fn compose_up_unsupported_published_port_startup_failure_reports_diagnostic_code() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
                image: alpine:3.20
                ports:
                  - "8125:8125/udp"
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-unsupported-published-port-startup-failure-reports-diagnostic-code.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(
            predicate::str::contains("compose_published_port_unsupported")
                .and(predicate::str::contains("service: `app`"))
                .and(predicate::str::contains("<host_ip omitted>:8125"))
                .and(predicate::str::contains("app:8125/udp"))
                .and(predicate::str::contains("Failed to start Docker Compose project").not()),
        );
}

#[test]
fn compose_up_records_published_port_runtime_state() {
    let requested_listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let requested_port = requested_listener.local_addr().unwrap().port();
    if requested_port == u16::MAX {
        return;
    }
    let planned_port = requested_port + 1;
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
    let up_marker = host_tools.path().join("compose-up");
    workspace.create_dir(".devcontainer").unwrap();
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
            format!(
                r#"
            services:
              app:
                image: alpine:3.20
                ports:
                  - "{requested_port}:3000"
            "#
            ),
        )
        .unwrap();
    let original_compose =
        fs::read_to_string(workspace.path().join(".devcontainer/compose.yaml")).unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-records-published-port-runtime-state.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", state_home.path())
        .env("DECUNE_FAKE_REQUESTED_PORT", requested_port.to_string())
        .env("DECUNE_FAKE_PLANNED_PORT", planned_port.to_string())
        .env("DECUNE_FAKE_UP_MARKER", &up_marker)
        .env("DECUNE_FAKE_WORKSPACE_ID", &workspace_id)
        .env(
            "DECUNE_FAKE_WORKSPACE_SLUG",
            safe_workspace_slug(workspace_root.file_name().unwrap().to_str().unwrap()),
        )
        .args(["up", "--detach", "--automatic-published-port-relocation"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let state_file = state_home
        .path()
        .join("decune")
        .join(&workspace_id)
        .join("state.toml");
    let state = fs::read_to_string(state_file).unwrap();
    assert!(state.contains("[[published_ports]]"));
    assert!(state.contains("source = \"compose\""));
    assert!(state.contains("type = \"published\""));
    assert!(state.contains("service = \"app\""));
    assert!(state.contains("port_entry_index = 0"));
    assert!(state.contains("host_ip_kind = \"omitted\""));
    assert!(state.contains(&format!("host_port = {requested_port}")));
    assert!(state.contains(&format!("host_port = {planned_port}")));
    assert!(state.contains("relocated = true"));
    assert!(state.contains("[[published_ports.actual_bindings]]"));
    assert!(state.contains("host_ip = \"0.0.0.0\""));
    assert!(state.contains("host_ip = \"::\""));
    assert_eq!(
        fs::read_to_string(workspace.path().join(".devcontainer/compose.yaml")).unwrap(),
        original_compose
    );
}

fn decune_with_fake_container_tools(workspace: &support::TempWorkspace) -> TestCommand {
    let container_tools_dir = fake_container_tools_bundle(workspace);
    let mut command = decune();
    command.env("DECUNE_CONTAINER_TOOLS_DIR", container_tools_dir);
    command
}

#[test]
fn compose_validation_runs_after_initialize_command_generated_files_exist() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "initializeCommand": "printf 'TAG=3.20\n' > .devcontainer/generated.env"
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
                image: "alpine:${TAG}"
                env_file: generated.env
            "#,
        )
        .unwrap();
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-validation-runs-after-initialize-command-generated-files-exist.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_TEST_WORKSPACE", &workspace_root)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    assert!(workspace_root.join(".devcontainer/generated.env").is_file());
}

#[test]
fn compose_up_preserves_primary_service_image_when_no_final_layer_is_needed() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
    let override_log = host_tools.path().join("generated-override.yaml");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-preserves-primary-service-image-when-no-final-layer-is-needed.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_OVERRIDE_LOG", &override_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let generated_override = fs::read_to_string(override_log).unwrap();
    assert!(generated_override.contains("image: 'alpine:3.20'"));
    assert!(!generated_override.contains("image: 'decune/"));
}

#[test]
fn compose_up_passes_decune_config_local_env_container_env_placeholder_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
            ".decune/config.toml",
            concat!(
                r#"
            version = 1

            [container_env]
            NPM_TOKEN = "$"#,
                "{localEnv:NPM_TOKEN}",
                "\"\n",
            ),
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
    let override_log = host_tools.path().join("generated-override.yaml");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-passes-local-env-derived-container-env-placeholder-env.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_OVERRIDE_LOG", &override_log)
        .env("NPM_TOKEN", "secret-token")
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let generated_override = fs::read_to_string(override_log).unwrap();
    assert!(generated_override.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
    assert!(!generated_override.contains("secret-token"));
}

#[test]
fn compose_up_applies_feature_final_image_only_to_primary_and_propagates_build_options() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/primary-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "updateRemoteUserUID": false,
              "features": {
                "./features/primary-tool": {}
              }
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
                image: "example/app:dev"
              sidecar:
                image: "example/sidecar:dev"
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/primary-tool/devcontainer-feature.json",
            r#"
            {
              "id": "primary-tool",
              "version": "1.0.0",
              "name": "Primary Tool",
              "containerEnv": {
                "FROM_PRIMARY_FEATURE": "yes"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/primary-tool/install.sh",
            "set -eu\n",
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let override_log = host_tools.path().join("generated-override.yaml");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-applies-feature-final-image-only-to-primary-and-propagates-build-options.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_OVERRIDE_LOG", &override_log)
        .args(["up", "--detach", "--no-cache", "--pull"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let generated_override = fs::read_to_string(override_log).unwrap();
    assert!(generated_override.contains("'app':"));
    assert!(generated_override.contains("image: 'decune/"));
    assert!(generated_override.contains("pull_policy: 'never'"));
    assert!(!generated_override.contains("FROM_PRIMARY_FEATURE"));
    assert!(!generated_override.contains("sidecar"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose"));
    assert!(commands.contains("build --with-dependencies --no-cache --pull"));
    assert!(commands.lines().any(|line| {
        line.contains("build --tag decune/")
            && line.contains("--no-cache")
            && !line.contains("--pull")
    }));
    assert!(
        !commands
            .lines()
            .any(|line| line.contains("build --tag decune/") && line.contains("--pull"))
    );
}

#[test]
fn compose_pull_adds_force_recreate_to_compose_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let state_home = support::TempWorkspace::new().unwrap();
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
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-pull-adds-force-recreate-to-compose-up.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", state_home.path())
        .args(["up", "--detach", "--pull"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(
        commands.contains(" pull --ignore-buildable --include-deps --policy always"),
        "{commands}"
    );
    assert!(commands.contains(" up -d --force-recreate"));
}

#[test]
fn compose_up_builds_selected_services_with_dependencies() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            r"
            services:
              base:
                build:
                  context: .
                  dockerfile: Dockerfile.base
              app:
                build:
                  context: .
                  dockerfile: Dockerfile.app
                depends_on:
                  - base
            ",
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-builds-selected-services-with-dependencies.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.lines().any(|line| {
        line.starts_with("compose ")
            && line.ends_with(" build --with-dependencies app")
            && !line.ends_with(" build --with-dependencies app app")
    }));
}

#[test]
fn compose_service_user_is_used_for_lifecycle_when_devcontainer_users_are_unset() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "updateRemoteUserUID": false,
              "postCreateCommand": "true"
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
                user: "appuser"
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-service-user-is-used-for-lifecycle-when-devcontainer-users-are-unset.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime_root = host_tools.path().join("runtime");
    let host_daemon_socket = runtime_root
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("host-daemon.sock");

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_EXPECT_HOST_DAEMON_SOCKET", &host_daemon_socket)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("exec --user appuser"));
    assert_container_cli_setup_precedes_lifecycle(&commands, "exec --user appuser");
}

#[test]
fn compose_exec_lifecycle_shell_attach_returns_shell_exit_and_defaults_to_stop_compose_shutdown() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "workspaceFolder": "/workspace",
              "userEnvProbe": "none",
              "postStartCommand": "printf post-start",
              "postAttachCommand": "printf post-attach"
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
            shell = "/usr/local/bin/decune-shell"
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-exec-lifecycle-shell-attach-returns-shell-exit-and-defaults-to-stop-compose-shutdown.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime_root = host_tools.path().join("runtime");
    let host_daemon_socket = runtime_root
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("host-daemon.sock");

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_EXPECT_HOST_DAEMON_SOCKET", &host_daemon_socket)
        .env("XDG_RUNTIME_DIR", &runtime_root)
        .arg("up")
        .arg(&workspace_root)
        .assert()
        .code(7)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose"));
    assert!(commands.contains("ps --format json app"));
    assert!(commands.contains(
        "exec --user root --workdir /workspace compose-app-id /bin/sh -lc printf post-start"
    ));
    assert!(commands.contains(
        "exec --user root --workdir /workspace compose-app-id /bin/sh -lc printf post-attach"
    ));
    assert!(commands.contains(
        "exec --interactive --user root --workdir /workspace compose-app-id /usr/local/bin/decune-shell"
    ));
    assert_container_cli_setup_precedes_lifecycle(
        &commands,
        "exec --user root --workdir /workspace compose-app-id /bin/sh -lc printf post-start",
    );
    assert!(
        commands
            .lines()
            .any(|command| command.starts_with("compose ") && command.ends_with(" stop"))
    );
}

fn assert_container_cli_setup_precedes_lifecycle(commands: &str, lifecycle_command: &str) {
    let symlink = commands
        .find("decune-container-cli-symlink enabled")
        .expect("container CLI symlink reconciliation command was not recorded");
    let lifecycle = commands
        .find(lifecycle_command)
        .expect("user lifecycle command was not recorded");

    assert!(
        symlink < lifecycle,
        "container CLI symlink reconciliation must precede the first user lifecycle command"
    );
}

#[test]
fn compose_dotfiles_attached_up_prepares_lifecycle_once_before_post_attach() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "workspaceFolder": "/workspace",
              "userEnvProbe": "none",
              "postStartCommand": "printf post-start",
              "postAttachCommand": "printf post-attach"
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
            shell = "/usr/local/bin/decune-shell"

            [[dotfiles]]
            source = "dotfiles/gitconfig"
            target = ".gitconfig"
            on_conflict = "backup"
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-dotfiles-attached-up-prepares-lifecycle-once-before-post-attach.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .arg("up")
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert_eq!(
        commands
            .matches("src='/opt/decune/dotfiles/.gitconfig'")
            .count(),
        1
    );
    assert!(commands.contains(
        "exec --user root --workdir /workspace compose-app-id /bin/sh -lc printf post-start"
    ));
    assert!(commands.contains(
        "exec --user root --workdir /workspace compose-app-id /bin/sh -lc printf post-attach"
    ));
    assert!(commands.contains(
        "exec --interactive --user root --workdir /workspace compose-app-id /usr/local/bin/decune-shell"
    ));
}

#[cfg(unix)]
#[test]
fn compose_dotfile_skeleton_override_uses_unique_backing_directory_mounts() {
    use std::collections::BTreeSet;

    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    write_multiple_dotfile_skeleton_sources(&workspace);
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "workspaceFolder": "/workspace",
              "userEnvProbe": "none"
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
            use_global_config = false

            [credentials.github]
            enabled = false

            [[dotfiles]]
            source = "dotfiles-src/tool-a"
            target = ".config/tool-a"
            read_only = true

            [[dotfiles]]
            source = "dotfiles-src/tool-b"
            target = ".config/tool-b"
            read_only = true
            "#,
        )
        .unwrap();
    let override_log = host_tools.path().join("generated-override.yaml");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-dotfile-skeleton-override-uses-backing-directory-mounts.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_OVERRIDE_LOG", &override_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let override_yaml = fs::read_to_string(override_log).unwrap();
    assert!(override_yaml.contains("target: '/opt/decune/dotfiles/.config/tool-a'"));
    assert!(override_yaml.contains("target: '/opt/decune/dotfiles/.config/tool-b'"));
    let backing_targets = override_yaml
        .lines()
        .filter_map(|line| line.trim().strip_prefix("target: '"))
        .filter_map(|target| target.strip_suffix('\''))
        .filter(|target| target.starts_with("/opt/decune/dotfile-backings/"))
        .collect::<BTreeSet<_>>();
    assert_eq!(backing_targets.len(), 4);
    assert_eq!(
        override_yaml
            .lines()
            .filter(|line| line.contains("target: '/opt/decune/dotfile-backings/"))
            .count(),
        backing_targets.len()
    );
    assert!(
        !override_yaml.contains("target: '/opt/decune/dotfiles/.config/tool-a/tool-a-config.yml'")
    );
    assert!(
        !override_yaml.contains("target: '/opt/decune/dotfiles/.config/tool-b/tool-b-config.yml'")
    );
    assert!(!override_yaml.contains("source: '/opt/decune"));
}

#[test]
fn compose_credentials_runs_git_https_helper_setup_in_primary_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "workspaceFolder": "/workspace",
              "userEnvProbe": "none"
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
              sidecar:
                image: "alpine:3.20"
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            enabled = true
            copy_user = false
            https = "host-helper"
            ssh_agent = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-credentials-runs-git-https-helper-setup-in-primary-container.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("exec "));
    assert!(commands.contains("compose-app-id /bin/sh -lc test -x /run/decune/git-credential-decune && test -w /run/decune/host-daemon.sock"));
    assert!(commands.contains("compose-app-id /bin/sh -lc set -e"));
    assert!(commands.contains("git config --global --add credential.helper"));
    assert!(commands.contains("/run/decune/git-credential-decune"));
    assert!(!commands.contains("compose-sidecar-id"));
}

#[test]
fn compose_stop_container_shutdown_succeeds_when_primary_is_already_stopped() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "workspaceFolder": "/workspace",
              "userEnvProbe": "none",
              "shutdownAction": "stopContainer"
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
            shell = "/usr/local/bin/decune-shell"
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let stopped_marker = host_tools.path().join("primary-stopped");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-stop-container-shutdown-succeeds-when-primary-is-already-stopped.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_STOPPED_MARKER", &stopped_marker)
        .arg("up")
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains(
        "exec --interactive --user root --workdir /workspace compose-app-id /usr/local/bin/decune-shell"
    ));
    assert!(commands.contains("stop --time 10 compose-app-id"));
    assert!(!commands.contains("compose stop"));
}

#[test]
fn compose_lifecycle_detach_skips_post_attach_shell_attach_and_shutdown() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": true,
              "workspaceFolder": "/workspace",
              "userEnvProbe": "none",
              "postStartCommand": "printf post-start",
              "postAttachCommand": "printf post-attach",
              "shutdownAction": "stopCompose"
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
            shell = "/usr/local/bin/decune-shell"
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-lifecycle-detach-skips-post-attach-shell-attach-and-shutdown.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains(
        "exec --user root --workdir /workspace compose-app-id /bin/sh -lc printf post-start"
    ));
    assert!(!commands.contains("post-attach"));
    assert!(!commands.contains("/usr/local/bin/decune-shell"));
    assert!(!commands.contains(" stop"));
}

#[test]
fn compose_up_detects_primary_service_command_exit_before_lifecycle() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app",
              "overrideCommand": false,
              "postStartCommand": "echo should-not-run"
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
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-detects-primary-service-command-exit-before-lifecycle.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Container exited during startup"))
        .stderr(predicate::str::contains("exit code 23"))
        .stderr(predicate::str::contains("Started dev container").not());
}

#[test]
fn compose_up_removes_orphans_when_primary_service_was_renamed() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
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
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-up-removes-orphans-when-primary-service-was-renamed.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .args(["up", "--detach"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Started dev container: compose-app-1",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("--remove-orphans"));
}

#[test]
fn compose_down_also_stops_leftover_image_mode_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app"
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
    let command_log = host_tools.path().join("commands.log");
    let ps_count = host_tools.path().join("ps-count");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-down-also-stops-leftover-image-mode-container.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_PS_COUNT", &ps_count)
        .arg("down")
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Stopped Docker Compose project"))
        .stderr(predicate::str::contains("Stopped dev container: old-image"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose stop"));
    assert!(commands.contains("docker stop --time 10 old-image-id"));
}

#[test]
fn compose_remove_also_removes_leftover_image_mode_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app"
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
    let command_log = host_tools.path().join("commands.log");
    let ps_count = host_tools.path().join("ps-count");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-remove-also-removes-leftover-image-mode-container.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_PS_COUNT", &ps_count)
        .args(["remove", "--no-confirm"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed Docker Compose project"))
        .stderr(predicate::str::contains("Removed dev container: old-image"))
        .stderr(predicate::str::contains("Removed dev container resources"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose down"));
    assert!(commands.contains("docker stop --time 10 old-image-id"));
    assert!(commands.contains("docker rm --force --volumes old-image-id"));
}

#[test]
fn compose_down_stops_existing_project_when_config_files_are_missing() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-down-stops-existing-project-when-config-files-are-missing.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_WORKSPACE_ID", &workspace_id)
        .arg("down")
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Stopped Docker Compose container: missing-app-1",
        ))
        .stderr(predicate::str::contains(
            "Stopped Docker Compose container: missing-db-1",
        ))
        .stderr(predicate::str::contains("No dev container found").not());

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("docker stop --time 10 compose-primary-id"));
    assert!(commands.contains("docker stop --time 10 compose-sidecar-id"));
    assert!(!commands.contains("compose stop"));
}

#[test]
fn compose_remove_removes_existing_project_when_config_files_are_missing() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let command_log = host_tools.path().join("commands.log");
    let removed_marker = host_tools.path().join("removed");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-remove-removes-existing-project-when-config-files-are-missing.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_REMOVED_MARKER", &removed_marker)
        .env("DECUNE_FAKE_WORKSPACE_ID", &workspace_id)
        .args(["remove", "--no-confirm"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Removed Docker Compose container: missing-app-1",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker Compose container: missing-db-1",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker volume: missing_project_data",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker network: missing_project_default",
        ))
        .stderr(predicate::str::contains("Removed dev container resources"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("docker rm --force --volumes compose-primary-id"));
    assert!(commands.contains("docker rm --force --volumes compose-sidecar-id"));
    assert!(commands.contains("docker volume rm --force missing_project_data"));
    assert!(commands.contains("docker network rm missing_project_default"));
    assert!(!commands.contains("compose down"));
    assert!(!commands.contains("image rm"));
}

#[test]
fn compose_remove_falls_back_to_labels_when_generated_override_is_stale() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{"dockerComposeFile":"compose.yaml","service":"app"}"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/compose.yaml",
            "services:\n  app:\n    image: alpine:3.20\n",
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-remove-falls-back-when-generated-override-is-stale.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let workspace_slug = workspace_root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap()
        .to_ascii_lowercase();
    let project_name = format!("decune-{workspace_slug}-{workspace_id}");
    let state_home = host_tools.path().join("state");
    let state_dir = state_home.join("decune").join(&workspace_id);
    fs::create_dir_all(&state_dir).unwrap();
    let generated_override = state_dir.join("compose.override.yaml");
    fs::write(
        &generated_override,
        r"
        services:
          app:
            image: alpine:3.20
          removed-sidecar:
            container_name: fixed-sidecar-workspace
            networks:
              default:
                aliases:
                  - fixed-sidecar
        ",
    )
    .unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_PROJECT_NAME", &project_name)
        .env("DECUNE_FAKE_WORKSPACE_ID", &workspace_id)
        .env("XDG_STATE_HOME", &state_home)
        .args(["remove", "--no-confirm"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Falling back to Docker labels because Docker Compose project removal failed",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker Compose container: stale-app-1",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker Compose container: stale-sidecar-1",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker volume: stale_project_data",
        ))
        .stderr(predicate::str::contains(
            "Removed Docker network: stale_project_default",
        ))
        .stderr(predicate::str::contains("Removed dev container resources"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose"));
    assert!(commands.contains(&generated_override.display().to_string()));
    assert!(commands.contains("down --volumes --remove-orphans"));
    assert!(commands.contains("rm --force --volumes compose-primary-id"));
    assert!(commands.contains("rm --force --volumes compose-sidecar-id"));
    assert!(commands.contains("volume rm --force stale_project_data"));
    assert!(commands.contains("network rm stale_project_default"));
    assert!(!state_dir.exists());
}

#[test]
fn compose_remove_images_removes_only_decune_generated_workspace_images() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "dockerComposeFile": "compose.yaml",
              "service": "app"
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
                image: "example/app:dev"
              sidecar:
                image: "example/sidecar:dev"
            "#,
        )
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let fake_path = fake_docker_path(
        &host_tools,
        "cli/compose/compose-remove-images-removes-only-decune-generated-workspace-images.sh",
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_repository = workspace_image_repository(&workspace_root);
    let state_home = host_tools.path().join("state");
    let generated_override = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("compose.override.yaml");
    fs::create_dir_all(generated_override.parent().unwrap()).unwrap();
    fs::write(
        &generated_override,
        r#"
        services:
          app:
            image: "decune/workspace:generated"
            command: ["sleep", "infinity"]
            volumes:
              - type: bind
                source: /tmp/decune-workspace
                target: /workspace
        volumes:
          cache:
            name: "fixed-cache-workspace"
        "#,
    )
    .unwrap();

    decune_with_fake_container_tools(&host_tools)
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_IMAGE_REPOSITORY", &image_repository)
        .env("XDG_STATE_HOME", &state_home)
        .args(["remove", "--no-confirm", "--images"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed Docker Compose project"))
        .stderr(predicate::str::contains(format!(
            "Removed Docker image: {image_repository}:final-hash"
        )))
        .stderr(predicate::str::contains("Removed dev container resources"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose"));
    assert!(commands.contains(&generated_override.display().to_string()));
    assert!(commands.contains("down --volumes --remove-orphans"));
    assert!(commands.contains(&format!(
        "image ls --all --format json {image_repository}:*"
    )));
    assert!(commands.contains(&format!(
        "image rm --no-prune --force {image_repository}:final-hash"
    )));
    assert!(!commands.contains("example/sidecar"));
    assert!(!commands.contains("--rmi"));
}

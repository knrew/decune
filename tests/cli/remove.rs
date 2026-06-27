use crate::harness::*;

fn fake_container_tools_bundle(workspace: &support::TempWorkspace) -> PathBuf {
    workspace
        .write_file("container-tools/linux-amd64/decune-forward-agent", b"agent")
        .unwrap();
    workspace
        .write_file(
            "container-tools/linux-amd64/git-credential-decune",
            b"helper",
        )
        .unwrap();
    workspace
        .write_file(
            "container-tools/manifest.json",
            r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"decune-forward-agent","platform":"linux-amd64","path":"linux-amd64/decune-forward-agent","sha256":"d4f0bc5a29de06b510f9aa428f1eedba926012b591fef7a518e776a7c9bd1824"},{"name":"git-credential-decune","platform":"linux-amd64","path":"linux-amd64/git-credential-decune","sha256":"e81d3b0e9d82feaaf5f6e55bdff24731d7eee08632ffa63801e6397290c5d20a"}]}"#,
        )
        .unwrap();
    workspace.path().join("container-tools")
}

#[test]
fn down_and_remove_manage_image_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
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
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
            .assert()
            .success();

        decune()
            .arg("down")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Stopped dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert_container_is_not_running(containers[0].id.as_deref().unwrap());
        });

        let stopped_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started existing dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0].id.as_deref(), Some(stopped_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state == "running")
            );
        });

        decune()
            .args(["rm", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert!(containers.is_empty());
        });

        decune()
            .arg("down")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "No dev container found for this workspace",
            ));

        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn down_removes_github_token_file_and_keeps_secret_directory_without_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let token_dir = runtime_home
        .join("decune")
        .join(&workspace_id)
        .join("secrets");
    let token_file = token_dir.join("github-token");
    let marker_file = token_dir.join("marker");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    fs::create_dir_all(&token_dir).unwrap();
    fs::write(&token_file, "github-test-secret\n").unwrap();
    fs::write(&marker_file, "keep\n").unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("down")
            .arg(&workspace_root)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "No dev container found for this workspace",
            ))
            .stderr(predicate::str::contains("github-test-secret").not());

        assert!(token_dir.is_dir());
        assert!(!token_file.exists());
        assert_eq!(fs::read_to_string(&marker_file).unwrap(), "keep\n");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_no_confirm_removes_github_token_file_before_docker_access() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let token_dir = runtime_home
        .join("decune")
        .join(&workspace_id)
        .join("secrets");
    let token_file = token_dir.join("github-token");
    let marker_file = token_dir.join("marker");
    let missing_docker_socket = path_roots.path().join("missing-docker.sock");

    fs::create_dir_all(&token_dir).unwrap();
    fs::write(&token_file, "github-test-secret\n").unwrap();
    fs::write(&marker_file, "keep\n").unwrap();

    decune()
        .args(["remove", "--no-confirm"])
        .arg(&workspace_root)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .env(
            "DOCKER_HOST",
            format!("unix://{}", missing_docker_socket.display()),
        )
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("github-test-secret").not());

    assert!(token_dir.is_dir());
    assert!(!token_file.exists());
    assert_eq!(fs::read_to_string(&marker_file).unwrap(), "keep\n");
}

#[test]
fn remove_without_no_confirm_fails_non_interactive_before_docker_and_state_cleanup() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let token_dir = runtime_dir.join("secrets");
    let token_file = token_dir.join("github-token");
    let missing_docker_socket = path_roots.path().join("missing-docker.sock");

    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&token_dir).unwrap();
    fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
    fs::write(runtime_dir.join("socket"), "").unwrap();
    fs::write(&token_file, "github-test-secret\n").unwrap();

    decune()
        .arg("remove")
        .arg(&workspace_root)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .env(
            "DOCKER_HOST",
            format!("unix://{}", missing_docker_socket.display()),
        )
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Cannot confirm remove in a non-interactive terminal",
        ))
        .stderr(predicate::str::contains("github-test-secret").not());

    assert!(state_dir.is_dir());
    assert!(runtime_dir.is_dir());
    assert!(token_file.is_file());
}

#[test]
fn remove_without_no_confirm_fails_non_interactive_without_removing_managed_volume() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let volume_name = format!("decune-remove-non-tty-{workspace_id}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_volumes(&workspace_root).unwrap();
        create_managed_volume(&workspace_root, &volume_name).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("remove")
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot confirm remove in a non-interactive terminal",
            ));

        runtime.block_on(async {
            let volumes = workspace_volumes(&workspace_root).unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });
    });

    runtime.block_on(async {
        cleanup_workspace_volumes(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_without_no_confirm_fails_non_interactive_without_removing_managed_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).unwrap();
        });

        decune()
            .arg("remove")
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot confirm remove in a non-interactive terminal",
            ));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state == "running")
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_no_confirm_stops_running_container_before_removal() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let marker = workspace_root.join("term-marker");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).unwrap();
        });

        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        assert_eq!(fs::read_to_string(&marker).unwrap(), "term\n");

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert!(containers.is_empty());
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_no_confirm_removes_state_and_runtime_directories() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let port_status_dir = runtime_home
        .join("decune")
        .join(format!("{workspace_id}-ports"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&port_status_dir).unwrap();
    fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
    fs::write(runtime_dir.join("socket"), "").unwrap();
    fs::write(port_status_dir.join("forward-status-stale.json"), "{}").unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        assert!(!state_dir.exists());
        assert!(!runtime_dir.exists());
        assert!(!port_status_dir.exists());
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_images_removes_workspace_images_only_when_requested() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_repository = workspace_image_repository(&workspace_root);
    let image_tag = format!("{image_repository}:remove-test");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
        create_workspace_image_tag(&workspace_root, "remove-test").unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).unwrap();
            assert_eq!(images, vec![image_tag.clone()]);
        });

        decune()
            .args(["remove", "--no-confirm", "--images"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).unwrap();
            assert!(images.is_empty());
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_all_workspaces_no_targets_succeeds_without_confirmation() {
    let temp = support::TempWorkspace::new().unwrap();
    let bin_dir = temp.path().join("bin");
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&state_home).unwrap();
    fs::create_dir_all(&runtime_home).unwrap();
    let docker_path = bin_dir.join("docker");
    fs::write(
        &docker_path,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
"#,
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No decune-managed workspace environments found",
        ));
}

#[test]
fn remove_all_workspaces_ignores_invalid_workspace_id_labels() {
    let temp = support::TempWorkspace::new().unwrap();
    let bin_dir = temp.path().join("bin");
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let victim_state_dir = state_home.join("victim");
    let victim_runtime_dir = runtime_home.join("victim");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&victim_state_dir).unwrap();
    fs::create_dir_all(&victim_runtime_dir).unwrap();
    fs::write(victim_state_dir.join("marker"), "keep\n").unwrap();
    fs::write(victim_runtime_dir.join("marker"), "keep\n").unwrap();
    let docker_path = bin_dir.join("docker");
    fs::write(
        &docker_path,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf '{"ID":"invalid-container"}\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  shift 2
  case "$*" in
    "invalid-container")
      printf '[{"Id":"invalid-container","Name":"/invalid","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"../victim","decune.workspace":"/work/invalid"}},"State":{"Running":true}}]\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'invalid-volume\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ]; then
  printf '[{"Name":"invalid-volume","Labels":{"decune.managed":"true","decune.workspace_id":"../victim"}}]\n'
  exit 0
fi

if [ "${1:-}" = stop ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
"#,
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No decune-managed workspace environments found",
        ));

    assert_eq!(
        fs::read_to_string(victim_state_dir.join("marker")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(victim_runtime_dir.join("marker")).unwrap(),
        "keep\n"
    );
    let commands = fs::read_to_string(command_log).unwrap();
    assert!(!commands.contains("rm --force --volumes invalid-container"));
    assert!(!commands.contains("volume rm --force invalid-volume"));
}

#[test]
fn remove_all_workspaces_ignores_invalid_state_directory_workspace_ids() {
    let temp = support::TempWorkspace::new().unwrap();
    let bin_dir = temp.path().join("bin");
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let invalid_workspace_id = "not-a-workspace";
    let invalid_state_dir = state_home.join("decune").join(invalid_workspace_id);
    let invalid_runtime_dir = runtime_home.join("decune").join(invalid_workspace_id);
    let state_workspace = temp.path().join("state-workspace");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&invalid_state_dir).unwrap();
    fs::create_dir_all(&invalid_runtime_dir).unwrap();
    fs::create_dir_all(&state_workspace).unwrap();
    fs::write(invalid_state_dir.join("marker"), "keep\n").unwrap();
    fs::write(invalid_runtime_dir.join("marker"), "keep\n").unwrap();
    fs::write(
        invalid_state_dir.join("state.toml"),
        format!(
            r#"version = 1
workspace = "{}"
container_id = "state-container"
image = "decune/state-workspace-not-a-workspace:statehash"
config_hash = "statehash"
compose_project_name = "user-owned"
created_at = "unix:1"
last_started_at = "unix:1"
"#,
            state_workspace.display()
        ),
    )
    .unwrap();
    let docker_path = bin_dir.join("docker");
    fs::write(
        &docker_path,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      exit 0
      ;;
    *"label=com.docker.compose.project=user-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      exit 0
      ;;
    *"label=com.docker.compose.project=user-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=com.docker.compose.project=user-owned"*)
      exit 0
      ;;
  esac
fi

echo "unexpected fake docker command: $*" >&2
exit 91
"#,
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No decune-managed workspace environments found",
        ));

    assert_eq!(
        fs::read_to_string(invalid_state_dir.join("marker")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(invalid_runtime_dir.join("marker")).unwrap(),
        "keep\n"
    );
    let commands = fs::read_to_string(command_log).unwrap();
    assert!(!commands.contains("user-owned"), "{commands}");
}

#[test]
fn remove_all_workspaces_no_confirm_removes_owned_resources_and_images() {
    let temp = support::TempWorkspace::new().unwrap();
    let bin_dir = temp.path().join("bin");
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let command_log = temp.path().join("docker.log");
    let state_workspace = temp.path().join("state-workspace");
    let state_workspace_id = "123456abcdef";
    let state_dir = state_home.join("decune").join(state_workspace_id);
    let runtime_dir = runtime_home.join("decune").join(state_workspace_id);
    fs::create_dir_all(&bin_dir).unwrap();
    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&state_workspace).unwrap();
    fs::write(
        state_dir.join("state.toml"),
        format!(
            r#"version = 1
workspace = "{}"
container_id = "state-container"
image = "decune/state-workspace-123456abcdef:statehash"
config_hash = "statehash"
compose_project_name = "state-owned"
created_at = "unix:1"
last_started_at = "unix:1"
"#,
            state_workspace.display()
        ),
    )
    .unwrap();
    let docker_path = bin_dir.join("docker");
    fs::write(
        &docker_path,
        r#"#!/usr/bin/env bash
set -euo pipefail
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"

if [ "${1:-}" = ps ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf '{"ID":"standalone-id"}\n'
      printf '{"ID":"compose-primary-id"}\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=compose-owned"*)
      printf '{"ID":"compose-primary-id"}\n'
      printf '{"ID":"compose-sidecar-id"}\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=state-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  shift 2
  case "$*" in
    "standalone-id compose-primary-id")
      printf '[{"Id":"standalone-id","Name":"/standalone","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"aaaaaaaaaaaa","decune.workspace":"/work/standalone-one"}},"State":{"Running":true}},{"Id":"compose-primary-id","Name":"/compose-owned-app-1","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"bbbbbbbbbbbb","decune.workspace":"/work/compose-one","com.docker.compose.project":"compose-owned","com.docker.compose.service":"app"}},"State":{"Running":true}}]\n'
      exit 0
      ;;
    "compose-primary-id compose-sidecar-id")
      printf '[{"Id":"compose-primary-id","Name":"/compose-owned-app-1","Config":{"Labels":{"decune.managed":"true","decune.workspace_id":"bbbbbbbbbbbb","com.docker.compose.project":"compose-owned"}},"State":{"Running":true}},{"Id":"compose-sidecar-id","Name":"/compose-owned-db-1","Config":{"Labels":{"com.docker.compose.project":"compose-owned","com.docker.compose.service":"db"}},"State":{"Running":false}}]\n'
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=decune.managed=true"*)
      printf 'standalone-volume\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=compose-owned"*)
      printf 'compose-volume\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=state-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = volume ] && [ "${2:-}" = inspect ]; then
  printf '[{"Name":"standalone-volume","Labels":{"decune.managed":"true","decune.workspace_id":"aaaaaaaaaaaa"}}]\n'
  exit 0
fi

if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case "$*" in
    *"label=com.docker.compose.project=compose-owned"*)
      printf 'compose-network\n'
      exit 0
      ;;
    *"label=com.docker.compose.project=state-owned"*)
      exit 0
      ;;
  esac
fi

if [ "${1:-}" = image ] && [ "${2:-}" = ls ]; then
  reference="${@: -1}"
  case "$reference" in
    decune/standalone-one-aaaaaaaaaaaa:*)
      printf '{"Repository":"decune/standalone-one-aaaaaaaaaaaa","Tag":"hash1"}\n'
      exit 0
      ;;
    decune/compose-one-bbbbbbbbbbbb:*)
      printf '{"Repository":"decune/compose-one-bbbbbbbbbbbb","Tag":"hash2"}\n'
      exit 0
      ;;
    decune/state-workspace-123456abcdef:*)
      printf '{"Repository":"decune/state-workspace-123456abcdef","Tag":"statehash"}\n'
      exit 0
      ;;
  esac
  exit 0
fi

if [ "${1:-}" = stop ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  exit 0
fi

echo "unexpected fake docker command: $*" >&2
exit 91
"#,
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["rm", "--all-workspaces", "--no-confirm", "--images"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Removed all decune-managed workspace environments",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(
        commands.contains("rm --force --volumes standalone-id"),
        "{commands}"
    );
    assert!(
        commands.contains("rm --force --volumes compose-primary-id"),
        "{commands}"
    );
    assert!(
        commands.contains("rm --force --volumes compose-sidecar-id"),
        "{commands}"
    );
    assert!(
        commands.contains("volume rm --force standalone-volume"),
        "{commands}"
    );
    assert!(
        commands.contains("volume rm --force compose-volume"),
        "{commands}"
    );
    assert!(
        commands.contains("network rm compose-network"),
        "{commands}"
    );
    assert!(
        commands.contains("image rm --no-prune --force decune/standalone-one-aaaaaaaaaaaa:hash1"),
        "{commands}"
    );
    assert!(
        commands.contains("image rm --no-prune --force decune/compose-one-bbbbbbbbbbbb:hash2"),
        "{commands}"
    );
    assert!(
        commands
            .contains("image rm --no-prune --force decune/state-workspace-123456abcdef:statehash"),
        "{commands}"
    );
    assert!(!commands.contains("user-owned"), "{commands}");
    assert!(!commands.contains("--rmi"), "{commands}");
    assert!(!state_dir.exists());
    assert!(!runtime_dir.exists());
}

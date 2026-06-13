use crate::harness::*;

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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = compose ]; then
  if [ ! -f "$DECUNE_TEST_WORKSPACE/.devcontainer/generated.env" ]; then
    echo "generated env missing before compose config" >&2
    exit 42
  fi
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      previous=
      generated_override=
      for argument in "$@"; do
        if [ "$previous" = "-f" ]; then
          case "$argument" in
            *compose.override.yaml) generated_override=$argument ;;
          esac
        fi
        previous=$argument
      done
      test -n "$generated_override"
      cat "$generated_override" > "$DECUNE_FAKE_OVERRIDE_LOG"
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"exited"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = wait ]; then
  printf '23\n'
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":false,"ExitCode":23}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":false,"ExitCode":23}}]\n'
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"}}}\n'
      exit 0
      ;;
    *" up -d "*)
      printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
      exit 0
      ;;
    *" ps --format json app "*)
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:alpine","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = ps ]; then
  case " $* " in
    *"com.docker.compose.service=app"*)
      exit 0
      ;;
    *"com.docker.compose.project="*)
      printf '{"ID":"old-compose-id"}\n'
      exit 0
      ;;
  esac
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'lookup-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  if [ "${3:-}" = old-compose-id ]; then
    printf '[{"Id":"old-compose-id","Name":"/old-service-1","Image":"alpine:3.19","ImageID":"sha256:old","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"old-workspace","decune.config_hash":"old-hash","com.docker.compose.project":"old-project","com.docker.compose.service":"old"}},"State":{"Running":true}}]\n'
    exit 0
  fi
  printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" stop "*)
      printf 'compose stop\n' >> "$DECUNE_FAKE_COMMAND_LOG"
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  count=0
  if [ -f "$DECUNE_FAKE_PS_COUNT" ]; then
    count=$(cat "$DECUNE_FAKE_PS_COUNT")
  fi
  count=$((count + 1))
  printf '%s' "$count" > "$DECUNE_FAKE_PS_COUNT"
  if [ "$count" -eq 1 ]; then
    exit 0
  fi
  printf '{"ID":"old-image-id"}\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"old-image-id","Name":"/old-image","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"workspace","decune.config_hash":"old-hash","devcontainer.config_file":".devcontainer/devcontainer.json"}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = stop ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
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
fn compose_clean_also_removes_leftover_image_mode_container() {
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" down "*)
      printf 'compose down\n' >> "$DECUNE_FAKE_COMMAND_LOG"
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  count=0
  if [ -f "$DECUNE_FAKE_PS_COUNT" ]; then
    count=$(cat "$DECUNE_FAKE_PS_COUNT")
  fi
  count=$((count + 1))
  printf '%s' "$count" > "$DECUNE_FAKE_PS_COUNT"
  if [ "$count" -eq 1 ]; then
    exit 0
  fi
  printf '{"ID":"old-image-id"}\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"old-image-id","Name":"/old-image","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"workspace","decune.config_hash":"old-hash","devcontainer.config_file":".devcontainer/devcontainer.json"}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = stop ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_PS_COUNT", &ps_count)
        .args(["clean", "--force"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed Docker Compose project"))
        .stderr(predicate::str::contains("Removed dev container: old-image"))
        .stderr(predicate::str::contains("Cleaned dev container resources"));

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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
project="decune-missing-config-$DECUNE_FAKE_WORKSPACE_ID"
if [ "${1:-}" = ps ]; then
  case " $* " in
    *"decune.workspace_id=$DECUNE_FAKE_WORKSPACE_ID"*)
      printf '{"ID":"compose-primary-id"}\n'
      exit 0
      ;;
    *"com.docker.compose.project=$project"*)
      printf '{"ID":"compose-primary-id"}\n'
      printf '{"ID":"compose-sidecar-id"}\n'
      exit 0
      ;;
  esac
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  case " $* " in
    *compose-sidecar-id*)
      printf '[{"Id":"compose-primary-id","Name":"/missing-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}},{"Id":"compose-sidecar-id","Name":"/missing-db-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"com.docker.compose.project":"%s","com.docker.compose.service":"db"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project" "$project"
      exit 0
      ;;
    *)
      printf '[{"Id":"compose-primary-id","Name":"/missing-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project"
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = stop ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);

    decune()
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
fn compose_clean_removes_existing_project_when_config_files_are_missing() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let command_log = host_tools.path().join("commands.log");
    let removed_marker = host_tools.path().join("removed");
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
project="decune-missing-config-$DECUNE_FAKE_WORKSPACE_ID"
if [ "${1:-}" = ps ]; then
  case " $* " in
    *"decune.workspace_id=$DECUNE_FAKE_WORKSPACE_ID"*)
      if [ -f "$DECUNE_FAKE_REMOVED_MARKER" ]; then
        exit 0
      fi
      printf '{"ID":"compose-primary-id"}\n'
      exit 0
      ;;
    *"com.docker.compose.project=$project"*)
      printf '{"ID":"compose-primary-id"}\n'
      printf '{"ID":"compose-sidecar-id"}\n'
      exit 0
      ;;
  esac
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  case " $* " in
    *compose-sidecar-id*)
      printf '[{"Id":"compose-primary-id","Name":"/missing-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}},{"Id":"compose-sidecar-id","Name":"/missing-db-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"com.docker.compose.project":"%s","com.docker.compose.service":"db"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project" "$project"
      exit 0
      ;;
    *)
      printf '[{"Id":"compose-primary-id","Name":"/missing-app-1","Image":"alpine:3.20","Config":{"Env":[],"Labels":{"decune.managed":"true","decune.workspace_id":"%s","com.docker.compose.project":"%s","com.docker.compose.service":"app"}},"State":{"Running":true}}]\n' "$DECUNE_FAKE_WORKSPACE_ID" "$project"
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = stop ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  : > "$DECUNE_FAKE_REMOVED_MARKER"
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  case " $* " in
    *"com.docker.compose.project=$project"*) printf 'missing_project_data\n' ;;
  esac
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  case " $* " in
    *"com.docker.compose.project=$project"*) printf 'missing_project_default\n' ;;
  esac
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = rm ]; then
  printf 'docker %s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_REMOVED_MARKER", &removed_marker)
        .env("DECUNE_FAKE_WORKSPACE_ID", &workspace_id)
        .args(["clean", "--force"])
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
        .stderr(predicate::str::contains("Cleaned dev container resources"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("docker rm --force --volumes compose-primary-id"));
    assert!(commands.contains("docker rm --force --volumes compose-sidecar-id"));
    assert!(commands.contains("docker volume rm --force missing_project_data"));
    assert!(commands.contains("docker network rm missing_project_default"));
    assert!(!commands.contains("compose down"));
    assert!(!commands.contains("image rm"));
}

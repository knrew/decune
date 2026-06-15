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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"example/app:dev"},"sidecar":{"image":"example/sidecar:dev"}}}\n'
      exit 0
      ;;
    *" build "*)
      case "$*" in
        *"--no-cache --pull"*) exit 0 ;;
      esac
      echo "compose build did not receive --no-cache and --pull: $*" >&2
      exit 42
      ;;
    *" pull "*)
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
if [ "${1:-}" = build ]; then
  case "$*" in
    *"--tag decune/"*"--pull"*)
      echo "Generated Feature build must not receive --pull: $*" >&2
      exit 43
      ;;
    *"--tag decune/"*"--no-cache"*) cat >/dev/null; exit 0 ;;
  esac
  echo "Feature build did not receive --no-cache: $*" >&2
  exit 43
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:test","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = pull ]; then
  printf '{"status":"pulled"}\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = pull ]; then
  printf '{"status":"pulled"}\n'
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
    assert!(generated_override.contains("'FROM_PRIMARY_FEATURE': 'yes'"));
    assert!(!generated_override.contains("sidecar"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose"));
    assert!(commands.contains("build --no-cache --pull"));
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20","user":"appuser"}}}\n'
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
  printf 'appuser:x:1001:1001::/home/appuser:/bin/sh\n'
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
    assert!(commands.contains("exec --user appuser"));
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
    *" stop "*)
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  case " $* " in
    *passwd*)
      printf 'root:x:0:0:root:/root:/bin/sh\n'
      exit 0
      ;;
    *" printf post-start"*|*" printf post-attach"*)
      exit 0
      ;;
    *" /usr/local/bin/decune-shell"*)
      exit 7
      ;;
  esac
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
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
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
    assert!(
        commands
            .lines()
            .any(|command| command.starts_with("compose ") && command.ends_with(" stop"))
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
    *" stop "*)
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  case " $* " in
    *passwd*)
      printf 'root:x:0:0:root:/root:/bin/sh\n'
      exit 0
      ;;
    *" printf post-start"*|*" printf post-attach"*)
      exit 0
      ;;
    *" /usr/local/bin/decune-shell"*)
      exit 0
      ;;
  esac
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" config --format json "*)
      printf '{"services":{"app":{"image":"alpine:3.20"},"sidecar":{"image":"alpine:3.20"}}}\n'
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
  case " $* " in
    *passwd*)
      printf 'root:x:0:0:root:/root:/bin/sh\n'
      exit 0
      ;;
    *"test -x /run/decune/git-credential-decune && test -w /run/decune/host-daemon.sock"*)
      exit 0
      ;;
    *"git config --global --add credential.helper /run/decune/git-credential-decune"*)
      exit 0
      ;;
  esac
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
      if [ -f "$DECUNE_FAKE_STOPPED_MARKER" ]; then
        printf '[]\n'
      else
        printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      fi
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  case " $* " in
    *passwd*)
      printf 'root:x:0:0:root:/root:/bin/sh\n'
      exit 0
      ;;
    *" /usr/local/bin/decune-shell"*)
      : > "$DECUNE_FAKE_STOPPED_MARKER"
      exit 0
      ;;
  esac
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
  if [ -f "$DECUNE_FAKE_STOPPED_MARKER" ]; then
    printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":false}}]\n'
  else
    printf '[{"Id":"compose-app-id","Name":"/compose-app-1","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  fi
  exit 0
fi
if [ "${1:-}" = stop ]; then
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
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
      printf '[{"ID":"compose-app-id","Name":"compose-app-1","Service":"app","State":"running"}]\n'
      exit 0
      ;;
    *" stop "*)
      echo "detached up must not stop compose for shutdownAction" >&2
      exit 42
      ;;
  esac
fi
if [ "${1:-}" = exec ]; then
  case " $* " in
    *passwd*)
      printf 'root:x:0:0:root:/root:/bin/sh\n'
      exit 0
      ;;
    *" printf post-start"*)
      exit 0
      ;;
    *" printf post-attach"*|*" /usr/local/bin/decune-shell"*)
      echo "detached up must not run attach lifecycle or shell" >&2
      exit 43
      ;;
  esac
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

#[test]
fn compose_clean_images_removes_only_decune_generated_workspace_images() {
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
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = compose ]; then
  case " $* " in
    *" down "*)
      case " $* " in
        *" --rmi "*) echo "compose down must not remove user images" >&2; exit 44 ;;
      esac
      exit 0
      ;;
  esac
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = volume ] && [ "${2:-}" = ls ]; then
  exit 0
fi
if [ "${1:-}" = network ] && [ "${2:-}" = ls ]; then
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = ls ]; then
  reference=
  for argument in "$@"; do
    reference=$argument
  done
  if [ "$reference" != "$DECUNE_FAKE_IMAGE_REPOSITORY:*" ]; then
    echo "unexpected image list reference: $reference" >&2
    exit 45
  fi
  printf '{"Repository":"%s","Tag":"final-hash"}\n' "$DECUNE_FAKE_IMAGE_REPOSITORY"
  printf '{"Repository":"example/sidecar","Tag":"dev"}\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  if [ "${3:-}" = "--no-prune" ] && [ "${4:-}" = "--force" ] && [ "${5:-}" = "$DECUNE_FAKE_IMAGE_REPOSITORY:final-hash" ] && [ "$#" -eq 5 ]; then
    exit 0
  fi
  echo "unexpected image removal: $*" >&2
  exit 46
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
    let image_repository = workspace_image_repository(&workspace_root);

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("DECUNE_FAKE_IMAGE_REPOSITORY", &image_repository)
        .args(["clean", "--force", "--images"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Removed Docker Compose project"))
        .stderr(predicate::str::contains(format!(
            "Removed Docker image: {image_repository}:final-hash"
        )))
        .stderr(predicate::str::contains("Cleaned dev container resources"));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(commands.contains("compose"));
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

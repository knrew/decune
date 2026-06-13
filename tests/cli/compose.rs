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
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Docker Compose lifecycle is not implemented yet",
        ));

    assert!(workspace_root.join(".devcontainer/generated.env").is_file());
}

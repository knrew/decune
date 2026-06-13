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

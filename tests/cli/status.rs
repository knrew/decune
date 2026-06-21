use crate::harness::*;

#[test]
fn status_reports_no_managed_workspace_environments() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);

    decune()
        .args(["status"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stdout("No decune-managed workspace environments found\n")
        .stderr(predicate::str::is_empty());
}

#[test]
fn status_workspace_reports_not_created() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{ "image": "alpine:3.20" }"#,
        )
        .unwrap();
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);

    decune()
        .args(["status"])
        .arg(workspace.path())
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stdout(predicate::str::contains("Runtime: not-created"))
        .stdout(predicate::str::contains("Mode: image"))
        .stdout(predicate::str::contains(
            "No active ports for this workspace",
        ))
        .stdout(predicate::str::contains(
            "Run decune up to create the environment.",
        ))
        .stderr(predicate::str::is_empty());
}

#[derive(Debug)]
struct StatusRoots {
    state: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
}

fn status_roots(temp: &support::TempWorkspace) -> StatusRoots {
    StatusRoots {
        state: temp.create_dir("state").unwrap(),
        cache: temp.create_dir("cache").unwrap(),
        config: temp.create_dir("config").unwrap(),
        runtime: temp.create_dir("runtime").unwrap(),
    }
}

fn fake_empty_docker_path(temp: &support::TempWorkspace) -> String {
    let bin_dir = temp.create_dir("bin").unwrap();
    let docker_path = bin_dir.join("docker");
    fs::write(
        &docker_path,
        r#"#!/bin/sh
case "$*" in
  "ps --all --filter label=decune.managed=true --format json") exit 0 ;;
  "ps --all --filter label=decune.managed=true --filter label=decune.workspace_id="*" --format json") exit 0 ;;
  "volume ls --filter label=decune.managed=true --format {{.Name}}") exit 0 ;;
  "volume ls --filter label=decune.managed=true --filter label=decune.workspace_id="*" --format {{.Name}}") exit 0 ;;
esac
echo "unexpected fake docker command: $*" >&2
exit 64
"#,
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

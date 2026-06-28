use crate::harness::*;
use std::{
    io::{Read, Write},
    thread,
};

const WORKSPACE_A_ID: &str = "aaaaaaaaaaaa";
const WORKSPACE_B_ID: &str = "bbbbbbbbbbbb";

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

#[test]
fn status_workspace_with_creatable_bind_mount_does_not_create_source() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"{ "image": "alpine:3.20" }"#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[mounts]]
source = "generated/cache"
target = "/cache"
type = "bind"
create = "directory"
"#,
        )
        .unwrap();
    let source = workspace.path().join("generated/cache");
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);

    let output = decune()
        .args(["status"])
        .arg(workspace.path())
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();

    assert!(!source.exists());
    assert!(output.contains("Runtime: not-created"));
    assert!(output.contains("Config: current"));
    assert!(output.contains(
        "not-created [info]: No decune-managed environment exists for this workspace yet."
    ));
    assert!(output.contains("not-created: Run decune up to create the environment."));
}

#[test]
fn status_workspace_detail_reports_issue_codes_severities_and_all_actions() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "mounts": [
                "source=${localWorkspaceFolder}/missing,target=/cache,type=bind"
              ]
            }
            "#,
        )
        .unwrap();
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);

    let output = decune()
        .args(["status"])
        .arg(workspace.path())
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains(
        "config-unreadable [warning]: The current devcontainer configuration could not be read."
    ));
    assert!(output.contains(
        "not-created [info]: No decune-managed environment exists for this workspace yet."
    ));
    assert!(output.contains("config-unreadable: Fix the configuration error, then retry."));
    assert!(output.contains("not-created: Run decune up to create the environment."));
}

#[test]
fn status_summary_reports_state_workspaces_sorted_by_path() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);
    let workspace_b = temp.create_dir("workspace-b").unwrap();
    let workspace_a = temp.create_dir("workspace-a").unwrap();
    write_state(&roots, WORKSPACE_B_ID, &workspace_b, None);
    write_state(&roots, WORKSPACE_A_ID, &workspace_a, Some("unix:1"));

    let output = decune()
        .args(["status"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();

    assert!(output.contains("Found 2 decune-managed workspace environments"));
    assert!(output.contains("ID"));
    assert!(output.contains("FWD/PUB"));
    assert!(output.contains("LAST_USED"));
    let first_workspace_position = output.find(WORKSPACE_A_ID).unwrap();
    let second_workspace_position = output.find(WORKSPACE_B_ID).unwrap();
    assert!(
        first_workspace_position < second_workspace_position,
        "{output}"
    );
}

#[test]
fn status_summary_does_not_fallback_last_used_to_created_or_started() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);
    let workspace = temp.create_dir("workspace").unwrap();
    write_state(&roots, WORKSPACE_A_ID, &workspace, None);

    let output = decune()
        .args(["status"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let row = status_row(&output, WORKSPACE_A_ID);
    let columns = row.split_whitespace().collect::<Vec<_>>();

    assert_eq!(columns[5], "0/0");
    assert_eq!(columns[7], "-");
}

#[test]
fn status_summary_reports_corrupt_state_without_removing_it() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);
    let state_dir = roots.state.join("decune").join(WORKSPACE_A_ID);
    fs::create_dir_all(&state_dir).unwrap();
    let state_path = state_dir.join("state.toml");
    fs::write(&state_path, "version = \"invalid\"\n").unwrap();

    let output = decune()
        .args(["status"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stderr(predicate::str::contains(
            "Ignoring invalid decune state file",
        ))
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let row = status_row(&output, WORKSPACE_A_ID);
    let columns = row.split_whitespace().collect::<Vec<_>>();

    assert_eq!(columns[1], "<unknown>");
    assert_eq!(columns[3], "unreadable");
    assert_eq!(columns[6], "1");
    assert!(state_path.exists());
}

#[test]
fn status_summary_reports_active_forwarded_port_count() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = status_roots(&temp);
    let workspace = temp.create_dir("workspace").unwrap();
    write_state(&roots, WORKSPACE_A_ID, &workspace, None);
    let _server = fake_forward_status_server(&roots, WORKSPACE_A_ID);

    let output = decune()
        .args(["status"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stderr(predicate::str::is_empty())
        .get_output()
        .stdout
        .clone();
    let output = String::from_utf8(output).unwrap();
    let row = status_row(&output, WORKSPACE_A_ID);
    let columns = row.split_whitespace().collect::<Vec<_>>();

    assert_eq!(columns[5], "1/0");
}

#[test]
fn status_workspace_without_devcontainer_metadata_is_an_error() {
    let workspace = support::TempWorkspace::new().unwrap();
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
        .failure()
        .stderr(predicate::str::contains(
            "Devcontainer metadata file was not found",
        ));
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

fn write_state(
    roots: &StatusRoots,
    workspace_id: &str,
    workspace: &Path,
    last_used_at: Option<&str>,
) {
    let state_dir = roots.state.join("decune").join(workspace_id);
    fs::create_dir_all(&state_dir).unwrap();
    let last_used = last_used_at
        .map(|value| format!("last_used_at = \"{value}\"\n"))
        .unwrap_or_default();
    fs::write(
        state_dir.join("state.toml"),
        format!(
            r#"version = 1
workspace = "{}"
container_id = "container-{workspace_id}"
image = "decune:test"
config_hash = "hash"
created_at = "unix:1"
last_started_at = "unix:1"
{last_used}
"#,
            workspace.display()
        ),
    )
    .unwrap();
}

fn status_row<'a>(output: &'a str, workspace_id: &str) -> &'a str {
    output
        .lines()
        .find(|line| line.starts_with(workspace_id))
        .unwrap_or_else(|| panic!("missing status row for {workspace_id} in:\n{output}"))
}

fn fake_forward_status_server(roots: &StatusRoots, workspace_id: &str) -> thread::JoinHandle<()> {
    let status_dir = roots
        .runtime
        .join("decune")
        .join(format!("{workspace_id}-ports"));
    fs::create_dir_all(&status_dir).unwrap();
    let socket_name = "forward-status-test.sock";
    let socket_path = status_dir.join(socket_name);
    let listener = UnixListener::bind(&socket_path).unwrap();
    fs::write(
        status_dir.join("forward-status-test.json"),
        format!(r#"{{"version":1,"session_id":"test","socket_name":"{socket_name}","pid":1}}"#),
    )
    .unwrap();

    thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        stream.read_to_end(&mut request).unwrap();
        stream
            .write_all(
                br#"{"version":1,"ports":[{"host_ip":"127.0.0.1","host_port":3100,"requested_host_port":3000,"service":null,"container_port":3000,"protocol":"tcp","source":"configured","label":"web"}]}"#,
            )
            .unwrap();
    })
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

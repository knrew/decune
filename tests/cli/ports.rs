use std::{
    fs::File,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    process::{Command as ProcessCommand, Stdio},
    thread,
    time::{Duration, Instant},
};

use crate::harness::*;

#[test]
fn ports_reports_no_active_host_ports() {
    let workspace = support::TempWorkspace::new().unwrap();

    decune()
        .args(["ports"])
        .arg(workspace.path())
        .assert()
        .success()
        .stdout("No active ports for this workspace\n")
        .stderr(predicate::str::is_empty());
}

#[test]
fn ports_json_reports_no_active_host_ports() {
    let workspace = support::TempWorkspace::new().unwrap();

    decune()
        .args(["ports", "--json"])
        .arg(workspace.path())
        .assert()
        .success()
        .stdout("[]\n")
        .stderr(predicate::str::is_empty());
}

#[test]
fn ports_all_reports_no_active_host_ports() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = ports_roots(&temp);

    decune()
        .args(["ports", "--all"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stdout("No active ports\n")
        .stderr(predicate::str::is_empty());
}

#[test]
fn ports_all_json_reports_no_active_host_ports() {
    let temp = support::TempWorkspace::new().unwrap();
    let fake_path = fake_empty_docker_path(&temp);
    let roots = ports_roots(&temp);

    decune()
        .args(["ports", "--all", "--json"])
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stdout("[]\n")
        .stderr(predicate::str::is_empty());
}

#[test]
fn ports_reports_relocated_compose_published_port() {
    let temp = support::TempWorkspace::new().unwrap();
    let workspace = temp.create_dir("workspace").unwrap();
    let workspace_root = workspace.canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let roots = ports_roots(&temp);
    write_relocated_compose_state(&roots, &workspace_id, &workspace_root, 3000, 3001);
    let fake_path = fake_compose_published_port_docker_path(&temp, &workspace_id, 3001);

    decune()
        .args(["ports"])
        .arg(&workspace_root)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stdout(
            predicate::str::contains("TYPE")
                .and(predicate::str::contains("STATE"))
                .and(predicate::str::contains("*:3001"))
                .and(predicate::str::contains("published"))
                .and(predicate::str::contains("web:3000/tcp"))
                .and(predicate::str::contains("compose"))
                .and(predicate::str::contains("*:3000"))
                .and(predicate::str::contains("relocated")),
        )
        .stderr(predicate::str::is_empty());
}

#[test]
fn ports_json_reports_relocated_compose_published_port_metadata() {
    let temp = support::TempWorkspace::new().unwrap();
    let workspace = temp.create_dir("workspace").unwrap();
    let workspace_root = workspace.canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let roots = ports_roots(&temp);
    write_relocated_compose_state(&roots, &workspace_id, &workspace_root, 3000, 3001);
    let fake_path = fake_compose_published_port_docker_path(&temp, &workspace_id, 3001);

    decune()
        .args(["ports", "--json"])
        .arg(&workspace_root)
        .env("PATH", &fake_path)
        .env("XDG_STATE_HOME", &roots.state)
        .env("XDG_CACHE_HOME", &roots.cache)
        .env("XDG_CONFIG_HOME", &roots.config)
        .env("XDG_RUNTIME_DIR", &roots.runtime)
        .assert()
        .success()
        .stdout(
            predicate::str::contains(r#""type": "published""#)
                .and(predicate::str::contains(r#""source": "compose""#))
                .and(predicate::str::contains(r#""service": "web""#))
                .and(predicate::str::contains(r#""port_entry_index": 0"#))
                .and(predicate::str::contains(r#""target": {"#))
                .and(predicate::str::contains(r#""port": 3000"#))
                .and(predicate::str::contains(r#""requested": {"#))
                .and(predicate::str::contains(r#""host_ip": null"#))
                .and(predicate::str::contains(r#""planned": {"#))
                .and(predicate::str::contains(r#""host_port": 3001"#))
                .and(predicate::str::contains(r#""actual_bindings": ["#))
                .and(predicate::str::contains(r#""host_ip": "0.0.0.0""#))
                .and(predicate::str::contains(r#""host_ip": "::""#))
                .and(predicate::str::contains(r#""relocated": true"#)),
        )
        .stderr(predicate::str::is_empty());
}

#[test]
fn up_detach_rejects_cli_port_before_workspace_resolution() {
    decune()
        .args(["up", "--detach", "-p", "3000", "/decune/missing-workspace"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Port forwarding is not supported with --detach",
        ));
}

#[derive(Debug)]
struct PortsRoots {
    state: PathBuf,
    cache: PathBuf,
    config: PathBuf,
    runtime: PathBuf,
}

fn ports_roots(temp: &support::TempWorkspace) -> PortsRoots {
    PortsRoots {
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

fn write_relocated_compose_state(
    roots: &PortsRoots,
    workspace_id: &str,
    workspace_root: &Path,
    requested_port: u16,
    planned_port: u16,
) {
    let state_dir = roots.state.join("decune").join(workspace_id);
    fs::create_dir_all(&state_dir).unwrap();
    fs::write(
        state_dir.join("state.toml"),
        format!(
            r#"version = 1
workspace = "{}"
container_id = "compose-web-id"
image = "decune:test"
config_hash = "hash"
compose_project_name = "decune-test-{workspace_id}"
created_at = "unix:1"
last_started_at = "unix:1"

[[published_ports]]
source = "compose"
type = "published"
service = "web"
port_entry_index = 0
relocated = true

[published_ports.target]
port = 3000
protocol = "tcp"

[published_ports.requested]
host_ip_kind = "omitted"
host_port = {requested_port}

[published_ports.planned]
host_ip_kind = "omitted"
host_port = {planned_port}

[[published_ports.actual_bindings]]
host_ip = "0.0.0.0"
host_port = {planned_port}

[[published_ports.actual_bindings]]
host_ip = "::"
host_port = {planned_port}
"#,
            workspace_root.display()
        ),
    )
    .unwrap();
}

fn fake_compose_published_port_docker_path(
    temp: &support::TempWorkspace,
    workspace_id: &str,
    planned_port: u16,
) -> String {
    let bin_dir = temp.create_dir("bin").unwrap();
    let docker_path = bin_dir.join("docker");
    fs::write(
        &docker_path,
        format!(
            r#"#!/bin/sh
set -eu
project="decune-test-{workspace_id}"
case "$*" in
  *"ps --all"*"label=decune.managed=true"*"label=decune.workspace_id={workspace_id}"*"--format json"*)
    exit 0
    ;;
  *"ps --all"*"label=com.docker.compose.project=$project"*"--format json"*)
    printf '{{"ID":"compose-web-id"}}\n'
    exit 0
    ;;
  "container inspect compose-web-id")
    printf '[{{"Id":"compose-web-id","Name":"/compose-web-1","Config":{{"Labels":{{"com.docker.compose.project":"%s","com.docker.compose.service":"web"}}}},"State":{{"Running":true}},"NetworkSettings":{{"Ports":{{"3000/tcp":[{{"HostIp":"0.0.0.0","HostPort":"%s"}},{{"HostIp":"::","HostPort":"%s"}}]}}}}}}]\n' "$project" "{planned_port}" "{planned_port}"
    exit 0
    ;;
esac
echo "unexpected fake docker command: $*" >&2
exit 64
"#
        ),
    )
    .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    format!(
        "{}:{}",
        bin_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

#[test]
fn rebuild_detach_rejects_cli_port_before_workspace_resolution() {
    decune()
        .args([
            "rebuild",
            "--detach",
            "-p",
            "3000",
            "/decune/missing-workspace",
        ])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Port forwarding is not supported with --detach",
        ));
}

#[test]
fn up_attached_forwards_manual_port_to_container_localhost() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
                '#!/bin/sh' \
                'sleep 60' \
                'exit 0' \
                >/usr/local/bin/decune-wait-shell \
              && chmod +x /usr/local/bin/decune-wait-shell \
              && printf '%s\n' \
                '#!/bin/sh' \
                'printf "HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nforward-ok"' \
                >/usr/local/bin/decune-http-response \
              && chmod +x /usr/local/bin/decune-http-response
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "workspaceFolder": "/workspace",
              "postStartCommand": "nc -lk -s 127.0.0.1 -p 4321 -e /usr/local/bin/decune-http-response >/tmp/decune-nc.log 2>&1 </dev/null &"
            }
            "#,
        )
        .unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-wait-shell"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let stderr_path = workspace_root.join(".decune-up-stderr");
    let host_port = available_host_port();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    let mut child = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("decune"));
        command
            .args([
                "up",
                "--no-auto-forward",
                "-p",
                &format!("{host_port}:4321"),
            ])
            .arg(&workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&stderr_path).unwrap()));
        child = Some(command.spawn().unwrap());

        if let Err(error) = wait_for_forwarded_http_response(host_port) {
            let status = child.as_mut().unwrap().try_wait().unwrap();
            let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!(
                "forwarded HTTP response did not arrive: {error}; child_status={status:?}; stderr={stderr}"
            );
        }
        decune()
            .args(["ports", "--json"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(
                predicate::str::contains(r#""host_port": "#)
                    .and(predicate::str::contains(host_port.to_string()))
                    .and(predicate::str::contains(r#""container_port": 4321"#))
                    .and(predicate::str::contains(r#""source": "configured""#)),
            );
        let child = child.as_mut().unwrap();
        child.kill().unwrap();
        _ = child.wait().unwrap();
        decune()
            .args(["ports", "--json"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout("[]\n");
    }));

    if let Some(mut child) = child
        && child.try_wait().unwrap().is_none()
    {
        _ = child.kill();
        _ = child.wait();
    }
    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_attached_forwards_manual_port_when_image_default_user_is_non_root() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -u 20001 -h /home/decune decune \
              && printf '%s\n' \
                '#!/bin/sh' \
                'sleep 60' \
                'exit 0' \
                >/usr/local/bin/decune-wait-shell \
              && chmod +x /usr/local/bin/decune-wait-shell \
              && printf '%s\n' \
                '#!/bin/sh' \
                'printf "HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nforward-ok"' \
                >/usr/local/bin/decune-http-response \
              && chmod +x /usr/local/bin/decune-http-response
            USER decune
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "workspaceFolder": "/workspace",
              "postStartCommand": "nc -lk -s 127.0.0.1 -p 4321 -e /usr/local/bin/decune-http-response >/tmp/decune-nc.log 2>&1 </dev/null &"
            }
            "#,
        )
        .unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-wait-shell"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let stderr_path = workspace_root.join(".decune-up-stderr");
    let host_port = available_host_port();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    let mut child = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("decune"));
        command
            .args([
                "up",
                "--no-auto-forward",
                "-p",
                &format!("{host_port}:4321"),
            ])
            .arg(&workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&stderr_path).unwrap()));
        child = Some(command.spawn().unwrap());

        if let Err(error) = wait_for_forwarded_http_response(host_port) {
            let status = child.as_mut().unwrap().try_wait().unwrap();
            let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!(
                "forwarded HTTP response did not arrive: {error}; child_status={status:?}; stderr={stderr}"
            );
        }
        let child = child.as_mut().unwrap();
        child.kill().unwrap();
        _ = child.wait().unwrap();
    }));

    if let Some(mut child) = child
        && child.try_wait().unwrap().is_none()
    {
        _ = child.kill();
        _ = child.wait();
    }
    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_attached_auto_forwards_new_container_listen_port() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_port = available_host_port_in_range(20_000, 32_000);
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
                '#!/bin/sh' \
                'sleep 60' \
                'exit 0' \
                >/usr/local/bin/decune-wait-shell \
              && chmod +x /usr/local/bin/decune-wait-shell \
              && printf '%s\n' \
                '#!/bin/sh' \
                'printf "HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nforward-ok"' \
                >/usr/local/bin/decune-http-response \
              && chmod +x /usr/local/bin/decune-http-response
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
            {{
              "build": {{
                "dockerfile": "Dockerfile"
              }},
              "workspaceMount": "source=${{localWorkspaceFolder}},target=/workspace,type=bind",
              "workspaceFolder": "/workspace",
              "postStartCommand": "nc -lk -s 127.0.0.1 -p {container_port} -e /usr/local/bin/decune-http-response >/tmp/decune-nc.log 2>&1 </dev/null &"
            }}
            "#
            ),
        )
        .unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-wait-shell"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let stderr_path = workspace_root.join(".decune-up-stderr");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    let mut child = None;
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut command = ProcessCommand::new(assert_cmd::cargo::cargo_bin("decune"));
        command
            .arg("up")
            .arg(&workspace_root)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(&stderr_path).unwrap()));
        child = Some(command.spawn().unwrap());

        if let Err(error) = wait_for_forwarded_http_response(container_port) {
            let status = child.as_mut().unwrap().try_wait().unwrap();
            let stderr = fs::read_to_string(&stderr_path).unwrap_or_default();
            panic!(
                "auto forwarded HTTP response did not arrive: {error}; child_status={status:?}; stderr={stderr}"
            );
        }
        let child = child.as_mut().unwrap();
        child.kill().unwrap();
        _ = child.wait().unwrap();
    }));

    if let Some(mut child) = child
        && child.try_wait().unwrap().is_none()
    {
        _ = child.kill();
        _ = child.wait();
    }
    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_publishes_app_port_to_requested_host_port() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_port = available_host_port();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
                '#!/bin/sh' \
                'printf "HTTP/1.0 200 OK\r\nContent-Length: 10\r\n\r\nforward-ok"' \
                >/usr/local/bin/decune-http-response \
              && chmod +x /usr/local/bin/decune-http-response
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
            {{
              "build": {{
                "dockerfile": "Dockerfile"
              }},
              "workspaceMount": "source=${{localWorkspaceFolder}},target=/workspace,type=bind",
              "workspaceFolder": "/workspace",
              "forwardPorts": [4321],
              "appPort": ["127.0.0.1:{host_port}:4321"],
              "postStartCommand": "nc -lk -p 4321 -e /usr/local/bin/decune-http-response >/tmp/decune-nc.log 2>&1 </dev/null &"
            }}
            "#
            ),
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach", "--no-auto-forward"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(
                predicate::str::contains("Port forwarding is ignored in detached mode")
                    .and(predicate::str::contains("Started dev container"))
                    .and(predicate::str::contains("publishes appPort through Docker").not()),
            );

        if let Err(error) = wait_for_forwarded_http_response(host_port) {
            panic!("published HTTP response did not arrive: {error}");
        }
        decune()
            .args(["ports", "--json"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(
                predicate::str::contains(r#""type": "published""#)
                    .and(predicate::str::contains(r#""source": "appPort""#))
                    .and(predicate::str::contains(r#""host_ip": "127.0.0.1""#))
                    .and(predicate::str::contains(format!(
                        r#""host_port": {host_port}"#
                    )))
                    .and(predicate::str::contains(r#""container_port": 4321"#)),
            );
        decune()
            .args(["ports", "--all", "--json"])
            .assert()
            .success()
            .stdout(
                predicate::str::contains(r#""type": "published""#)
                    .and(predicate::str::contains(r#""source": "appPort""#))
                    .and(predicate::str::contains(format!(
                        r#""workspace_id": "{}""#,
                        workspace_id(&workspace_root)
                    ))),
            );
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_warns_when_app_port_has_no_host_ip() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_port = available_host_port();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
            {{
              "image": "alpine:3.20",
              "appPort": ["{host_port}:4321"]
            }}
            "#
            ),
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach", "--no-auto-forward"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(
                predicate::str::contains("publishes appPort through Docker")
                    .and(predicate::str::contains("forwardPorts"))
                    .and(predicate::str::contains("[[ports]]"))
                    .and(predicate::str::contains("localhost-only"))
                    .and(predicate::str::contains("Started dev container")),
            );
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn available_host_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
}

fn available_host_port_in_range(start: u16, end: u16) -> u16 {
    for port in start..end {
        if let Ok(listener) = TcpListener::bind(("127.0.0.1", port)) {
            drop(listener);
            return port;
        }
    }
    panic!("no available host port in range {start}..{end}");
}

fn wait_for_forwarded_http_response(host_port: u16) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut last_error = None;
    while Instant::now() < deadline {
        match read_http_response(host_port) {
            Ok(response) if response.contains("forward-ok") => return Ok(()),
            Ok(response) => last_error = Some(format!("unexpected response: {response:?}")),
            Err(error) => last_error = Some(error.to_string()),
        }
        thread::sleep(Duration::from_millis(200));
    }

    Err(last_error.unwrap_or_else(|| "no attempts were made".to_owned()))
}

fn read_http_response(host_port: u16) -> std::io::Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", host_port))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")?;
    let mut response = String::new();
    stream.read_to_string(&mut response)?;
    Ok(response)
}

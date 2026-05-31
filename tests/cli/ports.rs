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
        let _ = child.wait().unwrap();
    }));

    if let Some(mut child) = child {
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
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
        let _ = child.wait().unwrap();
    }));

    if let Some(mut child) = child {
        if child.try_wait().unwrap().is_none() {
            let _ = child.kill();
            let _ = child.wait();
        }
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

fn available_host_port() -> u16 {
    let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
    listener.local_addr().unwrap().port()
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

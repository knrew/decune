use assert_cmd::Command;
use bollard::{
    Docker,
    models::{
        ContainerConfig, ContainerCreateBody, ContainerSummary, HostConfig, VolumeCreateRequest,
    },
    query_parameters::{
        CommitContainerOptionsBuilder, CreateContainerOptionsBuilder, CreateImageOptionsBuilder,
        ListContainersOptionsBuilder, ListImagesOptionsBuilder, ListVolumesOptionsBuilder,
        RemoveContainerOptionsBuilder, RemoveImageOptionsBuilder, RemoveVolumeOptionsBuilder,
        StartContainerOptionsBuilder, TagImageOptionsBuilder, WaitContainerOptionsBuilder,
    },
};
use futures_util::TryStreamExt;
use predicates::prelude::*;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fs,
    os::unix::{fs::PermissionsExt, net::UnixListener},
    path::Path,
};

mod support;

fn decune() -> Command {
    Command::cargo_bin("decune").unwrap()
}

#[test]
fn root_help_is_displayed() {
    decune()
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "Run dev containers from the command line.",
        ))
        .stdout(predicate::str::contains("up"))
        .stdout(predicate::str::contains("down"))
        .stdout(predicate::str::contains("clean"))
        .stdout(predicate::str::contains("rebuild"))
        .stderr(predicate::str::is_empty());
}

#[test]
fn command_help_is_displayed() {
    for command in ["up", "down", "clean", "rebuild"] {
        decune()
            .args([command, "--help"])
            .assert()
            .success()
            .stdout(predicate::str::contains("Workspace directory"))
            .stdout(predicate::str::contains("WORKSPACE"))
            .stderr(predicate::str::is_empty());
    }
}

#[test]
fn up_detach_creates_and_reuses_image_container() {
    let workspace = support::TempWorkspace::new().unwrap();
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state.to_string() == "running")
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_runs_initialize_when_reusing_running_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "initializeCommand": "printf x >> .decune-initialize-count"
            }
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let initialize_count_path = workspace_root.join(".decune-initialize-count");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));

        assert_eq!(fs::read_to_string(&initialize_count_path).unwrap(), "xx");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_runs_initialize_when_starting_stopped_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "initializeCommand": "printf x >> .decune-initialize-count"
            }
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let initialize_count_path = workspace_root.join(".decune-initialize-count");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .arg("down")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Stopped dev container"));

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started existing dev container"));

        assert_eq!(fs::read_to_string(&initialize_count_path).unwrap(), "xx");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_sets_git_credential_helper_through_host_daemon() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM ubuntu:24.04
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ]; then' \
              '  shift' \
              '  case "$*" in' \
              '    "--global --unset-all credential.helper")' \
              '      rm -f /tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global --add credential.helper "*)' \
              '      printf "%s\n" "$4" >/tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global user.name "*|"--global user.email "*)' \
              '      exit 0' \
              '      ;;' \
              '  esac' \
              'fi' \
              'if [ "$1" = credential ] && [ "$2" = fill ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(cat /tmp/decune-git-helper)' \
              '  "$helper" get <"$input"' \
              '  rm -f "$input"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git && chmod +x /usr/local/bin/git
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
              "postStartCommand": "printf 'protocol=https\nhost=example.test\n\n' | git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && grep -q 'password=test-secret' /tmp/decune-credential"
            }
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            r#"
            [credential]
              helper = "!f() { while IFS= read -r line; do [ -z \"$line\" ] && break; done; printf 'username=octo\npassword=test-secret\n'; }; f"
            "#,
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("test-secret").not());
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
fn up_detach_runs_git_credential_helper_as_nonroot_alpine_remote_user() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ]; then' \
              '  shift' \
              '  case "$*" in' \
              '    "--global --unset-all credential.helper")' \
              '      rm -f /tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global --add credential.helper "*)' \
              '      printf "%s\n" "$4" >/tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global user.name "*|"--global user.email "*)' \
              '      exit 0' \
              '      ;;' \
              '  esac' \
              'fi' \
              'if [ "$1" = credential ] && [ "$2" = fill ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(cat /tmp/decune-git-helper)' \
              '  "$helper" get <"$input"' \
              '  rm -f "$input"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git \
              && chmod +x /usr/local/bin/git
            "#,
                uid = current_uid(),
                gid = current_gid(),
            ),
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
              "remoteUser": "decune",
              "postStartCommand": "test \"$(id -un)\" = decune && test -x /run/decune/git-credential-decune && printf 'protocol=https\nhost=example.test\n\n' | git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && grep -q 'password=test-secret' /tmp/decune-credential"
            }
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            r#"
            [credential]
              helper = "!f() { while IFS= read -r line; do [ -z \"$line\" ] && break; done; printf 'username=octo\npassword=test-secret\n'; }; f"
            "#,
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("test-secret").not());
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
fn up_detach_sets_git_credential_home_for_nonroot_remote_user() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            ENV HOME=/root
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune \
              && chmod 700 /root \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ]; then' \
              '  shift' \
              '  case "$*" in' \
              '    "--global --unset-all credential.helper")' \
              '      if [ -f "$HOME/.gitconfig" ]; then sed "/^helper=/d" "$HOME/.gitconfig" >"$HOME/.gitconfig.tmp" && mv "$HOME/.gitconfig.tmp" "$HOME/.gitconfig"; fi' \
              '      exit 0' \
              '      ;;' \
              '    "--global --add credential.helper "*)' \
              '      mkdir -p "$HOME"' \
              '      printf "helper=%s\n" "$4" >>"$HOME/.gitconfig"' \
              '      exit 0' \
              '      ;;' \
              '    "--global user.name "*|"--global user.email "*)' \
              '      exit 0' \
              '      ;;' \
              '  esac' \
              'fi' \
              'if [ "$1" = credential ] && [ "$2" = fill ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(sed -n "s/^helper=//p" "$HOME/.gitconfig" | tail -n 1)' \
              '  "$helper" get <"$input"' \
              '  rm -f "$input"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git \
              && chmod +x /usr/local/bin/git
            "#,
                uid = current_uid(),
                gid = current_gid(),
            ),
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
              "remoteUser": "decune",
              "postStartCommand": "test \"$(id -un)\" = decune && printf 'protocol=https\nhost=example.test\n\n' | HOME=/home/decune git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && grep -q 'password=test-secret' /tmp/decune-credential"
            }
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            r#"
            [credential]
              helper = "!f() { while IFS= read -r line; do [ -z \"$line\" ] && break; done; printf 'username=octo\npassword=test-secret\n'; }; f"
            "#,
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("test-secret").not());
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
fn up_detach_denies_host_daemon_socket_to_non_remote_user_when_remote_uid_matches_host() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    let uid = current_uid();
    let gid = current_gid();
    let attacker_uid = if current_uid() == 20001 { 20002 } else { 20001 };
    let attacker_gid = if current_gid() == 20001 { 20002 } else { 20001 };
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM ubuntu:24.04
            RUN if getent group {gid} >/dev/null; then remote_group="$(getent group {gid} | cut -d: -f1)"; else groupadd -g {gid} decunegrp && remote_group=decunegrp; fi \
              && if getent passwd {uid} >/dev/null; then remote_user="$(getent passwd {uid} | cut -d: -f1)" && if [ "$remote_user" != decune ]; then usermod -l decune "$remote_user"; fi && usermod -d /home/decune -m decune && usermod -g "$remote_group" decune; else useradd -m -u {uid} -g "$remote_group" decune; fi \
              && groupadd -g {attacker_gid} attackergrp \
              && useradd -m -u {attacker_uid} -g attackergrp attacker \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ]; then' \
              '  shift' \
              '  case "$*" in' \
              '    "--global --unset-all credential.helper")' \
              '      rm -f /tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global --add credential.helper "*)' \
              '      printf "%s\n" "$4" >/tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global user.name "*|"--global user.email "*)' \
              '      exit 0' \
              '      ;;' \
              '  esac' \
              'fi' \
              'if [ "$1" = credential ] && [ "$2" = fill ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(cat /tmp/decune-git-helper)' \
              '  "$helper" get <"$input"' \
              '  rm -f "$input"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git && chmod +x /usr/local/bin/git
            "#,
            ),
        )
        .unwrap();
    let devcontainer_json = r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "remoteUser": "decune",
              "updateRemoteUserUID": false,
              "postStartCommand": "test \"$(id -un)\" = decune && test \"$(id -u)\" = \"__UID__\" && command -v su >/dev/null && owner=$(stat -c %u /run/decune/host-daemon.sock) && attacker=$(id -u attacker) && if [ \"$owner\" = \"$attacker\" ]; then echo 'attacker fixture uid matches host daemon owner' >&2; exit 24; fi && printf 'protocol=https\nhost=example.test\n\n' | git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && if su attacker -s /bin/sh -c \"printf 'protocol=https\nhost=example.test\n\n' | /run/decune/git-credential-decune get >/tmp/attacker-credential 2>/tmp/attacker-error\"; then echo 'attacker reached host daemon' >&2; exit 23; fi"
            }
            "#
    .replace("__UID__", &uid.to_string());
    workspace
        .write_file(".devcontainer/devcontainer.json", &devcontainer_json)
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            r#"
            [credential]
              helper = "!f() { while IFS= read -r line; do [ -z \"$line\" ] && break; done; printf 'username=octo\npassword=test-secret\n'; }; f"
            "#,
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("Falling back to root").not())
            .stderr(predicate::str::contains("attacker reached host daemon").not())
            .stderr(predicate::str::contains("test-secret").not());
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
fn up_detach_runs_git_credential_helper_when_remote_user_uid_differs_from_host_uid() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
    let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };
    let attacker_uid = if current_uid() == 20003 { 20004 } else { 20003 };
    let attacker_gid = if current_gid() == 20003 { 20004 } else { 20003 };
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM ubuntu:24.04
            RUN groupadd -g {remote_gid} decunegrp \
              && useradd -m -u {remote_uid} -g decunegrp decune \
              && groupadd -g {attacker_gid} attackergrp \
              && useradd -m -u {attacker_uid} -g attackergrp attacker \
              && echo 'attacker:decune-test' | chpasswd \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ]; then' \
              '  shift' \
              '  case "$*" in' \
              '    "--global --unset-all credential.helper")' \
              '      rm -f /tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global --add credential.helper "*)' \
              '      printf "%s\n" "$4" >/tmp/decune-git-helper' \
              '      exit 0' \
              '      ;;' \
              '    "--global user.name "*|"--global user.email "*)' \
              '      exit 0' \
              '      ;;' \
              '  esac' \
              'fi' \
              'if [ "$1" = credential ] && [ "$2" = fill ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(cat /tmp/decune-git-helper)' \
              '  "$helper" get <"$input"' \
              '  rm -f "$input"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git \
              && chmod +x /usr/local/bin/git
            "#,
            ),
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
              "remoteUser": "decune",
              "updateRemoteUserUID": false,
              "postStartCommand": "test \"$(id -un)\" = decune && test -x /run/decune/git-credential-decune && printf 'protocol=https\nhost=example.test\n\n' | git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && grep -q 'password=test-secret' /tmp/decune-credential && if printf 'decune-test\n' | su attacker -s /bin/sh -c \"printf 'protocol=https\nhost=example.test\n\n' | /run/decune/git-credential-decune get >/tmp/attacker-credential 2>/tmp/attacker-error\"; then echo 'attacker reached host daemon' >&2; exit 23; fi"
            }
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            r#"
            [credential]
              helper = "!f() { while IFS= read -r line; do [ -z \"$line\" ] && break; done; printf 'username=octo\npassword=test-secret\n'; }; f"
            "#,
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("attacker reached host daemon").not())
            .stderr(predicate::str::contains("test-secret").not());
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
fn up_detach_warns_when_github_cli_is_missing_in_container_without_leaking_token() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
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
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    let gh_path = host_tools
        .write_file(
            "bin/gh",
            "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = token ]; then printf 'github-test-secret\\n'; exit 0; fi\nexit 91\n",
        )
        .unwrap();
    fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        gh_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
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
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "GitHub CLI token forwarding is unavailable",
            ))
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let config = inspect.config.unwrap_or_default();
            let env = config.env.unwrap_or_default();
            assert!(
                env.iter()
                    .all(|entry| !entry.contains("github-test-secret"))
            );
            let labels = config.labels.unwrap_or_default();
            assert!(
                labels
                    .values()
                    .all(|value| !value.contains("github-test-secret"))
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_sets_github_cli_config_for_nonroot_remote_user() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = auth ] && [ "$2" = login ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  mkdir -p "$GH_CONFIG_DIR"' \
              '  cat > "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = status ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  grep -qx "$(printf %s%s github-test -secret)" "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake gh command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/gh \
              && chmod +x /usr/local/bin/gh
            "#,
                uid = current_uid(),
                gid = current_gid(),
            ),
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
              "remoteUser": "decune",
              "postStartCommand": "test \"$(id -un)\" = decune && gh auth status"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    let gh_path = host_tools
        .write_file(
            "bin/gh",
            "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = token ]; then printf 'github-test-secret\\n'; exit 0; fi\nexit 91\n",
        )
        .unwrap();
    fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        gh_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
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
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let env = inspect.config.unwrap_or_default().env.unwrap_or_default();
            assert!(
                env.iter()
                    .any(|entry| entry == "GH_CONFIG_DIR=/run/decune/gh")
            );
            assert!(
                env.iter()
                    .all(|entry| !entry.contains("github-test-secret"))
            );
        });
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
fn up_detach_sets_github_cli_config_when_remote_user_uid_differs_from_host_uid() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
    let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {remote_gid} decunegrp \
              && adduser -D -u {remote_uid} -G decunegrp -h /home/decune decune \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = auth ] && [ "$2" = login ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  mkdir -p "$GH_CONFIG_DIR"' \
              '  cat > "$GH_CONFIG_DIR/token"' \
              '  test ! -e /run/decune/gh-token/token || ! test -r /run/decune/gh-token/token' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = status ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  grep -qx "$(printf %s%s github-test -secret)" "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake gh command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/gh \
              && chmod +x /usr/local/bin/gh
            "#,
            ),
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
              "remoteUser": "decune",
              "updateRemoteUserUID": false,
              "postStartCommand": "test \"$(id -un)\" = decune && gh auth status"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    let gh_path = host_tools
        .write_file(
            "bin/gh",
            "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = token ]; then printf 'github-test-secret\\n'; exit 0; fi\nexit 91\n",
        )
        .unwrap();
    fs::set_permissions(&gh_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        gh_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
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
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let config = inspect.config.unwrap_or_default();
            let env = config.env.unwrap_or_default();
            assert!(
                env.iter()
                    .any(|entry| entry == "GH_CONFIG_DIR=/run/decune/gh")
            );
            assert!(
                env.iter()
                    .all(|entry| !entry.contains("github-test-secret"))
            );
            let labels = config.labels.unwrap_or_default();
            assert!(
                labels
                    .values()
                    .all(|value| !value.contains("github-test-secret"))
            );
        });
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
fn up_detach_mounts_ssh_agent_socket_and_sets_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    let socket_workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test \"${SSH_AUTH_SOCK:-}\" = /run/decune/ssh-agent.sock && test -S /run/decune/ssh-agent.sock"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            https = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let socket_path = socket_workspace.path().join("agent.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
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
            .env("SSH_AUTH_SOCK", &socket_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let env = inspect.config.unwrap_or_default().env.unwrap_or_default();
            assert!(
                env.iter()
                    .any(|entry| entry == "SSH_AUTH_SOCK=/run/decune/ssh-agent.sock")
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_copies_host_git_user_config_when_https_is_off() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ] && [ "$2" = --global ]; then' \
              '  case "$3" in' \
              '    user.name|user.email)' \
              '      mkdir -p "$HOME"' \
              '      printf "%s=%s\n" "$3" "$4" >> "$HOME/.gitconfig"' \
              '      exit 0' \
              '      ;;' \
              '  esac' \
              'fi' \
              'echo "unexpected fake container git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git \
              && chmod +x /usr/local/bin/git
            "#,
                uid = current_uid(),
                gid = current_gid(),
            ),
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
              "remoteUser": "decune",
              "postStartCommand": "test \"$(id -un)\" = decune && grep -qx 'user.name=Octo User' \"$HOME/.gitconfig\" && grep -qx 'user.email=octo@example.test' \"$HOME/.gitconfig\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            https = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let git_path = host_tools
        .write_file(
            "bin/git",
            "#!/bin/sh\nif [ \"$1\" = config ] && [ \"$2\" = --global ] && [ \"$3\" = --get ]; then case \"$4\" in user.name) printf 'Octo User\\n'; exit 0 ;; user.email) printf 'octo@example.test\\n'; exit 0 ;; esac; fi\nexit 1\n",
        )
        .unwrap();
    fs::set_permissions(&git_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        git_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
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
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
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
fn up_detach_copies_host_global_gitconfig_when_https_is_off_without_leaking_secret() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune
            "#,
                uid = current_uid(),
                gid = current_gid(),
            ),
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
              "remoteUser": "decune",
              "postStartCommand": "test \"$(id -un)\" = decune && test -f \"$HOME/.gitconfig\" && grep -q \"$(printf %s%s global -secret)\" \"$HOME/.gitconfig\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            copy_user = false
            copy_global_config = true
            https = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            "[credential]\n  helper = store\n[decune]\n  token = global-secret\n",
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("global-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let config = inspect.config.unwrap_or_default();
            let env = config.env.unwrap_or_default();
            assert!(env.iter().all(|entry| !entry.contains("global-secret")));
            let labels = config.labels.unwrap_or_default();
            assert!(
                labels
                    .values()
                    .all(|value| !value.contains("global-secret"))
            );
        });
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
fn up_detach_keeps_copied_host_gitconfig_after_dotfile_gitconfig_setup() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[decune]\n  source = dotfiles\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune \
              && printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = config ] && [ "$2" = --global ]; then' \
              '  case "$3" in' \
              '    --unset-all)' \
              '      if [ "$4" = credential.helper ] && [ -f "$HOME/.gitconfig" ]; then sed "/helper =/d;/helper=/d" "$HOME/.gitconfig" >"$HOME/.gitconfig.tmp" && mv "$HOME/.gitconfig.tmp" "$HOME/.gitconfig"; fi' \
              '      exit 0' \
              '      ;;' \
              '    --add)' \
              '      if [ "$4" = credential.helper ]; then printf "  helper = %s\n" "$5" >> "$HOME/.gitconfig"; exit 0; fi' \
              '      ;;' \
              '  esac' \
              'fi' \
              'echo "unexpected fake container git command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/git \
              && chmod +x /usr/local/bin/git
            "#,
                uid = current_uid(),
                gid = current_gid(),
            ),
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
              "remoteUser": "decune",
              "postStartCommand": "test \"$(id -un)\" = decune && test -f \"$HOME/.gitconfig\" && test ! -L \"$HOME/.gitconfig\" && grep -q 'global-secret' \"$HOME/.gitconfig\" && grep -q 'helper = /run/decune/git-credential-decune' \"$HOME/.gitconfig\" && ! grep -q 'source = dotfiles' \"$HOME/.gitconfig\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [[dotfiles]]
            source = "dotfiles/gitconfig"
            target = ".gitconfig"
            read_only = true
            on_conflict = "backup"

            [credentials.git]
            copy_global_config = true

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            "[credential]\n  helper = store\n[decune]\n  token = global-secret\n",
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("global-secret").not());
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
fn up_detach_does_not_expose_host_gitconfig_when_remote_user_uid_differs_from_host_uid() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_home = support::TempWorkspace::new().unwrap();
    let remote_uid = if current_uid() == 20001 { 20002 } else { 20001 };
    let remote_gid = if current_gid() == 20001 { 20002 } else { 20001 };
    let attacker_uid = if current_uid() == 20003 { 20004 } else { 20003 };
    let attacker_gid = if current_gid() == 20003 { 20004 } else { 20003 };
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            &format!(
                r#"
            FROM ubuntu:24.04
            RUN groupadd -g {remote_gid} decunegrp \
              && useradd -m -u {remote_uid} -g decunegrp decune \
              && groupadd -g {attacker_gid} attackergrp \
              && useradd -m -u {attacker_uid} -g attackergrp attacker \
              && echo 'attacker:decune-test' | chpasswd
            "#,
            ),
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
              "remoteUser": "decune",
              "updateRemoteUserUID": false,
              "postStartCommand": "test \"$(id -un)\" = decune && test -f \"$HOME/.gitconfig\" && grep -q \"$(printf %s%s global -secret)\" \"$HOME/.gitconfig\" && if printf 'decune-test\n' | su attacker -s /bin/sh -c \"test -r /run/decune/host-gitconfig && grep -q 'global-secret' /run/decune/host-gitconfig\"; then echo 'attacker read host gitconfig' >&2; exit 23; fi"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            copy_user = false
            copy_global_config = true
            https = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            "[credential]\n  helper = store\n[decune]\n  token = global-secret\n",
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
            .env("HOME", host_home.path())
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("attacker read host gitconfig").not())
            .stderr(predicate::str::contains("global-secret").not());
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
fn up_detach_does_not_report_started_when_lifecycle_fails() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "exit 7"
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Lifecycle stage postStartCommand failed",
            ))
            .stderr(predicate::str::contains("Started dev container").not());
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_attaches_configured_shell_and_returns_shell_exit_code() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '#!/bin/sh\nexit 7\n' >/usr/local/bin/decune-exit-7 \
              && chmod +x /usr/local/bin/decune-exit-7
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
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-exit-7"
            "#,
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
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .code(7)
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("Shell attach is not implemented").not());
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
fn up_attached_shell_receives_user_env_probe() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'export DECUNE_PROBED_FOR_ATTACH=from-login-shell' \
              'exec /bin/sh "$@"' \
              >/usr/local/bin/decune-probe-shell \
              && chmod +x /usr/local/bin/decune-probe-shell \
              && adduser -D -s /usr/local/bin/decune-probe-shell decune \
              && printf '%s\n' \
              '#!/bin/sh' \
              'test "$DECUNE_PROBED_FOR_ATTACH" = "from-login-shell" || exit 9' \
              'test "$DECUNE_REMOTE_ENV_FOR_ATTACH" = "from-remote-env" || exit 10' \
              'exit 0' \
              >/usr/local/bin/decune-shell-check \
              && chmod +x /usr/local/bin/decune-shell-check
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
              "remoteUser": "decune",
              "userEnvProbe": "loginShell",
              "remoteEnv": {
                "DECUNE_REMOTE_ENV_FOR_ATTACH": "from-remote-env"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-shell-check"
            "#,
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
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
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
fn up_config_shell_failure_does_not_fallback() {
    let workspace = support::TempWorkspace::new().unwrap();
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
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-missing-shell"
            "#,
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
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .code(127)
            .stdout(predicate::str::contains(
                "/usr/local/bin/decune-missing-shell",
            ))
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_uses_image_metadata_remote_user_and_remote_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-{}:latest",
        workspace_id(&workspace_root)
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}"
                }}
                "#
            ),
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-record-shell"
            "#,
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_devcontainer_metadata(&workspace_root, &image_tag)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detects_image_metadata_label_change_before_reuse() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-change-{}:latest",
        workspace_id(&workspace_root)
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_without_devcontainer_metadata(&image_tag)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            create_image_with_devcontainer_metadata(&workspace_root, &image_tag)
                .await
                .unwrap();
        });

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Run decune rebuild"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_reuses_image_metadata_when_source_tag_is_missing() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-pruned-{}:latest",
        workspace_id(&workspace_root)
    );
    let hold_tag = format!(
        "decune-test/image-metadata-pruned-{}:hold",
        workspace_id(&workspace_root)
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        remove_image_if_exists(&hold_tag).await.unwrap();
        create_image_with_devcontainer_metadata(&workspace_root, &image_tag)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            tag_image(&image_tag, &hold_tag).await.unwrap();
            remove_image_if_exists(&image_tag).await.unwrap();
        });

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let source_image_cleanup = remove_image_if_exists(&image_tag).await;
        let hold_image_cleanup = remove_image_if_exists(&hold_tag).await;
        container_cleanup
            .and(source_image_cleanup)
            .and(hold_image_cleanup)
            .unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_runs_initialize_before_image_pull() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "localhost:9/decune-test/initialize-image-{}:latest",
        workspace_id(&workspace_root)
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "initializeCommand": "docker tag alpine:3.20 {image_tag}"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        let docker = Docker::connect_with_defaults().unwrap();
        ensure_alpine_image(&docker).await.unwrap();
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_devcontainer_remote_user_overrides_image_metadata() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-override-{}:latest",
        workspace_id(&workspace_root)
    );
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "root",
                  "remoteEnv": {{
                    "EXPECTED_USER": "root"
                  }}
                }}
                "#
            ),
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-record-shell"
            "#,
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_devcontainer_metadata(&workspace_root, &image_tag)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_builds_dockerfile_container_and_honors_dockerignore() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "EXPECTED": "from-arg"
                },
                "target": "dev"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20 AS base
            ARG EXPECTED
            RUN test "$EXPECTED" = "from-arg"
            COPY . /context
            RUN test -f /context/app.txt && test ! -e /context/secret.env
            FROM base AS dev
            RUN true
            FROM alpine:3.20 AS unused
            RUN false
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/.dockerignore", "secret.env\n")
        .unwrap();
    workspace
        .write_file(".devcontainer/app.txt", "included\n")
        .unwrap();
    workspace
        .write_file(".devcontainer/secret.env", "excluded\n")
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
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Building Docker image"))
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert!(
                containers[0]
                    .image
                    .as_ref()
                    .is_some_and(|image| image.starts_with("decune/"))
            );
            let images = workspace_images(&workspace_root).await.unwrap();
            assert_eq!(images.len(), 1);
        });
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
fn up_dockerfile_metadata_label_warns_and_is_not_merged() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "postStartCommand": "test \"${FROM_DOCKERFILE_LABEL:-}\" = \"\" && test \"$(id -un)\" = \"root\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            LABEL devcontainer.metadata="{\"remoteUser\":\"nobody\",\"remoteEnv\":{\"FROM_DOCKERFILE_LABEL\":\"set\"}}"
            "#,
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
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Dockerfile image label devcontainer.metadata is not merged",
            ))
            .stderr(predicate::str::contains("Started dev container"));
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
fn up_detach_rejects_changed_create_config_without_replacing_container() {
    let workspace = support::TempWorkspace::new().unwrap();
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let first_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            containers[0].id.clone().unwrap()
        });

        workspace
            .write_file(
                ".devcontainer/devcontainer.json",
                r#"
                {
                  "image": "alpine:3.20",
                  "containerEnv": {
                    "DECUNE_CHANGED_CONFIG": "1"
                  }
                }
                "#,
            )
            .unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Run decune rebuild"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0].id.as_deref(), Some(first_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state.to_string() == "running")
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn down_and_clean_manage_image_container() {
    let workspace = support::TempWorkspace::new().unwrap();
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
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
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_container_is_not_running(containers[0].id.as_deref().unwrap()).await;
        });

        let stopped_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started existing dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0].id.as_deref(), Some(stopped_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state.to_string() == "running")
            );
        });

        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
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
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_uses_explicit_config_and_applies_create_settings() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer/explicit").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/explicit/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "containerEnv": {
                "DECUNE_EXPLICIT_CONFIG": "enabled"
              },
              "runArgs": [
                "--add-host", "decune.example:127.0.0.1",
                "--dns", "1.1.1.1"
              ]
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args([
                "up",
                "--detach",
                "--config",
                ".devcontainer/explicit/devcontainer.json",
            ])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let config = inspect.config.expect("container config should exist");
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let env = config.env.unwrap_or_default();

            assert!(
                env.iter()
                    .any(|entry| entry == "DECUNE_EXPLICIT_CONFIG=enabled")
            );
            assert!(
                host_config
                    .extra_hosts
                    .unwrap_or_default()
                    .iter()
                    .any(|entry| entry == "decune.example:127.0.0.1")
            );
            assert!(
                host_config
                    .dns
                    .unwrap_or_default()
                    .iter()
                    .any(|entry| entry == "1.1.1.1")
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn clean_force_stops_running_container_before_removal() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let marker = workspace_root.join("term-marker");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).await.unwrap();
        });

        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        assert_eq!(fs::read_to_string(&marker).unwrap(), "term\n");

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert!(containers.is_empty());
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn clean_force_removes_state_and_runtime_directories() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
    fs::write(runtime_dir.join("socket"), "").unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        assert!(!state_dir.exists());
        assert!(!runtime_dir.exists());
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn clean_images_removes_workspace_images_only_when_requested() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_repository = workspace_image_repository(&workspace_root);
    let image_tag = format!("{image_repository}:clean-test");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
        create_workspace_image_tag(&workspace_root, "clean-test")
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).await.unwrap();
            assert_eq!(images, vec![image_tag.clone()]);
        });

        decune()
            .args(["clean", "--force", "--images"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).await.unwrap();
            assert!(images.is_empty());
        });
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
fn up_detach_publishes_app_port() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "appPort": ["8080"]
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let ports = inspect
                .network_settings
                .and_then(|settings| settings.ports)
                .unwrap_or_default();
            let bindings = ports
                .get("8080/tcp")
                .and_then(|bindings| bindings.as_ref())
                .expect("expected appPort to publish 8080/tcp");
            let binding = bindings
                .first()
                .expect("expected at least one published appPort binding");

            assert!(
                binding
                    .host_port
                    .as_deref()
                    .is_some_and(|port| !port.is_empty())
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_applies_project_read_only_bind_mount() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("host-cache").unwrap();
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
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[mounts]]
source = "host-cache"
target = "/mnt/decune-cache"
type = "bind"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root.join("host-cache").canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/mnt/decune-cache"))
                .expect("expected configured bind mount");

            assert_eq!(mount.source.as_deref(), expected_source.to_str());
            assert_eq!(mount.read_only, Some(true));
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_uses_workspace_mount_as_workspace_folder() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.write_file("marker.txt", "workspace\n").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "postStartCommand": "test \"$(pwd)\" = \"/workspace\" && test -f marker.txt"
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
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let workspace_mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/workspace"))
                .expect("expected workspace mount");

            assert_eq!(workspace_mount.source.as_deref(), workspace_root.to_str());
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_resolves_remote_user_home_mount_target() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir("host-cache").unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/remote-user-home-mount-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "node",
                  "mounts": [
                    "source=${{localWorkspaceFolder}}/host-cache,target=${{remoteUserHome}}/.cache,type=bind"
                  ]
                }}
                "#
            ),
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_nonstandard_home_user(&workspace_root, &image_tag)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();

            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.target.as_deref() == Some("/usr/local/share/node/.cache")),
                "expected mount target to use the actual remote user home"
            );
            assert!(
                mounts
                    .iter()
                    .all(|mount| mount.target.as_deref() != Some("/home/node/.cache")),
                "remoteUserHome must not be guessed from the user name"
            );
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_resolves_remote_user_home_mount_target_for_dockerfile() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir("host-cache").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "remoteUser": "node",
              "mounts": [
                "source=${localWorkspaceFolder}/host-cache,target=${remoteUserHome}/.cache,type=bind"
              ]
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -h /usr/local/share/node node
            "#,
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
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();

            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.target.as_deref() == Some("/usr/local/share/node/.cache")),
                "expected mount target to use the actual remote user home"
            );
            assert!(
                mounts
                    .iter()
                    .all(|mount| mount.target.as_deref() != Some("/home/node/.cache")),
                "remoteUserHome must not be guessed from the user name"
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_resolves_bind_mount_after_initialize_command() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "initializeCommand": "mkdir -p host-cache"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[mounts]]
source = "host-cache"
target = "/mnt/decune-cache"
type = "bind"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root.join("host-cache");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/mnt/decune-cache"))
                .expect("expected initialized bind mount");

            assert_eq!(
                mount.source.as_deref(),
                Some(expected_source.canonicalize().unwrap().to_str().unwrap())
            );
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_sets_up_dotfile_symlink() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test -L /root/.gitconfig && test \"$(readlink /root/.gitconfig)\" = \"/opt/decune/dotfiles/.gitconfig\" && grep -q decune /root/.gitconfig && if sh -c 'printf x >> /root/.gitconfig' 2>/tmp/decune-dotfile-write-error; then exit 19; fi"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root
        .join("dotfiles/gitconfig")
        .canonicalize()
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/opt/decune/dotfiles/.gitconfig"))
                .expect("expected dotfile staging mount");

            assert_eq!(mount.source.as_deref(), expected_source.to_str());
            assert_eq!(mount.read_only, Some(true));
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[cfg(unix)]
#[test]
fn up_detach_resolves_dotfile_symlink_source() {
    use std::os::unix::fs as unix_fs;

    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("actual-nvim").unwrap();
    workspace
        .write_file("actual-nvim/init.lua", "return {}\n")
        .unwrap();
    unix_fs::symlink(
        workspace.path().join("actual-nvim"),
        workspace.path().join("linked-nvim"),
    )
    .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test -L /root/.config/nvim && test \"$(readlink /root/.config/nvim)\" = \"/opt/decune/dotfiles/.config/nvim\" && test -f /root/.config/nvim/init.lua"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "linked-nvim"
target = ".config/nvim"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root.join("actual-nvim").canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/opt/decune/dotfiles/.config/nvim"))
                .expect("expected dotfile staging mount");

            assert_eq!(mount.source.as_deref(), expected_source.to_str());
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_replaces_existing_dotfile_symlink() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nemail = decune@example.com\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "postStartCommand": "test -L /root/.gitconfig && test \"$(readlink /root/.gitconfig)\" = \"/opt/decune/dotfiles/.gitconfig\" && grep -q decune@example.com /root/.gitconfig"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN ln -s /tmp/old-gitconfig /root/.gitconfig
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
on_conflict = "replace-symlink"
"#,
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
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_fails_dotfile_setup_when_conflict_policy_is_fail() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '[user]\nname = original\n' > /root/.gitconfig
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
on_conflict = "fail"
"#,
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
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stderr(predicate::str::contains("Failed to setup dotfiles"))
            .stderr(predicate::str::contains("Dotfile target already exists"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_backs_up_existing_dotfile_target_when_requested() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/gitconfig", "[user]\nname = decune\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "postStartCommand": "test -L /root/.gitconfig && test \"$(readlink /root/.gitconfig)\" = \"/opt/decune/dotfiles/.gitconfig\" && backup=$(find /root -maxdepth 1 -name '.gitconfig.decune-backup-*' -type f | head -n 1) && test -n \"$backup\" && grep -q original \"$backup\" && grep -q decune /root/.gitconfig"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '[user]\nname = original\n' > /root/.gitconfig
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/gitconfig"
target = ".gitconfig"
on_conflict = "backup"
"#,
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
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_sets_up_dotfile_in_nonstandard_remote_user_home() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("dotfiles").unwrap();
    workspace
        .write_file("dotfiles/toolrc", "configured\n")
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/nonstandard-home-dotfiles-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "node",
                  "postStartCommand": "test -L /usr/local/share/node/.config/tool && test \"$(readlink /usr/local/share/node/.config/tool)\" = \"/opt/decune/dotfiles/.config/tool\" && grep -q configured /usr/local/share/node/.config/tool && test ! -e /home/node/.config/tool"
                }}
                "#
            ),
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[dotfiles]]
source = "dotfiles/toolrc"
target = ".config/tool"
"#,
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_nonstandard_home_user(&workspace_root, &image_tag)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn rebuild_recreates_container_and_preserves_managed_volume() {
    let workspace = support::TempWorkspace::new().unwrap();
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
    let volume_name = format!("decune-rebuild-test-{}", workspace_id(&workspace_root));

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_volumes(&workspace_root).await.unwrap();
        create_managed_volume(&workspace_root, &volume_name)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let first_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["rebuild", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Removed existing dev container for rebuild",
            ))
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_ne!(containers[0].id.as_deref(), Some(first_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state.to_string() == "running")
            );

            let volumes = workspace_volumes(&workspace_root).await.unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });

        let second_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["up", "--detach", "--rebuild"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Removed existing dev container for rebuild",
            ))
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_ne!(containers[0].id.as_deref(), Some(second_id.as_str()));

            let volumes = workspace_volumes(&workspace_root).await.unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let volume_cleanup = cleanup_workspace_volumes(&workspace_root).await;
        container_cleanup.and(volume_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

async fn workspace_containers(workspace_root: &Path) -> anyhow::Result<Vec<ContainerSummary>> {
    let docker = Docker::connect_with_defaults()?;
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![
            "decune.managed=true".to_owned(),
            format!("decune.workspace={}", workspace_root.display()),
        ],
    );
    let options = ListContainersOptionsBuilder::new()
        .all(true)
        .filters(&filters)
        .build();

    Ok(docker.list_containers(Some(options)).await?)
}

async fn cleanup_workspace_containers(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let containers = workspace_containers(workspace_root).await?;
    let options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();

    for container in containers {
        if let Some(id) = container.id {
            docker.remove_container(&id, Some(options.clone())).await?;
        }
    }

    Ok(())
}

async fn assert_container_is_not_running(container_id: &str) {
    let docker = Docker::connect_with_defaults().unwrap();
    let inspect = docker.inspect_container(container_id, None).await.unwrap();

    assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
}

async fn inspect_single_workspace_container(
    workspace_root: &Path,
) -> anyhow::Result<bollard::models::ContainerInspectResponse> {
    let docker = Docker::connect_with_defaults()?;
    let containers = workspace_containers(workspace_root).await?;

    anyhow::ensure!(containers.len() == 1, "expected one workspace container");

    let id = containers[0]
        .id
        .as_deref()
        .ok_or_else(|| anyhow::anyhow!("workspace container did not include an id"))?;

    Ok(docker.inspect_container(id, None).await?)
}

async fn workspace_volumes(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let docker = Docker::connect_with_defaults()?;
    let mut filters = HashMap::new();
    filters.insert(
        "label".to_owned(),
        vec![
            "decune.managed=true".to_owned(),
            format!("decune.workspace={}", workspace_root.display()),
        ],
    );
    let options = ListVolumesOptionsBuilder::default()
        .filters(&filters)
        .build();

    Ok(docker
        .list_volumes(Some(options))
        .await?
        .volumes
        .unwrap_or_default()
        .into_iter()
        .map(|volume| volume.name)
        .collect())
}

async fn cleanup_workspace_volumes(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let options = RemoveVolumeOptionsBuilder::default().force(true).build();

    for volume in workspace_volumes(workspace_root).await? {
        docker.remove_volume(&volume, Some(options.clone())).await?;
    }

    Ok(())
}

async fn workspace_images(workspace_root: &Path) -> anyhow::Result<Vec<String>> {
    let docker = Docker::connect_with_defaults()?;
    let image_repository = workspace_image_repository(workspace_root);
    let mut filters = HashMap::new();
    filters.insert(
        "reference".to_owned(),
        vec![format!("{image_repository}:*")],
    );
    let options = ListImagesOptionsBuilder::default()
        .all(true)
        .filters(&filters)
        .build();
    let mut images = docker
        .list_images(Some(options))
        .await?
        .into_iter()
        .flat_map(|image| image.repo_tags)
        .filter(|tag| tag.starts_with(&format!("{image_repository}:")))
        .collect::<Vec<_>>();
    images.sort();
    Ok(images)
}

async fn cleanup_workspace_images(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let options = RemoveImageOptionsBuilder::default()
        .force(true)
        .noprune(false)
        .build();

    for image in workspace_images(workspace_root).await? {
        docker
            .remove_image(&image, Some(options.clone()), None)
            .await?;
    }

    Ok(())
}

async fn create_workspace_image_tag(workspace_root: &Path, tag: &str) -> anyhow::Result<String> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let image_repository = workspace_image_repository(workspace_root);
    let options = TagImageOptionsBuilder::default()
        .repo(&image_repository)
        .tag(tag)
        .build();

    docker.tag_image("alpine:3.20", Some(options)).await?;

    Ok(format!("{image_repository}:{tag}"))
}

async fn create_image_without_devcontainer_metadata(image_tag: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let options = TagImageOptionsBuilder::default()
        .repo(repo)
        .tag(tag)
        .build();

    docker.tag_image("alpine:3.20", Some(options)).await?;

    Ok(())
}

async fn tag_image(source: &str, target: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let (repo, tag) = target
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {target}"))?;
    let options = TagImageOptionsBuilder::default()
        .repo(repo)
        .tag(tag)
        .build();

    docker.tag_image(source, Some(options)).await?;

    Ok(())
}

async fn create_image_with_devcontainer_metadata(
    workspace_root: &Path,
    image_tag: &str,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let container_name = format!(
        "decune-image-metadata-source-{}",
        workspace_id(workspace_root)
    );
    let remove_options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker
        .remove_container(&container_name, Some(remove_options.clone()))
        .await;

    let create_options = CreateContainerOptionsBuilder::default()
        .name(&container_name)
        .build();
    let script = r#"
        set -eu
        adduser -D -u 1000 -h /home/devuser devuser
        cat >/usr/local/bin/decune-record-shell <<'EOF'
#!/bin/sh
set -eu
actual_user="$(id -un)"
expected_user="${EXPECTED_USER:-}"
if [ "$actual_user" != "$expected_user" ]; then
    echo "expected shell user $expected_user, got $actual_user" >&2
    exit 11
fi
if [ "${FROM_IMAGE:-}" != "label" ]; then
    echo "expected FROM_IMAGE=label, got ${FROM_IMAGE:-}" >&2
    exit 12
fi
exit 0
EOF
        chmod +x /usr/local/bin/decune-record-shell
    "#;
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec!["-c".to_owned(), script.to_owned()]),
        ..Default::default()
    };

    docker.create_container(Some(create_options), body).await?;
    docker
        .start_container(
            &container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await?;

    let mut wait_stream = docker.wait_container(
        &container_name,
        Some(WaitContainerOptionsBuilder::default().build()),
    );
    let wait = wait_stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("container wait stream ended before status"))?;
    anyhow::ensure!(
        wait.status_code == 0,
        "image metadata fixture container exited with {}",
        wait.status_code
    );

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let metadata = r#"{"remoteUser":"devuser","remoteEnv":{"FROM_IMAGE":"label","EXPECTED_USER":"devuser"},"postStartCommand":"actual_user=$(id -un); expected_user=${EXPECTED_USER:-}; if [ \"$actual_user\" != \"$expected_user\" ]; then echo \"expected lifecycle user $expected_user, got $actual_user\" >&2; exit 11; fi; if [ \"${FROM_IMAGE:-}\" != \"label\" ]; then echo \"expected FROM_IMAGE=label, got ${FROM_IMAGE:-}\" >&2; exit 12; fi"}"#;
    let labels = HashMap::from([("devcontainer.metadata".to_owned(), metadata.to_owned())]);
    let commit_options = CommitContainerOptionsBuilder::default()
        .container(&container_name)
        .repo(repo)
        .tag(tag)
        .pause(false)
        .build();
    let config = ContainerConfig {
        user: Some("root".to_owned()),
        labels: Some(labels),
        ..Default::default()
    };

    docker.commit_container(commit_options, config).await?;
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

async fn create_image_with_nonstandard_home_user(
    workspace_root: &Path,
    image_tag: &str,
) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let container_name = format!(
        "decune-remote-user-home-source-{}",
        workspace_id(workspace_root)
    );
    let remove_options = RemoveContainerOptionsBuilder::default()
        .force(true)
        .v(true)
        .build();
    let _ = docker
        .remove_container(&container_name, Some(remove_options.clone()))
        .await;

    let create_options = CreateContainerOptionsBuilder::default()
        .name(&container_name)
        .build();
    let script = r#"
        set -eu
        adduser -D -h /usr/local/share/node node
    "#;
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec!["-c".to_owned(), script.to_owned()]),
        ..Default::default()
    };

    docker.create_container(Some(create_options), body).await?;
    docker
        .start_container(
            &container_name,
            Some(StartContainerOptionsBuilder::default().build()),
        )
        .await?;

    let mut wait_stream = docker.wait_container(
        &container_name,
        Some(WaitContainerOptionsBuilder::default().build()),
    );
    let wait = wait_stream
        .try_next()
        .await?
        .ok_or_else(|| anyhow::anyhow!("container wait stream ended before status"))?;
    anyhow::ensure!(
        wait.status_code == 0,
        "nonstandard home fixture container exited with {}",
        wait.status_code
    );

    let (repo, tag) = image_tag
        .rsplit_once(':')
        .ok_or_else(|| anyhow::anyhow!("test image tag must include a tag: {image_tag}"))?;
    let commit_options = CommitContainerOptionsBuilder::default()
        .container(&container_name)
        .repo(repo)
        .tag(tag)
        .pause(false)
        .build();

    docker
        .commit_container(commit_options, ContainerConfig::default())
        .await?;
    docker
        .remove_container(&container_name, Some(remove_options))
        .await?;

    Ok(())
}

async fn remove_image_if_exists(image: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;

    if docker.inspect_image(image).await.is_err() {
        return Ok(());
    }

    let options = RemoveImageOptionsBuilder::default()
        .force(true)
        .noprune(false)
        .build();
    docker.remove_image(image, Some(options), None).await?;

    Ok(())
}

async fn create_managed_volume(workspace_root: &Path, volume_name: &str) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    let workspace_id = workspace_id(workspace_root);
    let labels = HashMap::from([
        ("decune.managed".to_owned(), "true".to_owned()),
        (
            "decune.workspace".to_owned(),
            workspace_root.display().to_string(),
        ),
        ("decune.workspace_id".to_owned(), workspace_id),
    ]);
    let request = VolumeCreateRequest {
        name: Some(volume_name.to_owned()),
        labels: Some(labels),
        ..Default::default()
    };

    docker.create_volume(request).await?;

    Ok(())
}

async fn create_term_marker_container(workspace_root: &Path) -> anyhow::Result<()> {
    let docker = Docker::connect_with_defaults()?;
    ensure_alpine_image(&docker).await?;

    let workspace_id = workspace_id(workspace_root);
    let name = format!("decune-clean-term-test-{workspace_id}");
    let options = CreateContainerOptionsBuilder::default().name(&name).build();
    let labels = HashMap::from([
        ("decune.managed".to_owned(), "true".to_owned()),
        (
            "decune.workspace".to_owned(),
            workspace_root.display().to_string(),
        ),
        ("decune.workspace_id".to_owned(), workspace_id),
    ]);
    let body = ContainerCreateBody {
        image: Some("alpine:3.20".to_owned()),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        cmd: Some(vec![
            "-c".to_owned(),
            "trap 'echo term > /host/term-marker; exit 0' TERM\nwhile sleep 1 & wait $!; do :; done"
                .to_owned(),
        ]),
        labels: Some(labels),
        host_config: Some(HostConfig {
            binds: Some(vec![format!("{}:/host", workspace_root.display())]),
            ..Default::default()
        }),
        ..Default::default()
    };

    docker.create_container(Some(options), body).await?;
    docker
        .start_container(&name, Some(StartContainerOptionsBuilder::default().build()))
        .await?;

    Ok(())
}

async fn ensure_alpine_image(docker: &Docker) -> anyhow::Result<()> {
    if docker.inspect_image("alpine:3.20").await.is_ok() {
        return Ok(());
    }

    let options = CreateImageOptionsBuilder::default()
        .from_image("alpine")
        .tag("3.20")
        .build();
    let mut stream = docker.create_image(Some(options), None, None);

    while stream.try_next().await?.is_some() {}

    Ok(())
}

fn workspace_id(root: &Path) -> String {
    let digest = Sha256::digest(root.to_string_lossy().as_bytes());
    let mut id = String::with_capacity(12);

    for byte in digest.iter().take(6) {
        push_hex_byte(&mut id, *byte);
    }

    id
}

fn workspace_image_repository(root: &Path) -> String {
    let basename = root
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("workspace");

    format!(
        "decune/{}-{}",
        docker_name_segment(basename),
        workspace_id(root)
    )
}

fn docker_name_segment(value: &str) -> String {
    let mut output = String::new();
    let mut previous_was_separator = true;

    for character in value.chars().flat_map(char::to_lowercase) {
        if character.is_ascii_alphanumeric() {
            output.push(character);
            previous_was_separator = false;
        } else if !previous_was_separator {
            output.push('-');
            previous_was_separator = true;
        }
    }

    while output.ends_with('-') {
        output.pop();
    }

    if output.is_empty() {
        "workspace".to_owned()
    } else {
        output
    }
}

fn push_hex_byte(output: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789abcdef";

    output.push(HEX[(byte >> 4) as usize] as char);
    output.push(HEX[(byte & 0x0f) as usize] as char);
}

fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

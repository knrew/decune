use crate::harness::*;

#[test]
fn up_detach_routes_git_credential_helper_actions_through_host_daemon() {
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
              'if [ "$1" = credential ] && [ "$2" = approve ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(cat /tmp/decune-git-helper)' \
              '  "$helper" store <"$input"' \
              '  rm -f "$input"' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = credential ] && [ "$2" = reject ]; then' \
              '  input=$(mktemp)' \
              '  cat >"$input"' \
              '  helper=$(cat /tmp/decune-git-helper)' \
              '  "$helper" erase <"$input"' \
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
              "postStartCommand": "printf 'protocol=https\nhost=example.test\n\n' | git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && grep -q 'password=test-secret' /tmp/decune-credential && printf 'protocol=https\nhost=example.test\nusername=octo\npassword=test-secret\n\n' | git credential approve && printf 'protocol=https\nhost=example.test\nusername=octo\npassword=test-secret\n\n' | git credential reject"
            }
            "#,
        )
        .unwrap();
    host_home
        .write_file(
            ".gitconfig",
            r#"
            [credential]
              helper = "!f() { action=\"$1\"; if [ \"$action\" = get ]; then while IFS= read -r line; do [ -z \"$line\" ] && break; done; printf 'username=octo\npassword=test-secret\n'; exit 0; fi; if [ \"$action\" = store ] || [ \"$action\" = erase ]; then printf '%s\n' \"$action\" >> \"$HOME/credential-actions\"; while IFS= read -r line; do [ -z \"$line\" ] && break; done; exit 0; fi; }; f"
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

        assert_eq!(
            fs::read_to_string(host_home.path().join("credential-actions")).unwrap(),
            "store\nerase\n"
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
fn up_detach_uses_selected_git_credential_helper_independent_of_container_uname() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'echo riscv64' \
              >/usr/local/bin/uname && chmod +x /usr/local/bin/uname && \
              printf '%s\n' \
              '#!/bin/sh' \
              'if [ "$1" = config ] && [ "$2" = --global ] && [ "$3" = --unset-all ]; then exit 0; fi' \
              'if [ "$1" = config ] && [ "$2" = --global ] && [ "$3" = --add ]; then printf "%s\n" "$5" >/tmp/decune-git-helper; exit 0; fi' \
              'echo "unexpected git command: $*" >&2' \
              'exit 41' \
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
              "postStartCommand": "test \"$(cat /tmp/decune-git-helper)\" = /run/decune/git-credential-decune && test -x /run/decune/git-credential-decune"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [credentials.git]
            copy_user = false

            [credentials.github]
            enabled = false
            ",
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
            format!(
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
            format!(
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
            format!(
                r#"
            FROM ubuntu:24.04
            RUN if getent group {gid} >/dev/null; then remote_group="$(getent group {gid} | cut -d: -f1)"; else groupadd -g {gid} decunegrp && remote_group=decunegrp; fi \
              && if getent passwd {uid} >/dev/null; then remote_user="$(getent passwd {uid} | cut -d: -f1)" && if [ "$remote_user" != decune ]; then usermod -l decune "$remote_user"; fi && usermod -d /home/decune -m decune && usermod -g "$remote_group" decune; else useradd -m -u {uid} -g "$remote_group" decune; fi \
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
              "postStartCommand": "test \"$(id -un)\" = decune && test \"$(id -u)\" = \"__UID__\" && command -v su >/dev/null && owner=$(stat -c %u /run/decune/host-daemon.sock) && attacker=$(id -u attacker) && if [ \"$owner\" = \"$attacker\" ]; then echo 'attacker fixture uid matches host daemon owner' >&2; exit 24; fi && printf 'protocol=https\nhost=example.test\n\n' | git credential fill > /tmp/decune-credential && grep -q 'username=octo' /tmp/decune-credential && rm -f /tmp/attacker-started && if printf 'decune-test\n' | su attacker -s /bin/sh -c \"touch /tmp/attacker-started && printf 'protocol=https\nhost=example.test\n\n' | /run/decune/git-credential-decune get >/tmp/attacker-credential 2>/tmp/attacker-error\"; then echo 'attacker reached host daemon' >&2; exit 23; fi && test -f /tmp/attacker-started"
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
            format!(
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

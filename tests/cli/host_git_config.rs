use crate::harness::*;

#[test]
fn up_detach_copies_host_git_user_config_when_https_is_off() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_copies_host_git_user_config_when_helper_setup_fails() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
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
              'if [ "$1" = config ] && [ "$2" = --global ]; then' \
              '  case "$3 $4" in' \
              '    "--unset-all credential.helper")' \
              '      exit 0' \
              '      ;;' \
              '    "--add credential.helper")' \
              '      echo "helper setup blocked" >&2' \
              '      exit 87' \
              '      ;;' \
              '  esac' \
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
            r"
            version = 1

            [credentials.github]
            enabled = false
            ",
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
            .stderr(predicate::str::contains(
                "Git credential forwarding is unavailable",
            ));
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
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
            format!(
                r"
            FROM alpine:3.20
            RUN addgroup -g {gid} decunegrp \
              && adduser -D -u {uid} -G decunegrp -h /home/decune decune
            ",
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
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
            format!(
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
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
            format!(
                r"
            FROM ubuntu:24.04
            RUN groupadd -g {remote_gid} decunegrp \
              && useradd -m -u {remote_uid} -g decunegrp decune \
              && groupadd -g {attacker_gid} attackergrp \
              && useradd -m -u {attacker_uid} -g attackergrp attacker \
              && echo 'attacker:decune-test' | chpasswd
            ",
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

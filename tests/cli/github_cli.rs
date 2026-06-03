use crate::harness::*;

#[test]
fn up_detach_warns_when_github_cli_is_missing_and_auto_install_is_disabled_without_leaking_token() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let runtime_home = path_roots.path().join("runtime");
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

            [credentials.github]
            install_feature_if_missing = false
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
    let workspace_id = workspace_id(&workspace_root);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let github_token_file = runtime_dir.join("gh-token").join("token");
    let host_daemon_socket = runtime_dir.join("host-daemon.sock");
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
            .env("XDG_RUNTIME_DIR", &runtime_home)
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

        assert!(!github_token_file.exists());
        assert!(!host_daemon_socket.exists());
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
fn up_detach_recreates_container_when_github_cli_token_becomes_unavailable() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let empty_tools = support::TempWorkspace::new().unwrap();
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

            [credentials.github]
            install_feature_if_missing = false
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
    let fake_path = gh_path.parent().unwrap().display().to_string();
    let empty_path = empty_tools.create_dir("bin").unwrap();
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
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        let first_id = runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
            assert!(inspect_has_mount_target(&inspect, "/run/decune/gh-token"));
            assert!(inspect_has_mount_target(&inspect, "/run/decune/gh"));
            inspect.id.unwrap()
        });

        decune()
            .env("PATH", &empty_path)
            .env_remove("SSH_AUTH_SOCK")
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
            assert_ne!(inspect.id.as_deref(), Some(first_id.as_str()));
            assert!(!inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
            assert!(!inspect_has_mount_target(&inspect, "/run/decune/gh-token"));
            assert!(!inspect_has_mount_target(&inspect, "/run/decune/gh"));
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
fn up_detach_refreshes_github_cli_token_when_reusing_stopped_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = auth ] && [ "$2" = login ]; then' \
              '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
              '  mkdir -p "$GH_CONFIG_DIR"' \
              '  cat > "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
              '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake gh command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/gh \
              && chmod +x /usr/local/bin/gh
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
              "postStartCommand": "test \"${GH_CONFIG_DIR:-}\" = /run/decune/gh && grep -qx \"$(cat /workspace/expected-token)\" \"$GH_CONFIG_DIR/token\""
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
    workspace
        .write_file("expected-token", "first-secret\n")
        .unwrap();
    let host_token_path = host_tools.write_file("token", "first-secret\n").unwrap();
    let gh_path = host_tools
        .write_file(
            "bin/gh",
            "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = token ]; then cat \"$DECUNE_TEST_GH_TOKEN_FILE\"; exit 0; fi\nexit 91\n",
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
            .env("DECUNE_TEST_GH_TOKEN_FILE", &host_token_path)
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("first-secret").not());

        let first_id = runtime.block_on(async {
            inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap()
                .id
                .unwrap()
        });

        decune()
            .arg("down")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Stopped dev container"));

        fs::write(&host_token_path, "second-secret\n").unwrap();
        workspace
            .write_file("expected-token", "second-secret\n")
            .unwrap();

        decune()
            .env("PATH", &fake_path)
            .env("DECUNE_TEST_GH_TOKEN_FILE", &host_token_path)
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started existing dev container"))
            .stderr(predicate::str::contains("second-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            assert_eq!(inspect.id.as_deref(), Some(first_id.as_str()));
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
fn up_detach_refreshes_github_cli_token_when_reusing_running_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'set -eu' \
              'if [ "$1" = auth ] && [ "$2" = login ]; then' \
              '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
              '  mkdir -p "$GH_CONFIG_DIR"' \
              '  cat > "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
              '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
              '  exit 0' \
              'fi' \
              'echo "unexpected fake gh command: $*" >&2' \
              'exit 91' \
              >/usr/local/bin/gh \
              && chmod +x /usr/local/bin/gh
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
              "postStartCommand": "test \"${GH_CONFIG_DIR:-}\" = /run/decune/gh && grep -qx \"$(cat /workspace/expected-token)\" \"$GH_CONFIG_DIR/token\""
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
    workspace
        .write_file("expected-token", "first-secret\n")
        .unwrap();
    let host_token_path = host_tools.write_file("token", "first-secret\n").unwrap();
    let gh_path = host_tools
        .write_file(
            "bin/gh",
            "#!/bin/sh\nif [ \"$1\" = auth ] && [ \"$2\" = token ]; then cat \"$DECUNE_TEST_GH_TOKEN_FILE\"; exit 0; fi\nexit 91\n",
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
            .env("DECUNE_TEST_GH_TOKEN_FILE", &host_token_path)
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("first-secret").not());

        let first_id = runtime.block_on(async {
            inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap()
                .id
                .unwrap()
        });

        fs::write(&host_token_path, "second-secret\n").unwrap();
        workspace
            .write_file("expected-token", "second-secret\n")
            .unwrap();

        decune()
            .env("PATH", &fake_path)
            .env("DECUNE_TEST_GH_TOKEN_FILE", &host_token_path)
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"))
            .stderr(predicate::str::contains("second-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            assert_eq!(inspect.id.as_deref(), Some(first_id.as_str()));
            exec_single_workspace_container(
                &workspace_root,
                [
                    "/bin/sh",
                    "-lc",
                    "test \"${GH_CONFIG_DIR:-}\" = /run/decune/gh && grep -qx \"$(cat /workspace/expected-token)\" \"$GH_CONFIG_DIR/token\"",
                ],
            )
            .await
            .unwrap();
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

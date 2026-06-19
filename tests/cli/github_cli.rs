use crate::harness::*;

#[test]
fn up_detach_reports_when_github_cli_is_missing_and_auto_install_is_disabled_without_leaking_token()
{
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let runtime_home = path_roots.path().join("runtime");
    let state_home = path_roots.path().join("state");
    workspace.create_dir(".devcontainer").unwrap();
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
    let state_file = state_home
        .join("decune")
        .join(&workspace_id)
        .join("state.toml");
    let github_token_file = runtime_dir.join("secrets").join("github-token");
    let host_daemon_socket = runtime_dir.join("host-daemon.sock");
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
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .env("XDG_STATE_HOME", &state_home)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Notice: GitHub credential forwarding is enabled",
            ))
            .stderr(predicate::str::contains(
                "[credentials.github].enabled = false",
            ))
            .stderr(predicate::str::contains(
                "Warning: GitHub CLI token forwarding is unavailable",
            ))
            .stderr(predicate::str::contains("Building Docker image"))
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
            assert!(
                labels
                    .get("decune.config_hash")
                    .is_some_and(|hash| !hash.contains("github-test-secret"))
            );
            let logs = workspace_container_logs(&workspace_root).await.unwrap();
            assert!(!logs.contains("github-test-secret"));
            let images = workspace_images(&workspace_root).await.unwrap();
            assert!(
                images
                    .iter()
                    .all(|image| !image.contains("github-test-secret"))
            );
            let docker = Docker::connect_with_defaults().unwrap();
            for image in images {
                let inspect = docker.inspect_image(&image).await.unwrap();
                let labels = inspect.config.and_then(|config| config.labels);
                assert!(
                    labels
                        .unwrap_or_default()
                        .values()
                        .all(|value| !value.contains("github-test-secret"))
                );
            }
        });

        assert_eq!(fs::read_to_string(&github_token_file).unwrap(), "");
        assert_eq!(
            fs::metadata(&github_token_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert!(!host_daemon_socket.exists());
        assert!(
            !fs::read_to_string(&state_file)
                .unwrap()
                .contains("github-test-secret")
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
fn up_detach_does_not_run_remote_profile_as_root_during_github_cli_setup() {
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
                'if [ "$(id -u)" = 0 ] && [ -r /run/decune/secrets/github-token ]; then' \
                '  cat /run/decune/secrets/github-token > /tmp/decune-profile-leak' \
                'fi' \
                >/home/decune/.profile \
              && chown decune:decunegrp /home/decune/.profile \
              && printf '%s\n' \
                '#!/bin/sh' \
                'set -eu' \
                'if [ "$1" = auth ] && [ "$2" = login ]; then' \
                '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
                '  mkdir -p "$GH_CONFIG_DIR"' \
                '  test -r /run/decune/secrets/github-token' \
                '  cat > "$GH_CONFIG_DIR/token"' \
                '  exit 0' \
                'fi' \
                'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
                '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
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
              "remoteUser": "decune"
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
            exec_single_workspace_container(
                &workspace_root,
                ["test", "!", "-f", "/tmp/decune-profile-leak"],
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

#[test]
fn up_detach_uses_remote_user_login_path_for_github_cli_setup() {
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
              && mkdir -p /home/decune/.local/bin \
              && printf '%s\n' \
                'export PATH="$HOME/.local/bin:$PATH"' \
                >/home/decune/.profile \
              && printf '%s\n' \
                '#!/bin/sh' \
                'set -eu' \
                'if [ "$1" = auth ] && [ "$2" = login ]; then' \
                '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
                '  mkdir -p "$GH_CONFIG_DIR"' \
                '  test -r /run/decune/secrets/github-token' \
                '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
                '  cat > "$GH_CONFIG_DIR/token"' \
                '  exit 0' \
                'fi' \
                'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
                '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
                '  exit 0' \
                'fi' \
                'if [ "$1" = auth ] && [ "$2" = status ]; then' \
                '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
                '  test -w "$GH_CONFIG_DIR"' \
                '  grep -qx "$(printf %s%s github-test -secret)" "$GH_CONFIG_DIR/token"' \
                '  exit 0' \
                'fi' \
                'echo "unexpected fake gh command: $*" >&2' \
                'exit 91' \
                >/home/decune/.local/bin/gh \
              && chmod +x /home/decune/.local/bin/gh \
              && chown -R decune:decunegrp /home/decune
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
              "userEnvProbe": "loginShell",
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
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());
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
fn up_detach_reuses_dockerfile_container_without_github_cli_probe_build_when_auto_add_unneeded() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            COPY build-state /tmp/build-state
            RUN test "$(cat /tmp/build-state)" = ok
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

            [credentials.git]
            enabled = false

            [credentials.github]
            install_feature_if_missing = false
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/build-state", "ok\n")
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
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        let first_id = runtime.block_on(async {
            inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap()
                .id
                .unwrap()
        });

        decune()
            .env("PATH", &fake_path)
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"))
            .stderr(predicate::str::contains("Building Docker image").not())
            .stderr(predicate::str::contains("github-test-secret").not());

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
fn up_detach_sets_github_cli_config_for_nonroot_remote_user() {
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
              'if [ "$1" = auth ] && [ "$2" = login ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  mkdir -p "$GH_CONFIG_DIR"' \
              '  test -r /run/decune/secrets/github-token' \
              '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
              '  cat > "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = status ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  test -w "$GH_CONFIG_DIR"' \
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
            format!(
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
              '  test -r /run/decune/secrets/github-token' \
              '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
              '  cat > "$GH_CONFIG_DIR/token"' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  exit 0' \
              'fi' \
              'if [ "$1" = auth ] && [ "$2" = status ]; then' \
              '  test "${{GH_CONFIG_DIR:-}}" = /run/decune/gh' \
              '  test -w "$GH_CONFIG_DIR"' \
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
    let empty_path = empty_tools.create_dir("bin").unwrap();
    symlink_host_executable_into_path("docker", gh_path.parent().unwrap());
    symlink_host_executable_into_path("docker", &empty_path);
    let fake_path = gh_path.parent().unwrap().display().to_string();
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
            assert!(inspect_has_mount_target(
                &inspect,
                "/run/decune/secrets/github-token"
            ));
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
            assert!(!inspect_has_mount_target(
                &inspect,
                "/run/decune/secrets/github-token"
            ));
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
fn up_detach_reuses_auto_added_github_cli_feature_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let cache_home = tempfile::tempdir().unwrap();
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
    let workspace_root = workspace.path().canonicalize().unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:1111111111111111111111111111111111111111111111111111111111111111",
        r#"
        set -eu
        printf '%s\n' \
          '#!/bin/sh' \
          'set -eu' \
          'if [ "$1" = auth ] && [ "$2" = login ]; then' \
          '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
          '  mkdir -p "$GH_CONFIG_DIR"' \
          '  test -r /run/decune/secrets/github-token' \
          '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
          '  cat > "$GH_CONFIG_DIR/token"' \
          '  exit 0' \
          'fi' \
          'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
          '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
          '  exit 0' \
          'fi' \
          'echo "unexpected fake gh command: $*" >&2' \
          'exit 91' \
          >/usr/local/bin/gh
        chmod +x /usr/local/bin/gh
        "#,
    );
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
            .env("XDG_CACHE_HOME", cache_home.path())
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        let first_id = runtime.block_on(async {
            inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap()
                .id
                .unwrap()
        });

        decune()
            .env("PATH", &fake_path)
            .env("XDG_CACHE_HOME", cache_home.path())
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

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
fn up_detach_reuses_auto_added_github_cli_feature_container_when_source_tag_is_removed() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let cache_home = tempfile::tempdir().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let source_image = format!(
        "localhost:9/decune-test/github-cli-missing-source-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
            {{
              "image": "{source_image}"
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

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        r#"
        set -eu
        printf '%s\n' \
          '#!/bin/sh' \
          'set -eu' \
          'if [ "$1" = auth ] && [ "$2" = login ]; then' \
          '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
          '  mkdir -p "$GH_CONFIG_DIR"' \
          '  test -r /run/decune/secrets/github-token' \
          '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
          '  cat > "$GH_CONFIG_DIR/token"' \
          '  exit 0' \
          'fi' \
          'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
          '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
          '  exit 0' \
          'fi' \
          'echo "unexpected fake gh command: $*" >&2' \
          'exit 91' \
          >/usr/local/bin/gh
        chmod +x /usr/local/bin/gh
        "#,
    );
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
        remove_image_if_exists(&source_image).await.unwrap();
        create_image_without_devcontainer_metadata(&source_image)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .env("PATH", &fake_path)
            .env("XDG_CACHE_HOME", cache_home.path())
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
            inspect.id.unwrap()
        });

        runtime.block_on(async {
            remove_image_if_exists(&source_image).await.unwrap();
        });

        decune()
            .env("PATH", &fake_path)
            .env("XDG_CACHE_HOME", cache_home.path())
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            assert_eq!(inspect.id.as_deref(), Some(first_id.as_str()));
            assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let workspace_image_cleanup = cleanup_workspace_images(&workspace_root).await;
        let source_image_cleanup = remove_image_if_exists(&source_image).await;
        container_cleanup
            .and(workspace_image_cleanup)
            .and(source_image_cleanup)
            .unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_reuses_github_cli_source_container_when_source_tag_is_removed() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let cache_home = tempfile::tempdir().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let source_image = format!(
        "localhost:9/decune-test/github-cli-source-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
            {{
              "image": "{source_image}"
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

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:4444444444444444444444444444444444444444444444444444444444444444",
        "#!/bin/sh\nset -eu\necho 'github-cli Feature should not be installed' >&2\nexit 72\n",
    );
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
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
        remove_image_if_exists(&source_image).await.unwrap();
        create_image_with_github_cli(&source_image).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .env("PATH", &fake_path)
            .env("XDG_CACHE_HOME", cache_home.path())
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
            inspect.id.unwrap()
        });

        runtime.block_on(async {
            remove_image_if_exists(&source_image).await.unwrap();
        });

        decune()
            .env("PATH", &fake_path)
            .env("XDG_CACHE_HOME", cache_home.path())
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            assert_eq!(inspect.id.as_deref(), Some(first_id.as_str()));
            assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let workspace_image_cleanup = cleanup_workspace_images(&workspace_root).await;
        let source_image_cleanup = remove_image_if_exists(&source_image).await;
        container_cleanup
            .and(workspace_image_cleanup)
            .and(source_image_cleanup)
            .unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_detects_github_cli_from_container_env_path_before_auto_adding_feature() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let cache_home = tempfile::tempdir().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN mkdir -p /opt/gh/bin \
              && printf '%s\n' \
                '#!/bin/sh' \
                'set -eu' \
                'if [ "$1" = auth ] && [ "$2" = login ]; then' \
                '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
                '  mkdir -p "$GH_CONFIG_DIR"' \
                '  test -r /run/decune/secrets/github-token' \
                '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
                '  cat > "$GH_CONFIG_DIR/token"' \
                '  exit 0' \
                'fi' \
                'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
                '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
                '  exit 0' \
                'fi' \
                'echo "unexpected fake gh command: $*" >&2' \
                'exit 91' \
                >/opt/gh/bin/gh \
              && chmod +x /opt/gh/bin/gh
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
              "containerEnv": {
                "PATH": "/opt/gh/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
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

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        "#!/bin/sh\nset -eu\necho 'github-cli Feature should not be installed' >&2\nexit 72\n",
    );
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
            .env("XDG_CACHE_HOME", cache_home.path())
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());
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
fn up_detach_expands_container_env_remote_user_home_path_before_github_cli_probe() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let cache_home = tempfile::tempdir().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D decune \
              && mkdir -p /home/decune/.local/bin \
              && printf '%s\n' \
                '#!/bin/sh' \
                'set -eu' \
                'if [ "$1" = auth ] && [ "$2" = login ]; then' \
                '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
                '  mkdir -p "$GH_CONFIG_DIR"' \
                '  test -r /run/decune/secrets/github-token' \
                '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
                '  cat > "$GH_CONFIG_DIR/token"' \
                '  exit 0' \
                'fi' \
                'if [ "$1" = auth ] && [ "$2" = setup-git ]; then' \
                '  test "${GH_CONFIG_DIR:-}" = /run/decune/gh' \
                '  test "$(id -un)" = decune' \
                '  exit 0' \
                'fi' \
                'echo "unexpected fake gh command: $*" >&2' \
                'exit 91' \
                >/home/decune/.local/bin/gh \
              && chmod +x /home/decune/.local/bin/gh
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
              "containerEnv": {
                "PATH": "${remoteUserHome}/.local/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"
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

            [credentials.git]
            enabled = false
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        "#!/bin/sh\nset -eu\necho 'github-cli Feature should not be installed' >&2\nexit 72\n",
    );
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
            .env("XDG_CACHE_HOME", cache_home.path())
            .env_remove("SSH_AUTH_SOCK")
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());
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
              '  test -r /run/decune/secrets/github-token' \
              '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
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
              '  test -r /run/decune/secrets/github-token' \
              '  if sh -c "printf bad >> /run/decune/secrets/github-token" 2>/dev/null; then exit 92; fi' \
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

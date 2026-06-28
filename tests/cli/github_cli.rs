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
            r"
            FROM alpine:3.20
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [credentials.git]
            enabled = false

            [credentials.github]
            install_feature_if_missing = false
            ",
        )
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let state_file = state_home
        .join("decune")
        .join(&workspace_id)
        .join("state.toml");
    let github_token_file = runtime_dir.join("secrets").join("github-token");
    let host_daemon_socket = runtime_dir.join("host-daemon.sock");
    with_clean_workspace_containers_and_images(&workspace_root, || {
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

        assert_github_token_not_leaked(
            &workspace_root,
            &github_token_file,
            &host_daemon_socket,
            &state_file,
        );
    });
}

fn assert_github_token_not_leaked(
    workspace_root: &Path,
    github_token_file: &Path,
    host_daemon_socket: &Path,
    state_file: &Path,
) {
    let inspect = inspect_single_workspace_container(workspace_root).must();
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
    let logs = workspace_container_logs(workspace_root).must();
    assert!(!logs.contains("github-test-secret"));
    assert_github_token_not_leaked_in_images(workspace_root);
    assert_eq!(fs::read_to_string(github_token_file).must(), "");
    assert_eq!(
        fs::metadata(github_token_file).must().permissions().mode() & 0o777,
        0o600
    );
    assert!(!host_daemon_socket.exists());
    assert!(
        !fs::read_to_string(state_file)
            .must()
            .contains("github-test-secret")
    );
}

fn assert_github_token_not_leaked_in_images(workspace_root: &Path) {
    let images = workspace_images(workspace_root).must();
    assert!(
        images
            .iter()
            .all(|image| !image.contains("github-test-secret"))
    );
    for image in images {
        let inspect = inspect_image(&image).must();
        let labels = inspect.config.and_then(|config| config.labels);
        assert!(
            labels
                .unwrap_or_default()
                .values()
                .all(|value| !value.contains("github-test-secret"))
        );
    }
}

fn fake_gh_token_path(host_tools: &support::TempWorkspace) -> std::ffi::OsString {
    fake_gh_path(host_tools, "cli/fake-bin/gh-auth-token.sh")
}

fn fake_gh_token_file_path(host_tools: &support::TempWorkspace) -> std::ffi::OsString {
    fake_gh_path(host_tools, "cli/fake-bin/gh-auth-token-file.sh")
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    with_clean_workspace_containers_and_images(&workspace_root, || {
        decune()
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        exec_single_workspace_container(
            &workspace_root,
            ["test", "!", "-f", "/tmp/decune-profile-leak"],
        )
        .must();
    });
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
            r"
            version = 1

            [credentials.git]
            enabled = false

            [credentials.github]
            install_feature_if_missing = false
            ",
        )
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    with_clean_workspace_containers_and_images(&workspace_root, || {
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
            r"
            version = 1

            [credentials.git]
            enabled = false

            [credentials.github]
            install_feature_if_missing = false
            ",
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/build-state", "ok\n")
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
            assert_eq!(inspect.id.as_deref(), Some(first_id.as_str()));
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    with_clean_workspace_containers_and_images(&workspace_root, || {
        decune()
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        assert_github_cli_config_env(&workspace_root);
    });
}

fn assert_github_cli_config_env(workspace_root: &Path) {
    let inspect = inspect_single_workspace_container(workspace_root).must();
    let env = inspect.config.unwrap_or_default().env.unwrap_or_default();
    assert!(
        env.iter()
            .any(|entry| entry == "GH_CONFIG_DIR=/run/decune/gh")
    );
    assert!(
        env.iter()
            .all(|entry| !entry.contains("github-test-secret"))
    );
}

#[test]
fn up_detach_sets_github_cli_config_when_remote_user_uid_differs_from_host_uid() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    let remote_user_id = if current_uid() == 20001 { 20002 } else { 20001 };
    let remote_group_id = if current_gid() == 20001 { 20002 } else { 20001 };
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            format!(
                r#"
            FROM alpine:3.20
            RUN addgroup -g {remote_group_id} decunegrp \
              && adduser -D -u {remote_user_id} -G decunegrp -h /home/decune decune \
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    std::panic::catch_unwind(|| {
        decune()
            .env("PATH", &fake_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"))
            .stderr(predicate::str::contains("github-test-secret").not());

        assert_github_cli_config_env_and_labels(&workspace_root);
    })
    .unwrap();
}

fn assert_github_cli_config_env_and_labels(workspace_root: &Path) {
    let inspect = inspect_single_workspace_container(workspace_root).must();
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
            r"
            version = 1

            [credentials.git]
            enabled = false

            [credentials.github]
            install_feature_if_missing = false
            ",
        )
        .unwrap();
    let fake_path = fake_gh_token_path(&host_tools);
    let gh_bin = host_tools.path().join("bin");
    let empty_path = empty_tools.create_dir("bin").unwrap();
    symlink_host_executable_into_path("docker", &gh_bin);
    symlink_host_executable_into_path("docker", &empty_path);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
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
    let fake_path = fake_gh_token_path(&host_tools);
    with_clean_workspace_containers_and_images(&workspace_root, || {
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

        let first_id = workspace_container_id(&workspace_root);

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

        assert_workspace_container_id(&workspace_root, &first_id);
    });
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
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
    let fake_path = fake_gh_token_path(&host_tools);
    with_clean_workspace_and_source_image(
        &workspace_root,
        &source_image,
        create_image_without_devcontainer_metadata,
        || {
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

            let first_id = assert_github_cli_env_and_container_id(&workspace_root);

            remove_image_if_exists(&source_image).must();

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

            assert_reused_github_cli_container(&workspace_root, &first_id);
        },
    );
}

fn with_clean_workspace_and_source_image<F, C>(
    workspace_root: &Path,
    source_image: &str,
    create_source_image: C,
    body: F,
) where
    F: FnOnce() + std::panic::UnwindSafe,
    C: FnOnce(&str) -> anyhow::Result<()>,
{
    cleanup_workspace_containers(workspace_root).must();
    cleanup_workspace_images(workspace_root).must();
    remove_image_if_exists(source_image).must();
    create_source_image(source_image).must();

    let result = std::panic::catch_unwind(body);

    cleanup_workspace_containers(workspace_root)
        .and_then(|()| cleanup_workspace_images(workspace_root))
        .and_then(|()| remove_image_if_exists(source_image))
        .must();

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

fn assert_github_cli_env_and_container_id(workspace_root: &Path) -> String {
    let inspect = inspect_single_workspace_container(workspace_root).must();
    assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
    inspect
        .id
        .must_msg("workspace container should include an id")
}

fn workspace_container_id(workspace_root: &Path) -> String {
    inspect_single_workspace_container(workspace_root)
        .must()
        .id
        .must_msg("workspace container should include an id")
}

fn assert_workspace_container_id(workspace_root: &Path, expected_id: &str) {
    let inspect = inspect_single_workspace_container(workspace_root).must();
    assert_eq!(inspect.id.as_deref(), Some(expected_id));
}

fn assert_running_container_has_expected_github_token(workspace_root: &Path) {
    exec_single_workspace_container(
        workspace_root,
        [
            "/bin/sh",
            "-lc",
            "test \"${GH_CONFIG_DIR:-}\" = /run/decune/gh && grep -qx \"$(cat /workspace/expected-token)\" \"$GH_CONFIG_DIR/token\"",
        ],
    )
    .must();
}

fn assert_reused_github_cli_container(workspace_root: &Path, first_id: &str) {
    let inspect = inspect_single_workspace_container(workspace_root).must();
    assert_eq!(inspect.id.as_deref(), Some(first_id));
    assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:4444444444444444444444444444444444444444444444444444444444444444",
        "#!/bin/sh\nset -eu\necho 'github-cli Feature should not be installed' >&2\nexit 72\n",
    );
    let fake_path = fake_gh_token_path(&host_tools);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
        remove_image_if_exists(&source_image).unwrap();
        create_image_with_github_cli(&source_image).unwrap();
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
            assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
            inspect.id.unwrap()
        });

        runtime.block_on(async {
            remove_image_if_exists(&source_image).unwrap();
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
            assert_eq!(inspect.id.as_deref(), Some(first_id.as_str()));
            assert!(inspect_has_env(&inspect, "GH_CONFIG_DIR=/run/decune/gh"));
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let workspace_image_cleanup = cleanup_workspace_images(&workspace_root);
        let source_image_cleanup = remove_image_if_exists(&source_image);
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:2222222222222222222222222222222222222222222222222222222222222222",
        "#!/bin/sh\nset -eu\necho 'github-cli Feature should not be installed' >&2\nexit 72\n",
    );
    let fake_path = fake_gh_token_path(&host_tools);
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    write_fake_github_cli_feature_cache(
        &workspace_root,
        cache_home.path(),
        "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        "#!/bin/sh\nset -eu\necho 'github-cli Feature should not be installed' >&2\nexit 72\n",
    );
    let fake_path = fake_gh_token_path(&host_tools);
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    workspace
        .write_file("expected-token", "first-secret\n")
        .unwrap();
    let host_token_path = host_tools.write_file("token", "first-secret\n").unwrap();
    let fake_path = fake_gh_token_file_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    with_clean_workspace_containers_and_images(&workspace_root, || {
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

        let first_id = workspace_container_id(&workspace_root);

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

        assert_workspace_container_id(&workspace_root, &first_id);
    });
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
            r"
            version = 1

            [credentials.git]
            enabled = false
            ",
        )
        .unwrap();
    workspace
        .write_file("expected-token", "first-secret\n")
        .unwrap();
    let host_token_path = host_tools.write_file("token", "first-secret\n").unwrap();
    let fake_path = fake_gh_token_file_path(&host_tools);
    let workspace_root = workspace.path().canonicalize().unwrap();
    with_clean_workspace_containers_and_images(&workspace_root, || {
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

        let first_id = inspect_single_workspace_container(&workspace_root)
            .must()
            .id
            .must_msg("workspace container should include an id");

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

        assert_workspace_container_id(&workspace_root, &first_id);
        assert_running_container_has_expected_github_token(&workspace_root);
    });
}

use crate::harness::*;

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
fn up_detach_rejects_missing_state_for_existing_container_lifecycle() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "onCreateCommand": "printf x >> /tmp/decune-on-create-count"
            }
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_file = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("state.toml");
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
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let first_state = fs::read_to_string(&state_file).unwrap();
        assert!(first_state.contains("container_id = "));
        assert!(first_state.contains("on_create_completed = true"));
        fs::remove_file(&state_file).unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot safely reuse existing dev container without matching lifecycle state",
            ));

        let on_create_count = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-on-create-count"],
                )
                .await
            })
            .unwrap();
        assert_eq!(on_create_count, "x");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_rejects_missing_state_for_stopped_container_without_starting_it() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "onCreateCommand": "printf x >> /tmp/decune-on-create-count"
            }
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_file = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("state.toml");
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
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .arg("down")
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Stopped dev container"));

        fs::remove_file(&state_file).unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot safely reuse existing dev container without matching lifecycle state",
            ));

        let inspect = runtime
            .block_on(async { inspect_single_workspace_container(&workspace_root).await })
            .unwrap();
        assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_rejects_corrupt_state_for_stopped_container_without_starting_it() {
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
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_file = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("state.toml");
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
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        decune()
            .arg("down")
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Stopped dev container"));

        fs::write(&state_file, "version = [").unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Invalid decune state file"))
            .stderr(predicate::str::contains(state_file.display().to_string()));

        let inspect = runtime
            .block_on(async { inspect_single_workspace_container(&workspace_root).await })
            .unwrap();
        assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_persists_initial_lifecycle_state_before_on_create() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "onCreateCommand": "printf on-create >> /tmp/decune-lifecycle-order; exit 7",
              "updateContentCommand": "printf update-content >> /tmp/decune-lifecycle-order",
              "postCreateCommand": "printf post-create >> /tmp/decune-lifecycle-order"
            }
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_file = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("state.toml");
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
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Lifecycle stage onCreateCommand failed",
            ));

        let failed_state = fs::read_to_string(&state_file).unwrap();
        assert!(failed_state.contains("on_create_completed = false"));
        assert!(failed_state.contains("update_content_completed = false"));
        assert!(failed_state.contains("post_create_completed = false"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_resumes_pending_creation_lifecycle_when_reusing_running_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "onCreateCommand": "printf on-create >> /tmp/decune-lifecycle-order",
              "updateContentCommand": "test ! -f fail-update-content && printf update-content >> /tmp/decune-lifecycle-order",
              "postCreateCommand": "printf post-create >> /tmp/decune-lifecycle-order",
              "postStartCommand": "printf post-start >> /tmp/decune-lifecycle-order"
            }
            "#,
        )
        .unwrap();
    workspace.write_file("fail-update-content", "").unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_file = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("state.toml");
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
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Lifecycle stage updateContentCommand failed",
            ));

        let failed_state = fs::read_to_string(&state_file).unwrap();
        assert!(failed_state.contains("on_create_completed = true"));
        assert!(failed_state.contains("update_content_completed = false"));
        assert!(failed_state.contains("post_create_completed = false"));

        fs::remove_file(workspace_root.join("fail-update-content")).unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));

        let completed_state = fs::read_to_string(&state_file).unwrap();
        assert!(completed_state.contains("on_create_completed = true"));
        assert!(completed_state.contains("update_content_completed = true"));
        assert!(completed_state.contains("post_create_completed = true"));

        let lifecycle_order = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-lifecycle-order"],
                )
                .await
            })
            .unwrap();
        assert_eq!(
            lifecycle_order,
            "on-createupdate-contentpost-createpost-start"
        );
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_does_not_rerun_completed_creation_command_after_after_hook_failure() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "onCreateCommand": "printf on-create >> /tmp/decune-lifecycle-order",
              "updateContentCommand": "printf update-content >> /tmp/decune-lifecycle-order",
              "postCreateCommand": "printf post-create >> /tmp/decune-lifecycle-order",
              "postStartCommand": "printf post-start >> /tmp/decune-lifecycle-order"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [[hooks.after_on_create]]
            command = "test ! -f fail-after-on-create"
            where = "container"
            "#,
        )
        .unwrap();
    workspace.write_file("fail-after-on-create", "").unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_file = state_home
        .join("decune")
        .join(workspace_id(&workspace_root))
        .join("state.toml");
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
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Lifecycle stage after_on_create failed",
            ));

        let failed_state = fs::read_to_string(&state_file).unwrap();
        assert!(failed_state.contains("on_create_completed = true"));
        assert!(failed_state.contains("update_content_completed = false"));
        assert!(failed_state.contains("post_create_completed = false"));

        fs::remove_file(workspace_root.join("fail-after-on-create")).unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Reusing running dev container"));

        let completed_state = fs::read_to_string(&state_file).unwrap();
        assert!(completed_state.contains("on_create_completed = true"));
        assert!(completed_state.contains("update_content_completed = true"));
        assert!(completed_state.contains("post_create_completed = true"));

        let lifecycle_order = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-lifecycle-order"],
                )
                .await
            })
            .unwrap();
        assert_eq!(
            lifecycle_order,
            "on-createupdate-contentpost-createpost-start"
        );
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

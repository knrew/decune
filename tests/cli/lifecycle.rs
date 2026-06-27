use crate::harness::*;

fn fake_container_tools_bundle(workspace: &support::TempWorkspace) -> PathBuf {
    workspace
        .write_file("container-tools/linux-amd64/decune-forward-agent", b"agent")
        .unwrap();
    workspace
        .write_file(
            "container-tools/linux-amd64/git-credential-decune",
            b"helper",
        )
        .unwrap();
    workspace
        .write_file(
            "container-tools/manifest.json",
            r#"{"schemaVersion":1,"protocolVersion":1,"tools":[{"name":"decune-forward-agent","platform":"linux-amd64","path":"linux-amd64/decune-forward-agent","sha256":"d4f0bc5a29de06b510f9aa428f1eedba926012b591fef7a518e776a7c9bd1824"},{"name":"git-credential-decune","platform":"linux-amd64","path":"linux-amd64/git-credential-decune","sha256":"e81d3b0e9d82feaaf5f6e55bdff24731d7eee08632ffa63801e6397290c5d20a"}]}"#,
        )
        .unwrap();
    workspace.path().join("container-tools")
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state == "running")
            );
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
fn up_detach_rejects_reuse_when_local_env_derived_container_env_changes() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "containerEnv": {
                "NPM_TOKEN": "${localEnv:DECUNE_TEST_NPM_TOKEN}"
              }
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
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result =
        std::panic::catch_unwind(|| {
            decune()
                .args(["up", "--detach"])
                .arg(&workspace_root)
                .env("DECUNE_TEST_NPM_TOKEN", "first-secret")
                .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
                .assert()
                .success()
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains("Started dev container"))
                .stderr(predicate::str::contains("first-secret").not());

            let first = runtime
                .block_on(async { inspect_single_workspace_container(&workspace_root) })
                .unwrap();
            let first_id = first.id.clone().unwrap();
            let first_labels = first.config.as_ref().unwrap().labels.as_ref().unwrap();
            let first_hash = first_labels.get("decune.config_hash").unwrap().clone();
            assert!(inspect_has_env(&first, "NPM_TOKEN=first-secret"));
            assert!(!inspect_has_env(&first, "NPM_TOKEN=second-secret"));
            assert!(
                first_labels
                    .values()
                    .all(|value| !value.contains("first-secret"))
            );

            decune()
                .args(["up", "--detach"])
                .arg(&workspace_root)
                .env("DECUNE_TEST_NPM_TOKEN", "second-secret")
                .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
                .assert()
                .failure()
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(
                    "Dev container configuration changed. Run decune rebuild to recreate it.",
                ))
                .stderr(predicate::str::contains("second-secret").not());

            let unchanged = runtime
                .block_on(async { inspect_single_workspace_container(&workspace_root) })
                .unwrap();
            assert_eq!(unchanged.id.as_deref(), Some(first_id.as_str()));
            assert!(inspect_has_env(&unchanged, "NPM_TOKEN=first-secret"));

            decune()
                .args(["rebuild", "--detach"])
                .arg(&workspace_root)
                .env("DECUNE_TEST_NPM_TOKEN", "second-secret")
                .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
                .assert()
                .success()
                .stdout(predicate::str::is_empty())
                .stderr(predicate::str::contains(
                    "Removed existing dev container for rebuild",
                ))
                .stderr(predicate::str::contains("Started dev container"))
                .stderr(predicate::str::contains("second-secret").not());

            let second = runtime
                .block_on(async { inspect_single_workspace_container(&workspace_root) })
                .unwrap();
            let second_id = second.id.clone().unwrap();
            let second_labels = second.config.as_ref().unwrap().labels.as_ref().unwrap();
            let second_hash = second_labels.get("decune.config_hash").unwrap();
            assert_ne!(first_id, second_id);
            assert_ne!(&first_hash, second_hash);
            assert!(inspect_has_env(&second, "NPM_TOKEN=second-secret"));
            assert!(!inspect_has_env(&second, "NPM_TOKEN=first-secret"));
            assert!(second_labels.values().all(|value| {
                !value.contains("first-secret") && !value.contains("second-secret")
            }));
        });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            })
            .unwrap();
        assert_eq!(on_create_count, "x");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            .block_on(async { inspect_single_workspace_container(&workspace_root) })
            .unwrap();
        assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            .block_on(async { inspect_single_workspace_container(&workspace_root) })
            .unwrap();
        assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            })
            .unwrap();
        assert_eq!(
            lifecycle_order,
            "on-createupdate-contentpost-createpost-start"
        );
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_retries_failed_after_hook_without_rerunning_completed_creation_command() {
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
            command = "test ! -f fail-after-on-create && printf after-on-create >> /tmp/decune-lifecycle-order"
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        assert!(failed_state.contains("after_on_create_completed = false"));
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
        assert!(completed_state.contains("after_on_create_completed = true"));
        assert!(completed_state.contains("update_content_completed = true"));
        assert!(completed_state.contains("post_create_completed = true"));

        let lifecycle_order = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-lifecycle-order"],
                )
            })
            .unwrap();
        assert_eq!(
            lifecycle_order,
            "on-createafter-on-createupdate-contentpost-createpost-start"
        );
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            r"
            FROM alpine:3.20
            RUN printf '#!/bin/sh\nexit 7\n' >/usr/local/bin/decune-exit-7 \
              && chmod +x /usr/local/bin/decune-exit-7
            ",
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root);
        let image_cleanup = cleanup_workspace_images(&workspace_root);
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_expands_remote_env_from_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'test "$PATH" = "/usr/bin:/bin:/extra" || exit 11' \
              'test "$DECUNE_DEFAULT_ENV" = "fallback" || exit 12' \
              'printf "%s|%s" "$PATH" "$DECUNE_DEFAULT_ENV" >/tmp/decune-container-env-expansion' \
              >/usr/local/bin/decune-check-expanded-env \
              && chmod +x /usr/local/bin/decune-check-expanded-env
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
                "PATH": "/usr/bin:/bin"
              },
              "remoteEnv": {
                "PATH": "${containerEnv:PATH}:/extra",
                "DECUNE_DEFAULT_ENV": "${containerEnv:DECUNE_MISSING:fallback}"
              },
              "userEnvProbe": "none",
              "postStartCommand": ["/usr/local/bin/decune-check-expanded-env"]
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let output = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-container-env-expansion"],
                )
            })
            .unwrap();
        assert_eq!(output, "/usr/bin:/bin:/extra|fallback");
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
fn up_detach_expands_remote_env_from_actual_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r"
            FROM alpine:3.20
            ENV DECUNE_FROM_IMAGE=from-image
            ",
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
              "remoteEnv": {
                "PATH": "${containerEnv:PATH}:/image-extra",
                "DECUNE_IMAGE_ENV": "${containerEnv:DECUNE_FROM_IMAGE}"
              },
              "userEnvProbe": "none",
              "postStartCommand": [
                "/bin/sh",
                "-c",
                "test \"$PATH\" = \"/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/image-extra\" && test \"$DECUNE_IMAGE_ENV\" = from-image && printf \"%s|%s\" \"$PATH\" \"$DECUNE_IMAGE_ENV\" >/tmp/decune-actual-container-env-expansion"
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let output = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-actual-container-env-expansion"],
                )
            })
            .unwrap();
        assert_eq!(
            output,
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/image-extra|from-image"
        );
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
fn up_attached_expands_remote_env_from_actual_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            ENV DECUNE_FROM_IMAGE=from-image
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'test "$PATH" = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/attach-extra" || exit 11' \
              'test "$DECUNE_IMAGE_ENV" = "from-image" || exit 12' \
              'printf "%s|%s" "$PATH" "$DECUNE_IMAGE_ENV" >/tmp/decune-attached-container-env-expansion' \
              'exit 0' \
              >/usr/local/bin/decune-check-attached-env \
              && chmod +x /usr/local/bin/decune-check-attached-env
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
              "remoteEnv": {
                "PATH": "${containerEnv:PATH}:/attach-extra",
                "DECUNE_IMAGE_ENV": "${containerEnv:DECUNE_FROM_IMAGE}"
              },
              "userEnvProbe": "none",
              "shutdownAction": "none"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-check-attached-env"
            "#,
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
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let output = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-attached-container-env-expansion"],
                )
            })
            .unwrap();
        assert_eq!(
            output,
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin:/attach-extra|from-image"
        );
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
fn up_attached_defaults_to_stopping_image_container_after_shell_exit() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'printf ok > attached-shutdown-marker' \
              'exit 0' \
              >/usr/local/bin/decune-record-attached-shutdown \
              && chmod +x /usr/local/bin/decune-record-attached-shutdown
            ",
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
              "userEnvProbe": "none"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1
            shell = "/usr/local/bin/decune-record-attached-shutdown"
            "#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let marker = workspace_root.join("attached-shutdown-marker");
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
            .arg("up")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        assert_eq!(fs::read_to_string(&marker).unwrap(), "ok");
        let inspect = runtime
            .block_on(async { inspect_single_workspace_container(&workspace_root) })
            .unwrap();
        assert_eq!(inspect.state.and_then(|state| state.running), Some(false));
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
fn up_detach_rejects_container_env_self_reference() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "containerEnv": {
                "PATH": "${containerEnv:PATH}:/extra"
              }
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
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "containerEnv value must not reference containerEnv",
            ));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_warns_and_continues_when_user_env_probe_fails() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r"
            FROM alpine:3.20
            RUN adduser -D -s /bin/false decune
            ",
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
                "DECUNE_REMOTE_ENV_AFTER_FAILED_PROBE": "ok"
              },
              "postStartCommand": "test \"$DECUNE_REMOTE_ENV_AFTER_FAILED_PROBE\" = \"ok\" && printf ok >/tmp/decune-probe-fallback"
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Warning: User environment probe failed",
            ))
            .stderr(predicate::str::contains("Started dev container"));

        let output = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-probe-fallback"],
                )
            })
            .unwrap();
        assert_eq!(output, "ok");
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
fn up_detach_applies_probe_env_to_remote_process_not_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN printf '%s\n' \
              '#!/bin/sh' \
              'export DECUNE_PROBED_REMOTE_ONLY=from-probe' \
              'exec /bin/sh "$@"' \
              >/usr/local/bin/decune-probe-shell \
              && chmod +x /usr/local/bin/decune-probe-shell \
              && adduser -D -s /usr/local/bin/decune-probe-shell decune
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
              "postStartCommand": "test \"$DECUNE_PROBED_REMOTE_ONLY\" = \"from-probe\" && printf '%s' \"$DECUNE_PROBED_REMOTE_ONLY\" >/tmp/decune-probe-remote-only"
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
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let output = runtime
            .block_on(async {
                exec_single_workspace_container(
                    &workspace_root,
                    ["cat", "/tmp/decune-probe-remote-only"],
                )
            })
            .unwrap();
        assert_eq!(output, "from-probe");

        let inspect = runtime
            .block_on(async { inspect_single_workspace_container(&workspace_root) })
            .unwrap();
        assert!(!inspect_has_env(
            &inspect,
            "DECUNE_PROBED_REMOTE_ONLY=from-probe"
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            let containers = workspace_containers(&workspace_root).unwrap();
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
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0].id.as_deref(), Some(first_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state == "running")
            );
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
            let inspect = inspect_single_workspace_container(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

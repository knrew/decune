use crate::harness::*;

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
fn up_detach_ignores_unsupported_image_metadata_forward_ports() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-forward-{}:latest",
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
        create_image_with_devcontainer_metadata_label(
            &image_tag,
            r#"{"forwardPorts":["db:5432"]}"#,
        )
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
            .stderr(
                predicate::str::contains("Port forwarding is ignored in detached mode")
                    .and(predicate::str::contains("Started dev container")),
            );
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
fn up_detach_default_override_command_keeps_short_lived_image_running() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/override-default-{}:latest",
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
                  "postStartCommand": "true"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_cmd(&image_tag, vec!["true"])
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = remove_image_if_exists(&image_tag).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_override_command_false_respects_short_lived_image_command() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/override-false-exit-{}:latest",
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
                  "overrideCommand": false
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_cmd(&image_tag, vec!["true"])
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Container exited during startup"))
            .stderr(predicate::str::contains("Started dev container").not());
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
fn up_detach_override_command_false_detects_delayed_image_command_exit() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/override-false-delayed-exit-{}:latest",
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
                  "overrideCommand": false
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_cmd(&image_tag, vec!["/bin/sh", "-c", "sleep 1; exit 23"])
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Container exited during startup"))
            .stderr(predicate::str::contains("exit code 23"))
            .stderr(predicate::str::contains("Started dev container").not());
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
fn up_detach_detects_override_false_image_command_change_before_reuse() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/override-false-command-change-{}:latest",
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
                  "overrideCommand": false,
                  "postStartCommand": "test -f /tmp/decune-first-command"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_cmd(
            &image_tag,
            vec![
                "/bin/sh",
                "-c",
                "touch /tmp/decune-first-command && trap 'exit 0' TERM; while sleep 1 & wait $!; do :; done",
            ],
        )
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
            create_image_with_cmd(
                &image_tag,
                vec![
                    "/bin/sh",
                    "-c",
                    "touch /tmp/decune-second-command && trap 'exit 0' TERM; while sleep 1 & wait $!; do :; done",
                ],
            )
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
fn up_detach_image_metadata_entrypoint_defaults_to_keepalive() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-entrypoint-override-default-{}:latest",
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
                  "postStartCommand": "test -f /tmp/decune-image-metadata-entrypoint && test ! -f /tmp/decune-image-command"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_devcontainer_metadata_label_and_cmd(
            &image_tag,
            r#"{"entrypoint":"touch /tmp/decune-image-metadata-entrypoint"}"#,
            vec!["/bin/sh", "-c", "touch /tmp/decune-image-command"],
        )
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
fn up_detach_runs_image_metadata_only_entrypoint_without_feature_layer() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-entrypoint-{}:latest",
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
                  "postStartCommand": "test -f /tmp/decune-image-metadata-entrypoint && test -f /tmp/decune-image-command"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_devcontainer_metadata_label_and_cmd(
            &image_tag,
            r#"{"overrideCommand":false,"entrypoint":"touch /tmp/decune-image-metadata-entrypoint"}"#,
            vec![
                "/bin/sh",
                "-c",
                "touch /tmp/decune-image-command && trap 'exit 0' TERM; while sleep 1 & wait $!; do :; done",
            ],
        )
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
fn up_detach_runs_image_metadata_entrypoint_as_nonroot_image_user() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/image-metadata-entrypoint-nonroot-{}:latest",
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
                  "postStartCommand": "test \"$(id -un)\" = app && test -f /tmp/decune-image-metadata-entrypoint"
                }}
                "#
            ),
        )
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_nonroot_image_with_devcontainer_metadata_label_and_cmd(
            &image_tag,
            r#"{"entrypoint":"touch /tmp/decune-image-metadata-entrypoint"}"#,
            vec!["true"],
        )
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

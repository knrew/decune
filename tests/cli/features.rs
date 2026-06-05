use crate::harness::*;

#[test]
fn up_detach_applies_local_feature_layer_and_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/env-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "features": {
                "./features/env-tool": {
                  "value": "from-option"
                }
              },
              "remoteUser": "remoteuser",
              "postStartCommand": "test \"${FROM_FEATURE:-}\" = yes && test -f /usr/local/share/decune-feature-installed"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -u 1001 remoteuser
            USER root
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/env-tool/devcontainer-feature.json",
            r#"
            {
              "id": "env-tool",
              "version": "1.0.0",
              "name": "Env Tool",
              "options": {
                "value": {
                  "type": "string",
                  "default": "default"
                },
                "default-value": {
                  "type": "string",
                  "default": "from-default"
                }
              },
              "containerEnv": {
                "FROM_FEATURE": "yes"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/env-tool/install.sh",
            r#"
            set -eu
            test "${VALUE:-}" = "from-option"
            test "${DEFAULT_VALUE:-}" = "from-default"
            test "${FROM_FEATURE:-}" = yes
            test "${_CONTAINER_USER:-}" = root
            test "${_REMOTE_USER:-}" = remoteuser
            test "${_CONTAINER_USER_HOME:-}" = /root
            test "${_REMOTE_USER_HOME:-}" = /home/remoteuser
            mkdir -p /usr/local/share
            echo installed > /usr/local/share/decune-feature-installed
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
            .stderr(predicate::str::contains("Building Docker image"))
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let config = inspect.config.unwrap_or_default();
            let env = config.env.unwrap_or_default();
            assert!(env.iter().any(|entry| entry == "FROM_FEATURE=yes"));
            assert!(
                config
                    .image
                    .as_deref()
                    .is_some_and(|image| image.starts_with("decune/"))
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
fn up_detach_rejects_feature_metadata_remote_user() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/user-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/user-tool": {}
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/user-tool/devcontainer-feature.json",
            r#"
            {
              "id": "user-tool",
              "version": "1.0.0",
              "name": "User Tool",
              "remoteUser": "featureuser"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/user-tool/install.sh",
            r#"
            set -eu
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
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("remoteUser"));
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
fn up_detach_preserves_base_image_user_after_feature_layer() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/root-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "features": {
                "./features/root-tool": {}
              },
              "postStartCommand": "test \"$(id -un)\" = devuser && test -f /usr/local/share/decune-root-tool"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -u 1001 devuser
            USER devuser
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/root-tool/devcontainer-feature.json",
            r#"
            {
              "id": "root-tool",
              "version": "1.0.0",
              "name": "Root Tool"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/root-tool/install.sh",
            r#"
            set -eu
            test "$(id -un)" = root
            mkdir -p /usr/local/share
            echo installed > /usr/local/share/decune-root-tool
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_isolates_feature_option_env_between_features() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/alpha")
        .unwrap();
    workspace.create_dir(".devcontainer/features/beta").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/alpha": {
                  "version": "from-alpha"
                },
                "./features/beta": {}
              },
              "postStartCommand": "test -f /usr/local/share/decune-alpha && test -f /usr/local/share/decune-beta"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/alpha/devcontainer-feature.json",
            r#"
            {
              "id": "alpha",
              "version": "1.0.0",
              "name": "Alpha",
              "options": {
                "version": {
                  "type": "string",
                  "default": "default"
                }
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/alpha/install.sh",
            r#"
            set -eu
            test "${VERSION:-}" = "from-alpha"
            mkdir -p /usr/local/share
            echo installed > /usr/local/share/decune-alpha
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/beta/devcontainer-feature.json",
            r#"
            {
              "id": "beta",
              "version": "1.0.0",
              "name": "Beta"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/beta/install.sh",
            r#"
            set -eu
            test -z "${VERSION+x}"
            mkdir -p /usr/local/share
            echo installed > /usr/local/share/decune-beta
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
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_reuses_existing_container_without_reapplying_feature_metadata_label() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/lifecycle-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/lifecycle-tool": {}
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/lifecycle-tool/devcontainer-feature.json",
            r#"
            {
              "id": "lifecycle-tool",
              "version": "1.0.0",
              "name": "Lifecycle Tool",
              "postStartCommand": "echo feature-post-start >> /tmp/decune-feature-lifecycle"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/lifecycle-tool/install.sh",
            r#"
            set -eu
            mkdir -p /usr/local/share
            echo installed > /usr/local/share/decune-lifecycle-tool
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

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let container_labels = inspect
                .config
                .as_ref()
                .and_then(|config| config.labels.as_ref())
                .expect("container labels should exist");
            let container_config_hash = container_labels
                .get("decune.config_hash")
                .expect("container should include decune config hash label")
                .clone();
            let image = inspect
                .config
                .as_ref()
                .and_then(|config| config.image.clone())
                .expect("container image should exist");
            let docker = Docker::connect_with_defaults().unwrap();
            let image = docker.inspect_image(&image).await.unwrap();
            let labels = image.config.and_then(|config| config.labels);
            assert!(
                !labels
                    .as_ref()
                    .is_some_and(|labels| labels.contains_key("devcontainer.metadata")),
                "final Feature image must not store decune-applied Feature metadata in devcontainer.metadata"
            );
            assert_eq!(
                labels
                    .as_ref()
                    .and_then(|labels| labels.get("decune.config_hash")),
                Some(&container_config_hash),
                "final Feature image label must match the container config hash"
            );
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
        let image_cleanup = cleanup_workspace_images(&workspace_root).await;
        container_cleanup.and(image_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_runs_feature_lifecycle_before_user_lifecycle() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/alpha")
        .unwrap();
    workspace.create_dir(".devcontainer/features/beta").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/alpha": {},
                "./features/beta": {}
              },
              "postStartCommand": "printf 'user\n' >> /tmp/decune-lifecycle-order && test \"$(cat /tmp/decune-lifecycle-order)\" = \"alpha\nbeta\nuser\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/alpha/devcontainer-feature.json",
            r#"
            {
              "id": "alpha",
              "version": "1.0.0",
              "name": "Alpha",
              "postStartCommand": "printf 'alpha\n' >> /tmp/decune-lifecycle-order"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/features/alpha/install.sh", "set -eu\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/beta/devcontainer-feature.json",
            r#"
            {
              "id": "beta",
              "version": "1.0.0",
              "name": "Beta",
              "postStartCommand": "printf 'beta\n' >> /tmp/decune-lifecycle-order"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/features/beta/install.sh", "set -eu\n")
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
fn up_detach_runs_feature_entrypoint_before_lifecycle() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/entrypoint-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/entrypoint-tool": {}
              },
              "postStartCommand": "test -f /tmp/decune-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/devcontainer-feature.json",
            r#"
            {
              "id": "entrypoint-tool",
              "version": "1.0.0",
              "name": "Entrypoint Tool",
              "entrypoint": "touch /tmp/decune-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/install.sh",
            "set -eu\n",
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
fn up_detach_runs_feature_entrypoint_as_nonroot_image_user() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/entrypoint-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "features": {
                "./features/entrypoint-tool": {}
              },
              "postStartCommand": "test \"$(id -un)\" = app && test -f /tmp/decune-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -u 20001 app
            USER app
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/devcontainer-feature.json",
            r#"
            {
              "id": "entrypoint-tool",
              "version": "1.0.0",
              "name": "Entrypoint Tool",
              "entrypoint": "touch /tmp/decune-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/install.sh",
            "set -eu\n",
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
fn up_detach_runs_feature_entrypoint_when_override_command_is_false() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/entrypoint-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "overrideCommand": false,
              "features": {
                "./features/entrypoint-tool": {}
              },
              "postStartCommand": "test -f /tmp/decune-feature-entrypoint && test -f /tmp/decune-image-command"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            CMD ["/bin/sh", "-c", "touch /tmp/decune-image-command && trap 'exit 0' TERM; while sleep 1 & wait $!; do :; done"]
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/devcontainer-feature.json",
            r#"
            {
              "id": "entrypoint-tool",
              "version": "1.0.0",
              "name": "Entrypoint Tool",
              "entrypoint": "touch /tmp/decune-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/install.sh",
            "set -eu\n",
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
fn up_detach_runs_feature_entrypoints_in_install_order() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/alpha")
        .unwrap();
    workspace.create_dir(".devcontainer/features/beta").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/beta": {},
                "./features/alpha": {}
              },
              "postStartCommand": "actual=$(cat /tmp/decune-feature-entrypoint-order); expected=$(printf '%s\\n%s' alpha beta); test \"$actual\" = \"$expected\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/alpha/devcontainer-feature.json",
            r#"
            {
              "id": "alpha",
              "version": "1.0.0",
              "name": "Alpha",
              "entrypoint": "printf '%s\\n' alpha >> /tmp/decune-feature-entrypoint-order"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/features/alpha/install.sh", "set -eu\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/beta/devcontainer-feature.json",
            r#"
            {
              "id": "beta",
              "version": "1.0.0",
              "name": "Beta",
              "entrypoint": "printf '%s\\n' beta >> /tmp/decune-feature-entrypoint-order"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/features/beta/install.sh", "set -eu\n")
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
fn up_detach_waits_for_slow_feature_entrypoint_without_timing_out() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/entrypoint-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/entrypoint-tool": {}
              },
              "postStartCommand": "test -f /tmp/decune-slow-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/devcontainer-feature.json",
            r#"
            {
              "id": "entrypoint-tool",
              "version": "1.0.0",
              "name": "Entrypoint Tool",
              "entrypoint": "sleep 31; touch /tmp/decune-slow-feature-entrypoint"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/install.sh",
            "set -eu\n",
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
fn up_detach_fails_when_feature_entrypoint_exits_non_zero() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/entrypoint-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/entrypoint-tool": {}
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/devcontainer-feature.json",
            r#"
            {
              "id": "entrypoint-tool",
              "version": "1.0.0",
              "name": "Entrypoint Tool",
              "entrypoint": "echo decune-entrypoint-failed >&2; exit 23"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/install.sh",
            "set -eu\n",
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
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Container exited during startup"))
            .stderr(predicate::str::contains("Started dev container").not());
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
fn up_detach_fails_when_feature_entrypoint_exits_non_zero_after_delay() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/entrypoint-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "./features/entrypoint-tool": {}
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/devcontainer-feature.json",
            r#"
            {
              "id": "entrypoint-tool",
              "version": "1.0.0",
              "name": "Entrypoint Tool",
              "entrypoint": "sleep 1; echo decune-entrypoint-failed >&2; exit 23"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/entrypoint-tool/install.sh",
            "set -eu\n",
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
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Container exited during startup"))
            .stderr(predicate::str::contains("exit code 23"))
            .stderr(predicate::str::contains("Started dev container").not());
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
#[ignore = "requires public OCI registry access"]
fn up_detach_starts_container_with_public_devcontainer_feature() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "features": {
                "ghcr.io/devcontainers/features/common-utils:2": {
                  "installZsh": false,
                  "upgradePackages": false
                }
              },
              "postStartCommand": "test -d /usr/local/share/devcontainer-features"
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

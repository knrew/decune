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
              "image": "alpine:3.20",
              "features": {
                "./features/env-tool": {
                  "value": "from-option"
                }
              },
              "postStartCommand": "test \"${FROM_FEATURE:-}\" = yes && test -f /usr/local/share/decune-feature-installed"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/env-tool/devcontainer-feature.json",
            r#"
            {
              "id": "env-tool",
              "options": {
                "value": {
                  "type": "string",
                  "default": "default"
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
              "id": "root-tool"
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
              "id": "beta"
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
            let image = inspect
                .config
                .and_then(|config| config.image)
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

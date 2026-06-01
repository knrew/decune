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

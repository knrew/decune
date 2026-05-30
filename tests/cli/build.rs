use crate::harness::*;

#[test]
fn up_detach_builds_dockerfile_container_and_honors_dockerignore() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "EXPECTED": "from-arg"
                },
                "target": "dev"
              }
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20 AS base
            ARG EXPECTED
            RUN test "$EXPECTED" = "from-arg"
            COPY . /context
            RUN test -f /context/app.txt && test ! -e /context/secret.env
            FROM base AS dev
            RUN true
            FROM alpine:3.20 AS unused
            RUN false
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/.dockerignore", "secret.env\n")
        .unwrap();
    workspace
        .write_file(".devcontainer/app.txt", "included\n")
        .unwrap();
    workspace
        .write_file(".devcontainer/secret.env", "excluded\n")
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
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert!(
                containers[0]
                    .image
                    .as_ref()
                    .is_some_and(|image| image.starts_with("decune/"))
            );
            let images = workspace_images(&workspace_root).await.unwrap();
            assert_eq!(images.len(), 1);
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
fn up_dockerfile_metadata_label_warns_and_is_not_merged() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "postStartCommand": "test \"${FROM_DOCKERFILE_LABEL:-}\" = \"\" && test \"$(id -un)\" = \"root\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            LABEL devcontainer.metadata="{\"remoteUser\":\"nobody\",\"remoteEnv\":{\"FROM_DOCKERFILE_LABEL\":\"set\"}}"
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
            .stderr(predicate::str::contains(
                "Dockerfile image label devcontainer.metadata is not merged",
            ))
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

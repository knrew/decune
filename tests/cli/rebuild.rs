use crate::harness::*;

#[test]
fn rebuild_recreates_container_and_preserves_managed_volume() {
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
    let volume_name = format!("decune-rebuild-test-{}", workspace_id(&workspace_root));

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_volumes(&workspace_root).await.unwrap();
        create_managed_volume(&workspace_root, &volume_name)
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

        let first_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["rebuild", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Removed existing dev container for rebuild",
            ))
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_ne!(containers[0].id.as_deref(), Some(first_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state == "running")
            );

            let volumes = workspace_volumes(&workspace_root).await.unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });

        let second_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["up", "--detach", "--rebuild"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Removed existing dev container for rebuild",
            ))
            .stderr(predicate::str::contains("Started dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_ne!(containers[0].id.as_deref(), Some(second_id.as_str()));

            let volumes = workspace_volumes(&workspace_root).await.unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });
    });

    runtime.block_on(async {
        let container_cleanup = cleanup_workspace_containers(&workspace_root).await;
        let volume_cleanup = cleanup_workspace_volumes(&workspace_root).await;
        container_cleanup.and(volume_cleanup).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn rebuild_no_cache_reruns_dockerfile_build_steps() {
    let workspace = support::TempWorkspace::new().unwrap();
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
            RUN head -c 16 /dev/urandom | od -An -tx1 | tr -d ' \n' > /build-token
            ",
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

        let first_token = runtime
            .block_on(async {
                exec_single_workspace_container(&workspace_root, ["cat", "/build-token"]).await
            })
            .unwrap();

        decune()
            .args(["rebuild", "--detach", "--no-cache"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Removed existing dev container for rebuild",
            ))
            .stderr(predicate::str::contains("Started dev container"));

        let second_token = runtime
            .block_on(async {
                exec_single_workspace_container(&workspace_root, ["cat", "/build-token"]).await
            })
            .unwrap();

        assert_ne!(first_token, second_token);
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

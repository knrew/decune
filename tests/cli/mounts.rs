use crate::harness::*;

#[test]
fn up_detach_publishes_app_port() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "appPort": ["8080"]
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

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let ports = inspect
                .network_settings
                .and_then(|settings| settings.ports)
                .unwrap_or_default();
            let bindings = ports
                .get("8080/tcp")
                .and_then(|bindings| bindings.as_ref())
                .expect("expected appPort to publish 8080/tcp");
            let binding = bindings
                .first()
                .expect("expected at least one published appPort binding");

            assert!(
                binding
                    .host_port
                    .as_deref()
                    .is_some_and(|port| !port.is_empty())
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
fn up_detach_applies_project_read_only_bind_mount() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace.create_dir("host-cache").unwrap();
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

[[mounts]]
source = "host-cache"
target = "/mnt/decune-cache"
type = "bind"
read_only = true
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root.join("host-cache").canonicalize().unwrap();
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

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/mnt/decune-cache"))
                .expect("expected configured bind mount");

            assert_eq!(mount.source.as_deref(), expected_source.to_str());
            assert_eq!(mount.read_only, Some(true));
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
fn up_detach_rejects_workspace_mount_without_workspace_folder() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
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
                "workspaceFolder is required when workspaceMount is specified",
            ));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_uses_explicit_workspace_folder_with_workspace_mount() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir("app").unwrap();
    workspace
        .write_file("app/marker.txt", "workspace\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "workspaceFolder": "/workspace/app",
              "postStartCommand": "test \"$(pwd)\" = \"/workspace/app\" && test -f marker.txt"
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

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let workspace_mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/workspace"))
                .expect("expected workspace mount");

            assert_eq!(workspace_mount.source.as_deref(), workspace_root.to_str());
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
fn up_detach_rejects_workspace_folder_outside_workspace_mount_target() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
              "workspaceFolder": "/other"
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
                "workspaceFolder must be under the workspaceMount target",
            ));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn up_detach_resolves_remote_user_home_workspace_mount_target_before_validation() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.write_file("marker.txt", "workspace\n").unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/remote-user-home-workspace-mount-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "node",
                  "workspaceMount": "source=${{localWorkspaceFolder}},target=${{remoteUserHome}}/src,type=bind",
                  "workspaceFolder": "/usr/local/share/node/src",
                  "postStartCommand": "test \"$(pwd)\" = \"/usr/local/share/node/src\" && test -f marker.txt"
                }}
                "#
            ),
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_nonstandard_home_user(&workspace_root, &image_tag)
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
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();

            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.target.as_deref() == Some("/usr/local/share/node/src")),
                "expected workspace mount target to use the actual remote user home"
            );
            assert!(
                mounts
                    .iter()
                    .all(|mount| mount.target.as_deref() != Some("/root/src")),
                "workspaceMount target must not use preliminary root home"
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
fn up_detach_resolves_remote_user_home_workspace_folder_at_runtime() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.write_file("marker.txt", "workspace\n").unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/remote-user-home-workspace-folder-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "node",
                  "workspaceMount": "source=${{localWorkspaceFolder}},target=${{remoteUserHome}}/src,type=bind",
                  "workspaceFolder": "${{remoteUserHome}}/src",
                  "postStartCommand": "test \"$(pwd)\" = \"/usr/local/share/node/src\" && test -f marker.txt"
                }}
                "#
            ),
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_nonstandard_home_user(&workspace_root, &image_tag)
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
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();

            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.target.as_deref() == Some("/usr/local/share/node/src")),
                "expected workspace mount target to use the actual remote user home"
            );
            assert!(
                mounts
                    .iter()
                    .all(|mount| mount.target.as_deref() != Some("/root/src")),
                "workspaceFolder must not force the workspace mount to preliminary root home"
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
fn up_detach_resolves_remote_user_home_mount_target() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir("host-cache").unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_tag = format!(
        "decune-test/remote-user-home-mount-{}:latest",
        workspace_id(&workspace_root)
    );
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "image": "{image_tag}",
                  "remoteUser": "node",
                  "mounts": [
                    "source=${{localWorkspaceFolder}}/host-cache,target=${{remoteUserHome}}/.cache,type=bind"
                  ]
                }}
                "#
            ),
        )
        .unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        remove_image_if_exists(&image_tag).await.unwrap();
        create_image_with_nonstandard_home_user(&workspace_root, &image_tag)
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
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();

            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.target.as_deref() == Some("/usr/local/share/node/.cache")),
                "expected mount target to use the actual remote user home"
            );
            assert!(
                mounts
                    .iter()
                    .all(|mount| mount.target.as_deref() != Some("/home/node/.cache")),
                "remoteUserHome must not be guessed from the user name"
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
fn up_detach_resolves_remote_user_home_mount_target_for_dockerfile() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir("host-cache").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "remoteUser": "node",
              "mounts": [
                "source=${localWorkspaceFolder}/host-cache,target=${remoteUserHome}/.cache,type=bind"
              ]
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -h /usr/local/share/node node
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

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();

            assert!(
                mounts
                    .iter()
                    .any(|mount| mount.target.as_deref() == Some("/usr/local/share/node/.cache")),
                "expected mount target to use the actual remote user home"
            );
            assert!(
                mounts
                    .iter()
                    .all(|mount| mount.target.as_deref() != Some("/home/node/.cache")),
                "remoteUserHome must not be guessed from the user name"
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
fn up_detach_resolves_bind_mount_after_initialize_command() {
    let workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace.create_dir(".decune").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "initializeCommand": "mkdir -p host-cache"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
version = 1

[[mounts]]
source = "host-cache"
target = "/mnt/decune-cache"
type = "bind"
"#,
        )
        .unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let expected_source = workspace_root.join("host-cache");
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

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let host_config = inspect
                .host_config
                .expect("container host config should exist");
            let mounts = host_config.mounts.unwrap_or_default();
            let mount = mounts
                .iter()
                .find(|mount| mount.target.as_deref() == Some("/mnt/decune-cache"))
                .expect("expected initialized bind mount");

            assert_eq!(
                mount.source.as_deref(),
                Some(expected_source.canonicalize().unwrap().to_str().unwrap())
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

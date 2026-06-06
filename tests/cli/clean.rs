use crate::harness::*;

#[test]
fn down_and_clean_manage_image_container() {
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
            .success();

        decune()
            .arg("down")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Stopped dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_container_is_not_running(containers[0].id.as_deref().unwrap()).await;
        });

        let stopped_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started existing dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0].id.as_deref(), Some(stopped_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state.to_string() == "running")
            );
        });

        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert!(containers.is_empty());
        });

        decune()
            .arg("down")
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "No dev container found for this workspace",
            ));

        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn down_removes_github_token_file_and_keeps_secret_directory_without_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let token_dir = runtime_home
        .join("decune")
        .join(&workspace_id)
        .join("secrets");
    let token_file = token_dir.join("github-token");
    let marker_file = token_dir.join("marker");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    fs::create_dir_all(&token_dir).unwrap();
    fs::write(&token_file, "github-test-secret\n").unwrap();
    fs::write(&marker_file, "keep\n").unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("down")
            .arg(&workspace_root)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "No dev container found for this workspace",
            ))
            .stderr(predicate::str::contains("github-test-secret").not());

        assert!(token_dir.is_dir());
        assert!(!token_file.exists());
        assert_eq!(fs::read_to_string(&marker_file).unwrap(), "keep\n");
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn clean_force_removes_github_token_file_before_docker_access() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let token_dir = runtime_home
        .join("decune")
        .join(&workspace_id)
        .join("secrets");
    let token_file = token_dir.join("github-token");
    let marker_file = token_dir.join("marker");
    let missing_docker_socket = path_roots.path().join("missing-docker.sock");

    fs::create_dir_all(&token_dir).unwrap();
    fs::write(&token_file, "github-test-secret\n").unwrap();
    fs::write(&marker_file, "keep\n").unwrap();

    decune()
        .args(["clean", "--force"])
        .arg(&workspace_root)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .env(
            "DOCKER_HOST",
            format!("unix://{}", missing_docker_socket.display()),
        )
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("github-test-secret").not());

    assert!(token_dir.is_dir());
    assert!(!token_file.exists());
    assert_eq!(fs::read_to_string(&marker_file).unwrap(), "keep\n");
}

#[test]
fn clean_without_force_fails_non_interactive_before_docker_and_state_cleanup() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let token_dir = runtime_dir.join("secrets");
    let token_file = token_dir.join("github-token");
    let missing_docker_socket = path_roots.path().join("missing-docker.sock");

    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&token_dir).unwrap();
    fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
    fs::write(runtime_dir.join("socket"), "").unwrap();
    fs::write(&token_file, "github-test-secret\n").unwrap();

    decune()
        .arg("clean")
        .arg(&workspace_root)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .env(
            "DOCKER_HOST",
            format!("unix://{}", missing_docker_socket.display()),
        )
        .assert()
        .failure()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Cannot confirm clean in a non-interactive terminal",
        ))
        .stderr(predicate::str::contains("github-test-secret").not());

    assert!(state_dir.is_dir());
    assert!(runtime_dir.is_dir());
    assert!(token_file.is_file());
}

#[test]
fn clean_without_force_fails_non_interactive_without_removing_managed_volume() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let volume_name = format!("decune-clean-non-tty-{workspace_id}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_volumes(&workspace_root).await.unwrap();
        create_managed_volume(&workspace_root, &volume_name)
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("clean")
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot confirm clean in a non-interactive terminal",
            ));

        runtime.block_on(async {
            let volumes = workspace_volumes(&workspace_root).await.unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });
    });

    runtime.block_on(async {
        cleanup_workspace_volumes(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn clean_without_force_fails_non_interactive_without_removing_managed_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).await.unwrap();
        });

        decune()
            .arg("clean")
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot confirm clean in a non-interactive terminal",
            ));

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
fn clean_force_stops_running_container_before_removal() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let marker = workspace_root.join("term-marker");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).await.unwrap();
        });

        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        assert_eq!(fs::read_to_string(&marker).unwrap(), "term\n");

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).await.unwrap();
            assert!(containers.is_empty());
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
fn clean_force_removes_state_and_runtime_directories() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
    fs::write(runtime_dir.join("socket"), "").unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        assert!(!state_dir.exists());
        assert!(!runtime_dir.exists());
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn clean_images_removes_workspace_images_only_when_requested() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_repository = workspace_image_repository(&workspace_root);
    let image_tag = format!("{image_repository}:clean-test");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
        cleanup_workspace_images(&workspace_root).await.unwrap();
        create_workspace_image_tag(&workspace_root, "clean-test")
            .await
            .unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["clean", "--force"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).await.unwrap();
            assert_eq!(images, vec![image_tag.clone()]);
        });

        decune()
            .args(["clean", "--force", "--images"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Cleaned dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).await.unwrap();
            assert!(images.is_empty());
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

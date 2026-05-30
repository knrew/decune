use crate::harness::*;

#[test]
fn up_detach_mounts_ssh_agent_socket_and_sets_container_env() {
    let workspace = support::TempWorkspace::new().unwrap();
    let socket_workspace = support::TempWorkspace::new().unwrap();
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "image": "alpine:3.20",
              "postStartCommand": "test \"${SSH_AUTH_SOCK:-}\" = /run/decune/ssh-agent.sock && test -S /run/decune/ssh-agent.sock"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r#"
            version = 1

            [credentials.git]
            https = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let socket_path = socket_workspace.path().join("agent.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
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
            .env("SSH_AUTH_SOCK", &socket_path)
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
            let env = inspect.config.unwrap_or_default().env.unwrap_or_default();
            assert!(
                env.iter()
                    .any(|entry| entry == "SSH_AUTH_SOCK=/run/decune/ssh-agent.sock")
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
fn up_detach_recreates_container_when_ssh_agent_socket_becomes_unavailable() {
    let workspace = support::TempWorkspace::new().unwrap();
    let socket_workspace = support::TempWorkspace::new().unwrap();
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

            [credentials.git]
            https = "off"

            [credentials.github]
            enabled = false
            "#,
        )
        .unwrap();
    let socket_path = socket_workspace.path().join("agent.sock");
    let _listener = UnixListener::bind(&socket_path).unwrap();
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
            .env("SSH_AUTH_SOCK", &socket_path)
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started dev container"));

        let first_id = runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            assert!(inspect_has_env(
                &inspect,
                "SSH_AUTH_SOCK=/run/decune/ssh-agent.sock"
            ));
            assert!(inspect_has_mount_target(
                &inspect,
                "/run/decune/ssh-agent.sock"
            ));
            inspect.id.unwrap()
        });

        decune()
            .env_remove("SSH_AUTH_SOCK")
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
            assert_ne!(inspect.id.as_deref(), Some(first_id.as_str()));
            assert!(!inspect_has_env(
                &inspect,
                "SSH_AUTH_SOCK=/run/decune/ssh-agent.sock"
            ));
            assert!(!inspect_has_mount_target(
                &inspect,
                "/run/decune/ssh-agent.sock"
            ));
        });
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).await.unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

use crate::harness::*;

// 状態の無い Compose 環境で使用中の volume を残しても、所有情報を保存し、再削除できる。
// clean が途中でその情報を消すと、コンテナの消えた volume を辿れなくなる
//
// シナリオ:
//   1. 状態無しで、プロジェクト外から参照される volume とコンテナを用意する
//   2. remove と clean を実行する → コンテナだけ消え、volume と状態は残る
//   3. 外からの参照を解いて remove --all-workspaces を実行する → volume と状態が消える
#[test]
fn remove_all_recovers_compose_volume_ownership_without_state() {
    check_compose_volume_recovery(&["project-a"], false);
}

// 同じワークスペースの複数プロジェクトに使用中の volume があっても、すべて再削除できる
#[test]
fn remove_all_recovers_multiple_compose_projects_without_state() {
    check_compose_volume_recovery(&["project-a", "project-b"], false);
}

// 起動時の状態を保ったまま、ラベルから見つかった別プロジェクトの volume も再削除できる
#[test]
fn remove_all_preserves_existing_state_and_newly_discovered_compose_projects() {
    check_compose_volume_recovery(&["project-a", "project-b"], true);
}

fn check_compose_volume_recovery(projects: &[&str], existing_state: bool) {
    let temp = support::TempWorkspace::new().unwrap();
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let state_dir = state_home.join("decune/123456abcdef");
    let runtime_dir = runtime_home.join("decune/123456abcdef");
    let original_state = existing_state.then(|| {
        temp.write_fixture_template(
            "state/decune/123456abcdef/state.toml",
            "cli/harness/compose-state.toml",
            &[("__PROJECT__", "project-old")],
        )
        .unwrap();
        toml::from_str::<toml::Table>(&fs::read_to_string(state_dir.join("state.toml")).unwrap())
            .unwrap()
    });
    fs::create_dir_all(&runtime_dir).unwrap();
    let fake_data = temp.create_dir("docker-data").unwrap();
    for project in projects {
        fs::write(fake_data.join(format!("{project}-container")), "").unwrap();
        fs::write(fake_data.join(format!("{project}_db")), "").unwrap();
    }
    fs::write(fake_data.join("outside"), "").unwrap();
    let fake_path = fake_docker_path(&temp, "cli/remove/retained-compose-volumes.sh");
    let command = || {
        let mut command = decune();
        command
            .env("PATH", &fake_path)
            .env("DECUNE_FAKE_PROJECTS", projects.join(" "))
            .env("DECUNE_FAKE_DATA", &fake_data)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_CACHE_HOME", temp.path().join("cache"))
            .env("XDG_RUNTIME_DIR", &runtime_home);
        command
    };

    command()
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success();

    for project in projects {
        assert!(!fake_data.join(format!("{project}-container")).exists());
        assert!(fake_data.join(format!("{project}_db")).exists());
    }
    assert!(!runtime_dir.exists());
    assert!(state_dir.join("state.toml").exists());
    if let Some(original) = original_state {
        let retained: toml::Table =
            toml::from_str(&fs::read_to_string(state_dir.join("state.toml")).unwrap()).unwrap();
        for (key, value) in original {
            assert_eq!(retained.get(&key), Some(&value), "{key}");
        }
    }
    command()
        .arg("status")
        .assert()
        .success()
        .stdout(predicate::str::contains("/work/app"));
    let output = command()
        .args(["clean", "--no-confirm", "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let report: serde_json::Value = serde_json::from_slice(&output).unwrap();
    assert_eq!(report["targets"][0]["reason"], "managed_resource");

    fs::remove_file(fake_data.join("outside")).unwrap();
    command()
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success();

    for project in projects {
        assert!(!fake_data.join(format!("{project}_db")).exists());
    }
    assert!(!state_dir.exists());
}

#[test]
fn down_and_remove_manage_image_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
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
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
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
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert_container_is_not_running(containers[0].id.as_deref().unwrap());
        });

        let stopped_id = runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            containers[0].id.clone().unwrap()
        });

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Started existing dev container"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert_eq!(containers.len(), 1);
            assert_eq!(containers[0].id.as_deref(), Some(stopped_id.as_str()));
            assert!(
                containers[0]
                    .state
                    .as_ref()
                    .is_some_and(|state| state == "running")
            );
        });

        decune()
            .args(["rm", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
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
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
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
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_no_confirm_removes_github_token_file_before_docker_access() {
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
        .args(["remove", "--no-confirm"])
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
fn remove_without_no_confirm_fails_non_interactive_before_docker_and_state_cleanup() {
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
        .arg("remove")
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
            "Cannot confirm remove in a non-interactive terminal",
        ))
        .stderr(predicate::str::contains("github-test-secret").not());

    assert!(state_dir.is_dir());
    assert!(runtime_dir.is_dir());
    assert!(token_file.is_file());
}

#[test]
fn remove_without_no_confirm_fails_non_interactive_without_removing_managed_volume() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let volume_name = format!("decune-remove-non-tty-{workspace_id}");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_volumes(&workspace_root).unwrap();
        create_managed_volume(&workspace_root, &volume_name).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .arg("remove")
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot confirm remove in a non-interactive terminal",
            ));

        runtime.block_on(async {
            let volumes = workspace_volumes(&workspace_root).unwrap();
            assert_eq!(volumes, vec![volume_name.clone()]);
        });
    });

    runtime.block_on(async {
        cleanup_workspace_volumes(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_without_no_confirm_fails_non_interactive_without_removing_managed_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).unwrap();
        });

        decune()
            .arg("remove")
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Cannot confirm remove in a non-interactive terminal",
            ));

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
fn remove_no_confirm_stops_running_container_before_removal() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let marker = workspace_root.join("term-marker");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        runtime.block_on(async {
            create_term_marker_container(&workspace_root).unwrap();
        });

        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        assert_eq!(fs::read_to_string(&marker).unwrap(), "term\n");

        runtime.block_on(async {
            let containers = workspace_containers(&workspace_root).unwrap();
            assert!(containers.is_empty());
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
fn remove_no_confirm_removes_state_and_runtime_directories() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let runtime_home = path_roots.path().join("runtime");
    let workspace_id = workspace_id(&workspace_root);
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    let port_status_dir = runtime_home
        .join("decune")
        .join(format!("{workspace_id}-ports"));
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&port_status_dir).unwrap();
    fs::write(state_dir.join("state.toml"), "version = 1\n").unwrap();
    fs::write(runtime_dir.join("socket"), "").unwrap();
    fs::write(port_status_dir.join("forward-status-stale.json"), "{}").unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .env("XDG_STATE_HOME", &state_home)
            .env("XDG_RUNTIME_DIR", &runtime_home)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        assert!(!state_dir.exists());
        assert!(!runtime_dir.exists());
        assert!(!port_status_dir.exists());
    });

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
    });

    if let Err(payload) = result {
        std::panic::resume_unwind(payload);
    }
}

#[test]
fn remove_images_removes_workspace_images_only_when_requested() {
    let workspace = support::TempWorkspace::new().unwrap();
    let workspace_root = workspace.path().canonicalize().unwrap();
    let image_repository = workspace_image_repository(&workspace_root);
    let image_tag = format!("{image_repository}:remove-test");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    runtime.block_on(async {
        cleanup_workspace_containers(&workspace_root).unwrap();
        cleanup_workspace_images(&workspace_root).unwrap();
        create_workspace_image_tag(&workspace_root, "remove-test").unwrap();
    });

    let result = std::panic::catch_unwind(|| {
        decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).unwrap();
            assert_eq!(images, vec![image_tag.clone()]);
        });

        decune()
            .args(["remove", "--no-confirm", "--images"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains("Removed dev container resources"));

        runtime.block_on(async {
            let images = workspace_images(&workspace_root).unwrap();
            assert_eq!(images, Vec::<String>::new());
        });
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

// 削除したコンテナが mount していた、decune のラベルの無い named volume は消さず、
// 残した理由とともに出力の最後に示す。匿名 volume(`mounts` の `source` の無い volume と、
// イメージの `VOLUME` 命令の volume)は、残したものとして示さない
#[test]
fn remove_reports_kept_unlabeled_volume_and_omits_anonymous_volumes() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let user_volume = format!("decune-remove-kept-{}", workspace_id(&workspace_root));
    workspace.create_dir(".devcontainer").unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            "FROM alpine:3.20\nVOLUME /image-volume\n",
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            format!(
                r#"
                {{
                  "build": {{ "dockerfile": "Dockerfile" }},
                  "mounts": [
                    "source={user_volume},target=/data,type=volume",
                    "target=/anonymous,type=volume"
                  ]
                }}
                "#
            ),
        )
        .unwrap();
    docker_status(["volume", "create", &user_volume]).unwrap();

    with_clean_workspace_containers_and_images(&workspace_root, || {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
            .assert()
            .success();
        let container = inspect_single_workspace_container(&workspace_root).unwrap();
        let anonymous_volumes = container
            .mounts
            .unwrap_or_default()
            .into_iter()
            .filter(|mount| mount.typ.as_deref() == Some("volume"))
            .filter_map(|mount| mount.name)
            .filter(|name| name != &user_volume)
            .collect::<Vec<_>>();
        assert_eq!(anonymous_volumes.len(), 2, "{anonymous_volumes:?}");

        let output = decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .stdout(predicate::str::is_empty())
            .get_output()
            .clone();

        let stderr = String::from_utf8(output.stderr).unwrap();
        let last_line = stderr
            .lines()
            .rfind(|line| !line.trim().is_empty())
            .unwrap();
        assert!(last_line.contains(&user_volume), "{stderr}");
        assert!(
            last_line.contains("not a decune-managed volume of this workspace"),
            "{stderr}"
        );
        assert_eq!(stderr.matches("Kept Docker volume").count(), 1, "{stderr}");
        for anonymous_volume in &anonymous_volumes {
            assert!(!stderr.contains(anonymous_volume.as_str()), "{stderr}");
        }
        docker_status(["volume", "inspect", &user_volume]).unwrap();
    });

    docker_status(["volume", "rm", &user_volume]).unwrap();
}

fn kept_volume_lines(stderr: &str) -> Vec<&str> {
    stderr
        .lines()
        .filter(|line| line.contains("Kept Docker volume"))
        .collect()
}

// `decune up` が作らせた named volume は、そのワークスペースの decune-managed ボリュームで、
// `remove` で削除する
#[test]
fn remove_removes_named_volume_created_by_up() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let volume = format!("decune-remove-created-{}", workspace_id(&workspace_root));
    write_named_volume_devcontainer(&workspace, &volume);

    with_clean_workspace_containers_images_and_volumes(&workspace_root, || {
        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
            .assert()
            .success();
        assert!(volume_exists(&volume));

        let output = decune()
            .args(["remove", "--no-confirm"])
            .arg(&workspace_root)
            .assert()
            .success()
            .get_output()
            .clone();

        let stderr = String::from_utf8(output.stderr).unwrap();
        assert!(!volume_exists(&volume), "{stderr}");
        assert!(kept_volume_lines(&stderr).is_empty(), "{stderr}");
    });
}

// ワークスペース X の `up` が作らせた volume を、ワークスペース Y も mount しているとき、
// Y の `remove` はその volume を消さず、Y の decune-managed ボリュームでないものとして示す。
// Y の `up` のときには volume が既にあるので、Docker は Y のラベルを付けない
#[test]
fn remove_keeps_volume_created_by_another_workspace() {
    let owner = support::TempWorkspace::new().unwrap();
    let sharer = support::TempWorkspace::new().unwrap();
    let owner_tools_dir = fake_container_tools_bundle(&owner);
    let sharer_tools_dir = fake_container_tools_bundle(&sharer);
    let owner_root = owner.path().canonicalize().unwrap();
    let sharer_root = sharer.path().canonicalize().unwrap();
    let volume = format!("decune-remove-shared-{}", workspace_id(&owner_root));
    let state_home = tempfile::tempdir().unwrap();
    write_named_volume_devcontainer(&owner, &volume);
    write_named_volume_devcontainer(&sharer, &volume);

    with_clean_workspace_containers_images_and_volumes(&owner_root, || {
        with_clean_workspace_containers_images_and_volumes(&sharer_root, || {
            for (root, tools_dir) in [
                (&owner_root, &owner_tools_dir),
                (&sharer_root, &sharer_tools_dir),
            ] {
                decune()
                    .args(["up", "--detach"])
                    .arg(root)
                    .env("XDG_STATE_HOME", state_home.path())
                    .env("DECUNE_CONTAINER_TOOLS_DIR", tools_dir)
                    .assert()
                    .success();
            }

            let output = decune()
                .args(["remove", "--no-confirm"])
                .arg(&sharer_root)
                .env("XDG_STATE_HOME", state_home.path())
                .assert()
                .success()
                .get_output()
                .clone();

            let stderr = String::from_utf8(output.stderr).unwrap();
            let kept = kept_volume_lines(&stderr);
            assert_eq!(kept.len(), 1, "{stderr}");
            assert!(kept[0].contains(&volume), "{stderr}");
            assert!(
                kept[0].contains("not a decune-managed volume of this workspace"),
                "{stderr}"
            );
            assert!(volume_exists(&volume));
            assert!(workspace_containers(&sharer_root).unwrap().is_empty());
        });
    });
}

// ワークスペースの decune-managed ボリュームを、
// ワークスペースの外のコンテナ(停止中でもよい)が参照していると、
// `remove` はその volume を残して使用中として警告し、ほかの削除を終えて成功する
#[test]
fn remove_keeps_managed_volume_in_use_by_other_container() {
    let workspace = support::TempWorkspace::new().unwrap();
    let container_tools_dir = fake_container_tools_bundle(&workspace);
    let workspace_root = workspace.path().canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let volume = format!("decune-remove-in-use-{workspace_id}");
    let other_container = format!("decune-remove-in-use-other-{workspace_id}");
    let path_roots = tempfile::tempdir().unwrap();
    let state_home = path_roots.path().join("state");
    let state_dir = state_home.join("decune").join(&workspace_id);
    let runtime_home = path_roots.path().join("runtime");
    let runtime_dir = runtime_home.join("decune").join(&workspace_id);
    write_named_volume_devcontainer(&workspace, &volume);

    with_clean_workspace_containers_images_and_volumes(&workspace_root, || {
        let result = std::panic::catch_unwind(|| {
            decune()
                .args(["up", "--detach"])
                .arg(&workspace_root)
                .env("XDG_STATE_HOME", &state_home)
                .env("XDG_RUNTIME_DIR", &runtime_home)
                .env("DECUNE_CONTAINER_TOOLS_DIR", &container_tools_dir)
                .assert()
                .success();
            assert!(state_dir.exists());
            assert!(runtime_dir.exists());
            docker_status([
                "create",
                "--name",
                &other_container,
                "--mount",
                &format!("type=volume,source={volume},target=/data"),
                "alpine:3.20",
            ])
            .unwrap();

            let output = decune()
                .args(["remove", "--no-confirm"])
                .arg(&workspace_root)
                .env("XDG_STATE_HOME", &state_home)
                .env("XDG_RUNTIME_DIR", &runtime_home)
                .assert()
                .success()
                .get_output()
                .clone();

            let stderr = String::from_utf8(output.stderr).unwrap();
            let kept = kept_volume_lines(&stderr);
            assert_eq!(kept.len(), 1, "{stderr}");
            assert!(kept[0].starts_with("Warning:"), "{stderr}");
            assert!(kept[0].contains(&volume), "{stderr}");
            assert!(kept[0].contains("in use by another container"), "{stderr}");
            assert!(volume_exists(&volume));
            assert!(workspace_containers(&workspace_root).unwrap().is_empty());
            assert!(!state_dir.exists());
            assert!(!runtime_dir.exists());
        });
        _ = docker_status(["rm", "--force", &other_container]);
        if let Err(payload) = result {
            std::panic::resume_unwind(payload);
        }
    });
}

#[test]
fn remove_all_workspaces_no_targets_succeeds_without_confirmation() {
    let temp = support::TempWorkspace::new().unwrap();
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&state_home).unwrap();
    fs::create_dir_all(&runtime_home).unwrap();
    let fake_path = fake_docker_path(&temp, "cli/remove/all-workspaces-no-targets.sh");

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No decune-managed workspace environments found",
        ));
}

#[test]
fn remove_all_workspaces_ignores_invalid_workspace_id_labels() {
    let temp = support::TempWorkspace::new().unwrap();
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let victim_state_dir = state_home.join("victim");
    let victim_runtime_dir = runtime_home.join("victim");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&victim_state_dir).unwrap();
    fs::create_dir_all(&victim_runtime_dir).unwrap();
    fs::write(victim_state_dir.join("marker"), "keep\n").unwrap();
    fs::write(victim_runtime_dir.join("marker"), "keep\n").unwrap();
    let fake_path = fake_docker_path(&temp, "cli/remove/invalid-workspace-id-labels.sh");

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No decune-managed workspace environments found",
        ));

    assert_eq!(
        fs::read_to_string(victim_state_dir.join("marker")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(victim_runtime_dir.join("marker")).unwrap(),
        "keep\n"
    );
    let commands = fs::read_to_string(command_log).unwrap();
    assert!(!commands.contains("rm --force --volumes invalid-container"));
    assert!(!commands.contains("volume rm --force invalid-volume"));
}

#[test]
fn remove_all_workspaces_ignores_invalid_state_directory_workspace_ids() {
    let temp = support::TempWorkspace::new().unwrap();
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let invalid_workspace_id = "not-a-workspace";
    let invalid_state_dir = state_home.join("decune").join(invalid_workspace_id);
    let invalid_runtime_dir = runtime_home.join("decune").join(invalid_workspace_id);
    let state_workspace = temp.path().join("state-workspace");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&invalid_state_dir).unwrap();
    fs::create_dir_all(&invalid_runtime_dir).unwrap();
    fs::create_dir_all(&state_workspace).unwrap();
    fs::write(invalid_state_dir.join("marker"), "keep\n").unwrap();
    fs::write(invalid_runtime_dir.join("marker"), "keep\n").unwrap();
    fs::write(
        invalid_state_dir.join("state.toml"),
        format!(
            r#"version = 1
workspace = "{}"
container_id = "state-container"
image = "decune/state-workspace-not-a-workspace:statehash"
config_hash = "statehash"
compose_project_name = "user-owned"
created_at = "unix:1"
last_started_at = "unix:1"
"#,
            state_workspace.display()
        ),
    )
    .unwrap();
    let fake_path = fake_docker_path(&temp, "cli/remove/invalid-state-directory-workspace-ids.sh");

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "No decune-managed workspace environments found",
        ));

    assert_eq!(
        fs::read_to_string(invalid_state_dir.join("marker")).unwrap(),
        "keep\n"
    );
    assert_eq!(
        fs::read_to_string(invalid_runtime_dir.join("marker")).unwrap(),
        "keep\n"
    );
    let commands = fs::read_to_string(command_log).unwrap();
    assert!(!commands.contains("user-owned"), "{commands}");
}

// 状態もコンテナも残っておらず、decune-managed ボリュームだけが残るワークスペースも、
// `--all-workspaces` の対象として見つけ、その volume を削除する
#[test]
fn remove_all_workspaces_removes_workspace_with_only_managed_volume() {
    let temp = support::TempWorkspace::new().unwrap();
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let command_log = temp.path().join("docker.log");
    fs::create_dir_all(&state_home).unwrap();
    fs::create_dir_all(&runtime_home).unwrap();
    let fake_path = fake_docker_path(&temp, "cli/remove/volume-only-workspace.sh");

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["remove", "--all-workspaces", "--no-confirm"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Removed dev container resources for workspace id: aaaaaaaaaaaa",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(
        commands.contains("volume rm --force orphan-volume"),
        "{commands}"
    );
}

#[test]
fn remove_all_workspaces_no_confirm_removes_owned_resources_and_images() {
    let temp = support::TempWorkspace::new().unwrap();
    let state_home = temp.path().join("state");
    let runtime_home = temp.path().join("runtime");
    let command_log = temp.path().join("docker.log");
    let state_workspace = temp.path().join("state-workspace");
    let state_workspace_id = "123456abcdef";
    let state_dir = state_home.join("decune").join(state_workspace_id);
    let runtime_dir = runtime_home.join("decune").join(state_workspace_id);
    fs::create_dir_all(&state_dir).unwrap();
    fs::create_dir_all(&runtime_dir).unwrap();
    fs::create_dir_all(&state_workspace).unwrap();
    fs::write(
        state_dir.join("state.toml"),
        format!(
            r#"version = 1
workspace = "{}"
container_id = "state-container"
image = "decune/state-workspace-123456abcdef:statehash"
config_hash = "statehash"
compose_project_name = "state-owned"
created_at = "unix:1"
last_started_at = "unix:1"
"#,
            state_workspace.display()
        ),
    )
    .unwrap();
    let fake_path = fake_docker_path(&temp, "cli/remove/owned-resources-and-images.sh");

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .env("XDG_STATE_HOME", &state_home)
        .env("XDG_RUNTIME_DIR", &runtime_home)
        .args(["rm", "--all-workspaces", "--no-confirm", "--images"])
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains(
            "Removed all decune-managed workspace environments",
        ));

    let commands = fs::read_to_string(command_log).unwrap();
    assert!(
        commands.contains("rm --force --volumes standalone-id"),
        "{commands}"
    );
    assert!(
        commands.contains("rm --force --volumes compose-primary-id"),
        "{commands}"
    );
    assert!(
        commands.contains("rm --force --volumes compose-sidecar-id"),
        "{commands}"
    );
    assert!(
        commands.contains("volume rm --force standalone-volume"),
        "{commands}"
    );
    assert!(
        commands.contains("volume rm --force compose-volume"),
        "{commands}"
    );
    assert!(
        commands.contains("network rm compose-network"),
        "{commands}"
    );
    assert!(
        commands.contains("image rm --no-prune --force decune/standalone-one-aaaaaaaaaaaa:hash1"),
        "{commands}"
    );
    assert!(
        commands.contains("image rm --no-prune --force decune/compose-one-bbbbbbbbbbbb:hash2"),
        "{commands}"
    );
    assert!(
        commands
            .contains("image rm --no-prune --force decune/state-workspace-123456abcdef:statehash"),
        "{commands}"
    );
    assert!(!commands.contains("user-owned"), "{commands}");
    assert!(!commands.contains("--rmi"), "{commands}");
    assert!(!state_dir.exists());
    assert!(!runtime_dir.exists());
}

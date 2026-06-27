use crate::harness::*;

#[test]
fn up_detach_builds_with_safe_docker_resource_names_for_problem_workspace_basename() {
    let parent = tempfile::tempdir().unwrap();
    let workspace_root = parent.path().join("APP__Name...v2");
    fs::create_dir_all(workspace_root.join(".devcontainer")).unwrap();
    fs::write(
        workspace_root.join(".devcontainer/devcontainer.json"),
        r#"
        {
          "build": {
            "dockerfile": "Dockerfile"
          },
          "postStartCommand": "test \"$PWD\" = '/workspaces/APP__Name...v2' && test -f resource-name-marker.txt"
        }
        "#,
    )
    .unwrap();
    fs::write(
        workspace_root.join(".devcontainer/Dockerfile"),
        r"
        FROM alpine:3.20
        RUN true
        ",
    )
    .unwrap();
    fs::write(workspace_root.join("resource-name-marker.txt"), "ok\n").unwrap();
    let workspace_root = workspace_root.canonicalize().unwrap();
    let workspace_id = workspace_id(&workspace_root);
    let safe_slug = safe_workspace_slug(
        workspace_root
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap(),
    );
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
            .stderr(predicate::str::contains(format!(
                "Started dev container: decune-{safe_slug}-{workspace_id}"
            )));

        runtime.block_on(async {
            let inspect = inspect_single_workspace_container(&workspace_root)
                .await
                .unwrap();
            let expected_name = format!("/decune-{safe_slug}-{workspace_id}");
            assert_eq!(inspect.name.as_deref(), Some(expected_name.as_str()));
            assert!(inspect_has_mount_target(
                &inspect,
                "/workspaces/APP__Name...v2"
            ));

            let images = workspace_images(&workspace_root).await.unwrap();
            assert_eq!(images.len(), 1);
            assert!(
                images[0].starts_with(&format!("decune/{safe_slug}-{workspace_id}:")),
                "{}",
                images[0]
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
fn dockerfile_up_pull_does_not_pass_pull_to_generated_feature_layer() {
    let workspace = support::TempWorkspace::new().unwrap();
    let host_tools = support::TempWorkspace::new().unwrap();
    workspace
        .create_dir(".devcontainer/features/local-tool")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/devcontainer.json",
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile"
              },
              "updateRemoteUserUID": false,
              "features": {
                "./features/local-tool": {}
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
            RUN true
            ",
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/features/local-tool/devcontainer-feature.json",
            r#"
            {
              "id": "local-tool",
              "version": "1.0.0",
              "name": "Local Tool"
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/features/local-tool/install.sh", "set -eu\n")
        .unwrap();
    let command_log = host_tools.path().join("commands.log");
    let docker_path = host_tools
        .write_file(
            "bin/docker",
            r#"#!/bin/sh
set -eu
printf '%s\n' "$*" >> "$DECUNE_FAKE_COMMAND_LOG"
if [ "${1:-}" = build ]; then
  case "$*" in
    *"-base --file"*)
      case "$*" in
        *"--no-cache"*"--pull"*) cat >/dev/null; exit 0 ;;
      esac
      echo "User Dockerfile build did not receive --no-cache and --pull: $*" >&2
      exit 42
      ;;
    *)
      case "$*" in
        *"--pull"*)
          echo "Generated Docker build must not receive --pull: $*" >&2
          exit 43
          ;;
        *"--no-cache"*)
          cat >/dev/null
          exit 0
          ;;
      esac
      echo "Generated Docker build did not receive --no-cache: $*" >&2
      exit 44
      ;;
  esac
fi
if [ "${1:-}" = image ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"sha256:test","Os":"linux","Architecture":"amd64","Config":{"Labels":{},"Entrypoint":null,"Cmd":["/bin/sh"],"User":""}}]\n'
  exit 0
fi
if [ "${1:-}" = image ] && [ "${2:-}" = rm ]; then
  exit 0
fi
if [ "${1:-}" = ps ]; then
  exit 0
fi
if [ "${1:-}" = create ]; then
  printf 'fake-container-id\n'
  exit 0
fi
if [ "${1:-}" = start ]; then
  exit 0
fi
if [ "${1:-}" = exec ]; then
  printf 'root:x:0:0:root:/root:/bin/sh\n'
  exit 0
fi
if [ "${1:-}" = inspect ]; then
  printf '[{"Id":"fake-container-id","Name":"/decune-fake","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = container ] && [ "${2:-}" = inspect ]; then
  printf '[{"Id":"fake-container-id","Name":"/decune-fake","Config":{"Env":[],"Labels":{}},"State":{"Running":true}}]\n'
  exit 0
fi
if [ "${1:-}" = rm ]; then
  exit 0
fi
echo "unexpected fake docker command: $*" >&2
exit 91
"#,
        )
        .unwrap();
    fs::set_permissions(&docker_path, fs::Permissions::from_mode(0o755)).unwrap();
    let fake_path = format!(
        "{}:{}",
        docker_path.parent().unwrap().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let workspace_root = workspace.path().canonicalize().unwrap();

    decune()
        .env("PATH", &fake_path)
        .env("DECUNE_FAKE_COMMAND_LOG", &command_log)
        .args(["up", "--detach", "--no-cache", "--pull"])
        .arg(&workspace_root)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("Started dev container"));

    let commands = fs::read_to_string(command_log).unwrap();
    let build_commands = commands
        .lines()
        .filter(|line| line.starts_with("build "))
        .collect::<Vec<_>>();
    assert!(build_commands.len() >= 2, "{commands}");
    assert!(
        build_commands
            .iter()
            .filter(|line| line.contains("-base --file"))
            .all(|line| line.contains("--no-cache") && line.contains("--pull")),
        "{commands}"
    );
    assert!(build_commands.iter().any(|line| {
        !line.contains("-base --file") && line.contains("--no-cache") && !line.contains("--pull")
    }));
    assert!(
        build_commands
            .iter()
            .filter(|line| !line.contains("-base --file"))
            .all(|line| !line.contains("--pull")),
        "{commands}"
    );
}

#[test]
fn up_detach_rejects_changed_dockerfile_build_context_before_reuse() {
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
            r#"
            FROM alpine:3.20
            COPY build-state /tmp/build-state
            RUN test "$(cat /tmp/build-state)" = ok
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [credentials.git]
            enabled = false

            [credentials.github]
            enabled = false
            ",
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/build-state", "ok\n")
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

        workspace
            .write_file(".devcontainer/build-state", "changed\n")
            .unwrap();

        decune()
            .args(["up", "--detach"])
            .arg(&workspace_root)
            .assert()
            .failure()
            .stdout(predicate::str::is_empty())
            .stderr(predicate::str::contains(
                "Dev container configuration changed. Run decune rebuild to recreate it.",
            ));
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
fn up_detach_builds_with_dockerfile_specific_ignore_over_default_ignore() {
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
            COPY . /context
            RUN test -f /context/specific-kept.txt && test ! -e /context/specific-secret.env
            ",
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/.dockerignore", "specific-kept.txt\n")
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile.dockerignore",
            "specific-secret.env\n",
        )
        .unwrap();
    workspace
        .write_file(".devcontainer/specific-kept.txt", "included\n")
        .unwrap();
    workspace
        .write_file(".devcontainer/specific-secret.env", "excluded\n")
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
fn up_dockerfile_metadata_label_is_merged() {
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
              "postStartCommand": "test \"${FROM_DOCKERFILE_LABEL:-}\" = \"set\" && test \"$(id -un)\" = \"devuser\""
            }
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".devcontainer/Dockerfile",
            r#"
            FROM alpine:3.20
            RUN adduser -D -u 1000 -h /home/devuser devuser
            LABEL devcontainer.metadata="{\"remoteUser\":\"devuser\",\"updateRemoteUserUID\":false,\"remoteEnv\":{\"FROM_DOCKERFILE_LABEL\":\"set\"}}"
            "#,
        )
        .unwrap();
    workspace
        .write_file(
            ".decune/config.toml",
            r"
            version = 1

            [credentials.github]
            enabled = false
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
            .stderr(
                predicate::str::contains(
                    "Dockerfile image label devcontainer.metadata is not merged",
                )
                .not(),
            )
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

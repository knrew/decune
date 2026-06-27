use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) struct ExistingContainerReusePolicy {
    pub(super) pull: bool,
    pub(super) service_forward_requires_recreate: bool,
}

pub(super) fn should_reuse_existing_container(
    decision: &ExistingContainerDecision,
    policy: ExistingContainerReusePolicy,
) -> bool {
    matches!(
        decision,
        ExistingContainerDecision::ReuseRunning { .. }
            | ExistingContainerDecision::StartStopped { .. }
    ) && !policy.pull
        && !policy.service_forward_requires_recreate
}

pub(super) async fn start_stopped_existing_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    container_id: String,
    container_name: String,
) -> Result<(UpOutcome, WorkspaceState)> {
    let container = state_container_snapshot(plan, container_id.clone());
    let existing_state = reusable_lifecycle_state(workspace, &container)?;

    materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
    start_container_and_verify_running(
        client,
        &container_name,
        startup_verification_for_plan(plan),
    )
    .await?;

    let state = write_reused_started_state(
        workspace,
        container,
        state_compose_project_name(plan),
        existing_state,
        true,
    )?;
    Ok((
        UpOutcome {
            container_id,
            container_name,
            reused: true,
        },
        state,
    ))
}

pub(super) async fn compose_service_forward_requires_recreate(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
    service_forward: &[ServiceForwardRuntime],
) -> Result<bool> {
    for runtime in service_forward {
        let containers = list_compose_forwarding_service_containers(
            client,
            workspace_id,
            project_name,
            runtime.service(),
        )
        .await?;
        let Some(container) = containers.first() else {
            continue;
        };
        if compose_service_forward_container_requires_recreate(container, runtime.mount()) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn compose_service_forward_container_requires_recreate(
    container: &UpContainerSummary,
    required: &DockerMountSpec,
) -> bool {
    !container_has_mount(container, required)
}

fn container_has_mount(container: &UpContainerSummary, required: &DockerMountSpec) -> bool {
    let Some(existing_mounts) = &container.mounts else {
        return false;
    };
    existing_mounts.iter().any(|existing| {
        existing.source == required.source
            && existing.target == required.target
            && existing.mount_type == required.mount_type
            && existing.read_only == required.read_only
    })
}

pub(super) async fn recreate_existing_containers(
    client: &DockerClient,
    containers: &[UpContainerSummary],
) -> Result<()> {
    for container in containers {
        stop_container(client, &container.id, REBUILD_STOP_TIMEOUT_SECONDS).await?;
        remove_container(client, &container.id, true, false).await?;
        ui::done(&format!(
            "Removed existing dev container for rebuild: {}",
            container.name
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use super::super::test_support::{mount_policy, reusable_container};
    use super::*;
    use crate::{
        config::ConfigLayer,
        config::types::MountType,
        docker::{
            client::DockerClient,
            container::remove_container,
            exec::{ExecCommandSpec, exec_capture},
            image::{PullPolicy, ensure_image, remove_image},
        },
        up::{
            ExistingContainerDecision, UpMountSummary, UpOptions,
            existing::decide_existing_container,
            plan::build_up_plan,
            run_detached_up,
            start::create_and_start_container,
            test_support::{
                build_user_image, container_has_mount_target, test_workspace, write_devcontainer,
            },
        },
    };

    #[test]
    fn compose_service_forward_requires_recreate_when_runtime_mount_is_missing() {
        let required = DockerMountSpec {
            source: Some("/tmp/decune-runtime/forward/db".to_owned()),
            target: "/run/decune".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        };
        let missing = UpContainerSummary {
            id: "db-id".to_owned(),
            name: "project-db-1".to_owned(),
            image_id: None,
            config_hash: None,
            config_file: None,
            mounts: None,
            running: true,
        };
        let present = UpContainerSummary {
            mounts: Some(vec![UpMountSummary {
                source: Some("/tmp/decune-runtime/forward/db".to_owned()),
                target: "/run/decune".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
            }]),
            ..missing.clone()
        };

        assert!(compose_service_forward_container_requires_recreate(
            &missing, &required
        ));
        assert!(!compose_service_forward_container_requires_recreate(
            &present, &required
        ));
    }

    #[test]
    fn compose_reuse_policy_allows_running_container_without_pull() {
        let container = reusable_container("stable-hash");
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "project-app-1".to_owned()
            }
        );
        assert!(should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy::default()
        ));
    }

    #[test]
    fn compose_reuse_policy_blocks_running_container_when_pull_is_requested() {
        let container = reusable_container("stable-hash");
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            false,
        )
        .unwrap();

        assert!(!should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy {
                pull: true,
                service_forward_requires_recreate: false,
            }
        ));
    }

    #[test]
    fn compose_reuse_policy_blocks_rebuild_decision() {
        let container = reusable_container("stable-hash");
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            true,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
        assert!(!should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy::default()
        ));
    }

    #[test]
    fn compose_reuse_policy_rejects_changed_config_hash() {
        let container = reusable_container("old-hash");
        let error = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[]),
            false,
        )
        .unwrap_err();

        assert!(error.to_string().contains("Run decune rebuild"));
    }

    #[test]
    fn compose_reuse_policy_blocks_credential_mount_recreate() {
        let container = UpContainerSummary {
            mounts: Some(Vec::new()),
            ..reusable_container("stable-hash")
        };
        let required_mount = UpMountSummary {
            source: Some("/tmp/decune/gh".to_owned()),
            target: "/run/decune/gh".to_owned(),
            mount_type: MountType::Bind,
            read_only: true,
        };
        let decision = decide_existing_container(
            std::slice::from_ref(&container),
            "stable-hash",
            &mount_policy(&[required_mount]),
            false,
        )
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container]
            }
        );
        assert!(!should_reuse_existing_container(
            &decision,
            ExistingContainerReusePolicy::default()
        ));
    }
    #[test]
    fn up_detach_creates_and_reuses_container() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-detach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(inspect.state.and_then(|state| state.running), Some(true));

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_recreates_legacy_container_missing_decune_runtime_mount() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-legacy-runtime-mount");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20"
                }
                "#,
            );
            let legacy_plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = legacy_plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let legacy = create_and_start_container(
                    &client,
                    &workspace,
                    &legacy_plan,
                    false,
                    false,
                    false,
                )
                .await?;
                let legacy_inspect = client.cli().inspect_container(&container_name).await?;
                assert!(!container_has_mount_target(
                    &legacy_inspect.mounts,
                    "/run/decune"
                ));

                let recreated = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!recreated.reused);
                assert_ne!(legacy.container_id, recreated.container_id);

                let recreated_inspect = client.cli().inspect_container(&container_name).await?;
                assert!(container_has_mount_target(
                    &recreated_inspect.mounts,
                    "/run/decune"
                ));

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_reuses_container_when_built_image_tag_is_removed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-missing-image-tag");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN adduser -D vscode
                USER vscode
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  }
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_reuses_image_container_when_source_image_tag_is_removed() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-missing-source-image");
            let image = format!(
                "localhost:9/decune-test/reuse-source-image-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}",
                      "initializeCommand": "docker tag alpine:3.20 {image}"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                ensure_image(&client, "alpine:3.20", PullPolicy::Missing).await?;
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(first.container_name, container_name);
                assert!(!first.reused);

                remove_image(&client, &image, true).await?;

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_reuses_existing_image_config_user_when_source_tag_user_changes() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-reuse-image-user-change");
            let image = format!(
                "decune-test/reuse-image-user-change-{}:latest",
                workspace.id()
            );
            write_devcontainer(
                &workspace,
                &format!(
                    r#"
                    {{
                      "image": "{image}"
                    }}
                    "#
                ),
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                build_user_image(&client, &image, "olduser").await?;
                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                build_user_image(&client, &image, "newuser").await?;
                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert_eq!(second.container_name, container_name);
                assert!(second.reused);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec!["id".to_owned(), "-un".to_owned()],
                        user: Some("olduser".to_owned()),
                        working_dir: None,
                        env: BTreeMap::new(),
                        redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout)?, "olduser\n");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn rebuild_detach_recreates_without_post_attach() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-rebuild-detach-no-post-attach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": "printf post-start >/tmp/decune-rebuild-post-start",
                  "postAttachCommand": "printf post-attach >/tmp/decune-rebuild-post-attach"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let first = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: false,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!first.reused);

                let second = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    skip_global_config: false,
                    cli_layer: ConfigLayer::default(),
                    pull: false,
                    rebuild: true,
                    no_cache: false,
                    update_features: false,
                })
                .await?;
                assert!(!second.reused);
                assert_ne!(first.container_id, second.container_id);

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "test -f /tmp/decune-rebuild-post-start && test ! -e /tmp/decune-rebuild-post-attach && cat /tmp/decune-rebuild-post-start".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "post-start");

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
}

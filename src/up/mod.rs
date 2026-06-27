use anyhow::Result;

mod attach;
mod build;
mod exec_target;
mod existing;
mod forwarding;
mod lifecycle;
mod metadata;
mod mounts;
mod plan;
mod shell;
mod start;
mod types;
mod uid_gid;

use crate::{
    config::resolved::{ResolvedDevcontainerSource, ResolvedShutdownAction},
    docker::container::stop_container,
    runtime::compose_cli::{ComposeLifecyclePlan, ComposeStopOptions, DockerComposeCli},
    state,
};
use attach::attach_shell;
use forwarding::{start_forwarding_for_up, stop_forwarding, warn_about_detached_forwarding};
use lifecycle::{
    prepare_up_lifecycle, report_up_success, run_attach_lifecycle_for_up,
    run_container_start_lifecycle_for_up, start_host_daemon_for_up,
};
use start::ensure_container_started;

pub(crate) use mounts::mount_hash_inputs;
pub(in crate::up) use mounts::{
    WorkspaceLocationValidation, resolve_workspace_location, static_mount_variable_context,
    workspace_mount_plan_from_resolved,
};
pub(crate) use plan::{
    build_read_only_up_plan_with_forwarding_resolution, build_up_plan_with_forwarding_resolution,
};
pub(crate) use types::{
    ExistingContainerDecision, ForwardingResolution, MountResolution, UpContainerSummary,
    UpMountSummary, UpOptions, UpOutcome, UpPlan, UpPlanResolution, WorkspaceLocation,
};
pub(in crate::up) use uid_gid::static_uid_gid_sync_hash_input;

const SHUTDOWN_STOP_TIMEOUT_SECONDS: i32 = 10;

pub(crate) async fn run_detached_up(options: UpOptions) -> Result<UpOutcome> {
    let start_time = std::time::Instant::now();
    let started = Box::pin(ensure_container_started(
        options,
        ForwardingResolution::IgnoreDetached,
    ))
    .await?;
    warn_about_detached_forwarding(&started.plan);
    let _host_daemon = start_host_daemon_for_up(&started).await?;
    {
        let lifecycle = prepare_up_lifecycle(&started).await?;
        run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    }
    mark_started_workspace_used(&started)?;
    report_up_success(&started, start_time.elapsed());

    Ok(started.outcome)
}

pub(crate) async fn run_attached_up(options: UpOptions) -> Result<i32> {
    let start_time = std::time::Instant::now();
    let started = Box::pin(ensure_container_started(
        options,
        ForwardingResolution::Resolve,
    ))
    .await?;
    let _host_daemon = start_host_daemon_for_up(&started).await?;
    let lifecycle = prepare_up_lifecycle(&started).await?;
    run_container_start_lifecycle_for_up(&started, &lifecycle).await?;
    let forwarding = start_forwarding_for_up(&started).await?;
    let attach_result = async {
        run_attach_lifecycle_for_up(&lifecycle).await?;
        mark_started_workspace_used(&started)?;
        report_up_success(&started, start_time.elapsed());

        attach_shell(
            &started.client,
            &started.workspace,
            &started.plan,
            &started.outcome.container_name,
        )
        .await
    }
    .await;
    stop_forwarding(forwarding).await;

    let exit_code = attach_result?;
    apply_shutdown_action_after_attached_up(&started).await?;
    Ok(shell::clamp_exit_code(exit_code))
}

fn mark_started_workspace_used(started: &start::StartedUpContainer) -> Result<()> {
    let mut state = started.state.borrow_mut();
    state::mark_state_used(started.workspace.paths().state_dir(), &mut state)
}

async fn apply_shutdown_action_after_attached_up(
    started: &start::StartedUpContainer,
) -> Result<()> {
    match started.plan.config.devcontainer.shutdown_action {
        ResolvedShutdownAction::None => Ok(()),
        ResolvedShutdownAction::StopContainer => {
            stop_primary_container_after_attached_up(started).await
        }
        ResolvedShutdownAction::StopCompose => {
            let Some(compose_project) = &started.plan.compose_project else {
                return stop_primary_container_after_attached_up(started).await;
            };
            let Some(ResolvedDevcontainerSource::Compose(_)) =
                &started.plan.config.devcontainer.source
            else {
                return stop_primary_container_after_attached_up(started).await;
            };
            let lifecycle =
                ComposeLifecyclePlan::down(compose_project.command_plan_with_generated_override());
            DockerComposeCli::default()
                .stop(
                    &lifecycle.project,
                    ComposeStopOptions {
                        timeout_seconds: None,
                    },
                    &lifecycle.services,
                )
                .await
        }
    }
}

async fn stop_primary_container_after_attached_up(
    started: &start::StartedUpContainer,
) -> Result<()> {
    stop_container(
        &started.client,
        &started.outcome.container_id,
        SHUTDOWN_STOP_TIMEOUT_SECONDS,
    )
    .await
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, fs};

    use anyhow::Context;

    use super::{
        UpOptions,
        plan::build_up_plan,
        run_attached_up, run_detached_up,
        test_support::{test_workspace, write_devcontainer},
    };
    use crate::{
        config::ConfigLayer,
        docker::{
            client::DockerClient,
            container::{remove_container, stop_container},
            exec::{ExecCommandSpec, exec_capture},
            image::remove_image,
        },
    };

    #[test]
    fn up_detach_does_not_run_post_attach() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-detach-no-post-attach");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "postStartCommand": "printf post-start >/tmp/decune-post-start",
                  "postAttachCommand": "printf post-attach >/tmp/decune-post-attach"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                run_detached_up(UpOptions {
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

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "test -f /tmp/decune-post-start && test ! -e /tmp/decune-post-attach && cat /tmp/decune-post-start".to_owned(),
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
    #[test]
    fn up_attached_runs_post_attach_before_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-attached-post-attach-before-shell");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'test -f /tmp/decune-post-attach-before-shell || exit 9' \
                  'exit 0' \
                  >/usr/local/bin/decune-shell-check \
                  && chmod +x /usr/local/bin/decune-shell-check
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "printf ready >/tmp/decune-post-attach-before-shell"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1
    shell = "/usr/local/bin/decune-shell-check"
    "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                let exit_code = run_attached_up(UpOptions {
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
                assert_eq!(exit_code, 0);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_attached_runs_post_attach_each_attach_before_shutdown() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-attached-post-attach-each-attach");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' '#!/bin/sh' 'exit 0' >/usr/local/bin/decune-exit-0 \
                  && chmod +x /usr/local/bin/decune-exit-0
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "count_file=.decune-post-attach-count; count=0; if [ -f \"$count_file\" ]; then count=$(cat \"$count_file\"); fi; count=$((count + 1)); printf '%s' \"$count\" >\"$count_file\""
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1
    shell = "/usr/local/bin/decune-exit-0"
    "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                for _ in 0..2 {
                    let exit_code = run_attached_up(UpOptions {
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
                    assert_eq!(exit_code, 0);
                }

                assert_eq!(
                    fs::read_to_string(workspace.root().join(".decune-post-attach-count"))
                        .unwrap(),
                    "2"
                );

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_running_attached_runs_post_attach_each_attach_when_shutdown_action_none() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-running-post-attach-each-attach");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' '#!/bin/sh' 'exit 0' >/usr/local/bin/decune-exit-0 \
                  && chmod +x /usr/local/bin/decune-exit-0
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "count=0; if [ -f /tmp/decune-post-attach-count ]; then count=$(cat /tmp/decune-post-attach-count); fi; count=$((count + 1)); printf '%s' \"$count\" >/tmp/decune-post-attach-count",
                  "shutdownAction": "none"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1
    shell = "/usr/local/bin/decune-exit-0"
    "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                for _ in 0..2 {
                    let exit_code = run_attached_up(UpOptions {
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
                    assert_eq!(exit_code, 0);
                }

                let output = exec_capture(
                    &client,
                    &container_name,
                    &ExecCommandSpec {
                        command: vec![
                            "/bin/sh".to_owned(),
                            "-c".to_owned(),
                            "cat /tmp/decune-post-attach-count".to_owned(),
                        ],
                        user: None,
                        working_dir: None,
                        env: BTreeMap::new(),
            redactions: Vec::new(),
                        tty: false,
                    },
                )
                .await?;
                assert_eq!(String::from_utf8(output.stdout).unwrap(), "2");

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn up_attached_stopped_runs_start_attach_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-stopped-attached-lifecycle");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'test "$(cat /tmp/decune-stopped-attach-matrix)" = "ssa" || exit 9' \
                  'exit 0' \
                  >/usr/local/bin/decune-shell-check \
                  && chmod +x /usr/local/bin/decune-shell-check
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postStartCommand": "printf s >>/tmp/decune-stopped-attach-matrix",
                  "postAttachCommand": "printf a >>/tmp/decune-stopped-attach-matrix"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1
    shell = "/usr/local/bin/decune-shell-check"
    "#,
            )
            .unwrap();
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let image = plan.image.clone();
            let client = DockerClient::connect_from_env().unwrap();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                remove_image(&client, &image, true).await?;

                run_detached_up(UpOptions {
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
                stop_container(&client, &container_name, 10).await?;

                let exit_code = run_attached_up(UpOptions {
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
                assert_eq!(exit_code, 0);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
    #[test]
    fn rebuild_attached_recreates_runs_post_attach_before_shell() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-rebuild-attached-post-attach");
            fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
            fs::write(
                workspace.root().join(".devcontainer/Dockerfile"),
                r#"
                FROM alpine:3.20
                RUN printf '%s\n' \
                  '#!/bin/sh' \
                  'test -f /tmp/decune-rebuild-post-attach-before-shell || exit 9' \
                  'exit 0' \
                  >/usr/local/bin/decune-shell-check \
                  && chmod +x /usr/local/bin/decune-shell-check
                "#,
            )
            .unwrap();
            write_devcontainer(
                &workspace,
                r#"
                {
                  "build": {
                    "dockerfile": "Dockerfile"
                  },
                  "postAttachCommand": "printf ready >/tmp/decune-rebuild-post-attach-before-shell"
                }
                "#,
            );
            fs::create_dir_all(workspace.root().join(".decune")).unwrap();
            fs::write(
                workspace.root().join(".decune/config.toml"),
                r#"
    version = 1
    shell = "/usr/local/bin/decune-shell-check"
    "#,
            )
            .unwrap();
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

                let exit_code = run_attached_up(UpOptions {
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
                assert_eq!(exit_code, 0);

                let inspect = client.cli().inspect_container(&container_name).await?;
                let rebuilt_container_id = inspect
                    .id
                    .context("Docker inspect response did not include container id")?;
                assert_ne!(first.container_id, rebuilt_container_id);

                Ok(())
            }
            .await;

            let container_cleanup = remove_container(&client, &container_name, true, true).await;
            let image_cleanup = remove_image(&client, &image, true).await;
            result.and(container_cleanup).and(image_cleanup).unwrap();
        });
    }
}

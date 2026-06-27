use std::time::Duration;

use crate::{
    docker::{
        build::{DockerBuildInput, FEATURE_ENTRYPOINT_WRAPPER, build_image},
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, create_container,
            devcontainer_keepalive_command, remove_container, start_container,
        },
        dotfiles::materialize_dotfile_skeletons,
        image::{PullPolicy, ensure_image, image_startup_command},
        user::uid_gid_sync_runtime_user,
    },
    state::{self, LifecycleState, WorkspaceState},
    ui,
    up::{
        build::{
            build_workspace_image_layers, plan_requires_final_image_layer,
            prepare_base_image_for_plan,
        },
        types::{StartupVerification, UpOutcome, UpPlan},
    },
    workspace::Workspace,
};
use anyhow::{Context, Result, bail};

use super::{
    KEEPALIVE_STARTUP_CHECK_DELAY, ORIGINAL_COMMAND_STARTUP_MONITOR_WINDOW,
    ensure_feature_entrypoints_completed, state_compose_project_name, state_container_snapshot,
};

pub(in crate::up) async fn create_and_start_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    image_preparation: ImagePreparation,
) -> Result<UpOutcome> {
    create_and_start_container_inner(client, workspace, plan, image_preparation).await
}

async fn create_and_start_container_inner(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    image_preparation: ImagePreparation,
) -> Result<UpOutcome> {
    prepare_image_for_create(client, plan, image_preparation).await?;

    let has_feature_entrypoints = !plan.config.devcontainer.entrypoints.is_empty();
    let (entrypoint, command) = if has_feature_entrypoints {
        let command = if plan.config.devcontainer.override_command {
            let (entrypoint, command) = devcontainer_keepalive_command();
            let mut wrapped_command = vec![entrypoint.join(" ")];
            wrapped_command.extend(command);
            Some(wrapped_command)
        } else {
            let startup = image_startup_command(client, &plan.image).await?;
            let mut wrapped_command = startup.entrypoint;
            wrapped_command.extend(startup.command);
            (!wrapped_command.is_empty()).then_some(wrapped_command)
        };
        (Some(vec![FEATURE_ENTRYPOINT_WRAPPER.to_owned()]), command)
    } else if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        (Some(entrypoint), Some(command))
    } else {
        (None, None)
    };
    let spec = ContainerCreateSpec::from_resolved(ContainerCreateInput {
        image: &plan.image,
        resources: &plan.resources,
        config: &plan.config,
        entrypoint,
        command,
        working_dir: Some(plan.workspace_folder.clone()),
        mounts: plan.mounts.clone(),
    });
    let container_user = uid_gid_sync_runtime_user(
        &plan.effective_users.container_user.user,
        &plan.uid_gid_sync_plan,
    )?;
    let spec = ContainerCreateSpec {
        user: Some(container_user),
        ..spec
    };
    materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
    ui::status("Creating", "dev container");
    let container_id = create_container(client, &spec).await?;
    if let Err(state_error) = persist_initial_container_state(workspace, plan, &container_id) {
        let cleanup = remove_container(client, &plan.resources.container_name, true, true).await;
        return match cleanup {
            Ok(()) => Err(state_error.context(format!(
                "Failed to persist initial lifecycle state for Docker container: {}",
                plan.resources.container_name
            ))),
            Err(cleanup_error) => Err(state_error.context(format!(
                "Failed to persist initial lifecycle state for Docker container: {}. Failed to remove Docker container after state failure: {}: {cleanup_error:#}",
                plan.resources.container_name, plan.resources.container_name
            ))),
        };
    }
    ui::status("Starting", "dev container");
    start_new_container(
        client,
        workspace,
        &plan.resources.container_name,
        startup_verification_for_plan(plan),
    )
    .await?;

    Ok(UpOutcome {
        container_id,
        container_name: plan.resources.container_name.clone(),
        reused: false,
    })
}

pub(super) async fn prepare_image_for_create(
    client: &DockerClient,
    plan: &UpPlan,
    image_preparation: ImagePreparation,
) -> Result<()> {
    if plan_requires_final_image_layer(plan) {
        if !image_preparation.image_prepared {
            prepare_base_image_for_plan(
                client,
                plan,
                image_preparation.pull,
                image_preparation.no_cache,
            )
            .await?;
            build_workspace_image_layers(client, plan, image_preparation.no_cache).await?;
        }
    } else if let Some(context) = plan.build_context.clone() {
        if !image_preparation.image_prepared {
            let mut build_options = plan.build_options.clone();
            build_options.pull = image_preparation.pull;
            build_options.no_cache = image_preparation.no_cache;
            build_image(
                client,
                DockerBuildInput {
                    image_tag: plan.base_image.clone(),
                    labels: plan.resources.labels.clone().into_iter().collect(),
                    context,
                    options: build_options,
                },
            )
            .await?;
        }
    } else if !image_preparation.image_prepared {
        ensure_image(
            client,
            &plan.base_image,
            if image_preparation.pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub(in crate::up) struct ImagePreparation {
    pub(in crate::up) pull: bool,
    pub(in crate::up) no_cache: bool,
    pub(in crate::up) image_prepared: bool,
}

fn persist_initial_container_state(
    workspace: &Workspace,
    plan: &UpPlan,
    container_id: &str,
) -> Result<WorkspaceState> {
    state::sync_state_with_container_and_compose_project(
        workspace.paths().state_dir(),
        workspace.root(),
        state_container_snapshot(plan, container_id.to_owned()),
        state_compose_project_name(plan),
        LifecycleState::default(),
    )
}

pub(super) const fn startup_verification_for_plan(plan: &UpPlan) -> StartupVerification {
    if !plan.config.devcontainer.entrypoints.is_empty() {
        return StartupVerification::FeatureEntrypoints {
            monitor_delegated_command: !plan.config.devcontainer.override_command,
        };
    }

    if plan.config.devcontainer.override_command {
        StartupVerification::Keepalive
    } else {
        StartupVerification::OriginalCommand
    }
}

async fn start_new_container(
    client: &DockerClient,
    workspace: &Workspace,
    container_name: &str,
    verification: StartupVerification,
) -> Result<()> {
    match start_container_and_verify_running(client, container_name, verification).await {
        Ok(()) => Ok(()),
        Err(start_error) => {
            let cleanup = remove_container(client, container_name, true, true).await;
            match cleanup {
                Ok(()) => {
                    state::reconcile_state_without_container(workspace.paths().state_dir())?;
                    Err(start_error)
                }
                Err(cleanup_error) => Err(start_error.context(format!(
                    "Failed to remove Docker container after start failure: {container_name}: {cleanup_error:#}"
                ))),
            }
        }
    }
}

pub(super) async fn start_container_and_verify_running(
    client: &DockerClient,
    container_name: &str,
    verification: StartupVerification,
) -> Result<()> {
    start_container(client, container_name).await?;
    ensure_container_running_after_start(client, container_name, verification).await
}

pub(super) async fn ensure_container_running_after_start(
    client: &DockerClient,
    container_name: &str,
    verification: StartupVerification,
) -> Result<()> {
    match verification {
        StartupVerification::Keepalive => {
            tokio::time::sleep(KEEPALIVE_STARTUP_CHECK_DELAY).await;
            ensure_container_running_now(client, container_name).await
        }
        StartupVerification::OriginalCommand => {
            ensure_original_command_kept_container_running(client, container_name).await
        }
        StartupVerification::FeatureEntrypoints {
            monitor_delegated_command,
        } => {
            ensure_feature_entrypoints_completed(client, container_name).await?;
            if monitor_delegated_command {
                ensure_original_command_kept_container_running(client, container_name).await?;
            }
            Ok(())
        }
    }
}

pub(super) async fn ensure_container_running_now(
    client: &DockerClient,
    container_name: &str,
) -> Result<()> {
    let inspect = client
        .cli()
        .inspect_container(container_name)
        .await
        .with_context(|| {
            format!("Failed to inspect Docker container after start: {container_name}")
        })?;
    let Some(state) = inspect.state else {
        bail!("Container state is unavailable after start: {container_name}");
    };

    if state.running == Some(true) {
        return Ok(());
    }

    let exit = state
        .exit_code
        .map(|code| format!(" with exit code {code}"))
        .unwrap_or_default();
    bail!("Container exited during startup: {container_name}{exit}");
}

async fn ensure_original_command_kept_container_running(
    client: &DockerClient,
    container_name: &str,
) -> Result<()> {
    if let Some(exit_code) = wait_for_container_exit_within(
        client,
        container_name,
        ORIGINAL_COMMAND_STARTUP_MONITOR_WINDOW,
    )
    .await?
    {
        return Err(container_exited_during_startup_error(
            container_name,
            Some(exit_code),
        ));
    }

    ensure_container_running_now(client, container_name).await
}

async fn wait_for_container_exit_within(
    client: &DockerClient,
    container_name: &str,
    duration: Duration,
) -> Result<Option<i64>> {
    tokio::time::timeout(
        duration,
        wait_for_container_exit_code(client, container_name),
    )
    .await
    .map_or_else(|_| Ok(None), |exit_code| exit_code.map(Some))
}

pub(super) fn container_exited_during_startup_error(
    container_name: &str,
    exit_code: Option<i64>,
) -> anyhow::Error {
    let exit = exit_code
        .map(|code| format!(" with exit code {code}"))
        .unwrap_or_default();
    anyhow::anyhow!("Container exited during startup: {container_name}{exit}")
}

pub(in crate::up) async fn wait_for_container_exit_code(
    client: &DockerClient,
    container: &str,
) -> Result<i64> {
    client
        .cli()
        .wait_container(container)
        .await
        .with_context(|| format!("Failed to wait for Docker container: {container}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ConfigLayer,
        docker::{
            client::DockerClient,
            container::remove_container,
            user::{EffectiveUserResolveInput, resolve_effective_users},
        },
        up::{
            UpOptions,
            plan::build_up_plan,
            run_detached_up,
            start::list_workspace_containers,
            test_support::{test_up_plan_with_image_source, test_workspace, write_devcontainer},
        },
    };

    #[test]
    fn create_and_start_container_uses_effective_container_user() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let mut plan = test_up_plan_with_image_source("alpine:3.20");
            plan.workspace_folder = "/".to_owned();
            plan.effective_users = resolve_effective_users(EffectiveUserResolveInput {
                devcontainer_remote: None,
                devcontainer_container: Some("nobody"),
                image_metadata_remote: None,
                image_metadata_container: None,
                image_config: None,
            })
            .unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;
                let workspace = test_workspace("docker-up-effective-container-user-state");
                create_and_start_container(
                    &client,
                    &workspace,
                    &plan,
                    ImagePreparation {
                        pull: false,
                        no_cache: false,
                        image_prepared: false,
                    },
                )
                .await?;

                let inspect = client.cli().inspect_container(&container_name).await?;
                assert_eq!(
                    inspect.config.and_then(|config| config.user),
                    Some("nobody".to_owned())
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
    #[test]
    fn up_detach_removes_new_container_when_start_fails() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();

        runtime.block_on(async {
            let workspace = test_workspace("docker-up-start-failure-cleanup");
            write_devcontainer(
                &workspace,
                r#"
                {
                  "image": "alpine:3.20",
                  "containerUser": "decune-missing-user"
                }
                "#,
            );
            let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
            let container_name = plan.resources.container_name.clone();
            let client = DockerClient::connect_from_env();

            let result: anyhow::Result<()> = async {
                remove_container(&client, &container_name, true, true).await?;

                let error = run_detached_up(UpOptions {
                    workspace: workspace.root().to_path_buf(),
                    config_path: None,
                    cli_layer: ConfigLayer::default(),
                    config: crate::up::UpConfigOptions::default(),
                    build: crate::up::UpBuildOptions::default(),
                    reuse: crate::up::UpReuseOptions::default(),
                })
                .await
                .unwrap_err();
                assert!(
                    error
                        .to_string()
                        .contains("Remote user does not exist in container")
                );
                assert!(error.to_string().contains("decune-missing-user"));

                let containers = list_workspace_containers(&client, workspace.id()).await?;
                assert!(
                    !containers
                        .iter()
                        .any(|container| container.name == container_name)
                );

                Ok(())
            }
            .await;

            let cleanup = remove_container(&client, &container_name, true, true).await;
            result.and(cleanup).unwrap();
        });
    }
}

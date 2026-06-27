use super::*;

pub(in crate::up) async fn create_and_start_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    pull: bool,
    no_cache: bool,
    image_prepared: bool,
) -> Result<UpOutcome> {
    create_and_start_container_inner(client, workspace, plan, pull, no_cache, image_prepared).await
}

async fn create_and_start_container_inner(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    pull: bool,
    no_cache: bool,
    image_prepared: bool,
) -> Result<UpOutcome> {
    prepare_image_for_create(client, plan, pull, no_cache, image_prepared).await?;

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
    pull: bool,
    no_cache: bool,
    image_prepared: bool,
) -> Result<()> {
    if plan_requires_final_image_layer(plan) {
        if !image_prepared {
            prepare_base_image_for_plan(client, plan, pull, no_cache).await?;
            build_workspace_image_layers(client, plan, no_cache).await?;
        }
    } else if let Some(context) = plan.build_context.clone() {
        if !image_prepared {
            let mut build_options = plan.build_options.clone();
            build_options.pull = pull;
            build_options.no_cache = no_cache;
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
    } else if !image_prepared {
        ensure_image(
            client,
            &plan.base_image,
            if pull {
                PullPolicy::Always
            } else {
                PullPolicy::Missing
            },
        )
        .await?;
    }
    Ok(())
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

pub(super) fn startup_verification_for_plan(plan: &UpPlan) -> StartupVerification {
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
    match tokio::time::timeout(
        duration,
        wait_for_container_exit_code(client, container_name),
    )
    .await
    {
        Ok(exit_code) => exit_code.map(Some),
        Err(_) => Ok(None),
    }
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

use std::{
    cell::RefCell,
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use bollard::query_parameters::WaitContainerOptionsBuilder;
use futures_util::{
    FutureExt, TryStreamExt,
    future::{Either, select},
};

use crate::{
    devcontainer::lifecycle::{LifecycleRunPath, run_host_initialize_lifecycle},
    docker::{
        build::{
            DockerBuildInput, FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_WRAPPER, build_image,
        },
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, create_container,
            devcontainer_keepalive_command, remove_container, start_container, stop_container,
            workspace_container_list_options,
        },
        exec::{ExecCommandSpec, exec_capture_output},
        image::{PullPolicy, ensure_image, image_startup_command},
        mounts::{DockerMountSpec, normalize_container_path},
        user::uid_gid_sync_runtime_user,
    },
    host::{
        credentials::{
            DECUNE_RUNTIME_TARGET, GitCredentialRuntime, GithubCliRuntime, SshAgentRuntime,
            prepare_git_credential_runtime, prepare_github_cli_runtime, prepare_ssh_agent_runtime,
        },
        forward::{ForwardRuntime, prepare_forward_runtime},
    },
    state::{self, LifecycleState, StateContainerSnapshot, WorkspaceState},
    ui,
    up::{
        build::{
            build_workspace_image_layers, plan_requires_final_image_layer,
            prepare_base_image_for_plan,
        },
        existing::{
            self, CredentialRuntimeMountPolicy, container_summary, decide_existing_container,
        },
        metadata::{
            build_existing_container_decision_plan, existing_remote_user_image_for_decision,
            finalize_up_plan_mounts, prepare_image_based_metadata, warn_about_deferred_features,
        },
        plan::build_preliminary_up_plan_with_forwarding_resolution,
        types::{
            ExistingContainerDecision, ForwardingResolution, StartupVerification,
            UpContainerSummary, UpMountSummary, UpOptions, UpOutcome, UpPlan, UpPlanResolution,
        },
    },
    workspace::Workspace,
};

const REBUILD_STOP_TIMEOUT_SECONDS: i32 = 10;
const KEEPALIVE_STARTUP_CHECK_DELAY: Duration = Duration::from_millis(200);
const ORIGINAL_COMMAND_STARTUP_MONITOR_WINDOW: Duration = Duration::from_secs(2);
const FEATURE_ENTRYPOINT_SENTINEL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const FEATURE_ENTRYPOINT_SENTINEL_MODE: u32 = 0o666;
const FEATURE_ENTRYPOINT_RUNTIME_DIR_MODE: u32 = 0o711;

pub(in crate::up) struct StartedUpContainer {
    pub(in crate::up) client: DockerClient,
    pub(in crate::up) workspace: Workspace,
    pub(in crate::up) plan: UpPlan,
    pub(in crate::up) outcome: UpOutcome,
    pub(in crate::up) lifecycle_path: LifecycleRunPath,
    pub(in crate::up) state: RefCell<WorkspaceState>,
    _credentials: CredentialRuntime,
}

pub(in crate::up) struct CredentialRuntime {
    _git_credentials: GitCredentialRuntime,
    _github_cli: GithubCliRuntime,
    _ssh_agent: SshAgentRuntime,
    _forward: ForwardRuntime,
    mount_policy: CredentialRuntimeMountPolicy,
}

impl CredentialRuntime {
    fn new(
        git_credentials: GitCredentialRuntime,
        github_cli: GithubCliRuntime,
        ssh_agent: SshAgentRuntime,
        forward: ForwardRuntime,
    ) -> Self {
        let required_mounts = git_credentials
            .mounts()
            .iter()
            .chain(github_cli.mounts())
            .chain(ssh_agent.mounts())
            .chain(forward.mounts())
            .map(|mount| UpMountSummary {
                source: mount.source.clone(),
                target: mount.target.clone(),
                mount_type: mount.mount_type,
                read_only: mount.read_only,
            })
            .collect();

        Self {
            _git_credentials: git_credentials,
            _github_cli: github_cli,
            _ssh_agent: ssh_agent,
            _forward: forward,
            mount_policy: CredentialRuntimeMountPolicy::new(required_mounts),
        }
    }

    pub(in crate::up) fn mount_policy(&self) -> &CredentialRuntimeMountPolicy {
        &self.mount_policy
    }
}

fn started_up_container(
    client: DockerClient,
    workspace: Workspace,
    plan: UpPlan,
    outcome: UpOutcome,
    lifecycle_path: LifecycleRunPath,
    credentials: CredentialRuntime,
) -> Result<StartedUpContainer> {
    let state = sync_started_state(&workspace, &plan, &outcome, lifecycle_path)?;

    Ok(started_up_container_with_state(
        client,
        workspace,
        plan,
        outcome,
        lifecycle_path,
        credentials,
        state,
    ))
}

fn started_up_container_with_state(
    client: DockerClient,
    workspace: Workspace,
    plan: UpPlan,
    outcome: UpOutcome,
    lifecycle_path: LifecycleRunPath,
    credentials: CredentialRuntime,
    state: WorkspaceState,
) -> StartedUpContainer {
    StartedUpContainer {
        client,
        workspace,
        plan,
        outcome,
        lifecycle_path,
        state: RefCell::new(state),
        _credentials: credentials,
    }
}

fn sync_started_state(
    workspace: &Workspace,
    plan: &UpPlan,
    outcome: &UpOutcome,
    lifecycle_path: LifecycleRunPath,
) -> Result<WorkspaceState> {
    let container = StateContainerSnapshot {
        container_id: outcome.container_id.clone(),
        image: plan.image.clone(),
        config_hash: plan.resources.config_hash.clone(),
    };
    match lifecycle_path {
        LifecycleRunPath::New => state::sync_state_with_container(
            workspace.paths().state_dir(),
            workspace.root(),
            container,
            LifecycleState::default(),
        ),
        LifecycleRunPath::Started | LifecycleRunPath::Running => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            write_reused_started_state(workspace, container, existing)
        }
    }
}

fn reusable_lifecycle_state(
    workspace: &Workspace,
    container: &StateContainerSnapshot,
) -> Result<WorkspaceState> {
    let state_file = state::state_file_path(workspace.paths().state_dir());
    let existing = state::load_state_file(workspace.paths().state_dir())?;
    let Some(existing) =
        existing.filter(|state| state_matches_container_snapshot(state, container))
    else {
        bail!(
            "Cannot safely reuse existing dev container without matching lifecycle state: {}. Run decune rebuild to recreate it.",
            state_file.display()
        );
    };

    Ok(existing)
}

fn write_reused_started_state(
    workspace: &Workspace,
    container: StateContainerSnapshot,
    existing: WorkspaceState,
) -> Result<WorkspaceState> {
    state::write_state_for_container(
        workspace.paths().state_dir(),
        workspace.root(),
        container,
        existing.lifecycle,
        Some(existing.created_at),
    )
}

fn state_matches_container_snapshot(
    state: &WorkspaceState,
    container: &StateContainerSnapshot,
) -> bool {
    state.container_id == container.container_id && state.config_hash == container.config_hash
}

pub(in crate::up) async fn ensure_container_started(
    options: UpOptions,
    forwarding_resolution: ForwardingResolution,
) -> Result<StartedUpContainer> {
    let workspace = Workspace::resolve(&options.workspace)?;
    let preliminary_plan = build_preliminary_up_plan_with_forwarding_resolution(
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer.clone(),
        forwarding_resolution,
        options.update_features,
    )?;
    let plan_resolution = UpPlanResolution::new(forwarding_resolution, options.update_features);
    run_host_initialize_lifecycle(&preliminary_plan.config, workspace.root())?;

    let client = DockerClient::connect_from_env()?;
    let containers = list_workspace_containers(&client, workspace.id()).await?;
    if containers.is_empty() {
        state::reconcile_state_without_container(workspace.paths().state_dir())?;
    }

    if !options.rebuild && !containers.is_empty() {
        let existing_plan = build_existing_container_decision_plan(
            &client,
            &workspace,
            options.config_path.as_deref(),
            options.cli_layer.clone(),
            containers
                .first()
                .and_then(existing::existing_container_image_id),
            &preliminary_plan,
            plan_resolution,
        )
        .await?;
        let existing_container_image = containers
            .first()
            .and_then(existing::existing_container_image_id);
        let existing_remote_user_image = existing_remote_user_image_for_decision(
            &client,
            &existing_plan,
            existing_container_image,
        )
        .await?;
        let (existing_plan, _) = finalize_up_plan_mounts(
            &client,
            &workspace,
            existing_plan,
            existing_remote_user_image,
            containers
                .first()
                .and_then(existing::existing_container_config_hash),
            Some((options.pull, options.no_cache)),
            options.update_features,
        )
        .await?;
        let (existing_plan, credentials) =
            add_credential_runtime_mounts(existing_plan, workspace.paths().runtime_dir())?;

        match decide_existing_container(
            &containers,
            &existing_plan.resources.config_hash,
            credentials.mount_policy(),
            false,
        )? {
            ExistingContainerDecision::ReuseRunning { id, name } => {
                warn_about_deferred_features(&existing_plan.config);
                let outcome = UpOutcome {
                    container_id: id,
                    container_name: name,
                    reused: true,
                };
                return started_up_container(
                    client,
                    workspace,
                    existing_plan,
                    outcome,
                    LifecycleRunPath::Running,
                    credentials,
                );
            }
            ExistingContainerDecision::StartStopped { id, name } => {
                warn_about_deferred_features(&existing_plan.config);
                let (outcome, state) =
                    start_stopped_existing_container(&client, &workspace, &existing_plan, id, name)
                        .await?;
                return Ok(started_up_container_with_state(
                    client,
                    workspace,
                    existing_plan,
                    outcome,
                    LifecycleRunPath::Started,
                    credentials,
                    state,
                ));
            }
            ExistingContainerDecision::Create | ExistingContainerDecision::Recreate { .. } => {}
        }
    }

    let (plan, image_prepared) = prepare_image_based_metadata(
        &client,
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer,
        preliminary_plan,
        options.pull,
        plan_resolution,
    )
    .await?;
    let (plan, mount_image_prepared) = finalize_up_plan_mounts(
        &client,
        &workspace,
        plan,
        None,
        None,
        Some((options.pull, options.no_cache)),
        options.update_features,
    )
    .await?;
    let (plan, credentials) = add_credential_runtime_mounts(plan, workspace.paths().runtime_dir())?;
    let image_prepared =
        mount_image_prepared || (image_prepared && !plan_requires_final_image_layer(&plan));
    warn_about_deferred_features(&plan.config);

    match decide_existing_container(
        &containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        options.rebuild,
    )? {
        ExistingContainerDecision::Create => {
            let outcome = create_and_start_container(
                &client,
                &workspace,
                &plan,
                options.pull,
                options.no_cache,
                image_prepared,
            )
            .await?;
            started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::New,
                credentials,
            )
        }
        ExistingContainerDecision::Recreate { containers } => {
            recreate_existing_containers(&client, &containers).await?;
            let outcome = create_and_start_container(
                &client,
                &workspace,
                &plan,
                options.pull,
                options.no_cache,
                image_prepared,
            )
            .await?;
            started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::New,
                credentials,
            )
        }
        ExistingContainerDecision::ReuseRunning { id, name } => {
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Running,
                credentials,
            )
        }
        ExistingContainerDecision::StartStopped { id, name } => {
            let (outcome, state) =
                start_stopped_existing_container(&client, &workspace, &plan, id, name).await?;
            Ok(started_up_container_with_state(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Started,
                credentials,
                state,
            ))
        }
    }
}

async fn start_stopped_existing_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    container_id: String,
    container_name: String,
) -> Result<(UpOutcome, WorkspaceState)> {
    let container = StateContainerSnapshot {
        container_id: container_id.clone(),
        image: plan.image.clone(),
        config_hash: plan.resources.config_hash.clone(),
    };
    let existing_state = reusable_lifecycle_state(workspace, &container)?;

    start_container_and_verify_running(
        client,
        &container_name,
        startup_verification_for_plan(plan),
    )
    .await?;

    let state = write_reused_started_state(workspace, container, existing_state)?;
    Ok((
        UpOutcome {
            container_id,
            container_name,
            reused: true,
        },
        state,
    ))
}

fn add_credential_runtime_mounts(
    plan: UpPlan,
    runtime_dir: &Path,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = prepare_ssh_agent_runtime(&plan.config)?;
    let github_cli = prepare_github_cli_runtime(&plan.config, runtime_dir)?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir)?;
    add_prepared_credential_runtime_mounts(plan, runtime_dir, github_cli, ssh_agent, forward)
}

#[cfg(test)]
pub(in crate::up) fn add_credential_runtime_mounts_with_ssh_socket(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        None,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir)?;
    add_prepared_credential_runtime_mounts(plan, runtime_dir, github_cli, ssh_agent, forward)
}

#[cfg(test)]
pub(in crate::up) fn add_credential_runtime_mounts_with_inputs(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
    github_token: Option<&str>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        github_token,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir)?;
    add_prepared_credential_runtime_mounts(plan, runtime_dir, github_cli, ssh_agent, forward)
}

fn add_prepared_credential_runtime_mounts(
    mut plan: UpPlan,
    runtime_dir: &Path,
    github_cli: GithubCliRuntime,
    ssh_agent: SshAgentRuntime,
    forward: ForwardRuntime,
) -> Result<(UpPlan, CredentialRuntime)> {
    let git_credentials = prepare_git_credential_runtime(&plan.config, runtime_dir)?;
    extend_runtime_mounts(&mut plan.mounts, git_credentials.mounts());
    extend_runtime_mounts(&mut plan.mounts, github_cli.mounts());
    extend_runtime_mounts(&mut plan.mounts, ssh_agent.mounts());
    extend_runtime_mounts(&mut plan.mounts, forward.mounts());
    plan.config
        .devcontainer
        .container_env
        .extend(github_cli.container_env().clone());
    plan.config
        .devcontainer
        .container_env
        .extend(ssh_agent.container_env().clone());
    prepare_feature_entrypoint_sentinel_runtime(&plan, runtime_dir)?;

    Ok((
        plan,
        CredentialRuntime::new(git_credentials, github_cli, ssh_agent, forward),
    ))
}

fn prepare_feature_entrypoint_sentinel_runtime(plan: &UpPlan, runtime_dir: &Path) -> Result<()> {
    if plan.config.devcontainer.entrypoints.is_empty() {
        return Ok(());
    }

    fs::create_dir_all(runtime_dir).with_context(|| {
        format!(
            "Failed to create Feature entrypoint runtime directory: {}",
            runtime_dir.display()
        )
    })?;
    fs::set_permissions(
        runtime_dir,
        fs::Permissions::from_mode(FEATURE_ENTRYPOINT_RUNTIME_DIR_MODE),
    )
    .with_context(|| {
        format!(
            "Failed to set Feature entrypoint runtime directory permissions: {}",
            runtime_dir.display()
        )
    })?;

    let sentinel = feature_entrypoint_sentinel_runtime_path(runtime_dir)?;
    let _file = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(FEATURE_ENTRYPOINT_SENTINEL_MODE)
        .open(&sentinel)
        .with_context(|| {
            format!(
                "Failed to prepare Feature entrypoint sentinel: {}",
                sentinel.display()
            )
        })?;
    fs::set_permissions(
        &sentinel,
        fs::Permissions::from_mode(FEATURE_ENTRYPOINT_SENTINEL_MODE),
    )
    .with_context(|| {
        format!(
            "Failed to set Feature entrypoint sentinel permissions: {}",
            sentinel.display()
        )
    })?;

    Ok(())
}

fn feature_entrypoint_sentinel_runtime_path(runtime_dir: &Path) -> Result<PathBuf> {
    let sentinel_target = Path::new(FEATURE_ENTRYPOINT_SENTINEL);
    let relative = sentinel_target
        .strip_prefix(DECUNE_RUNTIME_TARGET)
        .with_context(|| {
            format!(
                "Feature entrypoint sentinel must be under {DECUNE_RUNTIME_TARGET}: {FEATURE_ENTRYPOINT_SENTINEL}"
            )
        })?;
    Ok(runtime_dir.join(relative))
}

fn extend_runtime_mounts(mounts: &mut Vec<DockerMountSpec>, runtime_mounts: &[DockerMountSpec]) {
    for mount in runtime_mounts {
        let target = normalize_container_path(&mount.target);
        if mounts
            .iter()
            .any(|existing| normalize_container_path(&existing.target) == target)
        {
            continue;
        }
        mounts.push(mount.clone());
    }
}

async fn recreate_existing_containers(
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

#[cfg(not(test))]
async fn create_and_start_container(
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
        crate::up::metadata::warn_about_unsupported_dockerfile_image_metadata(
            client,
            &plan.base_image,
        )
        .await?;
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

fn persist_initial_container_state(
    workspace: &Workspace,
    plan: &UpPlan,
    container_id: &str,
) -> Result<WorkspaceState> {
    state::sync_state_with_container(
        workspace.paths().state_dir(),
        workspace.root(),
        StateContainerSnapshot {
            container_id: container_id.to_owned(),
            image: plan.image.clone(),
            config_hash: plan.resources.config_hash.clone(),
        },
        LifecycleState::default(),
    )
}

fn startup_verification_for_plan(plan: &UpPlan) -> StartupVerification {
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

async fn start_container_and_verify_running(
    client: &DockerClient,
    container_name: &str,
    verification: StartupVerification,
) -> Result<()> {
    start_container(client, container_name).await?;
    ensure_container_running_after_start(client, container_name, verification).await
}

async fn ensure_container_running_after_start(
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

async fn ensure_container_running_now(client: &DockerClient, container_name: &str) -> Result<()> {
    let inspect = client
        .raw()
        .inspect_container(container_name, None)
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

async fn ensure_feature_entrypoints_completed(
    client: &DockerClient,
    container_name: &str,
) -> Result<()> {
    match select(
        wait_for_container_exit_code(client, container_name).boxed(),
        wait_for_feature_entrypoint_sentinel(client, container_name).boxed(),
    )
    .await
    {
        Either::Left((exit_code, _)) => {
            return Err(container_exited_during_startup_error(
                container_name,
                Some(exit_code?),
            ));
        }
        Either::Right((ready, _)) => {
            ready?;
            ensure_container_running_now(client, container_name).await?;
        }
    }

    Ok(())
}

async fn wait_for_feature_entrypoint_sentinel(
    client: &DockerClient,
    container_name: &str,
) -> Result<()> {
    loop {
        tokio::time::sleep(FEATURE_ENTRYPOINT_SENTINEL_POLL_INTERVAL).await;
        if feature_entrypoint_sentinel_is_current(client, container_name).await? {
            return Ok(());
        }
    }
}

async fn feature_entrypoint_sentinel_is_current(
    client: &DockerClient,
    container_name: &str,
) -> Result<bool> {
    let script = format!(
        r#"stat_line=$(cat /proc/1/stat 2>/dev/null || true)
stat_tail=${{stat_line#*) }}
set -- $stat_tail
startup_id="${{20:-}}"
test -n "$startup_id" && test -f {sentinel} && test "$(cat {sentinel})" = "$startup_id""#,
        sentinel = FEATURE_ENTRYPOINT_SENTINEL
    );
    let output = match exec_capture_output(
        client,
        container_name,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            user: None,
            working_dir: None,
            env: std::collections::BTreeMap::new(),
            tty: false,
        },
    )
    .await
    {
        Ok(output) => output,
        Err(_) => return Ok(false),
    };

    Ok(output.exit_code == 0)
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

fn container_exited_during_startup_error(
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
    let options = WaitContainerOptionsBuilder::default()
        .condition("not-running")
        .build();
    match client
        .raw()
        .wait_container(container, Some(options))
        .try_next()
        .await
    {
        Ok(Some(response)) => Ok(response.status_code),
        Ok(None) => Err(anyhow::anyhow!(
            "Docker container wait ended without a response: {container}"
        )),
        Err(bollard::errors::Error::DockerContainerWaitError { code, .. }) => Ok(code),
        Err(error) => {
            Err(error).with_context(|| format!("Failed to wait for Docker container: {container}"))
        }
    }
}

#[cfg(test)]
pub(in crate::up) async fn list_workspace_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    list_workspace_containers_inner(client, workspace_id).await
}

#[cfg(not(test))]
async fn list_workspace_containers(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    list_workspace_containers_inner(client, workspace_id).await
}

async fn list_workspace_containers_inner(
    client: &DockerClient,
    workspace_id: &str,
) -> Result<Vec<UpContainerSummary>> {
    let containers = client
        .raw()
        .list_containers(Some(workspace_container_list_options(workspace_id)))
        .await
        .with_context(|| {
            format!("Failed to list Docker containers for workspace: {workspace_id}")
        })?;

    Ok(containers
        .into_iter()
        .filter_map(container_summary)
        .collect())
}

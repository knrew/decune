use std::{
    cell::RefCell,
    collections::{BTreeMap, BTreeSet},
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use futures_util::{
    FutureExt,
    future::{Either, select},
};

use crate::{
    config::resolved::ResolvedDevcontainerSource,
    devcontainer::lifecycle::{LifecycleRunPath, run_host_initialize_lifecycle},
    docker::{
        build::{
            DockerBuildInput, FEATURE_ENTRYPOINT_SENTINEL, FEATURE_ENTRYPOINT_WRAPPER, build_image,
        },
        client::DockerClient,
        container::{
            ContainerCreateInput, ContainerCreateSpec, create_container,
            devcontainer_keepalive_command, remove_container, start_container, stop_container,
        },
        exec::{ExecCommandSpec, exec_capture_output},
        image::{PullPolicy, ensure_image, image_container_tool_platform, image_startup_command},
        mounts::{DockerMountSpec, normalize_container_path},
        user::uid_gid_sync_runtime_user,
    },
    host::{
        container_tools::ContainerToolPlatform,
        credentials::{
            DECUNE_RUNTIME_TARGET, GitCredentialRuntime, GithubCliRuntime, SshAgentRuntime,
            prepare_git_credential_runtime, prepare_github_cli_runtime, prepare_ssh_agent_runtime,
        },
        forward::{
            ForwardRuntime, ServiceForwardRuntime, prepare_forward_runtime,
            prepare_service_forward_runtimes,
        },
    },
    runtime::compose_cli::{
        ComposeBuildOptions, ComposeConfigService, ComposeIntrospector, ComposeLifecyclePlan,
        ComposeOverridePatch, ComposeOverrideServicePatch, ComposePrimaryImageResolver,
        ComposeProjectPlan, ComposePullOptions, ComposeServiceValidation, ComposeUpOptions,
        DockerComposeCli, write_compose_override,
    },
    state::{self, LifecycleState, StateContainerSnapshot, WorkspaceState},
    ui,
    up::{
        build::{
            build_workspace_image_layers, plan_requires_final_image_layer,
            prepare_base_image_for_plan,
        },
        existing::{self, CredentialRuntimeMountPolicy, decide_existing_container},
        metadata::{
            FinalizeUpPlanMountsOptions, build_existing_container_decision_plan,
            existing_remote_user_image_for_decision, finalize_up_plan_mounts,
            prepare_compose_image_metadata, prepare_image_based_metadata,
            warn_about_deferred_features,
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
    service_forward: Vec<ServiceForwardRuntime>,
    mount_policy: CredentialRuntimeMountPolicy,
}

impl CredentialRuntime {
    fn new(
        git_credentials: GitCredentialRuntime,
        github_cli: GithubCliRuntime,
        ssh_agent: SshAgentRuntime,
        forward: ForwardRuntime,
        service_forward: Vec<ServiceForwardRuntime>,
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
            service_forward,
            mount_policy: CredentialRuntimeMountPolicy::new(required_mounts),
        }
    }

    pub(in crate::up) fn mount_policy(&self) -> &CredentialRuntimeMountPolicy {
        &self.mount_policy
    }

    fn service_forward(&self) -> &[ServiceForwardRuntime] {
        &self.service_forward
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
    let container = state_container_snapshot(plan, outcome.container_id.clone());
    let compose_project_name = state_compose_project_name(plan);
    match lifecycle_path {
        LifecycleRunPath::New => state::sync_state_with_container_and_compose_project(
            workspace.paths().state_dir(),
            workspace.root(),
            container,
            compose_project_name,
            LifecycleState::default(),
        ),
        LifecycleRunPath::Started | LifecycleRunPath::Running => {
            let existing = reusable_lifecycle_state(workspace, &container)?;
            write_reused_started_state(workspace, container, compose_project_name, existing)
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
    compose_project_name: Option<String>,
    existing: WorkspaceState,
) -> Result<WorkspaceState> {
    state::write_state_for_container(
        workspace.paths().state_dir(),
        workspace.root(),
        container,
        compose_project_name,
        existing.lifecycle,
        Some(existing.created_at),
    )
}

fn state_compose_project_name(plan: &UpPlan) -> Option<String> {
    plan.compose_project
        .as_ref()
        .map(|project| project.project_name().to_owned())
}

fn state_container_snapshot(plan: &UpPlan, container_id: String) -> StateContainerSnapshot {
    StateContainerSnapshot {
        container_id,
        image: plan.image.clone(),
        config_hash: plan.resources.config_hash.clone(),
        config_file: plan
            .resources
            .labels
            .get("devcontainer.config_file")
            .cloned(),
    }
}

fn state_matches_container_snapshot(
    state: &WorkspaceState,
    container: &StateContainerSnapshot,
) -> bool {
    state::container_ids_match(&state.container_id, &container.container_id)
        && state.config_hash == container.config_hash
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct ExistingContainerReusePolicy {
    pull: bool,
    service_forward_requires_recreate: bool,
}

fn should_reuse_existing_container(
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
    run_host_initialize_lifecycle(&preliminary_plan.config, workspace.root())?;
    if preliminary_plan.compose_project.is_some() {
        validate_compose_canonical_model(&preliminary_plan).await?;
        return start_compose_project(workspace, preliminary_plan, options, forwarding_resolution)
            .await;
    }
    let plan_resolution = UpPlanResolution::new(forwarding_resolution, options.update_features);

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
            FinalizeUpPlanMountsOptions {
                update_features: options.update_features,
                compose_canonical_model: None,
                compose_primary_service_user: None,
                compose_primary_service: None,
            },
        )
        .await?;
        let platform =
            container_tool_platform_for_plan(&client, &existing_plan, existing_container_image)
                .await?;
        let (existing_plan, credentials) = add_credential_runtime_mounts(
            existing_plan,
            workspace.paths().runtime_dir(),
            platform,
        )?;

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
        FinalizeUpPlanMountsOptions {
            update_features: options.update_features,
            compose_canonical_model: None,
            compose_primary_service_user: None,
            compose_primary_service: None,
        },
    )
    .await?;
    let image_prepared =
        mount_image_prepared || (image_prepared && !plan_requires_final_image_layer(&plan));
    if !image_prepared {
        prepare_image_for_create(
            &client,
            &plan,
            options.pull,
            options.no_cache,
            image_prepared,
        )
        .await?;
    }
    let image_prepared = true;
    let platform = image_container_tool_platform(&client, &plan.image).await?;
    let (mut plan, credentials) =
        add_credential_runtime_mounts(plan, workspace.paths().runtime_dir(), platform)?;
    attach_compose_interpolation_env_to_plan(&mut plan);
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

async fn start_compose_project(
    workspace: Workspace,
    mut plan: UpPlan,
    options: UpOptions,
    forwarding_resolution: ForwardingResolution,
) -> Result<StartedUpContainer> {
    let Some(compose_project) = &plan.compose_project else {
        bail!("Docker Compose project plan is missing");
    };
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        bail!("Docker Compose devcontainer source is missing");
    };
    let user_lifecycle = ComposeLifecyclePlan::up(
        compose_project.command_plan_without_generated_override(),
        &compose.service,
        compose.run_services.as_deref(),
    );
    let cli = DockerComposeCli::default();
    cli.ensure_required_capabilities().await?;

    let user_config = ComposeIntrospector::new(cli.clone())
        .user_config(
            compose_project,
            &ComposeServiceValidation {
                primary_service: &compose.service,
                run_services: compose.run_services.as_deref(),
                workspace_folder: &plan.workspace_folder,
                project_name: compose_project.project_name(),
            },
        )
        .await?;
    let user_model = &user_config.model;
    let compose_primary_image = ComposePrimaryImageResolver {
        project_name: compose_project.project_name(),
        service: &compose.service,
    }
    .resolve(user_model)?;
    let primary_service_has_build = compose_primary_image.has_build;
    let compose_primary_image = compose_primary_image.base_image;
    let compose_primary_service_user = user_model
        .service(&compose.service)
        .and_then(|service| service.user.as_deref());
    let compose_primary_service = user_model.service(&compose.service).cloned();
    plan.base_image = compose_primary_image.clone();

    let client = DockerClient::connect_from_env()?;
    let existing_compose_project_containers =
        list_compose_project_containers(&client, workspace.id(), compose_project.project_name())
            .await?;
    let existing_compose_containers = list_compose_primary_containers(
        &client,
        workspace.id(),
        compose_project.project_name(),
        &compose.service,
    )
    .await?;

    if options.pull {
        cli.pull(
            &user_lifecycle.project,
            ComposePullOptions {
                always: true,
                ignore_buildable: true,
                // When runServices narrows the explicit pull targets, dependency images
                // are still delegated to Docker Compose instead of parsing depends_on.
                include_deps: true,
            },
            &user_lifecycle.services,
        )
        .await?;
    }
    if options.rebuild
        || options.no_cache
        || options.pull
        || (primary_service_has_build && existing_compose_containers.is_empty())
    {
        cli.build(
            &user_lifecycle.project,
            ComposeBuildOptions {
                with_dependencies: true,
                no_cache: options.no_cache,
                pull: options.pull,
            },
            &user_lifecycle.services,
        )
        .await?;
    }

    let existing_remote_user_image = if options.rebuild {
        None
    } else {
        existing_compose_containers
            .first()
            .and_then(existing::existing_container_image_id)
    };
    if existing_remote_user_image.is_none() && !primary_service_has_build {
        ensure_image(&client, &plan.base_image, PullPolicy::Missing).await?;
    }
    plan = prepare_compose_image_metadata(
        &client,
        &workspace,
        options.config_path.as_deref(),
        options.cli_layer.clone(),
        plan,
        &compose_primary_image,
        UpPlanResolution::new(forwarding_resolution, options.update_features),
    )
    .await?;
    plan.base_image = compose_primary_image.clone();
    let (plan, image_prepared) = finalize_up_plan_mounts(
        &client,
        &workspace,
        plan,
        existing_remote_user_image,
        existing_compose_containers
            .first()
            .and_then(existing::existing_container_config_hash),
        Some((options.pull && !primary_service_has_build, options.no_cache)),
        FinalizeUpPlanMountsOptions {
            update_features: options.update_features,
            compose_canonical_model: Some(&user_config.canonical_model),
            compose_primary_service_user,
            compose_primary_service: compose_primary_service.as_ref(),
        },
    )
    .await?;
    let mut plan = plan;
    if !plan_requires_final_image_layer(&plan) {
        plan.image = compose_primary_image.clone();
        plan.base_image = compose_primary_image;
    }
    if !image_prepared {
        prepare_image_for_create(&client, &plan, false, options.no_cache, false).await?;
    }
    let platform = image_container_tool_platform(&client, &plan.image).await?;
    let (mut plan, credentials) =
        add_credential_runtime_mounts(plan, workspace.paths().runtime_dir(), platform)?;
    attach_compose_interpolation_env_to_plan(&mut plan);
    warn_about_deferred_features(&plan.config);

    let Some(compose_project) = &plan.compose_project else {
        bail!("Docker Compose project plan is missing after finalization");
    };
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        bail!("Docker Compose devcontainer source is missing after finalization");
    };
    write_generated_compose_override(
        &client,
        compose_project,
        &compose.service,
        &plan,
        compose_primary_service.as_ref(),
        credentials.service_forward(),
    )
    .await?;
    let runtime_lifecycle = ComposeLifecyclePlan::up(
        compose_project.command_plan_with_generated_override(),
        &compose.service,
        compose.run_services.as_deref(),
    );

    if existing_compose_project_containers.is_empty() {
        state::reconcile_state_without_container(workspace.paths().state_dir())?;
    }
    let stale_compose_project =
        !existing_compose_project_containers.is_empty() && existing_compose_containers.is_empty();

    let decision = decide_existing_container(
        &existing_compose_containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        options.rebuild,
    )?;
    let service_forward_requires_recreate = compose_service_forward_requires_recreate(
        &client,
        workspace.id(),
        compose_project.project_name(),
        credentials.service_forward(),
    )
    .await?;
    let should_reuse = should_reuse_existing_container(
        &decision,
        ExistingContainerReusePolicy {
            pull: options.pull,
            service_forward_requires_recreate,
        },
    );
    let force_recreate = matches!(decision, ExistingContainerDecision::Recreate { .. })
        || options.rebuild
        || options.pull
        || stale_compose_project
        || service_forward_requires_recreate;
    let remove_orphans = matches!(decision, ExistingContainerDecision::Recreate { .. })
        || options.rebuild
        || stale_compose_project
        || service_forward_requires_recreate;
    match decision {
        ExistingContainerDecision::ReuseRunning { id, name } if should_reuse => {
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            return started_up_container(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Running,
                credentials,
            );
        }
        ExistingContainerDecision::StartStopped { id, name } if should_reuse => {
            cli.up(
                &runtime_lifecycle.project,
                ComposeUpOptions {
                    force_recreate: false,
                    remove_orphans: false,
                },
                &runtime_lifecycle.services,
            )
            .await?;
            ensure_container_running_after_start(
                &client,
                &name,
                startup_verification_for_plan(&plan),
            )
            .await?;
            let container = state_container_snapshot(&plan, id.clone());
            let existing_state = reusable_lifecycle_state(&workspace, &container)?;
            let state = write_reused_started_state(
                &workspace,
                container,
                state_compose_project_name(&plan),
                existing_state,
            )?;
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            return Ok(started_up_container_with_state(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Started,
                credentials,
                state,
            ));
        }
        ExistingContainerDecision::Create => {}
        ExistingContainerDecision::Recreate { .. } => {}
        ExistingContainerDecision::ReuseRunning { .. }
        | ExistingContainerDecision::StartStopped { .. } => {}
    }

    cli.up(
        &runtime_lifecycle.project,
        ComposeUpOptions {
            force_recreate,
            remove_orphans,
        },
        &runtime_lifecycle.services,
    )
    .await?;

    let container = ComposeIntrospector::new(cli)
        .resolve_service_container(&runtime_lifecycle.project, &compose.service)
        .await?;
    let outcome = UpOutcome {
        container_name: container.name.unwrap_or_else(|| container.id.clone()),
        container_id: container.id,
        reused: false,
    };
    ensure_container_running_after_start(
        &client,
        &outcome.container_name,
        startup_verification_for_plan(&plan),
    )
    .await?;
    let state = sync_started_state(&workspace, &plan, &outcome, LifecycleRunPath::New)?;

    Ok(started_up_container_with_state(
        client,
        workspace,
        plan,
        outcome,
        LifecycleRunPath::New,
        credentials,
        state,
    ))
}

async fn write_generated_compose_override(
    client: &DockerClient,
    project: &ComposeProjectPlan,
    primary_service: &str,
    plan: &UpPlan,
    compose_primary_service: Option<&ComposeConfigService>,
    service_forward: &[ServiceForwardRuntime],
) -> Result<()> {
    let path = project.generated_override_path();
    let startup = compose_override_startup(client, plan, compose_primary_service).await?;
    let patch = generated_compose_override_patch(primary_service, plan, startup, service_forward)?;
    write_compose_override(&path, &patch)
}

#[cfg(test)]
pub(in crate::up) fn generated_compose_override_content(
    primary_service: &str,
    plan: &UpPlan,
) -> Result<String> {
    let startup = if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        Some(ComposeOverrideStartup {
            entrypoint,
            command,
        })
    } else {
        None
    };
    generated_compose_override_content_with_startup(primary_service, plan, startup, &[])
}

async fn compose_override_startup(
    client: &DockerClient,
    plan: &UpPlan,
    compose_primary_service: Option<&ComposeConfigService>,
) -> Result<Option<ComposeOverrideStartup>> {
    if !plan.config.devcontainer.entrypoints.is_empty() {
        let command = if plan.config.devcontainer.override_command {
            let (entrypoint, command) = devcontainer_keepalive_command();
            let mut wrapped_command = vec![entrypoint.join(" ")];
            wrapped_command.extend(command);
            wrapped_command
        } else {
            let image_startup = image_startup_command(client, &plan.image).await?;
            let startup = crate::up::metadata::effective_startup_command(
                image_startup,
                compose_primary_service,
            );
            let mut wrapped_command = startup.entrypoint;
            wrapped_command.extend(startup.command);
            wrapped_command
        };
        return Ok(Some(ComposeOverrideStartup {
            entrypoint: vec![FEATURE_ENTRYPOINT_WRAPPER.to_owned()],
            command,
        }));
    }

    if plan.config.devcontainer.override_command {
        let (entrypoint, command) = devcontainer_keepalive_command();
        return Ok(Some(ComposeOverrideStartup {
            entrypoint,
            command,
        }));
    }

    Ok(None)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposeOverrideStartup {
    entrypoint: Vec<String>,
    command: Vec<String>,
}

#[cfg(test)]
fn generated_compose_override_content_with_startup(
    primary_service: &str,
    plan: &UpPlan,
    startup: Option<ComposeOverrideStartup>,
    service_forward: &[ServiceForwardRuntime],
) -> Result<String> {
    generated_compose_override_patch(primary_service, plan, startup, service_forward)?.to_yaml()
}

fn generated_compose_override_patch(
    primary_service: &str,
    plan: &UpPlan,
    startup: Option<ComposeOverrideStartup>,
    service_forward: &[ServiceForwardRuntime],
) -> Result<ComposeOverridePatch> {
    let mut service = ComposeOverrideServicePatch::new(primary_service)
        .image(&plan.image)
        .labels(&plan.resources.labels)
        .cap_add(&plan.config.devcontainer.cap_add)
        .security_opt(&plan.config.devcontainer.security_opt)
        .mounts(&plan.mounts);
    let mut used_placeholders = BTreeSet::new();
    for (key, value) in &plan.config.devcontainer.container_env {
        if let Some(sensitive) = plan.sensitive_container_env.get(key) {
            let placeholder = compose_container_env_placeholder(key, &mut used_placeholders);
            service =
                service.interpolated_environment(key, placeholder, sensitive.redactions.clone());
        } else {
            service = service.environment(key, value);
        }
    }
    if plan.image != plan.base_image {
        service = service.pull_policy_never();
    }
    if let Some(user) = compose_override_user(plan)? {
        service = service.user(user);
    }
    if plan.config.devcontainer.init {
        service = service.init(true);
    }
    if plan.config.devcontainer.privileged {
        service = service.privileged(true);
    }
    if let Some(startup) = startup {
        service = service
            .entrypoint(startup.entrypoint)
            .command(startup.command);
    }
    let mut patch = ComposeOverridePatch::new(service);
    for runtime in service_forward {
        patch = patch.service(
            ComposeOverrideServicePatch::new(runtime.service())
                .labels(&compose_service_forward_labels(&plan.resources.labels))
                .mount(runtime.mount().clone().into()),
        );
    }
    Ok(patch)
}

fn attach_compose_interpolation_env_to_plan(plan: &mut UpPlan) {
    let (env, redactions) = compose_interpolation_env(&plan.sensitive_container_env);
    plan.compose_interpolation_env = env.clone();
    plan.compose_interpolation_redactions = redactions.clone();
    if let Some(project) = plan.compose_project.take() {
        plan.compose_project = Some(project.with_generated_override_env(env, redactions));
    }
}

fn compose_interpolation_env(
    sensitive_env: &crate::config::variables::SensitiveEnvMap,
) -> (BTreeMap<String, String>, Vec<String>) {
    let mut env = BTreeMap::new();
    let mut redactions = Vec::new();
    let mut used_placeholders = BTreeSet::new();
    for (key, value) in sensitive_env.iter() {
        let placeholder = compose_container_env_placeholder(key, &mut used_placeholders);
        env.insert(placeholder, value.value.clone());
        redactions.extend(value.redactions.clone());
    }

    (env, redactions)
}

fn compose_container_env_placeholder(key: &str, used: &mut BTreeSet<String>) -> String {
    let mut safe = String::new();
    for ch in key.chars() {
        if ch.is_ascii_alphanumeric() {
            safe.push(ch.to_ascii_uppercase());
        } else {
            safe.push('_');
        }
    }
    if safe.is_empty() || safe.as_bytes()[0].is_ascii_digit() {
        safe.insert(0, '_');
    }

    let base = format!("DECUNE_CONTAINER_ENV_{safe}");
    if used.insert(base.clone()) {
        return base;
    }
    for index in 2.. {
        let candidate = format!("{base}_{index}");
        if used.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("placeholder collision loop always returns");
}

fn compose_service_forward_labels(labels: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut service_labels = BTreeMap::from([("decune.managed".to_owned(), "true".to_owned())]);
    if let Some(workspace_id) = labels.get("decune.workspace_id") {
        service_labels.insert("decune.workspace_id".to_owned(), workspace_id.clone());
    }
    service_labels
}

fn compose_override_user(plan: &UpPlan) -> Result<Option<String>> {
    if plan.config.devcontainer.container_user.is_some() {
        return uid_gid_sync_runtime_user(
            &plan.effective_users.container_user.user,
            &plan.uid_gid_sync_plan,
        )
        .map(Some);
    }

    if !matches!(
        plan.effective_users.container_user.source,
        crate::docker::user::RemoteUserSource::ComposeService
    ) {
        return Ok(None);
    }

    let runtime_user = uid_gid_sync_runtime_user(
        &plan.effective_users.container_user.user,
        &plan.uid_gid_sync_plan,
    )?;
    if runtime_user == plan.effective_users.container_user.user {
        return Ok(None);
    }

    Ok(Some(runtime_user))
}

async fn validate_compose_canonical_model(plan: &UpPlan) -> Result<()> {
    let Some(compose_project) = &plan.compose_project else {
        return Ok(());
    };
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        return Ok(());
    };
    let validation = ComposeServiceValidation {
        primary_service: &compose.service,
        run_services: compose.run_services.as_deref(),
        workspace_folder: &plan.workspace_folder,
        project_name: compose_project.project_name(),
    };

    ComposeIntrospector::default()
        .user_config_model(compose_project, &validation)
        .await?;
    Ok(())
}

async fn start_stopped_existing_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    container_id: String,
    container_name: String,
) -> Result<(UpOutcome, WorkspaceState)> {
    let container = state_container_snapshot(plan, container_id.clone());
    let existing_state = reusable_lifecycle_state(workspace, &container)?;

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

async fn container_tool_platform_for_plan(
    client: &DockerClient,
    plan: &UpPlan,
    existing_container_image: Option<&str>,
) -> Result<ContainerToolPlatform> {
    let image = existing_container_image.unwrap_or(&plan.image);
    image_container_tool_platform(client, image).await
}

fn add_credential_runtime_mounts(
    plan: UpPlan,
    runtime_dir: &Path,
    platform: ContainerToolPlatform,
) -> Result<(UpPlan, CredentialRuntime)> {
    let ssh_agent = prepare_ssh_agent_runtime(&plan.config)?;
    let github_cli = prepare_github_cli_runtime(&plan.config, runtime_dir)?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir, platform)?;
    let service_forward = prepare_service_forward_runtimes(
        &plan.forward_ports,
        primary_compose_service(&plan),
        runtime_dir,
        platform,
    )?;
    add_prepared_credential_runtime_mounts(
        plan,
        runtime_dir,
        github_cli,
        ssh_agent,
        forward,
        service_forward,
        platform,
    )
}

#[cfg(test)]
pub(in crate::up) fn add_credential_runtime_mounts_with_ssh_socket(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let platform = ContainerToolPlatform::LinuxAmd64;
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        None,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir, platform)?;
    let service_forward = prepare_service_forward_runtimes(
        &plan.forward_ports,
        primary_compose_service(&plan),
        runtime_dir,
        platform,
    )?;
    add_prepared_credential_runtime_mounts(
        plan,
        runtime_dir,
        github_cli,
        ssh_agent,
        forward,
        service_forward,
        platform,
    )
}

#[cfg(test)]
pub(in crate::up) fn add_credential_runtime_mounts_with_inputs(
    plan: UpPlan,
    runtime_dir: &Path,
    ssh_auth_sock: Option<&Path>,
    github_token: Option<&str>,
) -> Result<(UpPlan, CredentialRuntime)> {
    let platform = ContainerToolPlatform::LinuxAmd64;
    let ssh_agent = crate::host::credentials::prepare_ssh_agent_runtime_with_socket(
        &plan.config,
        ssh_auth_sock,
    )?;
    let github_cli = crate::host::credentials::prepare_github_cli_runtime_with_token(
        &plan.config,
        runtime_dir,
        github_token,
    )?;
    let forward = prepare_forward_runtime(&plan.forward_ports, runtime_dir, platform)?;
    let service_forward = prepare_service_forward_runtimes(
        &plan.forward_ports,
        primary_compose_service(&plan),
        runtime_dir,
        platform,
    )?;
    add_prepared_credential_runtime_mounts(
        plan,
        runtime_dir,
        github_cli,
        ssh_agent,
        forward,
        service_forward,
        platform,
    )
}

fn add_prepared_credential_runtime_mounts(
    mut plan: UpPlan,
    runtime_dir: &Path,
    github_cli: GithubCliRuntime,
    ssh_agent: SshAgentRuntime,
    forward: ForwardRuntime,
    service_forward: Vec<ServiceForwardRuntime>,
    platform: ContainerToolPlatform,
) -> Result<(UpPlan, CredentialRuntime)> {
    let git_credentials = prepare_git_credential_runtime(&plan.config, runtime_dir, platform)?;
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
        CredentialRuntime::new(
            git_credentials,
            github_cli,
            ssh_agent,
            forward,
            service_forward,
        ),
    ))
}

fn primary_compose_service(plan: &UpPlan) -> Option<&str> {
    match &plan.config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Compose(compose)) => Some(&compose.service),
        _ => None,
    }
}

async fn compose_service_forward_requires_recreate(
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

async fn prepare_image_for_create(
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
            redactions: Vec::new(),
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
    client
        .cli()
        .wait_container(container)
        .await
        .with_context(|| format!("Failed to wait for Docker container: {container}"))
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
    client
        .cli()
        .list_workspace_containers(workspace_id)
        .await
        .with_context(|| format!("Failed to list Docker containers for workspace: {workspace_id}"))
}

async fn list_compose_primary_containers(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
    service: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_compose_service_containers(workspace_id, project_name, service)
        .await
        .with_context(|| {
            format!(
                "Failed to list Docker Compose containers for workspace {workspace_id} service `{service}`"
            )
        })
}

async fn list_compose_forwarding_service_containers(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
    service: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_compose_service_containers_by_project(project_name, service)
        .await
        .with_context(|| {
            format!(
                "Failed to list Docker Compose containers for workspace {workspace_id} service `{service}`"
            )
        })
}

async fn list_compose_project_containers(
    client: &DockerClient,
    workspace_id: &str,
    project_name: &str,
) -> Result<Vec<UpContainerSummary>> {
    client
        .cli()
        .list_compose_project_containers(workspace_id, project_name)
        .await
        .with_context(|| {
            format!("Failed to list Docker Compose containers for workspace {workspace_id} project `{project_name}`")
        })
}

#[cfg(test)]
mod tests {
    use super::{
        ComposeOverrideStartup, ExistingContainerReusePolicy,
        attach_compose_interpolation_env_to_plan,
        compose_service_forward_container_requires_recreate, generated_compose_override_content,
        generated_compose_override_content_with_startup, should_reuse_existing_container,
        state_container_snapshot,
    };
    use crate::{
        config::{ConfigMergeInput, resolved::ResolvedConfig, types::MountType},
        docker::{
            build::DockerBuildOptions,
            mounts::{DockerMountSpec, MountBindOptions, MountBindPropagation, MountVolumeOptions},
            resource::DockerResources,
            user::{
                EffectiveUserResolveInput, EffectiveUsers, HostUserIds, ResolvedUserIds,
                UidGidSyncPlan, UidGidSyncTarget, UidGidSyncTargetKind, resolve_effective_users,
                resolve_effective_users_with_compose_service_user,
            },
        },
        host::forward::ServiceForwardRuntime,
        up::{
            CredentialRuntimeMountPolicy, ExistingContainerDecision, UpContainerSummary,
            UpMountSummary, UpPlan, decide_existing_container,
        },
    };
    use std::collections::BTreeMap;

    #[test]
    fn generated_compose_override_patches_only_primary_service() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.container_env =
            BTreeMap::from([("FROM_DECUNE".to_owned(), "1".to_owned())]);
        config.devcontainer.override_command = false;
        let mut resources = DockerResources {
            container_name: "unused".to_owned(),
            image_tag: "decune/test:hash".to_owned(),
            workspace_volume_name: "unused-volume".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "hash".to_owned(),
        };
        resources
            .labels
            .insert("decune.managed".to_owned(), "true".to_owned());
        let plan = UpPlan {
            image: "decune/test:hash".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources,
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspace".to_owned(),
            mounts: vec![DockerMountSpec {
                source: Some("/host/cache".to_owned()),
                target: "/cache".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            }],
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        };

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("'app':"));
        assert!(content.contains("image: 'decune/test:hash'"));
        assert!(content.contains("pull_policy: 'never'"));
        assert!(content.contains("'FROM_DECUNE': '1'"));
        assert!(content.contains("'decune.managed': 'true'"));
        assert!(content.contains("target: '/cache'"));
        assert!(!content.contains("sidecar"));
    }

    #[test]
    fn generated_compose_override_labels_explicit_sidecar_forwarding_service() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.resources.labels = BTreeMap::from([
            ("decune.managed".to_owned(), "true".to_owned()),
            ("decune.workspace_id".to_owned(), "workspace-id".to_owned()),
            ("decune.config_hash".to_owned(), "hash".to_owned()),
        ]);
        let service_forward = vec![ServiceForwardRuntime::for_test(
            "db",
            DockerMountSpec {
                source: Some("/tmp/decune-runtime/forward/db".to_owned()),
                target: "/run/decune".to_owned(),
                mount_type: MountType::Bind,
                read_only: false,
                consistency: None,
                bind_options: None,
                volume_options: None,
            },
        )];

        let content =
            generated_compose_override_content_with_startup("app", &plan, None, &service_forward)
                .unwrap();

        assert!(content.contains("  'db':\n"));
        assert!(content.contains("'decune.managed': 'true'"));
        assert!(content.contains("'decune.workspace_id': 'workspace-id'"));
        assert!(content.contains("target: '/run/decune'"));
    }

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
    fn generated_compose_override_does_not_override_pull_policy_for_original_image() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.image = "alpine:3.20".to_owned();
        plan.base_image = "alpine:3.20".to_owned();

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("image: 'alpine:3.20'"));
        assert!(!content.contains("pull_policy:"));
    }

    #[test]
    fn state_snapshot_records_final_image_tag_for_compose_plan() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.image = "decune/project-abc123:config-hash".to_owned();
        plan.base_image = "example/app:dev".to_owned();
        plan.resources.config_hash = "config-hash".to_owned();

        let snapshot = state_container_snapshot(&plan, "container-id".to_owned());

        assert_eq!(snapshot.image, "decune/project-abc123:config-hash");
        assert_eq!(snapshot.config_hash, "config-hash");
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
    fn generated_compose_override_writes_synced_container_user() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.container_user = Some("2001:2001".to_owned());
        plan.effective_users = resolve_effective_users(EffectiveUserResolveInput {
            devcontainer_remote_user: None,
            devcontainer_container_user: Some("2001:2001"),
            image_metadata_remote_user: None,
            image_metadata_container_user: None,
            image_config_user: None,
        })
        .unwrap();
        plan.uid_gid_sync_plan = sync_plan();

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("user: 'syncuser:1000'"));
        assert!(!content.contains("user: '2001:2001'"));
    }

    #[test]
    fn generated_compose_override_writes_compose_service_user_only_when_sync_changes_it() {
        let mut unchanged = generated_override_test_plan(Vec::new());
        unchanged.effective_users = resolve_effective_users_with_compose_service_user(
            EffectiveUserResolveInput {
                devcontainer_remote_user: None,
                devcontainer_container_user: None,
                image_metadata_remote_user: None,
                image_metadata_container_user: None,
                image_config_user: None,
            },
            Some("syncuser"),
        )
        .unwrap();
        let unchanged_content = generated_compose_override_content("app", &unchanged).unwrap();
        assert!(!unchanged_content.contains("user:"));

        let mut synced = unchanged;
        synced.effective_users = resolve_effective_users_with_compose_service_user(
            EffectiveUserResolveInput {
                devcontainer_remote_user: None,
                devcontainer_container_user: None,
                image_metadata_remote_user: None,
                image_metadata_container_user: None,
                image_config_user: None,
            },
            Some("2001:2001"),
        )
        .unwrap();
        synced.uid_gid_sync_plan = sync_plan();
        let synced_content = generated_compose_override_content("app", &synced).unwrap();

        assert!(synced_content.contains("user: 'syncuser:1000'"));
    }

    #[test]
    fn generated_compose_override_preserves_bind_mount_options() {
        let plan = generated_override_test_plan(vec![DockerMountSpec {
            source: Some("/host/tools".to_owned()),
            target: "/tools".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: Some("cached".to_owned()),
            bind_options: Some(MountBindOptions {
                propagation: Some(MountBindPropagation::RShared),
                create_mountpoint: Some(true),
                ..MountBindOptions::default()
            }),
            volume_options: None,
        }]);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("consistency: 'cached'"));
        assert!(content.contains("bind:\n"));
        assert!(content.contains("propagation: 'rshared'"));
        assert!(content.contains("create_host_path: true"));
    }

    #[test]
    fn generated_compose_override_disables_default_bind_source_creation() {
        let plan = generated_override_test_plan(vec![DockerMountSpec {
            source: Some("/host/cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Bind,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: None,
        }]);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("bind:\n"));
        assert!(content.contains("create_host_path: false"));
    }

    #[test]
    fn generated_compose_override_preserves_volume_mount_options() {
        let plan = generated_override_test_plan(vec![DockerMountSpec {
            source: Some("project-cache".to_owned()),
            target: "/cache".to_owned(),
            mount_type: MountType::Volume,
            read_only: false,
            consistency: None,
            bind_options: None,
            volume_options: Some(MountVolumeOptions {
                no_copy: Some(true),
                subpath: Some("deps".to_owned()),
                ..MountVolumeOptions::default()
            }),
        }]);

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("volume:\n"));
        assert!(content.contains("nocopy: true"));
        assert!(content.contains("subpath: 'deps'"));
    }

    #[test]
    fn generated_compose_override_redacts_local_env_derived_container_env() {
        let mut plan = generated_override_test_plan(Vec::new());
        plan.config.devcontainer.container_env =
            BTreeMap::from([("NPM_TOKEN".to_owned(), "secret-token".to_owned())]);
        plan.sensitive_container_env.insert(
            "NPM_TOKEN",
            crate::config::variables::SensitiveEnvValue {
                value: "secret-token".to_owned(),
                redactions: vec!["secret-token".to_owned()],
            },
        );

        let content = generated_compose_override_content("app", &plan).unwrap();

        assert!(content.contains("'NPM_TOKEN': '${DECUNE_CONTAINER_ENV_NPM_TOKEN}'"));
        assert!(!content.contains("secret-token"));
    }

    #[test]
    fn compose_interpolation_env_is_attached_to_generated_override_command_plan() {
        let mut plan = generated_override_test_plan(Vec::new());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("workspace");
        std::fs::create_dir(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        std::fs::create_dir(&devcontainer_dir).unwrap();
        std::fs::write(devcontainer_dir.join("compose.yaml"), "services: {}\n").unwrap();
        let workspace = crate::workspace::Workspace::resolve(&root).unwrap();
        plan.compose_project = Some(
            crate::runtime::compose_cli::ComposeProjectPlan::resolve(
                &workspace,
                &devcontainer_dir,
                &["compose.yaml".to_owned()],
            )
            .unwrap(),
        );
        plan.sensitive_container_env.insert(
            "NPM_TOKEN",
            crate::config::variables::SensitiveEnvValue {
                value: "secret-token".to_owned(),
                redactions: vec!["secret-token".to_owned()],
            },
        );
        attach_compose_interpolation_env_to_plan(&mut plan);

        let command = plan
            .compose_project
            .as_ref()
            .unwrap()
            .command_plan_with_generated_override()
            .command(["up", "-d"]);

        assert_eq!(
            command
                .env_value("DECUNE_CONTAINER_ENV_NPM_TOKEN")
                .map(String::as_str),
            Some("secret-token")
        );
        assert!(!command.sanitized_display().contains("secret-token"));
    }

    fn generated_override_test_plan(mounts: Vec<DockerMountSpec>) -> UpPlan {
        let mut config = ResolvedConfig::default();
        config.devcontainer.override_command = false;
        let resources = DockerResources {
            container_name: "unused".to_owned(),
            image_tag: "decune/test:hash".to_owned(),
            workspace_volume_name: "unused-volume".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "hash".to_owned(),
        };

        UpPlan {
            image: "decune/test:hash".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources,
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspace".to_owned(),
            mounts,
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        }
    }

    fn reusable_container(config_hash: &str) -> UpContainerSummary {
        UpContainerSummary {
            id: "container-id".to_owned(),
            name: "project-app-1".to_owned(),
            image_id: Some("sha256:image".to_owned()),
            config_hash: Some(config_hash.to_owned()),
            config_file: None,
            mounts: Some(Vec::new()),
            running: true,
        }
    }

    fn mount_policy(required_mounts: &[UpMountSummary]) -> CredentialRuntimeMountPolicy {
        CredentialRuntimeMountPolicy::new(required_mounts.to_vec())
    }

    fn sync_plan() -> UidGidSyncPlan {
        UidGidSyncPlan::Sync {
            target: UidGidSyncTarget {
                kind: UidGidSyncTargetKind::ContainerUser,
                user: "2001:2001".to_owned(),
                host: HostUserIds {
                    uid: 1000,
                    gid: 1000,
                },
            },
            container: ResolvedUserIds {
                name: "syncuser".to_owned(),
                uid: 2001,
                gid: 2001,
            },
        }
    }

    #[test]
    fn generated_compose_override_uses_feature_entrypoint_wrapper_startup() {
        let mut config = ResolvedConfig::default();
        config.devcontainer.override_command = false;
        config.devcontainer.entrypoints = vec!["touch /tmp/decune-feature-entrypoint".to_owned()];
        let resources = DockerResources {
            container_name: "unused".to_owned(),
            image_tag: "decune/test:hash".to_owned(),
            workspace_volume_name: "unused-volume".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "hash".to_owned(),
        };
        let plan = UpPlan {
            image: "decune/test:hash".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources,
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspace".to_owned(),
            mounts: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        };

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            Some(ComposeOverrideStartup {
                entrypoint: vec![
                    "/usr/local/share/decune/feature-entrypoint-wrapper.sh".to_owned(),
                ],
                command: vec!["/docker-entrypoint.sh".to_owned(), "server".to_owned()],
            }),
            &[],
        )
        .unwrap();

        assert!(content.contains("entrypoint:"));
        assert!(content.contains("'/usr/local/share/decune/feature-entrypoint-wrapper.sh'"));
        assert!(content.contains("command:"));
        assert!(content.contains("'/docker-entrypoint.sh'"));
        assert!(content.contains("'server'"));
    }

    #[test]
    fn generated_compose_override_preserves_multiline_command_values() {
        let config = ResolvedConfig::default();
        let resources = DockerResources {
            container_name: "unused".to_owned(),
            image_tag: "decune/test:hash".to_owned(),
            workspace_volume_name: "unused-volume".to_owned(),
            labels: BTreeMap::new(),
            config_hash: "hash".to_owned(),
        };
        let plan = UpPlan {
            image: "decune/test:hash".to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources,
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspace".to_owned(),
            mounts: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        };

        let content = generated_compose_override_content_with_startup(
            "app",
            &plan,
            Some(ComposeOverrideStartup {
                entrypoint: vec!["/bin/sh".to_owned()],
                command: vec![
                    "-c".to_owned(),
                    "trap 'exit 0' TERM\nwhile sleep 1 & wait $!; do :; done".to_owned(),
                ],
            }),
            &[],
        )
        .unwrap();

        assert!(content.contains("\"trap 'exit 0' TERM\\nwhile sleep 1 & wait $!; do :; done\""));
        assert!(!content.contains("TERM\nwhile"));
    }
}

use anyhow::{Result, bail};

use crate::{
    config::{layer::LayerDevcontainerCompose, resolved::ResolvedDevcontainerSource},
    devcontainer::lifecycle::LifecycleRunPath,
    docker::{
        client::DockerClient,
        dotfiles::materialize_dotfile_skeletons,
        image::{PullPolicy, ensure_image, image_container_tool_platform},
    },
    runtime::{
        compose_cli::{
            ComposeBuildOptions, ComposeCliCapabilities, ComposeConfigOutput, ComposeConfigService,
            ComposeIntrospector, ComposeLifecyclePlan, ComposePrimaryImageResolver,
            ComposeProjectPlan, ComposePullOptions, ComposeServiceValidation, ComposeUpOptions,
            DockerComposeCli,
        },
        compose_ports::{
            ComposePublishedPortPlan, ComposePublishedPortPlanningInput,
            ComposePublishedPortReservation, ComposePublishedPortStartupDiagnostics,
            compose_published_port_plan_has_relocations,
            validate_compose_published_port_diagnostics,
        },
    },
    state,
    up::{
        build::plan_requires_final_image_layer,
        existing::{self, decide_existing_container},
        metadata::{
            ComposePublishedPortFinalization, FinalizeUpPlanMountsOptions, finalize_up_plan_mounts,
            prepare_compose_image_metadata, report_deferred_config_messages,
        },
        types::{
            ExistingContainerDecision, ForwardingResolution, UpContainerSummary, UpOptions,
            UpOutcome, UpPlan, UpPlanResolution,
        },
    },
    workspace::Workspace,
};

use super::{
    CredentialRuntime, ExistingContainerReusePolicy, ImagePreparation, StartedUpContainer,
    add_credential_runtime_mounts, attach_compose_interpolation_env_to_plan,
    compose_service_forward_requires_recreate, container_tool_platform_for_plan,
    ensure_container_running_after_start, list_compose_primary_containers,
    list_compose_project_containers, list_existing_compose_project_published_ports,
    prepare_image_for_create, should_reuse_existing_container, started_up_container_with_state,
    startup_verification_for_plan, sync_started_compose_state,
    warn_on_compose_published_port_relocations, write_generated_compose_override,
};

fn compose_running_reuse_fast_path_enabled(
    options: &UpOptions,
    existing_compose_containers: &[UpContainerSummary],
) -> bool {
    !(options.build.pull
        || options.reuse.rebuild
        || options.build.no_cache
        || options.build.update_features)
        && existing_compose_containers
            .first()
            .is_some_and(|container| container.running)
}

struct ComposeRunningReuseInput<'a> {
    plan: UpPlan,
    options: &'a UpOptions,
    forwarding_resolution: ForwardingResolution,
    existing_compose_containers: &'a [UpContainerSummary],
    compose_primary_image: &'a str,
    user_config: &'a ComposeConfigOutput,
    compose_primary_service_user: Option<&'a str>,
    compose_primary_service: Option<&'a ComposeConfigService>,
    published_port_policy_input: &'a ComposePublishedPortPlanningInput,
    compose_published_ports: Option<ComposePublishedPortFinalization<'a>>,
}

async fn try_reuse_running_compose_container_before_image_prepare(
    client: &DockerClient,
    workspace: &Workspace,
    input: ComposeRunningReuseInput<'_>,
) -> Result<Option<StartedUpContainer>> {
    if !compose_running_reuse_fast_path_enabled(input.options, input.existing_compose_containers) {
        return Ok(None);
    }
    let Some(existing_container_image) = input
        .existing_compose_containers
        .first()
        .and_then(existing::existing_container_image_id)
    else {
        return Ok(None);
    };

    let resolution = compose_fast_path_resolution(&input);
    let mut plan = prepare_compose_image_metadata(
        client,
        workspace,
        input.options.config_path.as_deref(),
        input.options.cli_layer.clone(),
        input.plan,
        input.compose_primary_image,
        resolution,
    )
    .await?;
    plan.base_image = input.compose_primary_image.to_owned();
    let finalized = Box::pin(finalize_up_plan_mounts(
        client,
        workspace,
        plan,
        Some(existing_container_image),
        input
            .existing_compose_containers
            .first()
            .and_then(existing::existing_container_config_hash),
        Some((false, false)),
        FinalizeUpPlanMountsOptions {
            forwarding: input.forwarding_resolution,
            update_features: input.options.build.update_features,
            compose_canonical_model: Some(&input.user_config.canonical_model),
            compose_primary_service_user: input.compose_primary_service_user,
            compose_primary_service: input.compose_primary_service,
            compose_published_ports: input.compose_published_ports,
        },
    ))
    .await?;
    let mut plan = finalized.plan;
    let published_port_plan = finalized.compose_published_port_plan;
    if !plan_requires_final_image_layer(&plan) {
        plan.image = input.compose_primary_image.to_owned();
        plan.base_image = input.compose_primary_image.to_owned();
    }
    let platform =
        container_tool_platform_for_plan(client, &plan, Some(existing_container_image)).await?;
    let (mut plan, credentials) =
        add_credential_runtime_mounts(plan, workspace.paths().runtime_dir(), platform)?;
    attach_compose_interpolation_env_to_plan(&mut plan);
    report_deferred_config_messages(&plan.config);

    let Some(compose_project) = &plan.compose_project else {
        bail!("Docker Compose project plan is missing after finalization");
    };
    let decision = decide_existing_container(
        input.existing_compose_containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        input.options.reuse.rebuild,
    )?;
    let service_forward_requires_recreate = compose_service_forward_requires_recreate(
        client,
        workspace.id(),
        compose_project.project_name(),
        credentials.service_forward(),
    )
    .await?;
    let should_reuse = should_reuse_existing_container(
        &decision,
        ExistingContainerReusePolicy {
            pull: input.options.build.pull,
            service_forward_requires_recreate,
        },
    );

    if let ExistingContainerDecision::ReuseRunning { id, name } = decision
        && should_reuse
    {
        return started_fast_path_reused_compose_container(
            client,
            workspace,
            plan,
            credentials,
            UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            },
            input.published_port_policy_input,
            &published_port_plan,
        )
        .await
        .map(Some);
    }

    Ok(None)
}

const fn compose_fast_path_resolution(input: &ComposeRunningReuseInput<'_>) -> UpPlanResolution {
    UpPlanResolution::new(
        input.forwarding_resolution,
        input.options.build.update_features,
        input.options.config.skip_global_config,
    )
}

async fn started_fast_path_reused_compose_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: UpPlan,
    credentials: CredentialRuntime,
    outcome: UpOutcome,
    published_port_policy_input: &ComposePublishedPortPlanningInput,
    published_port_plan: &ComposePublishedPortPlan,
) -> Result<StartedUpContainer> {
    let state = sync_started_compose_state(
        client,
        workspace,
        &plan,
        &outcome,
        LifecycleRunPath::Running,
        published_port_policy_input,
        published_port_plan,
    )
    .await?;
    Ok(started_up_container_with_state(
        client.clone(),
        workspace.clone(),
        plan,
        outcome,
        LifecycleRunPath::Running,
        credentials,
        state,
    ))
}

struct ComposePlanSource<'a> {
    project: &'a ComposeProjectPlan,
    compose: &'a LayerDevcontainerCompose,
}

struct ComposeStartupContext {
    cli: DockerComposeCli,
    compose_capabilities: ComposeCliCapabilities,
    user_lifecycle: ComposeLifecyclePlan,
    user_config: ComposeConfigOutput,
    compose_primary_image: String,
    primary_service_has_build: bool,
    compose_primary_service_user: Option<String>,
    compose_primary_service: Option<ComposeConfigService>,
    published_port_policy_input: ComposePublishedPortPlanningInput,
    existing_compose_project_containers: Vec<UpContainerSummary>,
    existing_compose_containers: Vec<UpContainerSummary>,
    existing_project_published_ports: Vec<ComposePublishedPortReservation>,
    published_port_relocation_enabled: bool,
}

impl ComposeStartupContext {
    fn compose_published_ports(&self) -> Option<ComposePublishedPortFinalization<'_>> {
        self.published_port_relocation_enabled
            .then_some(ComposePublishedPortFinalization {
                input: &self.published_port_policy_input,
                existing_project_published_ports: &self.existing_project_published_ports,
            })
    }
}

struct FinalizedComposeStart {
    plan: UpPlan,
    credentials: CredentialRuntime,
    published_port_plan: ComposePublishedPortPlan,
    runtime_lifecycle: ComposeLifecyclePlan,
    service: String,
}

struct ComposeReusableStartInput<'a> {
    client: &'a DockerClient,
    workspace: &'a Workspace,
    plan: &'a UpPlan,
    context: &'a ComposeStartupContext,
    runtime_lifecycle: &'a ComposeLifecyclePlan,
    decision: &'a ExistingContainerDecision,
    should_reuse: bool,
    published_port_plan: &'a ComposePublishedPortPlan,
}

struct ComposeStartRunOptions {
    decision: ExistingContainerDecision,
    should_reuse: bool,
    force_recreate: bool,
    remove_orphans: bool,
}

pub(super) async fn start_compose_project(
    workspace: Workspace,
    mut plan: UpPlan,
    options: UpOptions,
    forwarding_resolution: ForwardingResolution,
) -> Result<StartedUpContainer> {
    let client = DockerClient::connect_from_env();
    let context = prepare_compose_startup_context(&client, &workspace, &mut plan).await?;

    if let Some(started) = Box::pin(try_reuse_running_compose_container_before_image_prepare(
        &client,
        &workspace,
        ComposeRunningReuseInput {
            plan: plan.clone(),
            options: &options,
            forwarding_resolution,
            existing_compose_containers: &context.existing_compose_containers,
            compose_primary_image: &context.compose_primary_image,
            user_config: &context.user_config,
            compose_primary_service_user: context.compose_primary_service_user.as_deref(),
            compose_primary_service: context.compose_primary_service.as_ref(),
            published_port_policy_input: &context.published_port_policy_input,
            compose_published_ports: context.compose_published_ports(),
        },
    ))
    .await?
    {
        return Ok(started);
    }

    prepare_compose_user_images(&context, &options).await?;
    let finalized = Box::pin(finalize_compose_start_plan(
        &client,
        &workspace,
        plan,
        &options,
        forwarding_resolution,
        &context,
    ))
    .await?;

    Box::pin(start_finalized_compose_project(
        client, workspace, options, context, finalized,
    ))
    .await
}

fn compose_plan_source<'a>(
    plan: &'a UpPlan,
    project_missing: &'static str,
    source_missing: &'static str,
) -> Result<ComposePlanSource<'a>> {
    let Some(project) = &plan.compose_project else {
        bail!("{project_missing}");
    };
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        bail!("{source_missing}");
    };

    Ok(ComposePlanSource { project, compose })
}

async fn prepare_compose_startup_context(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &mut UpPlan,
) -> Result<ComposeStartupContext> {
    let source = compose_plan_source(
        plan,
        "Docker Compose project plan is missing",
        "Docker Compose devcontainer source is missing",
    )?;
    let project_name = source.project.project_name().to_owned();
    let service = source.compose.service.clone();
    let run_services = source.compose.run_services.clone();
    let user_lifecycle = ComposeLifecyclePlan::up(
        source.project.command_plan_without_generated_override(),
        &service,
        run_services.as_deref(),
    );
    let cli = DockerComposeCli::default();
    let compose_capabilities = cli.ensure_required_capabilities().await?;
    let compose_service_validation = ComposeServiceValidation {
        primary_service: &service,
        run_services: run_services.as_deref(),
        workspace_folder: &plan.workspace_folder,
        project_name: &project_name,
    };
    let compose_introspector = ComposeIntrospector::new(cli.clone());
    let user_config = compose_introspector
        .user_config(source.project, &compose_service_validation)
        .await?;
    let user_model = &user_config.model;
    let compose_primary_image = ComposePrimaryImageResolver {
        project_name: &project_name,
        service: &service,
    }
    .resolve(user_model)?;
    let primary_service_has_build = compose_primary_image.has_build;
    let compose_primary_image = compose_primary_image.base_image;
    let compose_primary_service = user_model.service(&service).cloned();
    let compose_primary_service_user = compose_primary_service
        .as_ref()
        .and_then(|service| service.user.clone());
    let published_port_policy_input = compose_introspector
        .user_published_port_planning_input(
            source.project,
            &compose_service_validation,
            &user_lifecycle.services,
        )
        .await?;
    validate_compose_published_port_diagnostics(&published_port_policy_input)?;
    compose_primary_image.clone_into(&mut plan.base_image);

    let existing_compose_project_containers =
        list_compose_project_containers(client, workspace.id(), &project_name).await?;
    let existing_compose_containers =
        list_compose_primary_containers(client, workspace.id(), &project_name, &service).await?;
    let published_port_relocation_enabled = plan.config.compose.published_ports.relocation;
    let existing_project_published_ports =
        if published_port_relocation_enabled && !existing_compose_project_containers.is_empty() {
            list_existing_compose_project_published_ports(client, &project_name).await?
        } else {
            Vec::new()
        };

    Ok(ComposeStartupContext {
        cli,
        compose_capabilities,
        user_lifecycle,
        user_config,
        compose_primary_image,
        primary_service_has_build,
        compose_primary_service_user,
        compose_primary_service,
        published_port_policy_input,
        existing_compose_project_containers,
        existing_compose_containers,
        existing_project_published_ports,
        published_port_relocation_enabled,
    })
}

async fn prepare_compose_user_images(
    context: &ComposeStartupContext,
    options: &UpOptions,
) -> Result<()> {
    if options.build.pull {
        context
            .cli
            .pull(
                &context.user_lifecycle.project,
                ComposePullOptions {
                    always: true,
                    ignore_buildable: true,
                    // When runServices narrows the explicit pull targets, dependency images
                    // are still delegated to Docker Compose instead of parsing depends_on.
                    include_deps: true,
                },
                &context.user_lifecycle.services,
            )
            .await?;
    }
    if options.reuse.rebuild
        || options.build.no_cache
        || options.build.pull
        || (context.primary_service_has_build && context.existing_compose_containers.is_empty())
    {
        context
            .cli
            .build(
                &context.user_lifecycle.project,
                ComposeBuildOptions {
                    with_dependencies: true,
                    no_cache: options.build.no_cache,
                    pull: options.build.pull,
                },
                &context.user_lifecycle.services,
            )
            .await?;
    }

    Ok(())
}

async fn finalize_compose_start_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    options: &UpOptions,
    forwarding_resolution: ForwardingResolution,
    context: &ComposeStartupContext,
) -> Result<FinalizedComposeStart> {
    let existing_remote_user_image = if options.reuse.rebuild {
        None
    } else {
        context
            .existing_compose_containers
            .first()
            .and_then(existing::existing_container_image_id)
    };
    if existing_remote_user_image.is_none() && !context.primary_service_has_build {
        ensure_image(client, &plan.base_image, PullPolicy::Missing).await?;
    }
    plan = prepare_compose_image_metadata(
        client,
        workspace,
        options.config_path.as_deref(),
        options.cli_layer.clone(),
        plan,
        &context.compose_primary_image,
        UpPlanResolution::new(
            forwarding_resolution,
            options.build.update_features,
            options.config.skip_global_config,
        ),
    )
    .await?;
    plan.base_image.clone_from(&context.compose_primary_image);
    let finalized = Box::pin(finalize_up_plan_mounts(
        client,
        workspace,
        plan,
        existing_remote_user_image,
        context
            .existing_compose_containers
            .first()
            .and_then(existing::existing_container_config_hash),
        Some((
            options.build.pull && !context.primary_service_has_build,
            options.build.no_cache,
        )),
        FinalizeUpPlanMountsOptions {
            forwarding: forwarding_resolution,
            update_features: options.build.update_features,
            compose_canonical_model: Some(&context.user_config.canonical_model),
            compose_primary_service_user: context.compose_primary_service_user.as_deref(),
            compose_primary_service: context.compose_primary_service.as_ref(),
            compose_published_ports: context.compose_published_ports(),
        },
    ))
    .await?;
    let mut plan = finalized.plan;
    let published_port_plan = finalized.compose_published_port_plan;
    let published_port_override = finalized.compose_published_port_override;
    let image_prepared = finalized.image_prepared;
    handle_compose_published_port_relocations(context, &plan, &published_port_plan)?;
    prepare_finalized_compose_image(client, options, context, &mut plan, image_prepared).await?;
    let platform = image_container_tool_platform(client, &plan.image).await?;
    let (mut plan, credentials) =
        add_credential_runtime_mounts(plan, workspace.paths().runtime_dir(), platform)?;
    attach_compose_interpolation_env_to_plan(&mut plan);
    report_deferred_config_messages(&plan.config);

    let source = compose_plan_source(
        &plan,
        "Docker Compose project plan is missing after finalization",
        "Docker Compose devcontainer source is missing after finalization",
    )?;
    write_generated_compose_override(
        client,
        source.project,
        &source.compose.service,
        &plan,
        context.compose_primary_service.as_ref(),
        credentials.service_forward(),
        &published_port_override,
    )
    .await?;
    let runtime_lifecycle = ComposeLifecyclePlan::up(
        source.project.command_plan_with_generated_override(),
        &source.compose.service,
        source.compose.run_services.as_deref(),
    );
    let service = source.compose.service.clone();

    Ok(FinalizedComposeStart {
        plan,
        credentials,
        published_port_plan,
        runtime_lifecycle,
        service,
    })
}

fn handle_compose_published_port_relocations(
    context: &ComposeStartupContext,
    plan: &UpPlan,
    published_port_plan: &ComposePublishedPortPlan,
) -> Result<()> {
    if compose_published_port_plan_has_relocations(published_port_plan) {
        context.compose_capabilities.ensure_compose_override_tag()?;
        warn_on_compose_published_port_relocations(plan, published_port_plan);
    }
    Ok(())
}

async fn prepare_finalized_compose_image(
    client: &DockerClient,
    options: &UpOptions,
    context: &ComposeStartupContext,
    plan: &mut UpPlan,
    image_prepared: bool,
) -> Result<()> {
    if !plan_requires_final_image_layer(plan) {
        plan.image.clone_from(&context.compose_primary_image);
        plan.base_image.clone_from(&context.compose_primary_image);
    }
    if !image_prepared {
        prepare_image_for_create(
            client,
            plan,
            ImagePreparation {
                pull: false,
                no_cache: options.build.no_cache,
                image_prepared: false,
            },
        )
        .await?;
    }
    Ok(())
}

async fn compose_start_run_options(
    client: &DockerClient,
    workspace: &Workspace,
    options: &UpOptions,
    context: &ComposeStartupContext,
    plan: &UpPlan,
    credentials: &CredentialRuntime,
    runtime_lifecycle: &ComposeLifecyclePlan,
) -> Result<ComposeStartRunOptions> {
    let stale_compose_project = !context.existing_compose_project_containers.is_empty()
        && context.existing_compose_containers.is_empty();
    let decision = decide_existing_container(
        &context.existing_compose_containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        options.reuse.rebuild,
    )?;
    let service_forward_requires_recreate = compose_service_forward_requires_recreate(
        client,
        workspace.id(),
        &runtime_lifecycle.project.project_name,
        credentials.service_forward(),
    )
    .await?;
    let should_reuse = should_reuse_existing_container(
        &decision,
        ExistingContainerReusePolicy {
            pull: options.build.pull,
            service_forward_requires_recreate,
        },
    );
    let force_recreate = matches!(decision, ExistingContainerDecision::Recreate { .. })
        || options.reuse.rebuild
        || options.build.pull
        || stale_compose_project
        || service_forward_requires_recreate;
    let remove_orphans = matches!(decision, ExistingContainerDecision::Recreate { .. })
        || options.reuse.rebuild
        || stale_compose_project
        || service_forward_requires_recreate;
    Ok(ComposeStartRunOptions {
        decision,
        should_reuse,
        force_recreate,
        remove_orphans,
    })
}

async fn start_finalized_compose_project(
    client: DockerClient,
    workspace: Workspace,
    options: UpOptions,
    context: ComposeStartupContext,
    finalized: FinalizedComposeStart,
) -> Result<StartedUpContainer> {
    let FinalizedComposeStart {
        plan,
        credentials,
        published_port_plan,
        runtime_lifecycle,
        service,
    } = finalized;

    if context.existing_compose_project_containers.is_empty() {
        state::reconcile_state_without_container(workspace.paths().state_dir())?;
    }
    let run_options = compose_start_run_options(
        &client,
        &workspace,
        &options,
        &context,
        &plan,
        &credentials,
        &runtime_lifecycle,
    )
    .await?;
    if let Some((outcome, lifecycle_path, state)) =
        try_start_reusable_compose_container(ComposeReusableStartInput {
            client: &client,
            workspace: &workspace,
            plan: &plan,
            context: &context,
            runtime_lifecycle: &runtime_lifecycle,
            decision: &run_options.decision,
            should_reuse: run_options.should_reuse,
            published_port_plan: &published_port_plan,
        })
        .await?
    {
        return Ok(started_up_container_with_state(
            client,
            workspace,
            plan,
            outcome,
            lifecycle_path,
            credentials,
            state,
        ));
    }

    materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
    context
        .cli
        .up(
            &runtime_lifecycle.project,
            ComposeUpOptions {
                force_recreate: run_options.force_recreate,
                remove_orphans: run_options.remove_orphans,
            },
            &runtime_lifecycle.services,
            Some(compose_startup_diagnostics(
                &plan,
                &context.published_port_policy_input,
                &published_port_plan,
            )),
        )
        .await?;

    let container = ComposeIntrospector::new(context.cli)
        .resolve_service_container(&runtime_lifecycle.project, &service)
        .await?;
    let container_name = container.name.unwrap_or_else(|| container.id.clone());
    let outcome = UpOutcome {
        container_id: container.id,
        container_name,
        reused: false,
    };
    ensure_container_running_after_start(
        &client,
        &outcome.container_name,
        startup_verification_for_plan(&plan),
    )
    .await?;
    let state = sync_started_compose_state(
        &client,
        &workspace,
        &plan,
        &outcome,
        LifecycleRunPath::New,
        &context.published_port_policy_input,
        &published_port_plan,
    )
    .await?;

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

async fn try_start_reusable_compose_container(
    input: ComposeReusableStartInput<'_>,
) -> Result<Option<(UpOutcome, LifecycleRunPath, state::WorkspaceState)>> {
    let ComposeReusableStartInput {
        client,
        workspace,
        plan,
        context,
        runtime_lifecycle,
        decision,
        should_reuse,
        published_port_plan,
    } = input;
    if !should_reuse {
        return Ok(None);
    }

    match decision {
        ExistingContainerDecision::ReuseRunning { id, name } => {
            let outcome = UpOutcome {
                container_id: id.clone(),
                container_name: name.clone(),
                reused: true,
            };
            let state = sync_started_compose_state(
                client,
                workspace,
                plan,
                &outcome,
                LifecycleRunPath::Running,
                &context.published_port_policy_input,
                published_port_plan,
            )
            .await?;
            Ok(Some((outcome, LifecycleRunPath::Running, state)))
        }
        ExistingContainerDecision::StartStopped { id, name } => {
            materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
            context
                .cli
                .up(
                    &runtime_lifecycle.project,
                    ComposeUpOptions {
                        force_recreate: false,
                        remove_orphans: false,
                    },
                    &runtime_lifecycle.services,
                    Some(compose_startup_diagnostics(
                        plan,
                        &context.published_port_policy_input,
                        published_port_plan,
                    )),
                )
                .await?;
            ensure_container_running_after_start(client, name, startup_verification_for_plan(plan))
                .await?;
            let outcome = UpOutcome {
                container_id: id.clone(),
                container_name: name.clone(),
                reused: true,
            };
            let state = sync_started_compose_state(
                client,
                workspace,
                plan,
                &outcome,
                LifecycleRunPath::Started,
                &context.published_port_policy_input,
                published_port_plan,
            )
            .await?;
            Ok(Some((outcome, LifecycleRunPath::Started, state)))
        }
        ExistingContainerDecision::Create | ExistingContainerDecision::Recreate { .. } => Ok(None),
    }
}

const fn compose_startup_diagnostics<'a>(
    plan: &'a UpPlan,
    input: &'a ComposePublishedPortPlanningInput,
    published_port_plan: &'a ComposePublishedPortPlan,
) -> ComposePublishedPortStartupDiagnostics<'a> {
    ComposePublishedPortStartupDiagnostics {
        input,
        plan: published_port_plan,
        relocation_enabled: plan.config.compose.published_ports.relocation,
    }
}

pub(super) async fn validate_compose_canonical_model(plan: &UpPlan) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::super::test_support::{reusable_container, up_options_for_fast_path};
    use super::*;

    #[test]
    fn compose_running_reuse_fast_path_only_allows_running_container_without_mutating_flags() {
        let running = reusable_container("stable-hash");
        let mut options = up_options_for_fast_path();

        assert!(compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));

        let stopped = UpContainerSummary {
            running: false,
            ..running.clone()
        };
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            &[stopped],
        ));

        options.build.pull = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
        options = up_options_for_fast_path();
        options.reuse.rebuild = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
        options = up_options_for_fast_path();
        options.build.no_cache = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
        options = up_options_for_fast_path();
        options.build.update_features = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
    }
}

use std::collections::BTreeSet;

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        layer::LayerDevcontainerCompose,
        resolved::{ResolvedComposeCloneIsolation, ResolvedDevcontainerSource},
    },
    devcontainer::lifecycle::LifecycleRunPath,
    docker::{
        client::DockerClient,
        container::ContainerInspect,
        dotfiles::materialize_dotfile_skeletons,
        image::{PullPolicy, ensure_image, image_container_tool_platform},
        ports::{HostPortReservation, host_port_reservations_conflict},
        resource::{COMPOSE_NETWORK_LABEL, compose_project_name_from_labels},
    },
    runtime::{
        compose_cli::{
            ComposeBuildOptions, ComposeCliCapabilities, ComposeConfigModel, ComposeConfigOutput,
            ComposeConfigService, ComposeIntrospector, ComposeLifecyclePlan,
            ComposePrimaryImageResolver, ComposeProjectPlan, ComposePullOptions,
            ComposeServiceValidation, ComposeUpOptions, DockerComposeCli,
        },
        compose_isolation::{
            ComposeIsolationDaemonSnapshot, ComposeIsolationDockerIpamConfig,
            ComposeIsolationDockerNetwork, ComposeIsolationDockerResource,
            ComposeIsolationEndpointDeclaration, ComposeIsolationEndpointPlan,
            ComposeIsolationNameRewritePlan, ComposeIsolationNameRewritePlanInput,
            ComposeIsolationPersistedSubnet, ComposeIsolationPlanInput,
            ComposeIsolationResourceKind, ComposeIsolationScan, ComposeIsolationSubnetPlan,
            ComposeIsolationSubnetPlanInput, apply_compose_isolation_name_rewrites,
            apply_compose_isolation_subnet_plan, plan_compose_isolation,
            plan_compose_isolation_endpoints, plan_compose_isolation_name_rewrites,
            plan_compose_isolation_subnets, scan_compose_isolation,
            validate_compose_isolation_diagnostics,
        },
        compose_ports::{
            ComposePortProtocol, ComposePublishedPortEndpoint, ComposePublishedPortHostIp,
            ComposePublishedPortOverride, ComposePublishedPortPlan, ComposePublishedPortPlanEntry,
            ComposePublishedPortPlanningInput, ComposePublishedPortReservation,
            ComposePublishedPortStartupDiagnostics, compose_port_protocol_name,
            compose_published_port_endpoint_display, compose_published_port_plan_has_relocations,
            compose_published_port_planning_input, validate_compose_published_port_diagnostics,
        },
        docker_cli::{DockerNetworkInspect, DockerSwarmResourceInspect, DockerVolumeInspect},
    },
    state,
    text::non_empty_trimmed,
    ui,
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
    ComposeGeneratedOverrideRuntime, ComposeStateSyncInput, CredentialRuntime,
    ExistingContainerReusePolicy, ImagePreparation, StartedUpContainer,
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
    subnet_plan: &ComposeIsolationSubnetPlan,
) -> bool {
    !(options.build.pull
        || options.reuse.rebuild
        || options.build.no_cache
        || options.build.update_features)
        && subnet_plan.networks_to_remove.is_empty()
        && existing_compose_containers
            .first()
            .is_some_and(|container| container.running)
}

struct ComposeRunningReuseInput<'a> {
    plan: UpPlan,
    options: &'a UpOptions,
    forwarding_resolution: ForwardingResolution,
    existing_compose_project_containers: &'a [UpContainerSummary],
    existing_compose_containers: &'a [UpContainerSummary],
    existing_project_published_ports: &'a [ComposePublishedPortReservation],
    compose_primary_image: &'a str,
    user_config: &'a ComposeConfigOutput,
    compose_primary_service_user: Option<&'a str>,
    compose_primary_service: Option<&'a ComposeConfigService>,
    published_port_policy_input: &'a ComposePublishedPortPlanningInput,
    compose_published_ports: Option<ComposePublishedPortFinalization<'a>>,
    subnet_plan: &'a ComposeIsolationSubnetPlan,
}

async fn try_reuse_running_compose_container_before_image_prepare(
    client: &DockerClient,
    workspace: &Workspace,
    input: ComposeRunningReuseInput<'_>,
) -> Result<Option<StartedUpContainer>> {
    if !compose_running_reuse_fast_path_enabled(
        input.options,
        input.existing_compose_containers,
        input.subnet_plan,
    ) {
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
        input.plan.clone(),
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

    let (decision, should_reuse) = fast_path_existing_container_decision(
        client,
        workspace,
        &input,
        &plan,
        &credentials,
        &published_port_plan,
    )
    .await?;

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
            ComposeStateSyncInput {
                port_input: input.published_port_policy_input,
                port_plan: &published_port_plan,
                subnet_plan: input.subnet_plan,
            },
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

async fn fast_path_existing_container_decision(
    client: &DockerClient,
    workspace: &Workspace,
    input: &ComposeRunningReuseInput<'_>,
    plan: &UpPlan,
    credentials: &CredentialRuntime,
    published_port_plan: &ComposePublishedPortPlan,
) -> Result<(ExistingContainerDecision, bool)> {
    let Some(compose_project) = &plan.compose_project else {
        bail!("Docker Compose project plan is missing after finalization");
    };
    let decision = decide_existing_compose_container(&ComposeExistingContainerDecisionInput {
        containers: input.existing_compose_containers,
        project_containers: input.existing_compose_project_containers,
        expected_config_hash: &plan.resources.config_hash,
        mount_policy: credentials.mount_policy(),
        rebuild: input.options.reuse.rebuild,
        existing_project_published_ports: input.existing_project_published_ports,
        published_port_plan,
        warning: ComposePublishedPortRecreateWarning::Suppress,
    })?;
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
    Ok((decision, should_reuse))
}

async fn started_fast_path_reused_compose_container(
    client: &DockerClient,
    workspace: &Workspace,
    plan: UpPlan,
    credentials: CredentialRuntime,
    outcome: UpOutcome,
    runtime: ComposeStateSyncInput<'_>,
) -> Result<StartedUpContainer> {
    let state = sync_started_compose_state(
        client,
        workspace,
        &plan,
        &outcome,
        LifecycleRunPath::Running,
        runtime,
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
    name_rewrite_plan: ComposeIsolationNameRewritePlan,
    subnet_plan: ComposeIsolationSubnetPlan,
    endpoint_plan: ComposeIsolationEndpointPlan,
}

impl ComposeStartupContext {
    fn compose_published_ports(
        &self,
        preserve_existing_bindings: bool,
    ) -> Option<ComposePublishedPortFinalization<'_>> {
        let existing_project_published_ports = if preserve_existing_bindings {
            self.existing_project_published_ports.as_slice()
        } else {
            &[]
        };
        self.published_port_relocation_enabled
            .then_some(ComposePublishedPortFinalization {
                input: &self.published_port_policy_input,
                existing_project_published_ports,
            })
    }
}

struct FinalizedComposeStart {
    plan: UpPlan,
    credentials: CredentialRuntime,
    published_port_plan: ComposePublishedPortPlan,
    published_port_override: ComposePublishedPortOverride,
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

struct ComposeExistingContainerDecisionInput<'a> {
    containers: &'a [UpContainerSummary],
    project_containers: &'a [UpContainerSummary],
    expected_config_hash: &'a str,
    mount_policy: &'a existing::CredentialRuntimeMountPolicy,
    rebuild: bool,
    existing_project_published_ports: &'a [ComposePublishedPortReservation],
    published_port_plan: &'a ComposePublishedPortPlan,
    warning: ComposePublishedPortRecreateWarning,
}

struct ComposeStartRunOptionsInput<'a> {
    client: &'a DockerClient,
    workspace: &'a Workspace,
    options: &'a UpOptions,
    context: &'a ComposeStartupContext,
    plan: &'a UpPlan,
    credentials: &'a CredentialRuntime,
    runtime_lifecycle: &'a ComposeLifecyclePlan,
    published_port_plan: &'a ComposePublishedPortPlan,
}

struct ComposeNewStartInput<'a> {
    client: &'a DockerClient,
    workspace: &'a Workspace,
    plan: &'a UpPlan,
    context: &'a ComposeStartupContext,
    runtime_lifecycle: &'a ComposeLifecyclePlan,
    run_options: &'a ComposeStartRunOptions,
    service: &'a str,
    published_port_plan: &'a ComposePublishedPortPlan,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposePublishedPortRecreateWarning {
    Emit,
    Suppress,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ComposePublishedPortRecreateChange {
    service: String,
    target_port: u16,
    protocol: ComposePortProtocol,
    existing: ComposePublishedPortEndpoint,
    planned: ComposePublishedPortEndpoint,
}

fn decide_existing_compose_container(
    input: &ComposeExistingContainerDecisionInput<'_>,
) -> Result<ExistingContainerDecision> {
    let decision = decide_existing_container(
        input.containers,
        input.expected_config_hash,
        input.mount_policy,
        input.rebuild,
    )?;
    if !matches!(
        decision,
        ExistingContainerDecision::ReuseRunning { .. }
            | ExistingContainerDecision::StartStopped { .. }
    ) {
        return Ok(decision);
    }

    let changes = compose_published_port_recreate_changes(
        input.existing_project_published_ports,
        input.published_port_plan,
    );
    if changes.is_empty() {
        return Ok(decision);
    }
    if input.warning == ComposePublishedPortRecreateWarning::Emit {
        warn_on_compose_published_port_recreate(&changes);
    }
    let containers = if input.project_containers.is_empty() {
        input.containers.to_vec()
    } else {
        input.project_containers.to_vec()
    };
    Ok(ExistingContainerDecision::Recreate { containers })
}

fn compose_published_port_recreate_changes(
    existing_project_published_ports: &[ComposePublishedPortReservation],
    published_port_plan: &ComposePublishedPortPlan,
) -> Vec<ComposePublishedPortRecreateChange> {
    published_port_plan
        .entries
        .iter()
        .filter_map(|entry| {
            compose_published_port_recreate_change(existing_project_published_ports, entry)
        })
        .collect()
}

fn compose_published_port_recreate_change(
    existing_project_published_ports: &[ComposePublishedPortReservation],
    entry: &ComposePublishedPortPlanEntry,
) -> Option<ComposePublishedPortRecreateChange> {
    let existing = existing_project_published_ports
        .iter()
        .filter(|existing| compose_published_port_reservation_matches_entry(existing, entry))
        .collect::<Vec<_>>();
    if existing.is_empty()
        || existing
            .iter()
            .any(|existing| existing.endpoint.host_port == entry.planned.host_port)
    {
        return None;
    }
    let existing = existing.first()?;
    Some(ComposePublishedPortRecreateChange {
        service: entry.service.clone(),
        target_port: entry.target_port,
        protocol: entry.protocol.clone(),
        existing: existing.endpoint.clone(),
        planned: entry.planned.clone(),
    })
}

fn compose_published_port_reservation_matches_entry(
    reservation: &ComposePublishedPortReservation,
    entry: &ComposePublishedPortPlanEntry,
) -> bool {
    reservation.service == entry.service
        && reservation.target_port == entry.target_port
        && reservation.protocol == entry.protocol
        && compose_published_port_host_ips_conflict(&reservation.endpoint, &entry.planned)
}

fn compose_published_port_host_ips_conflict(
    existing: &ComposePublishedPortEndpoint,
    planned: &ComposePublishedPortEndpoint,
) -> bool {
    const HOST_PORT_FOR_HOST_IP_MATCH: u16 = 1;
    host_port_reservations_conflict(
        [HostPortReservation {
            host_ip: compose_published_port_reservation_host_ip(existing).to_owned(),
            host: HOST_PORT_FOR_HOST_IP_MATCH,
        }]
        .iter(),
        compose_published_port_reservation_host_ip(planned),
        HOST_PORT_FOR_HOST_IP_MATCH,
    )
}

fn compose_published_port_reservation_host_ip(endpoint: &ComposePublishedPortEndpoint) -> &str {
    match &endpoint.host_ip {
        ComposePublishedPortHostIp::Omitted => "0.0.0.0",
        ComposePublishedPortHostIp::Explicit(value) => value,
    }
}

fn warn_on_compose_published_port_recreate(changes: &[ComposePublishedPortRecreateChange]) {
    for change in changes {
        ui::warn(&format!(
            "Compose published port relocation will recreate the Compose project because service `{}` target {}/{} must move from {} to {}",
            change.service,
            change.target_port,
            compose_port_protocol_name(&change.protocol),
            compose_published_port_endpoint_display(&change.existing),
            compose_published_port_endpoint_display(&change.planned),
        ));
    }
}

pub(super) async fn start_compose_project(
    workspace: Workspace,
    mut plan: UpPlan,
    options: UpOptions,
    forwarding_resolution: ForwardingResolution,
) -> Result<StartedUpContainer> {
    let client = DockerClient::connect_from_env();
    let context = prepare_compose_startup_context(&client, &workspace, &mut plan, &options).await?;

    if let Some(started) = Box::pin(try_reuse_running_compose_container_before_image_prepare(
        &client,
        &workspace,
        ComposeRunningReuseInput {
            plan: plan.clone(),
            options: &options,
            forwarding_resolution,
            existing_compose_project_containers: &context.existing_compose_project_containers,
            existing_compose_containers: &context.existing_compose_containers,
            existing_project_published_ports: &context.existing_project_published_ports,
            compose_primary_image: &context.compose_primary_image,
            user_config: &context.user_config,
            compose_primary_service_user: context.compose_primary_service_user.as_deref(),
            compose_primary_service: context.compose_primary_service.as_ref(),
            published_port_policy_input: &context.published_port_policy_input,
            compose_published_ports: context.compose_published_ports(true),
            subnet_plan: &context.subnet_plan,
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
    options: &UpOptions,
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
    let active_user_config = if user_lifecycle.services.is_empty() {
        None
    } else {
        Some(
            compose_introspector
                .user_config_for_services(
                    source.project,
                    &compose_service_validation,
                    &user_lifecycle.services,
                )
                .await?,
        )
    };
    let active_user_config = active_user_config.as_ref().unwrap_or(&user_config);
    let (name_rewrite_plan, subnet_plan, endpoint_plan) = run_compose_isolation_preflight(
        client,
        workspace,
        &project_name,
        &active_user_config.model,
        &plan.config.compose.clone_isolation,
        &compose_capabilities,
        options.reuse.rebuild,
    )
    .await?;
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
    let published_port_policy_input = compose_published_port_planning_input(
        &active_user_config.model,
        &active_user_config.published_port_entries,
        &service,
        &user_lifecycle.services,
    );
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
        name_rewrite_plan,
        subnet_plan,
        endpoint_plan,
    })
}

async fn run_compose_isolation_preflight(
    client: &DockerClient,
    workspace: &Workspace,
    project_name: &str,
    model: &ComposeConfigModel,
    clone_isolation: &ResolvedComposeCloneIsolation,
    compose_capabilities: &ComposeCliCapabilities,
    rebuild: bool,
) -> Result<(
    ComposeIsolationNameRewritePlan,
    ComposeIsolationSubnetPlan,
    ComposeIsolationEndpointPlan,
)> {
    let scan = scan_compose_isolation(model, project_name);
    if scan.is_empty() && (!clone_isolation.enabled || clone_isolation.endpoints.is_empty()) {
        return Ok((
            ComposeIsolationNameRewritePlan::default(),
            ComposeIsolationSubnetPlan::default(),
            ComposeIsolationEndpointPlan::default(),
        ));
    }

    let name_rewrite_plan =
        plan_compose_isolation_name_rewrites(&ComposeIsolationNameRewritePlanInput {
            model,
            scan: &scan,
            workspace_id: workspace.id(),
            enabled: clone_isolation.enabled,
            rewrite_container_names: clone_isolation.names.rewrite_container_names,
            rewrite_resource_names: clone_isolation.names.rewrite_resource_names,
        });
    let name_effective_scan = apply_compose_isolation_name_rewrites(&scan, &name_rewrite_plan);

    let daemon = compose_isolation_daemon_snapshot(client, &name_effective_scan).await?;
    let persisted_subnets = state::load_state_file(workspace.paths().state_dir())?
        .map(|state| {
            state
                .clone_isolation
                .networks
                .into_iter()
                .map(|network| ComposeIsolationPersistedSubnet {
                    network: network.network,
                    requested_subnet: network.requested_subnet,
                    planned_subnet: network.planned_subnet,
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut subnet_plan = plan_compose_isolation_subnets(&ComposeIsolationSubnetPlanInput {
        model,
        project_name,
        workspace_id: workspace.id(),
        scan: &name_effective_scan,
        daemon: &daemon,
        state: &persisted_subnets,
        enabled: clone_isolation.enabled,
        relocation: clone_isolation.networks.relocation,
        subnet_pool: clone_isolation.networks.subnet_pool.as_deref(),
        subnet_prefix: clone_isolation.networks.subnet_prefix,
        rebuild,
    })?;
    if !subnet_plan.allocations.is_empty() {
        compose_capabilities.ensure_compose_override_tag()?;
    }
    let endpoint_declarations = if clone_isolation.enabled {
        clone_isolation
            .endpoints
            .iter()
            .map(|endpoint| ComposeIsolationEndpointDeclaration {
                service: endpoint.service.clone(),
                env: endpoint.env.clone(),
                value: endpoint.value.clone(),
            })
            .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    let (endpoint_plan, endpoint_findings) = plan_compose_isolation_endpoints(
        model,
        &name_effective_scan,
        &endpoint_declarations,
        &mut subnet_plan,
    )?;
    let effective_scan = apply_compose_isolation_subnet_plan(&name_effective_scan, &subnet_plan);
    let mut findings = plan_compose_isolation(&ComposeIsolationPlanInput {
        project_name,
        scan: &effective_scan,
        daemon: &daemon,
    });
    findings.extend(endpoint_findings);
    validate_compose_isolation_diagnostics(&findings)?;
    Ok((name_rewrite_plan, subnet_plan, endpoint_plan))
}

async fn compose_isolation_daemon_snapshot(
    client: &DockerClient,
    scan: &ComposeIsolationScan,
) -> Result<ComposeIsolationDaemonSnapshot> {
    let mut snapshot = ComposeIsolationDaemonSnapshot::default();

    if !scan.networks.is_empty()
        || scan.has_fixed_names_of_kind(ComposeIsolationResourceKind::Network)
    {
        for network in client.cli().list_network_inspects().await? {
            add_compose_isolation_network(&mut snapshot, network);
        }
    }
    for name in fixed_resource_names(scan, ComposeIsolationResourceKind::ServiceContainer) {
        if let Some(container) = client.cli().inspect_container_if_present(&name).await? {
            add_compose_isolation_container(&mut snapshot, &name, &container);
        }
    }
    for name in fixed_resource_names(scan, ComposeIsolationResourceKind::Volume) {
        if let Some(volume) = client.cli().inspect_volume_if_present(&name).await? {
            add_compose_isolation_volume(&mut snapshot, &name, &volume);
        }
    }
    for name in fixed_resource_names(scan, ComposeIsolationResourceKind::Config) {
        if let Some(config) = client.cli().inspect_config_if_present(&name).await? {
            add_compose_isolation_swarm_resource(
                &mut snapshot,
                ComposeIsolationResourceKind::Config,
                &name,
                &config,
            );
        }
    }
    for name in fixed_resource_names(scan, ComposeIsolationResourceKind::Secret) {
        if let Some(secret) = client.cli().inspect_secret_if_present(&name).await? {
            add_compose_isolation_swarm_resource(
                &mut snapshot,
                ComposeIsolationResourceKind::Secret,
                &name,
                &secret,
            );
        }
    }

    Ok(snapshot)
}

fn fixed_resource_names(
    scan: &ComposeIsolationScan,
    kind: ComposeIsolationResourceKind,
) -> Vec<String> {
    scan.fixed_names
        .iter()
        .filter(|fixed| fixed.kind == kind)
        .map(|fixed| fixed.name.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn add_compose_isolation_network(
    snapshot: &mut ComposeIsolationDaemonSnapshot,
    network: DockerNetworkInspect,
) {
    let Some(name) = network
        .name
        .as_deref()
        .and_then(non_empty_trimmed)
        .map(str::to_owned)
    else {
        return;
    };
    let compose_project = network
        .labels
        .as_ref()
        .and_then(compose_project_name_from_labels);
    let compose_network = network.labels.as_ref().and_then(|labels| {
        labels
            .get(COMPOSE_NETWORK_LABEL)
            .and_then(|network| non_empty_trimmed(network))
            .map(str::to_owned)
    });
    let has_attached_containers = !network.containers.is_empty();
    let ipam_configs = network
        .ipam
        .as_ref()
        .map(|ipam| {
            ipam.config
                .iter()
                .map(|config| ComposeIsolationDockerIpamConfig {
                    subnet: config.subnet.clone(),
                    gateway: config.gateway.clone(),
                })
                .collect()
        })
        .unwrap_or_default();
    snapshot.networks.push(ComposeIsolationDockerNetwork {
        name: name.clone(),
        compose_project: compose_project.clone(),
        compose_network,
        scope: network.scope,
        ipam_driver: network.ipam.and_then(|ipam| ipam.driver),
        ipam_configs,
        has_attached_containers,
    });
    snapshot.resources.push(ComposeIsolationDockerResource {
        kind: ComposeIsolationResourceKind::Network,
        name,
        compose_project,
    });
}

fn add_compose_isolation_container(
    snapshot: &mut ComposeIsolationDaemonSnapshot,
    requested_name: &str,
    container: &ContainerInspect,
) {
    let name = container
        .name
        .as_deref()
        .map(|name| name.trim_start_matches('/'))
        .and_then(non_empty_trimmed)
        .unwrap_or(requested_name)
        .to_owned();
    snapshot.resources.push(ComposeIsolationDockerResource {
        kind: ComposeIsolationResourceKind::ServiceContainer,
        name,
        compose_project: container
            .config
            .as_ref()
            .and_then(|config| config.labels.as_ref())
            .and_then(compose_project_name_from_labels),
    });
}

fn add_compose_isolation_volume(
    snapshot: &mut ComposeIsolationDaemonSnapshot,
    requested_name: &str,
    volume: &DockerVolumeInspect,
) {
    let name = volume
        .name
        .as_deref()
        .and_then(non_empty_trimmed)
        .unwrap_or(requested_name)
        .to_owned();
    snapshot.resources.push(ComposeIsolationDockerResource {
        kind: ComposeIsolationResourceKind::Volume,
        name,
        compose_project: volume
            .labels
            .as_ref()
            .and_then(compose_project_name_from_labels),
    });
}

fn add_compose_isolation_swarm_resource(
    snapshot: &mut ComposeIsolationDaemonSnapshot,
    kind: ComposeIsolationResourceKind,
    requested_name: &str,
    resource: &DockerSwarmResourceInspect,
) {
    let name = resource
        .spec
        .as_ref()
        .and_then(|spec| spec.name.as_deref())
        .and_then(non_empty_trimmed)
        .unwrap_or(requested_name)
        .to_owned();
    snapshot.resources.push(ComposeIsolationDockerResource {
        kind,
        name,
        compose_project: resource
            .spec
            .as_ref()
            .and_then(|spec| spec.labels.as_ref())
            .and_then(compose_project_name_from_labels),
    });
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
            compose_published_ports: context.compose_published_ports(!options.reuse.rebuild),
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
        published_port_override,
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
    input: ComposeStartRunOptionsInput<'_>,
) -> Result<ComposeStartRunOptions> {
    let ComposeStartRunOptionsInput {
        client,
        workspace,
        options,
        context,
        plan,
        credentials,
        runtime_lifecycle,
        published_port_plan,
    } = input;
    let stale_compose_project = !context.existing_compose_project_containers.is_empty()
        && context.existing_compose_containers.is_empty();
    let decision = decide_existing_compose_container(&ComposeExistingContainerDecisionInput {
        containers: &context.existing_compose_containers,
        project_containers: &context.existing_compose_project_containers,
        expected_config_hash: &plan.resources.config_hash,
        mount_policy: credentials.mount_policy(),
        rebuild: options.reuse.rebuild,
        existing_project_published_ports: &context.existing_project_published_ports,
        published_port_plan,
        warning: ComposePublishedPortRecreateWarning::Emit,
    })?;
    let service_forward_requires_recreate = compose_service_forward_requires_recreate(
        client,
        workspace.id(),
        &runtime_lifecycle.project.project_name,
        credentials.service_forward(),
    )
    .await?;
    let subnet_requires_recreate = !context.subnet_plan.networks_to_remove.is_empty();
    let should_reuse = !subnet_requires_recreate
        && should_reuse_existing_container(
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
        || service_forward_requires_recreate
        || subnet_requires_recreate;
    let remove_orphans = matches!(decision, ExistingContainerDecision::Recreate { .. })
        || options.reuse.rebuild
        || stale_compose_project
        || service_forward_requires_recreate
        || subnet_requires_recreate;
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
        published_port_override,
        runtime_lifecycle,
        service,
    } = finalized;

    if context.existing_compose_project_containers.is_empty() {
        state::reconcile_state_without_container(workspace.paths().state_dir())?;
    }
    let run_options = compose_start_run_options(ComposeStartRunOptionsInput {
        client: &client,
        workspace: &workspace,
        options: &options,
        context: &context,
        plan: &plan,
        credentials: &credentials,
        runtime_lifecycle: &runtime_lifecycle,
        published_port_plan: &published_port_plan,
    })
    .await?;
    remove_stale_compose_isolation_networks(&client, &context.subnet_plan).await?;
    if compose_start_requires_generated_override(&run_options) {
        write_finalized_generated_compose_override(
            &client,
            &context,
            &plan,
            &credentials,
            &published_port_override,
        )
        .await?;
    }
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

    let (outcome, state) = start_new_compose_container(ComposeNewStartInput {
        client: &client,
        workspace: &workspace,
        plan: &plan,
        context: &context,
        runtime_lifecycle: &runtime_lifecycle,
        run_options: &run_options,
        service: &service,
        published_port_plan: &published_port_plan,
    })
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

async fn remove_stale_compose_isolation_networks(
    client: &DockerClient,
    subnet_plan: &ComposeIsolationSubnetPlan,
) -> Result<()> {
    for network in &subnet_plan.networks_to_remove {
        client
            .cli()
            .remove_network(network)
            .await
            .with_context(|| {
                format!(
                    "Failed to recreate Docker network `{network}` for Compose clone isolation. Run decune down, then decune rebuild."
                )
            })?;
    }
    Ok(())
}

async fn start_new_compose_container(
    input: ComposeNewStartInput<'_>,
) -> Result<(UpOutcome, state::WorkspaceState)> {
    materialize_dotfile_skeletons(&input.plan.dotfile_skeletons)?;
    input
        .context
        .cli
        .up(
            &input.runtime_lifecycle.project,
            ComposeUpOptions {
                force_recreate: input.run_options.force_recreate,
                remove_orphans: input.run_options.remove_orphans,
            },
            &input.runtime_lifecycle.services,
            Some(compose_startup_diagnostics(
                input.plan,
                &input.context.published_port_policy_input,
                input.published_port_plan,
            )),
        )
        .await?;

    let container = ComposeIntrospector::new(input.context.cli.clone())
        .resolve_service_container(&input.runtime_lifecycle.project, input.service)
        .await?;
    let container_name = container.name.unwrap_or_else(|| container.id.clone());
    let outcome = UpOutcome {
        container_id: container.id,
        container_name,
        reused: false,
    };
    ensure_container_running_after_start(
        input.client,
        &outcome.container_name,
        startup_verification_for_plan(input.plan),
    )
    .await?;
    let state = sync_started_compose_state(
        input.client,
        input.workspace,
        input.plan,
        &outcome,
        LifecycleRunPath::New,
        ComposeStateSyncInput {
            port_input: &input.context.published_port_policy_input,
            port_plan: input.published_port_plan,
            subnet_plan: &input.context.subnet_plan,
        },
    )
    .await?;
    Ok((outcome, state))
}

const fn compose_start_requires_generated_override(options: &ComposeStartRunOptions) -> bool {
    !matches!(
        options.decision,
        ExistingContainerDecision::ReuseRunning { .. }
    ) || !options.should_reuse
}

async fn write_finalized_generated_compose_override(
    client: &DockerClient,
    context: &ComposeStartupContext,
    plan: &UpPlan,
    credentials: &CredentialRuntime,
    published_port_override: &ComposePublishedPortOverride,
) -> Result<()> {
    let source = compose_plan_source(
        plan,
        "Docker Compose project plan is missing after finalization",
        "Docker Compose devcontainer source is missing after finalization",
    )?;
    write_generated_compose_override(
        client,
        source.project,
        &source.compose.service,
        plan,
        ComposeGeneratedOverrideRuntime {
            compose_primary_service: context.compose_primary_service.as_ref(),
            service_forward: credentials.service_forward(),
            published_port_override,
            name_rewrite_plan: &context.name_rewrite_plan,
            subnet_plan: &context.subnet_plan,
            endpoint_plan: &context.endpoint_plan,
        },
    )
    .await
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
                ComposeStateSyncInput {
                    port_input: &context.published_port_policy_input,
                    port_plan: published_port_plan,
                    subnet_plan: &context.subnet_plan,
                },
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
                ComposeStateSyncInput {
                    port_input: &context.published_port_policy_input,
                    port_plan: published_port_plan,
                    subnet_plan: &context.subnet_plan,
                },
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
    use super::super::test_support::{mount_policy, reusable_container, up_options_for_fast_path};
    use super::*;
    use crate::runtime::compose_ports::{
        ComposePublishedPortAllocationReason, ComposePublishedPortPlanEntryType,
        ComposePublishedPortPlanSource, ComposePublishedPortPlannedEndpointProbe,
        ComposePublishedPortReservationSource,
    };

    #[test]
    fn compose_running_reuse_fast_path_only_allows_running_container_without_mutating_flags() {
        let running = reusable_container("stable-hash");
        let mut options = up_options_for_fast_path();

        assert!(compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
            &ComposeIsolationSubnetPlan::default(),
        ));

        let stopped = UpContainerSummary {
            running: false,
            ..running.clone()
        };
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            &[stopped],
            &ComposeIsolationSubnetPlan::default(),
        ));

        options.build.pull = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
            &ComposeIsolationSubnetPlan::default(),
        ));
        options = up_options_for_fast_path();
        options.reuse.rebuild = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
            &ComposeIsolationSubnetPlan::default(),
        ));
        options = up_options_for_fast_path();
        options.build.no_cache = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
            &ComposeIsolationSubnetPlan::default(),
        ));
        options = up_options_for_fast_path();
        options.build.update_features = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
            &ComposeIsolationSubnetPlan::default(),
        ));
    }

    #[test]
    fn compose_decision_recreates_when_published_port_plan_changes_existing_binding() {
        let container = reusable_container("stable-hash");
        let existing = vec![reservation(
            18300,
            ComposePublishedPortReservationSource::StoppedContainer,
        )];
        let plan = ComposePublishedPortPlan {
            entries: vec![plan_entry(18300, 18301)],
        };

        let policy = mount_policy(&[]);
        let decision = decide_existing_compose_container(&ComposeExistingContainerDecisionInput {
            containers: std::slice::from_ref(&container),
            project_containers: std::slice::from_ref(&container),
            expected_config_hash: "stable-hash",
            mount_policy: &policy,
            rebuild: false,
            existing_project_published_ports: &existing,
            published_port_plan: &plan,
            warning: ComposePublishedPortRecreateWarning::Suppress,
        })
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::Recreate {
                containers: vec![container],
            }
        );
    }

    #[test]
    fn compose_decision_reuses_when_published_port_plan_matches_existing_binding() {
        let container = reusable_container("stable-hash");
        let existing = vec![reservation(
            18300,
            ComposePublishedPortReservationSource::RunningContainer,
        )];
        let plan = ComposePublishedPortPlan {
            entries: vec![plan_entry(18300, 18300)],
        };

        let policy = mount_policy(&[]);
        let decision = decide_existing_compose_container(&ComposeExistingContainerDecisionInput {
            containers: std::slice::from_ref(&container),
            project_containers: std::slice::from_ref(&container),
            expected_config_hash: "stable-hash",
            mount_policy: &policy,
            rebuild: false,
            existing_project_published_ports: &existing,
            published_port_plan: &plan,
            warning: ComposePublishedPortRecreateWarning::Suppress,
        })
        .unwrap();

        assert_eq!(
            decision,
            ExistingContainerDecision::ReuseRunning {
                id: "container-id".to_owned(),
                name: "project-app-1".to_owned(),
            }
        );
    }

    fn reservation(
        host_port: u16,
        source: ComposePublishedPortReservationSource,
    ) -> ComposePublishedPortReservation {
        ComposePublishedPortReservation {
            service: "app".to_owned(),
            target_port: 80,
            protocol: ComposePortProtocol::Tcp,
            endpoint: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Explicit("0.0.0.0".to_owned()),
                host_port,
            },
            source,
        }
    }

    fn plan_entry(
        requested_host_port: u16,
        planned_host_port: u16,
    ) -> ComposePublishedPortPlanEntry {
        ComposePublishedPortPlanEntry {
            service: "app".to_owned(),
            port_entry_index: 0,
            source: ComposePublishedPortPlanSource::Compose,
            kind: ComposePublishedPortPlanEntryType::Published,
            target_port: 80,
            protocol: ComposePortProtocol::Tcp,
            requested: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Omitted,
                host_port: requested_host_port,
            },
            planned: ComposePublishedPortEndpoint {
                host_ip: ComposePublishedPortHostIp::Omitted,
                host_port: planned_host_port,
            },
            planned_endpoint_probe: ComposePublishedPortPlannedEndpointProbe::Available,
            relocated: requested_host_port != planned_host_port,
            allocation_reason: ComposePublishedPortAllocationReason::Unavailable,
        }
    }
}

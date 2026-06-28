use anyhow::{Result, bail};

use crate::{
    config::resolved::ResolvedDevcontainerSource,
    devcontainer::lifecycle::LifecycleRunPath,
    docker::{
        client::DockerClient,
        dotfiles::materialize_dotfile_skeletons,
        image::{PullPolicy, ensure_image, image_container_tool_platform},
    },
    runtime::{
        compose_cli::{
            ComposeBuildOptions, ComposeConfigOutput, ComposeConfigService, ComposeIntrospector,
            ComposeLifecyclePlan, ComposePrimaryImageResolver, ComposePullOptions,
            ComposeServiceValidation, ComposeUpOptions, DockerComposeCli,
        },
        compose_ports::{
            ComposePublishedPortPlanningInput, ComposePublishedPortStartupDiagnostics,
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
    ExistingContainerReusePolicy, StartedUpContainer, add_credential_runtime_mounts,
    attach_compose_interpolation_env_to_plan, compose_service_forward_requires_recreate,
    container_tool_platform_for_plan, ensure_container_running_after_start,
    list_compose_primary_containers, list_compose_project_containers,
    list_existing_compose_project_published_ports, prepare_image_for_create,
    should_reuse_existing_container, started_up_container_with_state,
    startup_verification_for_plan, sync_started_compose_state,
    warn_on_compose_published_port_relocations, write_generated_compose_override,
};

fn compose_running_reuse_fast_path_enabled(
    options: &UpOptions,
    existing_compose_containers: &[UpContainerSummary],
) -> bool {
    !(options.pull || options.rebuild || options.no_cache || options.update_features)
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
    let ComposeRunningReuseInput {
        mut plan,
        options,
        forwarding_resolution,
        existing_compose_containers,
        compose_primary_image,
        user_config,
        compose_primary_service_user,
        compose_primary_service,
        published_port_policy_input,
        compose_published_ports,
    } = input;
    if !compose_running_reuse_fast_path_enabled(options, existing_compose_containers) {
        return Ok(None);
    }
    let Some(existing_container_image) = existing_compose_containers
        .first()
        .and_then(existing::existing_container_image_id)
    else {
        return Ok(None);
    };

    plan = prepare_compose_image_metadata(
        client,
        workspace,
        options.config_path.as_deref(),
        options.cli_layer.clone(),
        plan,
        compose_primary_image,
        UpPlanResolution::new(
            forwarding_resolution,
            options.update_features,
            options.skip_global_config,
        ),
    )
    .await?;
    plan.base_image = compose_primary_image.to_owned();
    let finalized = finalize_up_plan_mounts(
        client,
        workspace,
        plan,
        Some(existing_container_image),
        existing_compose_containers
            .first()
            .and_then(existing::existing_container_config_hash),
        Some((false, false)),
        FinalizeUpPlanMountsOptions {
            forwarding: forwarding_resolution,
            update_features: options.update_features,
            compose_canonical_model: Some(&user_config.canonical_model),
            compose_primary_service_user,
            compose_primary_service,
            compose_published_ports,
        },
    )
    .await?;
    let mut plan = finalized.plan;
    let published_port_plan = finalized.compose_published_port_plan;
    if !plan_requires_final_image_layer(&plan) {
        plan.image = compose_primary_image.to_owned();
        plan.base_image = compose_primary_image.to_owned();
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
        existing_compose_containers,
        &plan.resources.config_hash,
        credentials.mount_policy(),
        options.rebuild,
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
            pull: options.pull,
            service_forward_requires_recreate,
        },
    );

    if let ExistingContainerDecision::ReuseRunning { id, name } = decision
        && should_reuse
    {
        let outcome = UpOutcome {
            container_id: id,
            container_name: name,
            reused: true,
        };
        let state = sync_started_compose_state(
            client,
            workspace,
            &plan,
            &outcome,
            LifecycleRunPath::Running,
            published_port_policy_input,
            &published_port_plan,
        )
        .await?;
        return Ok(Some(started_up_container_with_state(
            client.clone(),
            workspace.clone(),
            plan,
            outcome,
            LifecycleRunPath::Running,
            credentials,
            state,
        )));
    }

    Ok(None)
}

pub(super) async fn start_compose_project(
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
    let compose_capabilities = cli.ensure_required_capabilities().await?;

    let compose_service_validation = ComposeServiceValidation {
        primary_service: &compose.service,
        run_services: compose.run_services.as_deref(),
        workspace_folder: &plan.workspace_folder,
        project_name: compose_project.project_name(),
    };
    let compose_introspector = ComposeIntrospector::new(cli.clone());
    let user_config = compose_introspector
        .user_config(compose_project, &compose_service_validation)
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
    let published_port_policy_input = compose_introspector
        .user_published_port_planning_input(
            compose_project,
            &compose_service_validation,
            &user_lifecycle.services,
        )
        .await?;
    validate_compose_published_port_diagnostics(&published_port_policy_input)?;

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
    let mut existing_project_published_ports = Vec::new();
    if plan.config.compose.published_ports.relocation
        && !existing_compose_project_containers.is_empty()
    {
        existing_project_published_ports =
            list_existing_compose_project_published_ports(&client, compose_project.project_name())
                .await?;
    }
    let compose_published_ports = plan.config.compose.published_ports.relocation.then_some(
        ComposePublishedPortFinalization {
            input: &published_port_policy_input,
            existing_project_published_ports: &existing_project_published_ports,
        },
    );

    if let Some(started) = try_reuse_running_compose_container_before_image_prepare(
        &client,
        &workspace,
        ComposeRunningReuseInput {
            plan: plan.clone(),
            options: &options,
            forwarding_resolution,
            existing_compose_containers: &existing_compose_containers,
            compose_primary_image: &compose_primary_image,
            user_config: &user_config,
            compose_primary_service_user,
            compose_primary_service: compose_primary_service.as_ref(),
            published_port_policy_input: &published_port_policy_input,
            compose_published_ports,
        },
    )
    .await?
    {
        return Ok(started);
    }

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
        UpPlanResolution::new(
            forwarding_resolution,
            options.update_features,
            options.skip_global_config,
        ),
    )
    .await?;
    plan.base_image = compose_primary_image.clone();
    let finalized = finalize_up_plan_mounts(
        &client,
        &workspace,
        plan,
        existing_remote_user_image,
        existing_compose_containers
            .first()
            .and_then(existing::existing_container_config_hash),
        Some((options.pull && !primary_service_has_build, options.no_cache)),
        FinalizeUpPlanMountsOptions {
            forwarding: forwarding_resolution,
            update_features: options.update_features,
            compose_canonical_model: Some(&user_config.canonical_model),
            compose_primary_service_user,
            compose_primary_service: compose_primary_service.as_ref(),
            compose_published_ports,
        },
    )
    .await?;
    let mut plan = finalized.plan;
    let published_port_plan = finalized.compose_published_port_plan;
    let published_port_override = finalized.compose_published_port_override;
    let image_prepared = finalized.image_prepared;
    if compose_published_port_plan_has_relocations(&published_port_plan) {
        compose_capabilities.ensure_compose_override_tag()?;
        warn_on_compose_published_port_relocations(&plan, &published_port_plan);
    }
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
    report_deferred_config_messages(&plan.config);

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
        &published_port_override,
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
            let state = sync_started_compose_state(
                &client,
                &workspace,
                &plan,
                &outcome,
                LifecycleRunPath::Running,
                &published_port_policy_input,
                &published_port_plan,
            )
            .await?;
            return Ok(started_up_container_with_state(
                client,
                workspace,
                plan,
                outcome,
                LifecycleRunPath::Running,
                credentials,
                state,
            ));
        }
        ExistingContainerDecision::StartStopped { id, name } if should_reuse => {
            materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
            cli.up(
                &runtime_lifecycle.project,
                ComposeUpOptions {
                    force_recreate: false,
                    remove_orphans: false,
                },
                &runtime_lifecycle.services,
                Some(ComposePublishedPortStartupDiagnostics {
                    input: &published_port_policy_input,
                    plan: &published_port_plan,
                    relocation_enabled: plan.config.compose.published_ports.relocation,
                }),
            )
            .await?;
            ensure_container_running_after_start(
                &client,
                &name,
                startup_verification_for_plan(&plan),
            )
            .await?;
            let outcome = UpOutcome {
                container_id: id,
                container_name: name,
                reused: true,
            };
            let state = sync_started_compose_state(
                &client,
                &workspace,
                &plan,
                &outcome,
                LifecycleRunPath::Started,
                &published_port_policy_input,
                &published_port_plan,
            )
            .await?;
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
        ExistingContainerDecision::Create
        | ExistingContainerDecision::Recreate { .. }
        | ExistingContainerDecision::ReuseRunning { .. }
        | ExistingContainerDecision::StartStopped { .. } => {}
    }

    materialize_dotfile_skeletons(&plan.dotfile_skeletons)?;
    cli.up(
        &runtime_lifecycle.project,
        ComposeUpOptions {
            force_recreate,
            remove_orphans,
        },
        &runtime_lifecycle.services,
        Some(ComposePublishedPortStartupDiagnostics {
            input: &published_port_policy_input,
            plan: &published_port_plan,
            relocation_enabled: plan.config.compose.published_ports.relocation,
        }),
    )
    .await?;

    let container = ComposeIntrospector::new(cli)
        .resolve_service_container(&runtime_lifecycle.project, &compose.service)
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
        &published_port_policy_input,
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

        options.pull = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
        options = up_options_for_fast_path();
        options.rebuild = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
        options = up_options_for_fast_path();
        options.no_cache = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
        options = up_options_for_fast_path();
        options.update_features = true;
        assert!(!compose_running_reuse_fast_path_enabled(
            &options,
            std::slice::from_ref(&running),
        ));
    }
}

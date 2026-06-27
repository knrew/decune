use std::path::Path;

use anyhow::Result;

use crate::{
    config::{
        ConfigHashInput, config_hash, resolved::ResolvedDevcontainerSource,
        variables::expand_container_env_tracked,
    },
    docker::{
        build::build_hash_input,
        client::DockerClient,
        resource::DockerResources,
        user::{
            EffectiveUserResolveInput, HostPlatform, current_host_user_ids, image_config_user,
            resolve_effective_users_from_image, resolve_effective_users_with_compose_service_user,
            resolve_remote_user_from_image, resolve_uid_gid_sync_plan_from_image,
        },
    },
    runtime::compose_ports::{
        ComposePublishedPortDiagnostic, ComposePublishedPortOverride, ComposePublishedPortPlan,
        compose_published_port_override, plan_compose_published_ports_with_existing_project,
    },
    ui,
    up::{
        mounts::{
            WorkspaceLocationValidation, mount_variable_context, resolve_workspace_location,
            workspace_mount_plan_from_resolved,
        },
        plan::{
            add_internal_hash_versions, base_image_source, expand_runtime_devcontainer_fields,
            feature_lock_hash_inputs, final_image_source,
        },
        types::{ForwardingResolution, MountResolution, UpPlan},
        uid_gid::{
            effective_user_input_from_plan, plan_requires_uid_gid_sync_layer,
            uid_gid_sync_hash_input, uid_gid_sync_plan_requires_layer, uid_gid_sync_warning,
        },
    },
    workspace::Workspace,
};

use super::{
    ComposePublishedPortFinalization, FinalizeUpPlanMountsOptions,
    compose_generated_override_hash_input, startup_command_hash_input,
};

pub(super) async fn finalize_mounts_and_resources_for_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup_image: &str,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<FinalizeMountsAndResourcesResult> {
    let compose_base_image = matches!(
        plan.config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Compose(_))
    )
    .then(|| plan.base_image.clone());
    let effective_users = resolve_effective_users_for_image(
        client,
        lookup_image,
        effective_user_input_from_plan(&plan),
        options.compose_primary_service_user,
    )
    .await?;
    let remote_user =
        resolve_remote_user_from_image(client, lookup_image, &effective_users).await?;
    let uid_gid_sync_plan = resolve_uid_gid_sync_plan_from_image(
        client,
        lookup_image,
        &effective_users,
        plan.config.devcontainer.update_remote_user_uid,
        HostPlatform::current(),
        current_host_user_ids(),
    )
    .await?;
    if let Some(warning) = uid_gid_sync_warning(
        &plan.config_layers,
        &uid_gid_sync_plan,
        plan.config.devcontainer.update_remote_user_uid,
        HostPlatform::current(),
    ) {
        ui::warn(&warning);
    }
    let remote_user_name = remote_user.user.clone();
    let remote_user_home = remote_user.home.clone();
    let workspace_location = resolve_workspace_location(
        workspace,
        &plan.config,
        WorkspaceLocationValidation::RuntimeResolved,
        MountResolution::Resolve,
        |workspace_folder| {
            mount_variable_context(
                workspace,
                workspace_folder,
                remote_user_name.clone(),
                remote_user_home.clone(),
            )
        },
    )?;
    let mount_variables = mount_variable_context(
        workspace,
        &workspace_location.workspace_folder,
        remote_user_name,
        remote_user_home,
    );
    plan.config.devcontainer.workspace_folder = Some(workspace_location.workspace_folder.clone());
    expand_runtime_devcontainer_fields(&mut plan.config, &mount_variables)?;
    let expanded_container_env =
        expand_container_env_tracked(&plan.config.devcontainer.container_env, &mount_variables)?;
    plan.config.devcontainer.container_env = expanded_container_env.values;
    plan.sensitive_container_env = expanded_container_env.sensitive;
    let mount_plan = workspace_mount_plan_from_resolved(
        workspace_location.workspace_mount,
        workspace.root(),
        &plan.config,
        &mount_variables,
        MountResolution::Resolve,
        workspace.paths().state_dir(),
    )?;
    let mounts = mount_plan.mounts;
    plan.forward_ports = match options.forwarding {
        ForwardingResolution::Resolve => {
            crate::docker::ports::resolve_forward_ports(&plan.config.ports.entries)?
        }
        ForwardingResolution::IgnoreDetached => Vec::new(),
    };
    let (compose_published_port_plan, compose_published_port_override) =
        finalized_compose_published_ports(&plan, options.compose_published_ports)?;
    let mut hash_input = ConfigHashInput::new(&plan.config);
    if let Some(context) = &plan.build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    if let Some(compose_project) = &plan.compose_project {
        hash_input.compose_files = compose_project.config_hash_files().to_vec();
    }
    hash_input.sensitive_container_env_keys = plan
        .sensitive_container_env
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    hash_input.sensitive_build_arg_keys = plan
        .sensitive_build_args
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    hash_input.compose_canonical_model = options.compose_canonical_model.cloned();
    let devcontainer_file = Path::new(&plan.resources.labels["devcontainer.config_file"]);
    hash_input.feature_locks = match &plan.feature_install {
        Some(feature_install) => feature_install.lock_entries.clone(),
        None => feature_lock_hash_inputs(
            workspace,
            devcontainer_file,
            &plan.config,
            options.update_features,
        )?,
    };
    hash_input.resolved_mounts = crate::up::mount_hash_inputs(&mounts);
    hash_input.startup_command =
        startup_command_hash_input(client, &plan, lookup_image, options.compose_primary_service)
            .await?;
    if let Some(compose_project) = &plan.compose_project {
        hash_input.compose_generated_override = compose_generated_override_hash_input(
            &compose_project.generated_override_path(),
            &plan,
            &mounts,
            hash_input.startup_command.as_ref(),
            &compose_published_port_override,
        );
    }
    add_internal_hash_versions(&mut hash_input, &plan.config);
    let config_file = plan
        .resources
        .labels
        .get("devcontainer.config_file")
        .cloned()
        .unwrap_or_default();
    let pre_uid_gid_sync_resources =
        uid_gid_sync_plan_requires_layer(&uid_gid_sync_plan).then(|| {
            DockerResources::from_workspace(
                workspace,
                config_hash(&hash_input),
                config_file.clone(),
            )
        });
    hash_input.uid_gid_sync = uid_gid_sync_hash_input(
        &uid_gid_sync_plan,
        plan.config.devcontainer.update_remote_user_uid,
        HostPlatform::current(),
    );
    let hash = config_hash(&hash_input);
    let resources = DockerResources::from_workspace(workspace, hash, config_file);
    let image = final_image_source(&plan.config, &resources, &uid_gid_sync_plan)?;
    let base_image_resources = pre_uid_gid_sync_resources.as_ref().unwrap_or(&resources);
    let base_image = if let Some(compose_base_image) = compose_base_image {
        Ok(compose_base_image)
    } else {
        base_image_source(&plan.config, base_image_resources, &uid_gid_sync_plan)
    }?;

    plan.image = image;
    plan.base_image = base_image;
    plan.resources = resources;
    plan.pre_uid_gid_sync_resources = pre_uid_gid_sync_resources;
    plan.effective_users = effective_users;
    plan.uid_gid_sync_plan = uid_gid_sync_plan;
    if plan_requires_uid_gid_sync_layer(&plan) {
        plan.uid_gid_sync_build_context_dir = Some(
            workspace
                .paths()
                .cache_dir()
                .join("uid-gid-sync-build-context"),
        );
    }
    plan.workspace_folder = workspace_location.workspace_folder;
    plan.mounts = mounts;
    plan.dotfile_skeletons = mount_plan.dotfile_skeletons;

    Ok(FinalizeMountsAndResourcesResult {
        plan,
        compose_published_port_plan,
        compose_published_port_override,
    })
}

pub(super) struct FinalizeMountsAndResourcesResult {
    pub(super) plan: UpPlan,
    pub(super) compose_published_port_plan: ComposePublishedPortPlan,
    pub(super) compose_published_port_override: ComposePublishedPortOverride,
}

pub(super) fn finalized_compose_published_ports(
    plan: &UpPlan,
    context: Option<ComposePublishedPortFinalization<'_>>,
) -> Result<(ComposePublishedPortPlan, ComposePublishedPortOverride)> {
    if !plan.config.compose.published_ports.relocation {
        return Ok((
            ComposePublishedPortPlan::default(),
            ComposePublishedPortOverride::default(),
        ));
    }
    let Some(context) = context else {
        return Ok((
            ComposePublishedPortPlan::default(),
            ComposePublishedPortOverride::default(),
        ));
    };

    let port_plan = plan_compose_published_ports_with_existing_project(
        context.input,
        true,
        &plan.forward_ports,
        context.existing_project_published_ports,
    )
    .map_err(ComposePublishedPortDiagnostic::from_plan_error)?;
    let port_override = compose_published_port_override(&context.input.port_entries, &port_plan)?;
    Ok((port_plan, port_override))
}

pub(super) async fn resolve_effective_users_for_image(
    client: &DockerClient,
    image: &str,
    input: EffectiveUserResolveInput<'_>,
    compose_primary_service_user: Option<&str>,
) -> Result<crate::docker::user::EffectiveUsers> {
    let Some(compose_primary_service_user) = compose_primary_service_user else {
        return resolve_effective_users_from_image(client, image, input).await;
    };
    let image_config_user = image_config_user(client, image).await?;

    resolve_effective_users_with_compose_service_user(
        EffectiveUserResolveInput {
            image_config: image_config_user.as_deref(),
            ..input
        },
        Some(compose_primary_service_user),
    )
}

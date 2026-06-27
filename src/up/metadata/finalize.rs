use super::*;

pub(in crate::up) async fn finalize_up_plan_mounts(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    remote_user_image: Option<&str>,
    existing_container_config_hash: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<FinalizeUpPlanResult> {
    let update_features = options.update_features;
    let using_existing_remote_user_image = remote_user_image.is_some();
    let mut lookup_image = remote_user_image.map(ToOwned::to_owned);
    let mut lookup_base_image = None;
    let mut stale_lookup_images = Vec::new();
    let mut image_prepared = false;
    let mut deferred_workspace_layer = false;
    plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;
    if lookup_image.is_none() {
        if plan.build_context.is_some() {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok(FinalizeUpPlanResult::new(plan, false));
            };
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            lookup_base_image = Some(plan.base_image.clone());
            lookup_image = Some(plan.base_image.clone());
            image_prepared = true;
            deferred_workspace_layer = plan_requires_workspace_layer(&plan);
        } else if plan_requires_workspace_layer(&plan) {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok(FinalizeUpPlanResult::new(plan, false));
            };
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            lookup_base_image = Some(plan.base_image.clone());
            build_feature_layer_image(client, &plan, no_cache).await?;
            lookup_image = Some(plan.image.clone());
            image_prepared = true;
        } else {
            lookup_image = Some(plan.base_image.clone());
        }
    };
    let mut lookup_image = lookup_image.expect("lookup image must be set");
    let dockerfile_metadata =
        dockerfile_image_metadata_for_plan(client, &plan, &lookup_image, options.forwarding)
            .await?;
    if !dockerfile_metadata.layers.is_empty() {
        let skip_global_config = plan.config_layers.global.is_none();
        plan = rebuild_up_plan_with_image_metadata_layers(
            workspace,
            plan,
            dockerfile_metadata.layers,
            options.forwarding == ForwardingResolution::IgnoreDetached
                && dockerfile_metadata.has_forward_ports,
            MountResolution::Resolve,
            UpPlanResolution::new(options.forwarding, update_features, skip_global_config),
        )?;
        plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;
        if plan_requires_workspace_layer(&plan) && !using_existing_remote_user_image {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok(FinalizeUpPlanResult::new(plan, false));
            };
            if lookup_image != plan.base_image {
                stale_lookup_images.push(lookup_image.clone());
            }
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            lookup_base_image = Some(plan.base_image.clone());
            build_feature_layer_image(client, &plan, no_cache).await?;
            lookup_image = plan.image.clone();
            image_prepared = true;
            deferred_workspace_layer = false;
        }
    }
    if deferred_workspace_layer
        && plan_requires_workspace_layer(&plan)
        && !using_existing_remote_user_image
    {
        let Some((_, no_cache)) = build_for_lookup else {
            return Ok(FinalizeUpPlanResult::new(plan, false));
        };
        build_feature_layer_image(client, &plan, no_cache).await?;
        lookup_image = plan.image.clone();
        image_prepared = true;
    }
    let lookup = ImageLookupPreparation {
        image: &mut lookup_image,
        remote_user_image,
        base_image: &mut lookup_base_image,
        image_prepared: &mut image_prepared,
        build_options: if using_existing_remote_user_image {
            None
        } else {
            build_for_lookup
        },
        command_probe_build_options: build_for_lookup,
    };
    plan = Box::pin(maybe_auto_add_github_cli_feature_to_plan(
        client,
        workspace,
        plan,
        lookup,
        existing_container_config_hash,
        options,
    ))
    .await?;
    if plan.config.features.is_empty() {
        remove_feature_lock_file(&workspace.root().join(".decune").join("features.lock.toml"))?;
    }
    let finalized = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan,
        &lookup_image,
        options,
    ))
    .await?;
    plan = finalized.plan;

    if image_prepared && plan_requires_final_image_layer(&plan) {
        if let Some((pull, no_cache)) = build_for_lookup {
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            build_workspace_image_layers(client, &plan, no_cache).await?;
        }
        if plan.image != lookup_image {
            remove_image(client, &lookup_image, false).await?;
        }
        if let Some(lookup_base_image) = lookup_base_image
            && lookup_base_image != plan.base_image
        {
            remove_image(client, &lookup_base_image, false).await?;
        }
    } else if image_prepared && plan.image != lookup_image {
        tag_image(client, &lookup_image, &plan.image).await?;
        remove_image(client, &lookup_image, false).await?;
    }
    for stale_lookup_image in stale_lookup_images {
        if stale_lookup_image != plan.image
            && stale_lookup_image != plan.base_image
            && stale_lookup_image != lookup_image
        {
            remove_image(client, &stale_lookup_image, false).await?;
        }
    }

    Ok(FinalizeUpPlanResult {
        plan,
        image_prepared,
        compose_published_port_plan: finalized.compose_published_port_plan,
        compose_published_port_override: finalized.compose_published_port_override,
    })
}

pub(in crate::up) struct FinalizeUpPlanResult {
    pub(in crate::up) plan: UpPlan,
    pub(in crate::up) image_prepared: bool,
    pub(in crate::up) compose_published_port_plan: ComposePublishedPortPlan,
    pub(in crate::up) compose_published_port_override: ComposePublishedPortOverride,
}

impl FinalizeUpPlanResult {
    fn new(plan: UpPlan, image_prepared: bool) -> Self {
        Self {
            plan,
            image_prepared,
            compose_published_port_plan: ComposePublishedPortPlan::default(),
            compose_published_port_override: ComposePublishedPortOverride::default(),
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(in crate::up) struct ComposePublishedPortFinalization<'a> {
    pub(in crate::up) input: &'a ComposePublishedPortPlanningInput,
    pub(in crate::up) existing_project_published_ports: &'a [ComposePublishedPortReservation],
}

#[derive(Debug, Clone, Copy)]
pub(in crate::up) struct FinalizeUpPlanMountsOptions<'a> {
    pub(in crate::up) forwarding: ForwardingResolution,
    pub(in crate::up) update_features: bool,
    pub(in crate::up) compose_canonical_model: Option<&'a JsonValue>,
    pub(in crate::up) compose_primary_service_user: Option<&'a str>,
    pub(in crate::up) compose_primary_service: Option<&'a ComposeConfigService>,
    pub(in crate::up) compose_published_ports: Option<ComposePublishedPortFinalization<'a>>,
}

impl Default for FinalizeUpPlanMountsOptions<'_> {
    fn default() -> Self {
        Self {
            forwarding: ForwardingResolution::Resolve,
            update_features: false,
            compose_canonical_model: None,
            compose_primary_service_user: None,
            compose_primary_service: None,
            compose_published_ports: None,
        }
    }
}

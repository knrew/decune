use super::*;

pub(in crate::up) async fn build_existing_container_decision_plan(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    existing_container_image_id: Option<&str>,
    preliminary_plan: &UpPlan,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    if preliminary_plan.build_context.is_some() {
        return build_up_plan_with_forwarding_resolution(
            workspace,
            explicit_config_path,
            cli_layer,
            resolution.forwarding,
            resolution.update_features,
            resolution.skip_global_config,
        );
    }

    let include_forward_ports = resolution.forwarding == ForwardingResolution::Resolve;
    let image_metadata = match image_devcontainer_metadata_layers_if_present_with_forward_ports(
        client,
        &preliminary_plan.base_image,
        include_forward_ports,
    )
    .await?
    {
        Some(image_metadata) => image_metadata,
        None => {
            let Some(image_id) = existing_container_image_id else {
                return build_up_plan_with_forwarding_resolution(
                    workspace,
                    explicit_config_path,
                    cli_layer,
                    resolution.forwarding,
                    resolution.update_features,
                    resolution.skip_global_config,
                );
            };
            let Some(image_metadata) =
                image_devcontainer_metadata_layers_if_present_with_forward_ports(
                    client,
                    image_id,
                    include_forward_ports,
                )
                .await?
            else {
                return build_up_plan_with_forwarding_resolution(
                    workspace,
                    explicit_config_path,
                    cli_layer,
                    resolution.forwarding,
                    resolution.update_features,
                    resolution.skip_global_config,
                );
            };
            image_metadata
        }
    };

    if image_metadata.layers.is_empty() {
        return build_up_plan_with_forwarding_resolution(
            workspace,
            explicit_config_path,
            cli_layer,
            resolution.forwarding,
            resolution.update_features,
            resolution.skip_global_config,
        );
    }

    build_up_plan_with_image_metadata_and_forwarding_resolution(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata.layers,
        !include_forward_ports && image_metadata.has_forward_ports,
        resolution,
    )
}

pub(in crate::up) async fn prepare_image_based_metadata(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    preliminary_plan: UpPlan,
    pull: bool,
    resolution: UpPlanResolution,
) -> Result<(UpPlan, bool)> {
    if preliminary_plan.build_context.is_some() {
        return Ok((
            build_up_plan_with_forwarding_resolution(
                workspace,
                explicit_config_path,
                cli_layer,
                resolution.forwarding,
                resolution.update_features,
                resolution.skip_global_config,
            )?,
            false,
        ));
    }

    ensure_image(
        client,
        &preliminary_plan.base_image,
        if pull {
            PullPolicy::Always
        } else {
            PullPolicy::Missing
        },
    )
    .await?;
    let include_forward_ports = resolution.forwarding == ForwardingResolution::Resolve;
    let image_metadata = image_devcontainer_metadata_layers_with_forward_ports(
        client,
        &preliminary_plan.base_image,
        include_forward_ports,
    )
    .await?;
    if image_metadata.layers.is_empty() {
        return Ok((
            build_up_plan_with_forwarding_resolution(
                workspace,
                explicit_config_path,
                cli_layer,
                resolution.forwarding,
                resolution.update_features,
                resolution.skip_global_config,
            )?,
            true,
        ));
    }

    let plan = build_up_plan_with_image_metadata_and_forwarding_resolution(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata.layers,
        !include_forward_ports && image_metadata.has_forward_ports,
        resolution,
    )?;

    Ok((plan, true))
}

pub(in crate::up) async fn prepare_compose_image_metadata(
    client: &DockerClient,
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    preliminary_plan: UpPlan,
    compose_primary_image: &str,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    let include_forward_ports = resolution.forwarding == ForwardingResolution::Resolve;
    let image_metadata = image_devcontainer_metadata_layers_with_forward_ports(
        client,
        compose_primary_image,
        include_forward_ports,
    )
    .await?;
    if image_metadata.layers.is_empty() {
        return Ok(preliminary_plan);
    }

    let mut plan = build_up_plan_with_image_metadata_and_forwarding_resolution(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata.layers,
        !include_forward_ports && image_metadata.has_forward_ports,
        resolution,
    )?;
    plan.base_image = compose_primary_image.to_owned();
    Ok(plan)
}

pub(super) async fn dockerfile_image_metadata_for_plan(
    client: &DockerClient,
    plan: &UpPlan,
    image: &str,
    forwarding: ForwardingResolution,
) -> Result<crate::docker::image::ImageMetadataLayers> {
    if plan.build_context.is_none() {
        return Ok(crate::docker::image::ImageMetadataLayers {
            layers: Vec::new(),
            has_forward_ports: false,
        });
    }

    image_devcontainer_metadata_layers_with_forward_ports(
        client,
        image,
        forwarding == ForwardingResolution::Resolve,
    )
    .await
}

pub(in crate::up) async fn existing_remote_user_image_for_decision<'a>(
    client: &DockerClient,
    plan: &UpPlan,
    existing_container_image: Option<&'a str>,
) -> Result<Option<&'a str>> {
    if plan.build_context.is_some() {
        return Ok(existing_container_image);
    }

    if effective_users_depend_on_image_config_user(plan) {
        return Ok(existing_container_image);
    }

    if local_image_presence(client, &plan.base_image).await? == LocalImagePresence::Present {
        return Ok(None);
    }

    Ok(existing_container_image)
}

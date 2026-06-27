use super::*;

pub(super) async fn startup_command_hash_input(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_image: &str,
    compose_primary_service: Option<&ComposeConfigService>,
) -> Result<Option<StartupCommandHashInput>> {
    if plan.config.devcontainer.override_command {
        return Ok(None);
    }

    let image = startup_command_image(client, plan, lookup_image).await?;
    let image_startup = image_startup_command(client, &image).await?;
    let startup = effective_startup_command(image_startup, compose_primary_service);

    Ok(Some(StartupCommandHashInput {
        entrypoint: startup.entrypoint,
        command: startup.command,
    }))
}

pub(in crate::up) fn effective_startup_command(
    image_startup: ImageStartupCommand,
    compose_primary_service: Option<&ComposeConfigService>,
) -> ImageStartupCommand {
    let Some(service) = compose_primary_service else {
        return image_startup;
    };

    ImageStartupCommand {
        entrypoint: service
            .entrypoint
            .clone()
            .unwrap_or(image_startup.entrypoint),
        command: service.command.clone().unwrap_or(image_startup.command),
    }
}

async fn startup_command_image(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_image: &str,
) -> Result<String> {
    match &plan.config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image))
            if local_image_presence(client, image).await? == LocalImagePresence::Present =>
        {
            Ok(image.clone())
        }
        _ => Ok(lookup_image.to_owned()),
    }
}

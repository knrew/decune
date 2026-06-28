use std::{collections::BTreeMap, sync::atomic::Ordering};

use anyhow::{Context, Result};

use crate::{
    config::{
        layer::LayerFeature,
        resolve_config,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
        types::GithubCredentialsMode,
        variables::expand_container_env_tracked,
    },
    docker::{
        client::DockerClient,
        container::{ContainerCreateSpec, ContainerHostConfig, create_container, start_container},
        user::resolve_remote_user_from_image,
    },
    host::credentials::host_github_auth_token_available,
    ui,
    up::{
        build::{build_feature_layer_image, prepare_base_image_for_plan},
        mounts::{WorkspaceLocationValidation, mount_variable_context, resolve_workspace_location},
        plan::{base_image_source, final_image_source},
        start::wait_for_container_exit_code,
        types::{MountResolution, UpPlan},
        uid_gid::effective_user_input_from_plan,
    },
    workspace::Workspace,
};

use super::{
    FinalizeUpPlanMountsOptions, GITHUB_CLI_FEATURE_CANONICAL_ID, GITHUB_CLI_FEATURE_REF,
    IMAGE_COMMAND_PROBE_SEQUENCE, finalize_mounts_and_resources_for_plan,
    prepare_feature_metadata_for_plan, resolve_effective_users_for_image,
};

pub(super) struct ImageLookupPreparation<'a> {
    pub(super) image: &'a mut String,
    pub(super) remote_user_image: Option<&'a str>,
    pub(super) base_image: &'a mut Option<String>,
    pub(super) image_prepared: &'a mut bool,
    pub(super) build_options: Option<(bool, bool)>,
    pub(super) command_probe_build_options: Option<(bool, bool)>,
}

struct CommandProbeImage {
    pub(super) image: String,
    uses_existing_image: bool,
}

fn prepare_command_probe_image_for_plan(
    _client: &DockerClient,
    _plan: &UpPlan,
    remote_user_image: Option<&str>,
    _build_for_lookup: Option<(bool, bool)>,
) -> Option<CommandProbeImage> {
    let remote_user_image = remote_user_image?;

    // Existing-container reconciliation should probe the image that the
    // container is actually using. Building current inputs here would make a
    // metadata probe observe changes before the explicit config-hash/rebuild
    // decision below.
    Some(CommandProbeImage {
        image: remote_user_image.to_owned(),
        uses_existing_image: true,
    })
}

pub(super) async fn maybe_auto_add_github_cli_feature_to_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup: ImageLookupPreparation<'_>,
    existing_container_config_hash: Option<&str>,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<UpPlan> {
    if config_has_github_cli_feature(&plan.config) {
        return Ok(plan);
    }

    let host_token_available = host_github_auth_token_available();
    if !should_auto_add_github_cli_feature(&plan.config, host_token_available, false) {
        return Ok(plan);
    }

    let command_probe_image = prepare_command_probe_image_for_plan(
        client,
        &plan,
        lookup.remote_user_image,
        lookup.command_probe_build_options,
    )
    .unwrap_or_else(|| CommandProbeImage {
        image: (*lookup.image).clone(),
        uses_existing_image: false,
    });
    let command_probe_env = command_probe_container_env(
        client,
        workspace,
        &plan,
        &command_probe_image.image,
        options.compose_primary_service_user,
    )
    .await?;
    let image_has_gh =
        image_has_command(client, &command_probe_image.image, "gh", &command_probe_env).await?;
    if image_has_gh && command_probe_image.uses_existing_image {
        return Box::pin(choose_github_cli_feature_plan_for_existing_image_probe(
            client,
            workspace,
            plan,
            &lookup,
            existing_container_config_hash,
            options,
        ))
        .await;
    }

    if !should_auto_add_github_cli_feature(&plan.config, host_token_available, image_has_gh) {
        return Ok(plan);
    }

    ui::info("Adding GitHub CLI Feature for GitHub token forwarding");
    plan = add_github_cli_feature_to_plan(plan)?;
    plan = prepare_feature_metadata_for_plan(workspace, plan, options.update_features).await?;

    if let Some((pull, no_cache)) = lookup.build_options {
        prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
        *lookup.base_image = Some(plan.base_image.clone());
        build_feature_layer_image(client, &plan, no_cache).await?;
        lookup.image.clone_from(&plan.image);
        *lookup.image_prepared = true;
    }

    Ok(plan)
}

async fn choose_github_cli_feature_plan_for_existing_image_probe(
    client: &DockerClient,
    workspace: &Workspace,
    plan: UpPlan,
    lookup: &ImageLookupPreparation<'_>,
    existing_container_config_hash: Option<&str>,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<UpPlan> {
    let Some(existing_container_config_hash) = existing_container_config_hash else {
        return Ok(plan);
    };

    let finalized_plan = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan.clone(),
        lookup.image,
        options,
    ))
    .await?;
    if finalized_plan.plan.resources.config_hash == existing_container_config_hash {
        return Ok(plan);
    }

    if !should_auto_add_github_cli_feature(&plan.config, true, false) {
        return Ok(plan);
    }

    let candidate = add_github_cli_feature_to_plan(plan.clone())?;
    let candidate =
        prepare_feature_metadata_for_plan(workspace, candidate, options.update_features).await?;
    let finalized_candidate = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        candidate.clone(),
        lookup.image,
        options,
    ))
    .await?;
    if finalized_candidate.plan.resources.config_hash == existing_container_config_hash {
        return Ok(candidate);
    }

    Ok(plan)
}

async fn command_probe_container_env(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    image: &str,
    compose_primary_service_user: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let effective_users = resolve_effective_users_for_image(
        client,
        image,
        effective_user_input_from_plan(plan),
        compose_primary_service_user,
    )
    .await?;
    let remote_user = resolve_remote_user_from_image(client, image, &effective_users).await?;
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
        remote_user.user,
        remote_user.home,
    );

    expand_container_env_tracked(&plan.config.devcontainer.container_env, &mount_variables)
        .map(|expanded| expanded.values)
}

pub(in crate::up) fn should_auto_add_github_cli_feature(
    config: &ResolvedConfig,
    host_token_available: bool,
    image_has_gh: bool,
) -> bool {
    config.credentials.github.enabled
        && config.credentials.github.mode == GithubCredentialsMode::GhTokenFile
        && config.credentials.github.install_feature_if_missing
        && host_token_available
        && !image_has_gh
        && !config_has_github_cli_feature(config)
}

fn config_has_github_cli_feature(config: &ResolvedConfig) -> bool {
    config
        .features
        .iter()
        .any(|feature| feature.canonical_id == GITHUB_CLI_FEATURE_CANONICAL_ID)
}

pub(in crate::up) fn add_github_cli_feature_to_plan(mut plan: UpPlan) -> Result<UpPlan> {
    if config_has_github_cli_feature(&plan.config) {
        return Ok(plan);
    }
    let preserve_base_image = matches!(
        plan.config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Compose(_))
    );

    let mut cli_layer = plan.config_layers.cli.take().unwrap_or_default();
    cli_layer
        .features
        .push(LayerFeature::new(GITHUB_CLI_FEATURE_REF.to_owned()));
    plan.config_layers.cli = Some(cli_layer);
    plan.config = resolve_config(plan.config_layers.clone());
    plan.feature_install = None;
    plan.image = final_image_source(&plan.config, &plan.resources, &plan.uid_gid_sync_plan)?;
    if !preserve_base_image {
        plan.base_image =
            base_image_source(&plan.config, &plan.resources, &plan.uid_gid_sync_plan)?;
    }

    Ok(plan)
}

async fn image_has_command(
    client: &DockerClient,
    image: &str,
    command: &str,
    env: &BTreeMap<String, String>,
) -> Result<bool> {
    let probe_id = IMAGE_COMMAND_PROBE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let name = format!("decune-command-probe-{}-{probe_id}", std::process::id());
    let spec = ContainerCreateSpec {
        image: image.to_owned(),
        name: name.clone(),
        entrypoint: Some(vec!["/bin/sh".to_owned()]),
        command: Some(vec![
            "-c".to_owned(),
            format!("command -v {command} >/dev/null 2>&1"),
        ]),
        labels: BTreeMap::new(),
        env: env.clone(),
        working_dir: None,
        user: None,
        mounts: Vec::new(),
        publish_ports: Vec::new(),
        host_config: ContainerHostConfig::default(),
    };
    let container_id = create_container(client, &spec).await?;
    let result = async {
        start_container(client, &container_id).await?;
        wait_for_container_exit_code(client, &container_id).await
    }
    .await;
    let cleanup =
        crate::docker::container::remove_container(client, &container_id, true, true).await;

    match (result, cleanup) {
        (Ok(exit_code), Ok(())) => Ok(exit_code == 0),
        (Err(error), Ok(())) => Err(error),
        (Ok(_), Err(cleanup_error)) => Err(cleanup_error)
            .with_context(|| format!("Failed to remove command probe container: {name}")),
        (Err(error), Err(cleanup_error)) => Err(error).context(format!(
            "Failed to remove command probe container {name}: {cleanup_error:#}"
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::{add_github_cli_feature_to_plan, should_auto_add_github_cli_feature};
    use crate::{
        config::{resolved::ResolvedConfig, types::GithubCredentialsMode},
        up::test_support::test_up_plan_with_image_source,
    };

    #[test]
    fn github_cli_auto_add_requires_token_and_missing_container_binary() {
        let mut config = ResolvedConfig::default();

        assert!(should_auto_add_github_cli_feature(&config, true, false));
        assert!(!should_auto_add_github_cli_feature(&config, false, false));
        assert!(!should_auto_add_github_cli_feature(&config, true, true));

        config.credentials.github.install_feature_if_missing = false;
        assert!(!should_auto_add_github_cli_feature(&config, true, false));

        config.credentials.github.install_feature_if_missing = true;
        config.credentials.github.enabled = false;
        assert!(!should_auto_add_github_cli_feature(&config, true, false));

        config.credentials.github.enabled = true;
        config.credentials.github.mode = GithubCredentialsMode::Off;
        assert!(!should_auto_add_github_cli_feature(&config, true, false));
    }
    #[test]
    fn github_cli_auto_add_injects_feature_once() {
        let plan = test_up_plan_with_image_source("alpine:3.20");

        let plan = add_github_cli_feature_to_plan(plan).unwrap();
        let plan = add_github_cli_feature_to_plan(plan).unwrap();

        let github_cli_features = plan
            .config
            .features
            .iter()
            .filter(|feature| feature.canonical_id == "ghcr.io/devcontainers/features/github-cli")
            .collect::<Vec<_>>();
        assert_eq!(github_cli_features.len(), 1);
        assert_eq!(
            github_cli_features[0].id,
            "ghcr.io/devcontainers/features/github-cli:1"
        );
    }
    #[test]
    fn github_cli_auto_add_retickets_image_sources_to_workspace_layer() {
        let plan = test_up_plan_with_image_source("ubuntu:24.04");

        let plan = add_github_cli_feature_to_plan(plan).unwrap();

        assert_eq!(plan.base_image, "ubuntu:24.04");
        assert_eq!(plan.image, plan.resources.image_tag);
        assert_ne!(plan.image, plan.base_image);
    }
}

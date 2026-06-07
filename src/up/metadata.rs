use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, StartupCommandHashInput, config_hash,
        layer::LayerFeature,
        resolve_config,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource, ResolvedPortAttributes},
        types::GithubCredentialsMode,
        variables::expand_container_env,
    },
    devcontainer::features::{prepare_feature_install_plan, remove_feature_lock_file},
    docker::{
        build::{DockerBuildInput, build_hash_input, build_image},
        client::DockerClient,
        container::{ContainerCreateSpec, ContainerHostConfig, create_container, start_container},
        image::{
            LocalImagePresence, PullPolicy, ensure_image,
            image_devcontainer_metadata_layers_if_present_with_forward_ports,
            image_devcontainer_metadata_layers_with_forward_ports,
            image_has_devcontainer_metadata_label_if_present, image_startup_command,
            local_image_presence, remove_image, tag_image,
        },
        resource::DockerResources,
        user::{
            HostPlatform, current_host_user_ids, resolve_effective_users_from_image,
            resolve_remote_user_from_image, resolve_uid_gid_sync_plan_from_image,
        },
    },
    host::credentials::host_github_auth_token_available,
    ui,
    up::{
        build::{
            build_feature_layer_image, build_workspace_image_layers,
            plan_requires_final_image_layer, plan_requires_workspace_layer,
            prepare_base_image_for_plan,
        },
        mounts::{
            mount_variable_context, resolve_workspace_location, workspace_mounts_from_resolved,
        },
        plan::{
            add_internal_hash_versions, base_image_source,
            build_up_plan_with_forwarding_resolution,
            build_up_plan_with_image_metadata_and_forwarding_resolution, feature_lock_hash_inputs,
            final_image_source,
        },
        start::wait_for_container_exit_code,
        types::{ForwardingResolution, MountResolution, UpPlan, UpPlanResolution},
        uid_gid::{
            effective_user_input_from_config_layers, effective_users_depend_on_image_config_user,
            plan_requires_uid_gid_sync_layer, pre_uid_gid_sync_layer_resources,
            uid_gid_sync_hash_input, uid_gid_sync_plan_requires_layer, uid_gid_sync_warning,
        },
    },
    workspace::Workspace,
};

const GITHUB_CLI_FEATURE_REF: &str = "ghcr.io/devcontainers/features/github-cli:1";
const GITHUB_CLI_FEATURE_CANONICAL_ID: &str = "ghcr.io/devcontainers/features/github-cli";
static IMAGE_COMMAND_PROBE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct ImageLookupPreparation<'a> {
    image: &'a mut String,
    remote_user_image: Option<&'a str>,
    base_image: &'a mut Option<String>,
    image_prepared: &'a mut bool,
    build_options: Option<(bool, bool)>,
    command_probe_build_options: Option<(bool, bool)>,
}

struct CommandProbeImage {
    image: String,
    uses_existing_image: bool,
}

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
        let image = existing_container_image_id.unwrap_or(&preliminary_plan.image);
        warn_about_unsupported_dockerfile_image_metadata(client, image).await?;
        return build_up_plan_with_forwarding_resolution(
            workspace,
            explicit_config_path,
            cli_layer,
            resolution.forwarding,
            resolution.update_features,
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
        );
    }

    build_up_plan_with_image_metadata_and_forwarding_resolution(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata.layers,
        !include_forward_ports && image_metadata.has_forward_ports,
        resolution.forwarding,
        resolution.update_features,
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
        resolution.forwarding,
        resolution.update_features,
    )?;

    Ok((plan, true))
}

pub(in crate::up) async fn finalize_up_plan_mounts(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    remote_user_image: Option<&str>,
    existing_container_config_hash: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
    update_features: bool,
) -> Result<(UpPlan, bool)> {
    let using_existing_remote_user_image = remote_user_image.is_some();
    let mut lookup_image = remote_user_image.map(ToOwned::to_owned);
    let mut lookup_base_image = None;
    let mut image_prepared = false;
    plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;
    if lookup_image.is_none() {
        if plan_requires_workspace_layer(&plan) {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok((plan, false));
            };
            prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
            lookup_base_image = Some(plan.base_image.clone());
            build_feature_layer_image(client, &plan, no_cache).await?;
            lookup_image = Some(plan.image.clone());
            image_prepared = true;
        } else if let Some(context) = plan.build_context.clone() {
            let Some((pull, no_cache)) = build_for_lookup else {
                return Ok((plan, false));
            };
            let mut build_options = plan.build_options.clone();
            build_options.pull = pull;
            build_options.no_cache = no_cache;
            build_image(
                client,
                DockerBuildInput {
                    image_tag: plan.base_image.clone(),
                    labels: pre_uid_gid_sync_layer_resources(&plan)
                        .labels
                        .clone()
                        .into_iter()
                        .collect(),
                    context,
                    options: build_options,
                },
            )
            .await?;
            lookup_image = Some(plan.base_image.clone());
            image_prepared = true;
        } else {
            lookup_image = Some(plan.base_image.clone());
        }
    };
    let mut lookup_image = lookup_image.expect("lookup image must be set");
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
        update_features,
    ))
    .await?;
    if plan.config.features.is_empty() {
        remove_feature_lock_file(&workspace.root().join(".decune").join("features.lock.toml"))?;
    }
    plan = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan,
        &lookup_image,
        update_features,
    ))
    .await?;

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

    Ok((plan, image_prepared))
}

async fn finalize_mounts_and_resources_for_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup_image: &str,
    update_features: bool,
) -> Result<UpPlan> {
    let effective_users = resolve_effective_users_from_image(
        client,
        lookup_image,
        effective_user_input_from_config_layers(&plan.config_layers),
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
    let workspace_location =
        resolve_workspace_location(workspace, &plan.config, |workspace_folder| {
            mount_variable_context(
                workspace,
                workspace_folder,
                remote_user_name.clone(),
                remote_user_home.clone(),
            )
        })?;
    let mount_variables = mount_variable_context(
        workspace,
        &workspace_location.workspace_folder,
        remote_user_name,
        remote_user_home,
    );
    plan.config.devcontainer.container_env =
        expand_container_env(&plan.config.devcontainer.container_env, &mount_variables)?;
    let mounts = workspace_mounts_from_resolved(
        workspace_location.workspace_mount,
        workspace.root(),
        &plan.config,
        &mount_variables,
        MountResolution::Resolve,
    )?;
    let mut hash_input = ConfigHashInput::new(&plan.config);
    if let Some(context) = &plan.build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    let devcontainer_file = Path::new(&plan.resources.labels["devcontainer.config_file"]);
    hash_input.feature_locks = match &plan.feature_install {
        Some(feature_install) => feature_install.lock_entries.clone(),
        None => {
            feature_lock_hash_inputs(workspace, devcontainer_file, &plan.config, update_features)?
        }
    };
    hash_input.resolved_mounts = crate::up::mount_hash_inputs(&mounts);
    hash_input.startup_command = startup_command_hash_input(client, &plan, lookup_image).await?;
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
    let base_image = base_image_source(&plan.config, base_image_resources, &uid_gid_sync_plan)?;

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

    Ok(plan)
}

async fn startup_command_hash_input(
    client: &DockerClient,
    plan: &UpPlan,
    lookup_image: &str,
) -> Result<Option<StartupCommandHashInput>> {
    if plan.config.devcontainer.override_command {
        return Ok(None);
    }

    let image = startup_command_image(client, plan, lookup_image).await?;
    let startup = image_startup_command(client, &image).await?;

    Ok(Some(StartupCommandHashInput {
        entrypoint: startup.entrypoint,
        command: startup.command,
    }))
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

async fn prepare_command_probe_image_for_plan(
    _client: &DockerClient,
    _plan: &UpPlan,
    remote_user_image: Option<&str>,
    _build_for_lookup: Option<(bool, bool)>,
) -> Result<Option<CommandProbeImage>> {
    let Some(remote_user_image) = remote_user_image else {
        return Ok(None);
    };

    // Existing-container reconciliation should probe the image that the
    // container is actually using. Building current inputs here would make a
    // metadata probe observe changes before the explicit config-hash/rebuild
    // decision below.
    Ok(Some(CommandProbeImage {
        image: remote_user_image.to_owned(),
        uses_existing_image: true,
    }))
}

async fn prepare_feature_metadata_for_plan(
    workspace: &Workspace,
    mut plan: UpPlan,
    update_features: bool,
) -> Result<UpPlan> {
    if plan.feature_install.is_some() {
        return Ok(plan);
    }
    if plan.config.features.is_empty() {
        if !plan.config.devcontainer.entrypoints.is_empty() {
            plan.feature_build_context_dir =
                Some(workspace.paths().cache_dir().join("feature-build-context"));
        }
        return Ok(plan);
    }

    let features = plan.config.features.clone();
    let override_feature_install_order = plan
        .config
        .devcontainer
        .override_feature_install_order
        .clone();
    let devcontainer_file = PathBuf::from(&plan.resources.labels["devcontainer.config_file"]);
    let workspace_root = workspace.root().to_path_buf();
    let feature_archive_cache_dir = workspace.paths().feature_archive_cache_dir().to_path_buf();
    let feature_extract_dir = workspace
        .paths()
        .cache_dir()
        .join("features")
        .join("extracted");
    let Some(feature_install) = tokio::task::spawn_blocking(move || {
        prepare_feature_install_plan(
            &features,
            &devcontainer_file,
            &workspace_root,
            &feature_archive_cache_dir,
            &feature_extract_dir,
            &override_feature_install_order,
            update_features,
        )
    })
    .await
    .context("Feature install planning task failed")??
    else {
        return Ok(plan);
    };
    plan.config_layers.feature_metadata = feature_install.metadata_layers.clone();
    plan.config = resolve_config(plan.config_layers.clone());
    plan.feature_install = Some(feature_install);
    plan.feature_build_context_dir =
        Some(workspace.paths().cache_dir().join("feature-build-context"));

    Ok(plan)
}

async fn maybe_auto_add_github_cli_feature_to_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup: ImageLookupPreparation<'_>,
    existing_container_config_hash: Option<&str>,
    update_features: bool,
) -> Result<UpPlan> {
    if config_has_github_cli_feature(&plan.config) {
        return Ok(plan);
    }

    let host_token_available = host_github_auth_token_available()?;
    if !should_auto_add_github_cli_feature(&plan.config, host_token_available, false) {
        return Ok(plan);
    }

    let command_probe_image = prepare_command_probe_image_for_plan(
        client,
        &plan,
        lookup.remote_user_image,
        lookup.command_probe_build_options,
    )
    .await?
    .unwrap_or(CommandProbeImage {
        image: (*lookup.image).clone(),
        uses_existing_image: false,
    });
    let command_probe_env =
        command_probe_container_env(client, workspace, &plan, &command_probe_image.image).await?;
    let image_has_gh =
        image_has_command(client, &command_probe_image.image, "gh", &command_probe_env).await?;
    if image_has_gh && command_probe_image.uses_existing_image {
        return Box::pin(choose_github_cli_feature_plan_for_existing_image_probe(
            client,
            workspace,
            plan,
            &lookup,
            existing_container_config_hash,
            update_features,
        ))
        .await;
    }

    if !should_auto_add_github_cli_feature(&plan.config, host_token_available, image_has_gh) {
        return Ok(plan);
    }

    ui::info("Adding GitHub CLI Feature for GitHub token forwarding");
    plan = add_github_cli_feature_to_plan(plan)?;
    plan = prepare_feature_metadata_for_plan(workspace, plan, update_features).await?;

    if let Some((pull, no_cache)) = lookup.build_options {
        prepare_base_image_for_plan(client, &plan, pull, no_cache).await?;
        *lookup.base_image = Some(plan.base_image.clone());
        build_feature_layer_image(client, &plan, no_cache).await?;
        *lookup.image = plan.image.clone();
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
    update_features: bool,
) -> Result<UpPlan> {
    let Some(existing_container_config_hash) = existing_container_config_hash else {
        return Ok(plan);
    };

    let finalized_plan = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        plan.clone(),
        lookup.image,
        update_features,
    ))
    .await?;
    if finalized_plan.resources.config_hash == existing_container_config_hash {
        return Ok(plan);
    }

    if !should_auto_add_github_cli_feature(&plan.config, true, false) {
        return Ok(plan);
    }

    let candidate = add_github_cli_feature_to_plan(plan.clone())?;
    let candidate =
        prepare_feature_metadata_for_plan(workspace, candidate, update_features).await?;
    let finalized_candidate = Box::pin(finalize_mounts_and_resources_for_plan(
        client,
        workspace,
        candidate.clone(),
        lookup.image,
        update_features,
    ))
    .await?;
    if finalized_candidate.resources.config_hash == existing_container_config_hash {
        return Ok(candidate);
    }

    Ok(plan)
}

async fn command_probe_container_env(
    client: &DockerClient,
    workspace: &Workspace,
    plan: &UpPlan,
    image: &str,
) -> Result<BTreeMap<String, String>> {
    let effective_users = resolve_effective_users_from_image(
        client,
        image,
        effective_user_input_from_config_layers(&plan.config_layers),
    )
    .await?;
    let remote_user = resolve_remote_user_from_image(client, image, &effective_users).await?;
    let remote_user_name = remote_user.user.clone();
    let remote_user_home = remote_user.home.clone();
    let workspace_location =
        resolve_workspace_location(workspace, &plan.config, |workspace_folder| {
            mount_variable_context(
                workspace,
                workspace_folder,
                remote_user_name.clone(),
                remote_user_home.clone(),
            )
        })?;
    let mount_variables = mount_variable_context(
        workspace,
        &workspace_location.workspace_folder,
        remote_user.user,
        remote_user.home,
    );

    expand_container_env(&plan.config.devcontainer.container_env, &mount_variables)
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

    let mut cli_layer = plan.config_layers.cli.take().unwrap_or_default();
    cli_layer
        .features
        .push(LayerFeature::new(GITHUB_CLI_FEATURE_REF.to_owned()));
    plan.config_layers.cli = Some(cli_layer);
    plan.config = resolve_config(plan.config_layers.clone());
    plan.feature_install = None;
    plan.image = final_image_source(&plan.config, &plan.resources, &plan.uid_gid_sync_plan)?;
    plan.base_image = base_image_source(&plan.config, &plan.resources, &plan.uid_gid_sync_plan)?;

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

pub(in crate::up) fn warn_about_deferred_features(config: &ResolvedConfig) {
    for warning in untrusted_repository_warnings(config) {
        ui::warn(&warning);
    }
}

pub(in crate::up) fn untrusted_repository_warnings(config: &ResolvedConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    if config.devcontainer.privileged {
        warnings.push(
            "This dev container requests privileged mode; review untrusted repositories before running it."
                .to_owned(),
        );
    }
    if !config.devcontainer.cap_add.is_empty() {
        warnings.push(format!(
            "This dev container adds Linux capabilities ({}); review untrusted repositories before running it.",
            config.devcontainer.cap_add.join(", ")
        ));
    }
    if !config.devcontainer.security_opt.is_empty() {
        warnings.push(format!(
            "This dev container sets Docker security options ({}); review untrusted repositories before running it.",
            config.devcontainer.security_opt.join(", ")
        ));
    }
    if config.devcontainer.publish_ports.iter().any(|port| {
        port.host_ip
            .as_deref()
            .is_none_or(|host_ip| host_ip != "127.0.0.1")
    }) {
        warnings.push(
            "This dev container publishes appPort through Docker, which may bind outside localhost when no host IP is specified. Use forwardPorts, decune [[ports]], or CLI -p for localhost-only access."
                .to_owned(),
        );
    }
    warnings.extend(unsupported_port_attribute_warnings(config));
    if !config.devcontainer.mounts.is_empty() {
        warnings.push(
            "This dev container declares additional mounts; review host paths before running untrusted repositories."
                .to_owned(),
        );
    }
    warnings
}

fn unsupported_port_attribute_warnings(config: &ResolvedConfig) -> Vec<String> {
    let mut warnings = Vec::new();

    for (key, attributes) in &config.devcontainer.port_attributes {
        warnings.extend(unsupported_single_port_attribute_warnings(
            &format!("portsAttributes.{key}"),
            attributes,
        ));
    }
    if let Some(attributes) = &config.devcontainer.other_ports_attributes {
        warnings.extend(unsupported_single_port_attribute_warnings(
            "otherPortsAttributes",
            attributes,
        ));
    }

    warnings
}

fn unsupported_single_port_attribute_warnings(
    path: &str,
    attributes: &ResolvedPortAttributes,
) -> Vec<String> {
    let mut warnings = Vec::new();

    if let Some(protocol) = &attributes.unsupported_protocol {
        warnings.push(format!(
            "{path}.protocol is ignored in decune v0.1 (value: {protocol}); raw TCP forwarding only supports label, onAutoForward, and requireLocalPort."
        ));
    }
    if attributes.unsupported_elevate_if_needed.is_some() {
        warnings.push(format!(
            "{path}.elevateIfNeeded is ignored in decune v0.1; low-port privilege elevation is not supported."
        ));
    }

    warnings
}

pub(in crate::up) async fn warn_about_unsupported_dockerfile_image_metadata(
    client: &DockerClient,
    image: &str,
) -> Result<()> {
    if image_has_devcontainer_metadata_label_if_present(client, image).await? == Some(true) {
        ui::warn(&format!(
            "Dockerfile image label devcontainer.metadata is not merged in decune v0.1: {image}. Move this metadata to devcontainer.json or use an image-based devcontainer."
        ));
    }

    Ok(())
}

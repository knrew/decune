use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

use anyhow::{Context, Result};
use serde_json::Value as JsonValue;

use crate::{
    config::{
        ComposeGeneratedOverrideHashInput, ConfigHashInput, ConfigLayer, StartupCommandHashInput,
        canonical::{CanonicalWriter, sha256_hex},
        config_hash,
        layer::LayerFeature,
        resolve_config,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource, ResolvedPortAttributes},
        types::{GitHttpsMode, GithubCredentialsMode, MountType, SshAgentMode},
        variables::expand_container_env_tracked,
    },
    devcontainer::features::{prepare_feature_install_plan, remove_feature_lock_file},
    docker::{
        build::{DockerBuildInput, build_hash_input, build_image},
        client::DockerClient,
        container::{ContainerCreateSpec, ContainerHostConfig, create_container, start_container},
        image::{
            ImageStartupCommand, LocalImagePresence, PullPolicy, ensure_image,
            image_devcontainer_metadata_layers_if_present_with_forward_ports,
            image_devcontainer_metadata_layers_with_forward_ports,
            image_has_devcontainer_metadata_label_if_present, image_startup_command,
            local_image_presence, remove_image, tag_image,
        },
        mounts::{DockerMountSpec, devcontainer_mount_type},
        resource::DockerResources,
        user::{
            EffectiveUserResolveInput, HostPlatform, current_host_user_ids, image_config_user,
            resolve_effective_users_from_image, resolve_effective_users_with_compose_service_user,
            resolve_remote_user_from_image, resolve_uid_gid_sync_plan_from_image,
        },
    },
    host::credentials::host_github_auth_token_available,
    runtime::compose_cli::ComposeConfigService,
    ui,
    up::{
        build::{
            build_feature_layer_image, build_workspace_image_layers,
            plan_requires_final_image_layer, plan_requires_workspace_layer,
            prepare_base_image_for_plan,
        },
        mounts::{
            WorkspaceLocationValidation, mount_variable_context, resolve_workspace_location,
            workspace_mounts_from_resolved,
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
        resolution.forwarding,
        resolution.update_features,
    )?;
    plan.base_image = compose_primary_image.to_owned();
    Ok(plan)
}

pub(in crate::up) async fn finalize_up_plan_mounts(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    remote_user_image: Option<&str>,
    existing_container_config_hash: Option<&str>,
    build_for_lookup: Option<(bool, bool)>,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<(UpPlan, bool)> {
    let update_features = options.update_features;
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
        options,
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
        options,
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

#[derive(Debug, Clone, Copy, Default)]
pub(in crate::up) struct FinalizeUpPlanMountsOptions<'a> {
    pub(in crate::up) update_features: bool,
    pub(in crate::up) compose_canonical_model: Option<&'a JsonValue>,
    pub(in crate::up) compose_primary_service_user: Option<&'a str>,
    pub(in crate::up) compose_primary_service: Option<&'a ComposeConfigService>,
}

async fn finalize_mounts_and_resources_for_plan(
    client: &DockerClient,
    workspace: &Workspace,
    mut plan: UpPlan,
    lookup_image: &str,
    options: FinalizeUpPlanMountsOptions<'_>,
) -> Result<UpPlan> {
    let compose_base_image = matches!(
        plan.config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Compose(_))
    )
    .then(|| plan.base_image.clone());
    let effective_users = resolve_effective_users_for_image(
        client,
        lookup_image,
        effective_user_input_from_config_layers(&plan.config_layers),
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
    let expanded_container_env =
        expand_container_env_tracked(&plan.config.devcontainer.container_env, &mount_variables)?;
    plan.config.devcontainer.container_env = expanded_container_env.values;
    plan.sensitive_container_env = expanded_container_env.sensitive;
    let mounts = workspace_mounts_from_resolved(
        workspace_location.workspace_mount,
        workspace.root(),
        &plan.config,
        &mount_variables,
        MountResolution::Resolve,
        workspace.paths().state_dir(),
    )?;
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
            compose_project.generated_override_path(),
            &plan,
            &mounts,
            hash_input.startup_command.as_ref(),
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

    Ok(plan)
}

fn compose_generated_override_hash_input(
    path: PathBuf,
    plan: &UpPlan,
    mounts: &[DockerMountSpec],
    startup_command: Option<&StartupCommandHashInput>,
) -> Option<ComposeGeneratedOverrideHashInput> {
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &plan.config.devcontainer.source
    else {
        return None;
    };

    let mut writer = CanonicalWriter::default();
    writer.object("ComposeGeneratedOverrideContent", |writer| {
        writer.field("primary_service", |writer| writer.string(&compose.service));
        writer.field("image", |writer| {
            writer.string(generated_override_semantic_image(plan));
        });
        writer.field("pull_policy", |writer| {
            if generated_override_semantic_pull_policy_never(plan) {
                writer.string("never");
            } else {
                writer.none();
            }
        });
        writer.field("labels", |writer| {
            let labels = generated_override_semantic_labels(&plan.resources.labels);
            writer.map(labels.iter(), |writer, value| writer.string(value));
        });
        writer.field("environment", |writer| {
            let environment = plan
                .config
                .devcontainer
                .container_env
                .iter()
                .map(|(key, value)| {
                    let value = if plan.sensitive_container_env.contains_key(key) {
                        "<localEnv-derived-value>".to_owned()
                    } else {
                        value.clone()
                    };
                    (key.clone(), value)
                })
                .collect::<BTreeMap<_, _>>();
            writer.map(environment.iter(), |writer, value| {
                writer.string(value);
            });
        });
        writer.field("container_user", |writer| {
            writer.option_string(plan.config.devcontainer.container_user.as_deref());
        });
        writer.field("init", |writer| writer.bool(plan.config.devcontainer.init));
        writer.field("privileged", |writer| {
            writer.bool(plan.config.devcontainer.privileged);
        });
        writer.field("cap_add", |writer| {
            writer.seq(plan.config.devcontainer.cap_add.iter(), |writer, value| {
                writer.string(value);
            });
        });
        writer.field("security_opt", |writer| {
            writer.seq(
                plan.config.devcontainer.security_opt.iter(),
                |writer, value| {
                    writer.string(value);
                },
            );
        });
        writer.field("mounts", |writer| {
            let inputs = crate::up::mount_hash_inputs(mounts);
            writer.seq(inputs.iter(), |writer, mount| {
                writer.object("Mount", |writer| {
                    writer.field("source", |writer| {
                        writer.option_string(mount.source.as_deref());
                    });
                    writer.field("target", |writer| writer.string(&mount.target));
                    writer.field("type", |writer| {
                        writer.string(match mount.mount_type {
                            MountType::Bind => "bind",
                            MountType::Volume => "volume",
                            MountType::Tmpfs => "tmpfs",
                        });
                    });
                    writer.field("read_only", |writer| writer.bool(mount.read_only));
                    writer.field("consistency", |writer| {
                        writer.option_string(mount.consistency.as_deref());
                    });
                });
            });
        });
        writer.field("startup_command", |writer| match startup_command {
            Some(startup_command) => {
                writer.object("StartupCommand", |writer| {
                    writer.field("entrypoint", |writer| {
                        writer.seq(startup_command.entrypoint.iter(), |writer, value| {
                            writer.string(value);
                        });
                    });
                    writer.field("command", |writer| {
                        writer.seq(startup_command.command.iter(), |writer, value| {
                            writer.string(value);
                        });
                    });
                });
            }
            None => writer.none(),
        });
    });

    Some(ComposeGeneratedOverrideHashInput {
        path: path.display().to_string(),
        content_hash: sha256_hex(writer.finish().as_bytes()),
    })
}

fn generated_override_semantic_image(plan: &UpPlan) -> &str {
    if plan.image == plan.base_image {
        &plan.image
    } else {
        "<decune-generated-image>"
    }
}

fn generated_override_semantic_pull_policy_never(plan: &UpPlan) -> bool {
    plan.image != plan.base_image
}

fn generated_override_semantic_labels(
    labels: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    labels
        .iter()
        .filter(|(key, _)| generated_override_label_is_semantic_hash_input(key))
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

fn generated_override_label_is_semantic_hash_input(key: &str) -> bool {
    key != "decune.config_hash" && !key.starts_with("com.docker.compose.")
}

async fn resolve_effective_users_for_image(
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
            image_config_user: image_config_user.as_deref(),
            ..input
        },
        Some(compose_primary_service_user),
    )
}

async fn startup_command_hash_input(
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
    options: FinalizeUpPlanMountsOptions<'_>,
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
    if finalized_plan.resources.config_hash == existing_container_config_hash {
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
    compose_primary_service_user: Option<&str>,
) -> Result<BTreeMap<String, String>> {
    let effective_users = resolve_effective_users_for_image(
        client,
        image,
        effective_user_input_from_config_layers(&plan.config_layers),
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

    if matches!(
        config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Dockerfile(_))
    ) {
        warnings.push(
            "This dev container builds a workspace Dockerfile, which can execute arbitrary build steps. Review Dockerfile contents before running untrusted repositories."
                .to_owned(),
        );
    }
    if !config.features.is_empty() {
        warnings.push(
            "This dev container installs Features, whose install.sh scripts execute during image build. Review Feature sources and lock digests before running untrusted repositories."
                .to_owned(),
        );
    }
    if !config.devcontainer.entrypoints.is_empty() {
        warnings.push(
            "This dev container configures entrypoint commands that execute when the container starts. Review entrypoint scripts before running untrusted repositories."
                .to_owned(),
        );
    }
    if config
        .devcontainer
        .lifecycle
        .as_ref()
        .is_some_and(|lifecycle| lifecycle.has_commands())
    {
        warnings.push(
            "This dev container defines lifecycle commands that execute on the host or in the container. Review lifecycle commands before running untrusted repositories."
                .to_owned(),
        );
    }
    if config
        .devcontainer
        .user_env_probe
        .is_some_and(|probe| probe != crate::config::layer::LayerUserEnvProbe::None)
    {
        warnings.push(
            "This dev container enables userEnvProbe, which can run shell startup files in the container. Set userEnvProbe to \"none\" for untrusted repositories."
                .to_owned(),
        );
    }
    if config.devcontainer.privileged {
        warnings.push(
            "This dev container requests privileged mode, which grants broad container privileges. Remove privileged=true before running untrusted repositories."
                .to_owned(),
        );
    }
    if !config.devcontainer.cap_add.is_empty() {
        warnings.push(format!(
            "This dev container adds Linux capabilities ({}), which can weaken container isolation. Remove capAdd entries before running untrusted repositories.",
            config.devcontainer.cap_add.join(", ")
        ));
    }
    if !config.devcontainer.security_opt.is_empty() {
        warnings.push(format!(
            "This dev container sets Docker security options ({}), which can weaken container isolation. Remove securityOpt entries before running untrusted repositories.",
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
    if has_extra_bind_mounts(config) {
        warnings.push(
            "This dev container declares additional bind mounts that can expose host files. Review mount sources or remove extra bind mounts before running untrusted repositories."
                .to_owned(),
        );
    }
    if config.credentials.git.enabled
        && (config.credentials.git.copy_user
            || config.credentials.git.copy_global_config
            || config.credentials.git.https == GitHttpsMode::HostHelper)
    {
        warnings.push(
            "Git credential forwarding is enabled; set [credentials.git].enabled = false before running untrusted repositories."
                .to_owned(),
        );
    }
    if config.credentials.git.enabled && config.credentials.git.ssh_agent != SshAgentMode::Off {
        warnings.push(
            "SSH agent forwarding is enabled; set [credentials.git].enabled = false or ssh_agent = \"off\" before running untrusted repositories."
                .to_owned(),
        );
    }
    if config.credentials.github.enabled
        && config.credentials.github.mode != GithubCredentialsMode::Off
    {
        warnings.push(
            "GitHub credential forwarding is enabled; set [credentials.github].enabled = false before running untrusted repositories."
                .to_owned(),
        );
    }
    warnings
}

fn has_extra_bind_mounts(config: &ResolvedConfig) -> bool {
    config
        .mounts
        .iter()
        .any(|mount| mount.mount_type == MountType::Bind)
        || config
            .devcontainer
            .mounts
            .iter()
            .any(devcontainer_mount_is_bind_or_unknown)
        || config
            .devcontainer
            .workspace_mount
            .as_ref()
            .is_some_and(|mount| {
                devcontainer_mount_is_bind_or_unknown(
                    &crate::config::layer::LayerDevcontainerMount::String(mount.clone()),
                )
            })
}

fn devcontainer_mount_is_bind_or_unknown(
    mount: &crate::config::layer::LayerDevcontainerMount,
) -> bool {
    match devcontainer_mount_type(mount) {
        Ok(mount_type) => mount_type == MountType::Bind,
        Err(_) => true,
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ConfigMergeInput, layer::LayerDevcontainerCompose},
        docker::{
            build::DockerBuildOptions,
            resource::DockerResources,
            user::{EffectiveUsers, UidGidSyncPlan},
        },
        up::types::UpPlan,
    };

    #[test]
    fn effective_startup_command_uses_compose_overrides() {
        let image_startup = ImageStartupCommand {
            entrypoint: vec!["/image-entrypoint.sh".to_owned()],
            command: vec!["image-cmd".to_owned()],
        };
        let service = ComposeConfigService {
            entrypoint: Some(vec!["/service-entrypoint.sh".to_owned()]),
            command: Some(vec!["service-cmd".to_owned()]),
            ..ComposeConfigService::default()
        };

        let startup = effective_startup_command(image_startup, Some(&service));

        assert_eq!(
            startup.entrypoint,
            vec!["/service-entrypoint.sh".to_owned()]
        );
        assert_eq!(startup.command, vec!["service-cmd".to_owned()]);
    }

    #[test]
    fn effective_startup_command_falls_back_to_image_parts_independently() {
        let image_startup = ImageStartupCommand {
            entrypoint: vec!["/image-entrypoint.sh".to_owned()],
            command: vec!["image-cmd".to_owned()],
        };
        let service = ComposeConfigService {
            command: Some(vec!["service-cmd".to_owned()]),
            ..ComposeConfigService::default()
        };

        let startup = effective_startup_command(image_startup, Some(&service));

        assert_eq!(startup.entrypoint, vec!["/image-entrypoint.sh".to_owned()]);
        assert_eq!(startup.command, vec!["service-cmd".to_owned()]);
    }

    #[test]
    fn generated_override_semantic_hash_changes_for_meaningful_override_change() {
        let first = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("stable-hash", "decune/test:first", "1.0.0"),
            &[],
            None,
        )
        .unwrap();
        let second = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("stable-hash", "decune/test:first", "1.0.1"),
            &[],
            None,
        )
        .unwrap();

        assert_ne!(first.content_hash, second.content_hash);
    }

    #[test]
    fn generated_override_semantic_hash_excludes_hash_derived_values() {
        let first = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("first-hash", "decune/test:first-hash", "1.0.0"),
            &[],
            None,
        )
        .unwrap();
        let second = compose_generated_override_hash_input(
            PathBuf::from("/state/compose.override.yaml"),
            &compose_hash_plan("second-hash", "decune/test:second-hash", "1.0.0"),
            &[],
            None,
        )
        .unwrap();

        assert_eq!(first.content_hash, second.content_hash);
    }

    fn compose_hash_plan(config_hash: &str, image: &str, version: &str) -> UpPlan {
        let mut config = ResolvedConfig::default();
        config.devcontainer.source = Some(ResolvedDevcontainerSource::Compose(
            LayerDevcontainerCompose {
                files: vec!["compose.yaml".to_owned()],
                service: "app".to_owned(),
                run_services: None,
            },
        ));

        UpPlan {
            image: image.to_owned(),
            base_image: "alpine:3.20".to_owned(),
            build_context: None,
            build_options: DockerBuildOptions::default(),
            feature_install: None,
            feature_build_context_dir: None,
            uid_gid_sync_build_context_dir: None,
            resources: DockerResources {
                container_name: "decune-test".to_owned(),
                image_tag: image.to_owned(),
                workspace_volume_name: "decune-test-workspace".to_owned(),
                labels: BTreeMap::from([
                    ("decune.managed".to_owned(), "true".to_owned()),
                    ("decune.workspace_id".to_owned(), "workspace-id".to_owned()),
                    ("decune.config_hash".to_owned(), config_hash.to_owned()),
                    ("decune.version".to_owned(), version.to_owned()),
                    (
                        "com.docker.compose.project".to_owned(),
                        "user-project".to_owned(),
                    ),
                ]),
                config_hash: config_hash.to_owned(),
            },
            pre_uid_gid_sync_resources: None,
            compose_project: None,
            config_layers: ConfigMergeInput::default(),
            config,
            sensitive_container_env: Default::default(),
            sensitive_build_args: Default::default(),
            compose_interpolation_env: Default::default(),
            compose_interpolation_redactions: Vec::new(),
            effective_users: EffectiveUsers::root(),
            uid_gid_sync_plan: UidGidSyncPlan::default(),
            workspace_folder: "/workspace".to_owned(),
            mounts: Vec::new(),
            forward_ports: Vec::new(),
            ignored_detached_forwarding: false,
        }
    }
}

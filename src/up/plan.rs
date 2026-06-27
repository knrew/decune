use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, FeatureLockHashEntry, config_hash,
        layer::LayerRunArg,
        load::load_config_file,
        resolve_config,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
        variables::{
            SensitiveEnvMap, VariableContext, expand_string_map_tracked, expand_variables,
            references_remote_user_home_variable, references_remote_user_variable,
        },
    },
    devcontainer::{
        features::{
            FeatureRef, parse_feature_ref_from_devcontainer_dir, read_feature_lock_file,
            resolve_locked_feature_ref,
        },
        json::DevcontainerJson,
        metadata::parse_metadata,
    },
    docker::{
        build::{
            DockerBuildOptions, ResolvedBuildContext, build_hash_input, resolve_build_context,
        },
        ports::resolve_forward_ports,
        resource::DockerResources,
        user::{EffectiveUsers, UidGidSyncPlan},
    },
    runtime::compose_cli::ComposeProjectPlan,
    workspace::Workspace,
};

use super::mounts::default_workspace_folder;
use super::{
    ForwardingResolution, MountResolution, UpPlan, UpPlanResolution, WorkspaceLocation,
    WorkspaceLocationValidation, mount_hash_inputs, resolve_workspace_location,
    static_mount_variable_context, static_uid_gid_sync_hash_input,
    workspace_mount_plan_from_resolved,
};

const FEATURE_ENTRYPOINT_SHIM_HASH_VERSION: &str = "3";
const FEATURE_LAYER_HASH_VERSION: &str = "2";

#[cfg(test)]
pub(crate) fn build_up_plan(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(ForwardingResolution::Resolve, false, false),
    )
}

#[cfg(test)]
pub(crate) fn build_up_plan_with_update_features(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(ForwardingResolution::Resolve, update_features, false),
    )
}

#[cfg(test)]
pub(crate) fn build_up_plan_with_image_metadata(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata,
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(ForwardingResolution::Resolve, false, false),
    )
}

pub(super) fn build_preliminary_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
    skip_global_config: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::DeferConfigMounts,
        UpPlanResolution::new(forwarding_resolution, update_features, skip_global_config),
    )
}

pub(crate) fn build_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
    skip_global_config: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(forwarding_resolution, update_features, skip_global_config),
    )
}

pub(crate) fn build_read_only_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
    skip_global_config: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::ReadOnly,
        UpPlanResolution::new(forwarding_resolution, update_features, skip_global_config),
    )
}

pub(super) fn build_up_plan_with_image_metadata_and_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
    ignored_image_metadata_forwarding: bool,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata,
        ignored_image_metadata_forwarding,
        MountResolution::Resolve,
        resolution,
    )
}

pub(super) fn rebuild_up_plan_with_image_metadata_layers(
    workspace: &Workspace,
    mut plan: UpPlan,
    image_metadata: Vec<ConfigLayer>,
    ignored_image_metadata_forwarding: bool,
    mount_resolution: MountResolution,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    let devcontainer_file = PathBuf::from(
        plan.resources
            .labels
            .get("devcontainer.config_file")
            .context("Up plan is missing devcontainer.config_file label")?,
    );
    plan.config_layers.image_metadata = image_metadata;
    plan.config_layers.feature_metadata.clear();
    let config_layers = plan.config_layers.clone();
    let workspace_validation = match mount_resolution {
        MountResolution::Resolve | MountResolution::ReadOnly => {
            WorkspaceLocationValidation::ConfigResolved
        }
        MountResolution::DeferConfigMounts => WorkspaceLocationValidation::Preliminary,
    };
    let mut config = resolve_config(config_layers.clone());
    let static_expansion = expand_static_plan_fields(
        workspace,
        &devcontainer_file,
        &mut config,
        workspace_validation,
        mount_resolution,
    )?;
    let mount_variables = static_mount_variable_context(
        workspace,
        &static_expansion.workspace_location.workspace_folder,
        &config,
    );
    let compose_project = compose_project_plan(workspace, &devcontainer_file, &config)?;
    let mount_plan = workspace_mount_plan_from_resolved(
        static_expansion.workspace_location.workspace_mount.clone(),
        workspace.root(),
        &config,
        &mount_variables,
        mount_resolution,
        workspace.paths().state_dir(),
    )?;
    let mounts = mount_plan.mounts;
    let mut hash_input = ConfigHashInput::new(&config);
    if let Some(context) = &static_expansion.build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    hash_input.sensitive_build_arg_keys = static_expansion
        .sensitive_build_args
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    if let Some(compose_project) = &compose_project {
        hash_input.compose_files = compose_project.config_hash_files().to_vec();
    }
    hash_input.feature_locks = feature_lock_hash_inputs(
        workspace,
        &devcontainer_file,
        &config,
        resolution.update_features,
    )?;
    if mount_resolution.resolves_config_mounts() {
        hash_input.resolved_mounts = mount_hash_inputs(&mounts);
    }
    add_internal_hash_versions(&mut hash_input, &config);
    hash_input.uid_gid_sync =
        static_uid_gid_sync_hash_input(&config_layers, config.devcontainer.update_remote_user_uid);
    let hash = config_hash(&hash_input);
    let resources =
        DockerResources::from_workspace(workspace, hash, devcontainer_file.display().to_string());
    let base_image = base_image_source(&config, &resources, &UidGidSyncPlan::default())?;
    let image = final_image_source(&config, &resources, &UidGidSyncPlan::default())?;
    let forward_ports = match resolution.forwarding {
        ForwardingResolution::Resolve => {
            validate_service_qualified_forward_ports(&config)?;
            resolve_forward_ports(&config.ports.entries)?
        }
        ForwardingResolution::IgnoreDetached => Vec::new(),
    };
    let ignored_detached_forwarding = resolution.forwarding == ForwardingResolution::IgnoreDetached
        && (plan.ignored_detached_forwarding
            || ignored_image_metadata_forwarding
            || !config.ports.entries.is_empty());

    Ok(UpPlan {
        image,
        base_image,
        build_context: static_expansion.build_context,
        build_options: static_expansion.build_options,
        feature_install: None,
        feature_build_context_dir: None,
        uid_gid_sync_build_context_dir: None,
        resources,
        pre_uid_gid_sync_resources: None,
        compose_project,
        config_layers,
        config,
        sensitive_container_env: Default::default(),
        sensitive_build_args: static_expansion.sensitive_build_args,
        compose_interpolation_env: Default::default(),
        compose_interpolation_redactions: Vec::new(),
        effective_users: EffectiveUsers::root(),
        uid_gid_sync_plan: UidGidSyncPlan::default(),
        workspace_folder: static_expansion.workspace_location.workspace_folder,
        mounts,
        dotfile_skeletons: mount_plan.dotfile_skeletons,
        forward_ports,
        ignored_detached_forwarding,
    })
}

fn build_up_plan_inner(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
    ignored_image_metadata_forwarding: bool,
    mount_resolution: MountResolution,
    resolution: UpPlanResolution,
) -> Result<UpPlan> {
    let devcontainer_json = DevcontainerJson::load(workspace.root(), explicit_config_path)?;
    let metadata = parse_metadata(devcontainer_json.value().clone())?;
    let devcontainer_layer = match resolution.forwarding {
        ForwardingResolution::Resolve => metadata.to_config_layer()?,
        ForwardingResolution::IgnoreDetached => metadata.to_config_layer_without_forward_ports()?,
    };
    let project_raw = load_config_file(workspace.paths().project_config_path())?;
    let use_global_config =
        !resolution.skip_global_config && project_raw.use_global_config.unwrap_or(true);
    let global_layer = if use_global_config {
        Some(ConfigLayer::from_raw_decune_with_origin(
            load_config_file(workspace.paths().global_config_path())?,
            crate::config::path::ConfigPathOrigin::Global,
        ))
    } else {
        None
    };
    let project_layer = ConfigLayer::from_raw_decune_with_origin(
        project_raw,
        crate::config::path::ConfigPathOrigin::Project,
    );
    let config_layers = ConfigMergeInput {
        image_metadata,
        global: global_layer,
        devcontainer: Some(devcontainer_layer),
        project: Some(project_layer),
        cli: Some(cli_layer),
        ..ConfigMergeInput::default()
    };
    let workspace_validation = match mount_resolution {
        MountResolution::Resolve | MountResolution::ReadOnly => {
            WorkspaceLocationValidation::ConfigResolved
        }
        MountResolution::DeferConfigMounts => WorkspaceLocationValidation::Preliminary,
    };
    let mut config = resolve_config(config_layers.clone());
    let static_expansion = expand_static_plan_fields(
        workspace,
        devcontainer_json.path(),
        &mut config,
        workspace_validation,
        mount_resolution,
    )?;
    let mount_variables = static_mount_variable_context(
        workspace,
        &static_expansion.workspace_location.workspace_folder,
        &config,
    );
    let compose_project = compose_project_plan(workspace, devcontainer_json.path(), &config)?;
    let mount_plan = workspace_mount_plan_from_resolved(
        static_expansion.workspace_location.workspace_mount.clone(),
        workspace.root(),
        &config,
        &mount_variables,
        mount_resolution,
        workspace.paths().state_dir(),
    )?;
    let mounts = mount_plan.mounts;
    let mut hash_input = ConfigHashInput::new(&config);
    if let Some(context) = &static_expansion.build_context {
        hash_input.build = Some(build_hash_input(context)?);
    }
    hash_input.sensitive_build_arg_keys = static_expansion
        .sensitive_build_args
        .iter()
        .map(|(key, _)| key.clone())
        .collect();
    if let Some(compose_project) = &compose_project {
        hash_input.compose_files = compose_project.config_hash_files().to_vec();
    }
    hash_input.feature_locks = feature_lock_hash_inputs(
        workspace,
        devcontainer_json.path(),
        &config,
        resolution.update_features,
    )?;
    if mount_resolution.resolves_config_mounts() {
        hash_input.resolved_mounts = mount_hash_inputs(&mounts);
    }
    add_internal_hash_versions(&mut hash_input, &config);
    hash_input.uid_gid_sync =
        static_uid_gid_sync_hash_input(&config_layers, config.devcontainer.update_remote_user_uid);
    let hash = config_hash(&hash_input);
    let resources = DockerResources::from_workspace(
        workspace,
        hash,
        devcontainer_json.path().display().to_string(),
    );
    let base_image = base_image_source(&config, &resources, &UidGidSyncPlan::default())?;
    let image = final_image_source(&config, &resources, &UidGidSyncPlan::default())?;
    let forward_ports = match resolution.forwarding {
        ForwardingResolution::Resolve => {
            validate_service_qualified_forward_ports(&config)?;
            resolve_forward_ports(&config.ports.entries)?
        }
        ForwardingResolution::IgnoreDetached => Vec::new(),
    };
    let ignored_detached_forwarding = resolution.forwarding == ForwardingResolution::IgnoreDetached
        && (ignored_image_metadata_forwarding
            || !metadata.forward_ports().is_empty()
            || !config.ports.entries.is_empty());

    Ok(UpPlan {
        image,
        base_image,
        build_context: static_expansion.build_context,
        build_options: static_expansion.build_options,
        feature_install: None,
        feature_build_context_dir: None,
        uid_gid_sync_build_context_dir: None,
        resources,
        pre_uid_gid_sync_resources: None,
        compose_project,
        config_layers,
        config,
        sensitive_container_env: Default::default(),
        sensitive_build_args: static_expansion.sensitive_build_args,
        compose_interpolation_env: Default::default(),
        compose_interpolation_redactions: Vec::new(),
        effective_users: EffectiveUsers::root(),
        uid_gid_sync_plan: UidGidSyncPlan::default(),
        workspace_folder: static_expansion.workspace_location.workspace_folder,
        mounts,
        dotfile_skeletons: mount_plan.dotfile_skeletons,
        forward_ports,
        ignored_detached_forwarding,
    })
}

pub(super) struct StaticPlanExpansion {
    pub(super) workspace_location: WorkspaceLocation,
    pub(super) build_context: Option<ResolvedBuildContext>,
    pub(super) build_options: DockerBuildOptions,
    pub(super) sensitive_build_args: SensitiveEnvMap,
}

pub(super) fn expand_static_plan_fields(
    workspace: &Workspace,
    devcontainer_file: &Path,
    config: &mut ResolvedConfig,
    workspace_validation: WorkspaceLocationValidation,
    mount_resolution: MountResolution,
) -> Result<StaticPlanExpansion> {
    let preliminary_variables =
        static_mount_variable_context(workspace, &default_workspace_folder(workspace), config);
    expand_static_user_fields(config, &preliminary_variables)?;
    let workspace_location = resolve_workspace_location(
        workspace,
        config,
        workspace_validation,
        mount_resolution,
        |workspace_folder| static_mount_variable_context(workspace, workspace_folder, config),
    )?;
    if should_store_static_workspace_folder(config)? {
        config.devcontainer.workspace_folder = Some(workspace_location.workspace_folder.clone());
    }
    let mount_variables =
        static_mount_variable_context(workspace, &workspace_location.workspace_folder, config);
    let sensitive_build_args = expand_static_devcontainer_fields(config, &mount_variables)?;
    let (build_context, mut build_options) =
        dockerfile_build_input(workspace.root(), devcontainer_file, config)?;
    build_options.build_arg_redactions = sensitive_build_args.redaction_values();

    Ok(StaticPlanExpansion {
        workspace_location,
        build_context,
        build_options,
        sensitive_build_args,
    })
}

fn should_store_static_workspace_folder(config: &ResolvedConfig) -> Result<bool> {
    match config.devcontainer.workspace_folder.as_deref() {
        Some(workspace_folder) => Ok(!references_remote_user_variable(workspace_folder)?),
        None => Ok(true),
    }
}

fn expand_static_user_fields(
    config: &mut ResolvedConfig,
    variables: &VariableContext,
) -> Result<()> {
    if let Some(remote_user) = &mut config.devcontainer.remote_user {
        *remote_user =
            expand_variables(remote_user, variables).context("Failed to expand remoteUser")?;
    }
    if let Some(container_user) = &mut config.devcontainer.container_user {
        *container_user = expand_variables(container_user, variables)
            .context("Failed to expand containerUser")?;
    }

    Ok(())
}

fn expand_static_devcontainer_fields(
    config: &mut ResolvedConfig,
    variables: &VariableContext,
) -> Result<SensitiveEnvMap> {
    let mut sensitive_build_args = SensitiveEnvMap::default();

    if let Some(ResolvedDevcontainerSource::Dockerfile(build)) = &mut config.devcontainer.source {
        reject_runtime_user_home_in_build_value(build.args.values(), "build.args")?;
        reject_runtime_user_home_in_build_value(build.target.iter(), "build.target")?;
        reject_runtime_user_home_in_build_value(build.cache_from.iter(), "build.cacheFrom")?;
        let static_remote_user_available = config.devcontainer.remote_user.is_some()
            || config.devcontainer.container_user.is_some();
        if !static_remote_user_available {
            reject_remote_user_in_build_value(build.args.values(), "build.args")?;
            reject_remote_user_in_build_value(build.target.iter(), "build.target")?;
            reject_remote_user_in_build_value(build.cache_from.iter(), "build.cacheFrom")?;
        }

        let expanded_args = expand_string_map_tracked(&build.args, variables)
            .context("Failed to expand build.args")?;
        build.args = expanded_args.values;
        sensitive_build_args = expanded_args.sensitive;

        if let Some(target) = &mut build.target {
            *target =
                expand_variables(target, variables).context("Failed to expand build.target")?;
        }
        for cache in &mut build.cache_from {
            *cache =
                expand_variables(cache, variables).context("Failed to expand build.cacheFrom")?;
        }
    }

    expand_runtime_independent_string_values(
        &mut config.devcontainer.cap_add,
        variables,
        "runArgs value",
    )?;
    expand_runtime_independent_string_values(
        &mut config.devcontainer.security_opt,
        variables,
        "runArgs value",
    )?;
    expand_runtime_independent_run_args(&mut config.devcontainer.run_args, variables)?;

    Ok(sensitive_build_args)
}

pub(super) fn expand_runtime_devcontainer_fields(
    config: &mut ResolvedConfig,
    variables: &VariableContext,
) -> Result<()> {
    expand_string_values(&mut config.devcontainer.cap_add, variables, "runArgs value")?;
    expand_string_values(
        &mut config.devcontainer.security_opt,
        variables,
        "runArgs value",
    )?;
    expand_run_args(&mut config.devcontainer.run_args, variables)
}

fn reject_runtime_user_home_in_build_value<'a>(
    values: impl IntoIterator<Item = &'a String>,
    field: &str,
) -> Result<()> {
    for value in values {
        if references_remote_user_home_variable(value)? {
            bail!(
                "{field} must not reference ${{remoteUserHome}} because it is resolved from the runtime container passwd database after the image is built"
            );
        }
    }

    Ok(())
}

fn reject_remote_user_in_build_value<'a>(
    values: impl IntoIterator<Item = &'a String>,
    field: &str,
) -> Result<()> {
    for value in values {
        if references_remote_user_variable(value)? {
            bail!(
                "{field} must not reference ${{remoteUser}} unless remoteUser or containerUser is configured before the Dockerfile build"
            );
        }
    }

    Ok(())
}

fn expand_runtime_independent_run_args(
    run_args: &mut [LayerRunArg],
    variables: &VariableContext,
) -> Result<()> {
    for run_arg in run_args {
        match run_arg {
            LayerRunArg::AddHost(value)
            | LayerRunArg::Dns(value)
            | LayerRunArg::DnsSearch(value)
            | LayerRunArg::Passthrough { value, .. } => {
                if !references_remote_user_variable(value)? {
                    *value = expand_variables(value, variables)
                        .context("Failed to expand runArgs value")?;
                }
            }
        }
    }

    Ok(())
}

fn expand_run_args(run_args: &mut [LayerRunArg], variables: &VariableContext) -> Result<()> {
    for run_arg in run_args {
        match run_arg {
            LayerRunArg::AddHost(value)
            | LayerRunArg::Dns(value)
            | LayerRunArg::DnsSearch(value)
            | LayerRunArg::Passthrough { value, .. } => {
                *value =
                    expand_variables(value, variables).context("Failed to expand runArgs value")?;
            }
        }
    }

    Ok(())
}

fn expand_runtime_independent_string_values(
    values: &mut [String],
    variables: &VariableContext,
    field: &str,
) -> Result<()> {
    for value in values {
        if !references_remote_user_variable(value)? {
            *value = expand_variables(value, variables)
                .with_context(|| format!("Failed to expand {field}"))?;
        }
    }

    Ok(())
}

fn expand_string_values(
    values: &mut [String],
    variables: &VariableContext,
    field: &str,
) -> Result<()> {
    for value in values {
        *value = expand_variables(value, variables)
            .with_context(|| format!("Failed to expand {field}"))?;
    }

    Ok(())
}

fn compose_project_plan(
    workspace: &Workspace,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
) -> Result<Option<ComposeProjectPlan>> {
    let Some(ResolvedDevcontainerSource::Compose(compose)) = &config.devcontainer.source else {
        return Ok(None);
    };
    let devcontainer_dir = devcontainer_file.parent().ok_or_else(|| {
        anyhow::anyhow!(
            "Failed to resolve devcontainer metadata directory: {}",
            devcontainer_file.display()
        )
    })?;

    ComposeProjectPlan::resolve(workspace, devcontainer_dir, &compose.files).map(Some)
}

fn validate_service_qualified_forward_ports(config: &ResolvedConfig) -> Result<()> {
    if matches!(
        config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Compose(_))
    ) {
        return Ok(());
    }

    if let Some(port) = config
        .ports
        .entries
        .iter()
        .find(|port| port.service.is_some())
    {
        let service = port.service.as_deref().unwrap_or_default();
        bail!(
            "Service-qualified port forwarding is only supported in Docker Compose mode: {service}:{}",
            port.container
        );
    }

    Ok(())
}

pub(super) fn feature_lock_hash_inputs(
    workspace: &Workspace,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
    update_features: bool,
) -> Result<Vec<FeatureLockHashEntry>> {
    if config.features.is_empty() {
        return Ok(Vec::new());
    }

    let devcontainer_dir = devcontainer_file.parent().with_context(|| {
        format!(
            "Failed to resolve devcontainer directory for {}",
            devcontainer_file.display()
        )
    })?;
    let references = config
        .features
        .iter()
        .map(|feature| {
            parse_feature_ref_from_devcontainer_dir(&feature.id, devcontainer_dir)
                .with_context(|| format!("Failed to parse Feature ref: {}", feature.id))
        })
        .collect::<Result<Vec<_>>>()?;

    if update_features {
        return Ok(Vec::new());
    }

    let lock_path = workspace.root().join(".decune").join("features.lock.toml");
    let lock = read_feature_lock_file(&lock_path)?;
    let mut entries = Vec::new();

    for reference in references {
        let _resolved = resolve_locked_feature_ref(&reference, &lock, false);
        let canonical_id = reference.canonical_id().to_owned();

        if let FeatureRef::Oci(reference) = reference
            && let Some(digest) = lock.digest_for_reference(&reference)
        {
            entries.push(FeatureLockHashEntry {
                feature_id: canonical_id,
                digest: digest.to_owned(),
            });
        }
    }

    Ok(entries)
}

pub(super) fn add_internal_hash_versions(input: &mut ConfigHashInput<'_>, config: &ResolvedConfig) {
    if !config.features.is_empty() {
        input.internal_versions.insert(
            "feature_layer".to_owned(),
            FEATURE_LAYER_HASH_VERSION.to_owned(),
        );
    }
    if !config.devcontainer.entrypoints.is_empty() {
        input.internal_versions.insert(
            "feature_entrypoint_shim".to_owned(),
            FEATURE_ENTRYPOINT_SHIM_HASH_VERSION.to_owned(),
        );
    }
}

pub(super) fn final_image_source(
    config: &ResolvedConfig,
    resources: &DockerResources,
    uid_gid_sync_plan: &UidGidSyncPlan,
) -> Result<String> {
    if config_requires_workspace_layer(config)
        || uid_gid_sync_plan_requires_layer(uid_gid_sync_plan)
    {
        return Ok(resources.image_tag.clone());
    }

    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        Some(ResolvedDevcontainerSource::Compose(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

pub(super) fn base_image_source(
    config: &ResolvedConfig,
    resources: &DockerResources,
    _uid_gid_sync_plan: &UidGidSyncPlan,
) -> Result<String> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Image(image)) => Ok(image.clone()),
        Some(ResolvedDevcontainerSource::Dockerfile(_))
            if config_requires_workspace_layer(config) =>
        {
            Ok(format!("{}-base", resources.image_tag))
        }
        Some(ResolvedDevcontainerSource::Dockerfile(_)) => Ok(resources.image_tag.clone()),
        Some(ResolvedDevcontainerSource::Compose(_)) => Ok(resources.image_tag.clone()),
        None => bail!("Devcontainer image is required"),
    }
}

fn dockerfile_build_input(
    workspace_root: &Path,
    devcontainer_file: &Path,
    config: &ResolvedConfig,
) -> Result<(Option<ResolvedBuildContext>, DockerBuildOptions)> {
    match &config.devcontainer.source {
        Some(ResolvedDevcontainerSource::Dockerfile(build)) => Ok((
            Some(resolve_build_context(
                workspace_root,
                devcontainer_file,
                build,
            )?),
            DockerBuildOptions {
                build_args: build.args.clone(),
                options: build.options.clone(),
                target: build.target.clone(),
                cache_from: build.cache_from.clone(),
                ..DockerBuildOptions::default()
            },
        )),
        _ => Ok((None, DockerBuildOptions::default())),
    }
}

pub(super) fn config_requires_workspace_layer(config: &ResolvedConfig) -> bool {
    !config.features.is_empty() || !config.devcontainer.entrypoints.is_empty()
}

fn uid_gid_sync_plan_requires_layer(plan: &UidGidSyncPlan) -> bool {
    matches!(plan, UidGidSyncPlan::Sync { .. })
}

#[cfg(test)]
mod tests {
    use std::{fs, net::TcpListener, path::PathBuf};

    use crate::{
        config::{
            ConfigLayer,
            layer::LayerRunArg,
            resolved::{ResolvedDevcontainerSource, ResolvedPublishPort},
            types::{MountType, PortProtocol},
        },
        docker::{
            mounts::{MountBindOptions, MountBindPropagation, MountVolumeOptions},
            ports::ResolvedForwardPort,
        },
        up::{
            ForwardingResolution,
            test_support::{config_hash_for_mount, test_mount, test_volume_mount, test_workspace},
        },
        workspace::Workspace,
    };

    use super::super::mounts::default_workspace_folder;
    use super::{
        build_preliminary_up_plan_with_forwarding_resolution, build_up_plan,
        build_up_plan_with_forwarding_resolution, build_up_plan_with_image_metadata,
        build_up_plan_with_update_features,
    };

    struct EnvVarGuard {
        name: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        fn capture(name: &'static str) -> Self {
            Self {
                name,
                previous: std::env::var_os(name),
            }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => unsafe {
                    std::env::set_var(self.name, value);
                },
                None => unsafe {
                    std::env::remove_var(self.name);
                },
            }
        }
    }

    fn write_devcontainer(workspace: &Workspace, contents: &str) {
        let devcontainer_dir = workspace.root().join(".devcontainer");
        fs::create_dir_all(&devcontainer_dir).unwrap();
        fs::write(devcontainer_dir.join("devcontainer.json"), contents).unwrap();
    }

    fn set_xdg_config_home(path: &std::path::Path) -> EnvVarGuard {
        let guard = EnvVarGuard::capture("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("XDG_CONFIG_HOME", path);
        }
        guard
    }

    #[test]
    fn build_up_plan_uses_image_source_and_default_workspace_mount() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Image Plan!");
        fs::create_dir(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"image":"alpine:3.20"}"#,
        )
        .unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, "alpine:3.20");
        assert_eq!(plan.base_image, "alpine:3.20");
        assert_eq!(plan.workspace_folder, "/workspaces/Image Plan!");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(root.to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspaces/Image Plan!");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
        assert!(!plan.mounts[0].read_only);
    }

    #[test]
    fn build_up_plan_keeps_global_layer_by_default() {
        let config_home = tempfile::tempdir().unwrap();
        let _guard = set_xdg_config_home(config_home.path());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Global Config Default");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(&workspace, r#"{"image":"alpine:3.20"}"#);

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert!(plan.config_layers.global.is_some());
    }

    #[test]
    fn project_config_can_skip_global_layer() {
        let config_home = tempfile::tempdir().unwrap();
        let _guard = set_xdg_config_home(config_home.path());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Project Global Opt Out");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(&workspace, r#"{"image":"alpine:3.20"}"#);
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            "version = 1\nuse_global_config = false\n",
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert!(plan.config_layers.global.is_none());
    }

    #[test]
    fn cli_skip_global_config_can_skip_global_layer() {
        let config_home = tempfile::tempdir().unwrap();
        let _guard = set_xdg_config_home(config_home.path());
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Cli Global Opt Out");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(&workspace, r#"{"image":"alpine:3.20"}"#);

        let plan = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::Resolve,
            false,
            true,
        )
        .unwrap();

        assert!(plan.config_layers.global.is_none());
    }

    #[test]
    fn build_up_plan_treats_default_workspace_folder_as_literal() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Project ${unknown}");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(&workspace, r#"{"image":"alpine:3.20"}"#);

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspaces/Project ${unknown}");
        assert_eq!(plan.mounts[0].target, "/workspaces/Project ${unknown}");
    }

    #[test]
    fn build_up_plan_expands_build_args_and_hashes_local_env_values() {
        let env_name = "DECUNE_TEST_PLAN_BUILD_ARG_SCOPE";
        let _guard = EnvVarGuard::capture(env_name);
        unsafe {
            std::env::remove_var(env_name);
        }

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Arg Variables");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        fs::write(
            devcontainer_dir.join("Dockerfile"),
            "FROM alpine\nARG VARIANT\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            &format!(
                r#"
                {{
                  "build": {{
                    "dockerfile": "Dockerfile",
                    "args": {{
                      "VARIANT": "${{localEnv:{env_name}:bookworm}}"
                    }},
                    "target": "stage-${{localWorkspaceFolderBasename}}",
                    "cacheFrom": "type=registry,ref=example.test/${{localWorkspaceFolderBasename}}:cache"
                  }}
                }}
                "#,
            ),
        );

        let defaulted = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(
            defaulted
                .build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("bookworm")
        );
        assert!(!defaulted.sensitive_build_args.contains_key("VARIANT"));
        assert_eq!(
            defaulted.build_options.target.as_deref(),
            Some("stage-Build Arg Variables")
        );
        assert_eq!(
            defaulted.build_options.cache_from,
            vec!["type=registry,ref=example.test/Build Arg Variables:cache"]
        );

        unsafe {
            std::env::set_var(env_name, "secret-bookworm");
        }
        let from_env = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(
            from_env
                .build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("secret-bookworm")
        );
        assert!(from_env.sensitive_build_args.contains_key("VARIANT"));
        assert!(
            from_env
                .build_options
                .build_arg_redactions
                .iter()
                .any(|value| value == "secret-bookworm")
        );
        assert_ne!(
            defaulted.resources.config_hash,
            from_env.resources.config_hash
        );

        unsafe {
            std::env::set_var(env_name, "secret-trixie");
        }
        let changed_env = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_ne!(
            from_env.resources.config_hash,
            changed_env.resources.config_hash
        );
    }

    #[test]
    fn build_up_plan_expands_workspace_folder_variables_before_validation() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Workspace Variables");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "/workspaces/${localWorkspaceFolderBasename}"
            }
            "#,
        );
        let basename = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(basename.workspace_folder, "/workspaces/Workspace Variables");

        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "${containerWorkspaceFolder}/subdir"
            }
            "#,
        );
        let container_workspace = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        assert_eq!(
            container_workspace.workspace_folder,
            "/workspaces/Workspace Variables/subdir"
        );
    }

    #[test]
    fn build_up_plan_expands_user_fields_and_run_args_values() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("User Run Args");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "remoteUser": "${localWorkspaceFolderBasename}-remote",
              "containerUser": "${localWorkspaceFolderBasename}-container",
              "runArgs": [
                "--cap-add=SYS_${localWorkspaceFolderBasename}",
                "--security-opt", "label=${localWorkspaceFolderBasename}",
                "--add-host", "api.${localWorkspaceFolderBasename}:127.0.0.1",
                "--dns", "dns-${localWorkspaceFolderBasename}",
                "--dns-search=${localWorkspaceFolderBasename}.test",
                "--hostname", "host-${localWorkspaceFolderBasename}"
              ]
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.config.devcontainer.remote_user.as_deref(),
            Some("User Run Args-remote")
        );
        assert_eq!(
            plan.config.devcontainer.container_user.as_deref(),
            Some("User Run Args-container")
        );
        assert_eq!(plan.config.devcontainer.cap_add, vec!["SYS_User Run Args"]);
        assert_eq!(
            plan.config.devcontainer.security_opt,
            vec!["label=User Run Args"]
        );
        assert_eq!(
            plan.config.devcontainer.run_args,
            vec![
                LayerRunArg::AddHost("api.User Run Args:127.0.0.1".to_owned()),
                LayerRunArg::Dns("dns-User Run Args".to_owned()),
                LayerRunArg::DnsSearch("User Run Args.test".to_owned()),
                LayerRunArg::Passthrough {
                    option: "--hostname".to_owned(),
                    value: "host-User Run Args".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn build_up_plan_keeps_runtime_user_dependent_fields_for_runtime_expansion() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Runtime User Fields");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "image": "alpine:3.20",
              "workspaceFolder": "${remoteUserHome}/src",
              "runArgs": [
                "--cap-add=SYS_${remoteUser}",
                "--security-opt", "label=${remoteUser}",
                "--add-host", "api.${remoteUser}:127.0.0.1",
                "--dns", "${remoteUser}",
                "--dns-search=${remoteUser}.test",
                "--hostname", "host-${remoteUser}"
              ]
            }
            "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/root/src");
        assert_eq!(
            plan.config.devcontainer.workspace_folder.as_deref(),
            Some("${remoteUserHome}/src")
        );
        assert_eq!(plan.config.devcontainer.cap_add, vec!["SYS_${remoteUser}"]);
        assert_eq!(
            plan.config.devcontainer.security_opt,
            vec!["label=${remoteUser}"]
        );
        assert_eq!(
            plan.config.devcontainer.run_args,
            vec![
                LayerRunArg::AddHost("api.${remoteUser}:127.0.0.1".to_owned()),
                LayerRunArg::Dns("${remoteUser}".to_owned()),
                LayerRunArg::DnsSearch("${remoteUser}.test".to_owned()),
                LayerRunArg::Passthrough {
                    option: "--hostname".to_owned(),
                    value: "host-${remoteUser}".to_owned(),
                },
            ]
        );
    }

    #[test]
    fn build_up_plan_rejects_remote_user_home_in_build_fields() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Remote User Home");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "REMOTE_HOME": "${remoteUserHome}"
                }
              }
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("build.args must not reference ${remoteUserHome}")
        );
    }

    #[test]
    fn build_up_plan_rejects_remote_user_in_build_fields_when_not_static() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Remote User");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "REMOTE_USER": "${remoteUser}"
                }
              }
            }
            "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("build.args must not reference ${remoteUser}")
        );
    }

    #[test]
    fn build_up_plan_expands_remote_user_in_build_fields_from_container_user() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Build Container User");
        fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();
        write_devcontainer(
            &workspace,
            r#"
            {
              "build": {
                "dockerfile": "Dockerfile",
                "args": {
                  "REMOTE_USER": "${remoteUser}"
                }
              },
              "containerUser": "node"
            }
            "#,
        );
        fs::write(
            workspace.root().join(".devcontainer").join("Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.build_options
                .build_args
                .get("REMOTE_USER")
                .map(String::as_str),
            Some("node")
        );
    }

    #[test]
    fn build_up_plan_adds_compose_project_plan_for_compose_source() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Compose Plan");
        fs::create_dir(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        fs::write(devcontainer_dir.join("compose.yaml"), "services: {}\n").unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"dockerComposeFile":"compose.yaml","service":"app"}"#,
        )
        .unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let compose = plan
            .compose_project
            .as_ref()
            .expect("compose source should produce a compose project plan");

        assert_eq!(
            compose.project_name(),
            format!("decune-compose-plan-{}", workspace.id())
        );
        assert_eq!(
            compose.generated_override_path(),
            workspace.paths().state_dir().join("compose.override.yaml")
        );
    }

    #[test]
    fn build_up_plan_config_hash_changes_when_compose_file_changes() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Compose Hash");
        fs::create_dir(&root).unwrap();
        let devcontainer_dir = root.join(".devcontainer");
        fs::create_dir(&devcontainer_dir).unwrap();
        let compose_file = devcontainer_dir.join("compose.yaml");
        fs::write(&compose_file, "services: {}\n").unwrap();
        fs::write(
            devcontainer_dir.join("devcontainer.json"),
            r#"{"dockerComposeFile":"compose.yaml","service":"app"}"#,
        )
        .unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::write(&compose_file, "services:\n  app:\n    image: alpine:3.20\n").unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.resources.config_hash, second.resources.config_hash);
    }
    #[test]
    fn build_up_plan_records_image_source_labels_and_workspace_mount() {
        let workspace = test_workspace("image-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceFolder": "/workspace"
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, "alpine:3.20");
        assert!(plan.build_context.is_none());
        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, "/workspaces/image-plan");
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
        assert!(!plan.mounts[0].read_only);
        assert!(matches!(
            plan.config.devcontainer.source,
            Some(ResolvedDevcontainerSource::Image(ref image)) if image == "alpine:3.20"
        ));
        assert_eq!(
            plan.resources.labels["devcontainer.config_file"],
            workspace
                .root()
                .join(".devcontainer/devcontainer.json")
                .display()
                .to_string()
        );
    }
    #[test]
    fn build_up_plan_includes_feature_lock_digest_in_config_hash() {
        let workspace = test_workspace("feature-lock-hash");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "features": {
            "ghcr.io/example/features/tool:1": {}
          }
        }
        "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/features.lock.toml"),
            r#"
version = 1

[[features]]
id = "ghcr.io/example/features/tool"
ref = "ghcr.io/example/features/tool:1"
digest = "sha256:locked"
"#,
        )
        .unwrap();

        let locked = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(baseline.resources.config_hash, locked.resources.config_hash);
    }
    #[test]
    fn build_up_plan_ignores_feature_lock_digest_when_features_are_updated() {
        let workspace = test_workspace("feature-lock-update-hash");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "features": {
            "ghcr.io/example/features/tool:1": {}
          }
        }
        "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/features.lock.toml"),
            r#"
version = 1

[[features]]
id = "ghcr.io/example/features/tool"
ref = "ghcr.io/example/features/tool:1"
digest = "sha256:locked"
"#,
        )
        .unwrap();

        let locked = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let updated =
            build_up_plan_with_update_features(&workspace, None, ConfigLayer::default(), true)
                .unwrap();

        assert_ne!(baseline.resources.config_hash, locked.resources.config_hash);
        assert_eq!(
            baseline.resources.config_hash,
            updated.resources.config_hash
        );
    }
    #[test]
    fn build_up_plan_rejects_invalid_feature_ref_with_ref_in_error() {
        let workspace = test_workspace("invalid-feature-ref");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "features": {
            "ghcr.io/features": {}
          }
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains("ghcr.io/features"), "{error:#}");
    }
    #[test]
    fn build_up_plan_separates_forward_ports_from_app_port_publish() {
        let workspace = test_workspace("port-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        let baseline = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "forwardPorts": [3000]
        }
        "#,
        );
        let forwarding = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "forwardPorts": [3000],
          "appPort": ["127.0.0.1:18080:8080"]
        }
        "#,
        );
        let published = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            forwarding.forward_ports,
            vec![ResolvedForwardPort {
                service: None,
                container: 3000,
                requested_host: 3000,
                host: 3000,
                host_ip: "127.0.0.1".to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: false,
                label: None,
            }]
        );
        assert!(forwarding.config.devcontainer.publish_ports.is_empty());
        assert_eq!(
            published.config.devcontainer.publish_ports,
            vec![ResolvedPublishPort {
                container: 8080,
                host: Some(18080),
                host_ip: Some("127.0.0.1".to_owned()),
                protocol: PortProtocol::Tcp,
            }]
        );
        assert_eq!(
            baseline.resources.config_hash,
            forwarding.resources.config_hash
        );
        assert_ne!(
            forwarding.resources.config_hash,
            published.resources.config_hash
        );
    }
    #[test]
    fn build_up_plan_rejects_service_qualified_forward_ports_without_compose_source() {
        let workspace = test_workspace("service-forward-port-image-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "forwardPorts": ["db:5432"]
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }
    #[test]
    fn build_up_plan_rejects_service_qualified_forward_ports_with_dockerfile_source() {
        let workspace = test_workspace("service-forward-port-dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine:3.20\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
        {
          "build": {
            "dockerfile": "Dockerfile"
          },
          "forwardPorts": ["db:5432"]
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }
    #[test]
    fn build_up_plan_rejects_service_qualified_decune_ports_without_compose_source() {
        let workspace = test_workspace("service-decune-port-image-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[ports]]
service = "db"
container = 5432
"#,
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(error.to_string().contains(
            "Service-qualified port forwarding is only supported in Docker Compose mode"
        ));
        assert!(error.to_string().contains("db:5432"));
    }
    #[test]
    fn detached_up_plan_keeps_config_hash_stable_when_forward_ports_are_ignored() {
        let workspace = test_workspace("detached-forward-port-hash-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "forwardPorts": [3000]
        }
        "#,
        );

        let attached = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        let detached = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::IgnoreDetached,
            false,
            false,
        )
        .unwrap();

        assert_eq!(
            attached.forward_ports,
            vec![ResolvedForwardPort {
                service: None,
                container: 3000,
                requested_host: 3000,
                host: 3000,
                host_ip: "127.0.0.1".to_owned(),
                protocol: PortProtocol::Tcp,
                require_local: false,
                label: None,
            }]
        );
        assert!(detached.forward_ports.is_empty());
        assert!(detached.ignored_detached_forwarding);
        assert_eq!(
            attached.resources.config_hash,
            detached.resources.config_hash
        );
    }
    #[test]
    fn detached_up_plan_ignores_forward_ports_without_binding_host_port() {
        let workspace = test_workspace("detached-port-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let host_port = listener.local_addr().unwrap().port();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            format!(
                r#"
version = 1

[[ports]]
container = 4321
host = {host_port}
require_local = true
"#
            ),
        )
        .unwrap();

        let plan = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::IgnoreDetached,
            false,
            false,
        )
        .unwrap();

        assert!(plan.forward_ports.is_empty());
        assert_eq!(plan.config.ports.entries.len(), 1);
        assert!(plan.ignored_detached_forwarding);
    }
    #[test]
    fn detached_up_plan_ignores_unsupported_devcontainer_forward_ports_before_conversion() {
        let workspace = test_workspace("detached-unsupported-forward-port-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "forwardPorts": ["db:5432"]
        }
        "#,
        );

        let plan = build_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::IgnoreDetached,
            false,
            false,
        )
        .unwrap();

        assert!(plan.forward_ports.is_empty());
        assert!(plan.config.ports.entries.is_empty());
        assert!(plan.ignored_detached_forwarding);
    }
    #[test]
    fn build_up_plan_rejects_workspace_mount_without_workspace_folder() {
        let workspace = test_workspace("workspace-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "workspaceFolder is required when workspaceMount is specified"
        );
    }
    #[test]
    fn preliminary_up_plan_defers_workspace_mount_without_workspace_folder() {
        let workspace = test_workspace("preliminary-workspace-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind"
        }
        "#,
        );

        let plan = build_preliminary_up_plan_with_forwarding_resolution(
            &workspace,
            None,
            ConfigLayer::default(),
            ForwardingResolution::Resolve,
            false,
            false,
        )
        .unwrap();

        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts[0].target, "/workspace");
    }
    #[test]
    fn build_up_plan_uses_explicit_workspace_folder_for_workspace_mount_variables() {
        let workspace = test_workspace("workspace-mount-variable-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=${containerWorkspaceFolder},type=bind",
          "workspaceFolder": "/workspace"
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspace");
        assert_eq!(plan.mounts.len(), 1);
        assert_eq!(
            plan.mounts[0].source.as_deref(),
            Some(workspace.root().to_str().unwrap())
        );
        assert_eq!(plan.mounts[0].target, plan.workspace_folder);
        assert_eq!(plan.mounts[0].mount_type, MountType::Bind);
    }
    #[test]
    fn build_up_plan_defers_workspace_folder_mount_target_check_until_runtime() {
        let workspace = test_workspace("workspace-folder-outside-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
          "workspaceFolder": "/other"
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/other");
        assert_eq!(plan.mounts[0].target, "/workspace");
    }
    #[test]
    fn build_up_plan_rejects_relative_workspace_folder() {
        let workspace = test_workspace("relative-workspace-folder-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceFolder": "workspace"
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert_eq!(
            error.to_string(),
            "workspaceFolder must be an absolute container path: workspace"
        );
    }
    #[test]
    fn build_up_plan_uses_explicit_workspace_folder_with_workspace_mount() {
        let workspace = test_workspace("workspace-folder-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=/workspace,type=bind",
          "workspaceFolder": "/workspace/app"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "project-cache"
target = "/opt/${containerWorkspaceFolderBasename}"
type = "volume"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/workspace/app");
        assert_eq!(plan.mounts[0].target, "/workspace");
        assert_eq!(plan.mounts[1].target, "/opt/app");
    }
    #[test]
    fn build_up_plan_rejects_mount_target_that_conflicts_with_workspace_mount() {
        let workspace = test_workspace("workspace-mount-conflict-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            format!(
                r#"
version = 1

[[mounts]]
source = "project-cache"
target = "{}"
type = "volume"
"#,
                default_workspace_folder(&workspace)
            ),
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target conflicts with workspace mount target")
        );
    }
    #[test]
    fn build_up_plan_rejects_mount_target_that_normalizes_to_workspace_mount() {
        let workspace = test_workspace("normalized-workspace-mount-conflict-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            format!(
                r#"
version = 1

[[mounts]]
source = "project-cache"
target = "{}/."
type = "volume"
"#,
                default_workspace_folder(&workspace)
            ),
        )
        .unwrap();

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Mount target conflicts with workspace mount target")
        );
    }
    #[test]
    fn build_up_plan_rejects_workspace_mount_under_reserved_decune_path() {
        let workspace = test_workspace("reserved-workspace-mount-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceMount": "source=${localWorkspaceFolder},target=/run/decune/workspace,type=bind",
          "workspaceFolder": "/run/decune/workspace"
        }
        "#,
        );

        let error = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap_err();
        let message = format!("{error:#}");

        assert!(message.contains("Mount target is reserved for decune internal use"));
    }
    #[test]
    fn build_up_plan_merges_image_metadata_and_includes_it_in_config_hash() {
        let workspace = test_workspace("image-metadata-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        let image_layer = ConfigLayer {
            devcontainer: Some(crate::config::layer::LayerDevcontainerMetadata {
                remote_user: Some("image-user".to_owned()),
                remote_env: [("FROM_IMAGE".to_owned(), "1".to_owned())].into(),
                ..crate::config::layer::LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        };
        let changed_image_layer = ConfigLayer {
            devcontainer: Some(crate::config::layer::LayerDevcontainerMetadata {
                remote_user: Some("image-user".to_owned()),
                remote_env: [("FROM_IMAGE".to_owned(), "2".to_owned())].into(),
                ..crate::config::layer::LayerDevcontainerMetadata::default()
            }),
            ..ConfigLayer::default()
        };

        let plan = build_up_plan_with_image_metadata(
            &workspace,
            None,
            ConfigLayer::default(),
            vec![image_layer],
        )
        .unwrap();
        let changed = build_up_plan_with_image_metadata(
            &workspace,
            None,
            ConfigLayer::default(),
            vec![changed_image_layer],
        )
        .unwrap();

        assert_eq!(
            plan.config.devcontainer.remote_user.as_deref(),
            Some("image-user")
        );
        assert_eq!(
            plan.config
                .devcontainer
                .remote_env
                .get("FROM_IMAGE")
                .map(String::as_str),
            Some("1")
        );
        assert_ne!(plan.resources.config_hash, changed.resources.config_hash);
    }
    #[test]
    fn build_up_plan_uses_dockerfile_source_and_build_context() {
        let workspace = test_workspace("dockerfile-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
        {
          "build": {
            "dockerfile": "Dockerfile",
            "args": {
              "VARIANT": "bookworm"
            },
            "options": [
              "--platform=linux/amd64",
              "--network",
              "host"
            ],
            "target": "dev",
            "cacheFrom": "type=registry,ref=example.test/cache:latest"
          }
        }
        "#,
        );

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.image, plan.resources.image_tag);
        let build_context = plan
            .build_context
            .expect("build context should be resolved");
        assert_eq!(
            build_context.context_dir,
            workspace.root().join(".devcontainer")
        );
        assert_eq!(
            build_context.dockerfile_path,
            workspace.root().join(".devcontainer/Dockerfile")
        );
        assert_eq!(
            build_context.dockerfile_in_context,
            PathBuf::from("Dockerfile")
        );
        assert_eq!(
            plan.build_options
                .build_args
                .get("VARIANT")
                .map(String::as_str),
            Some("bookworm")
        );
        assert_eq!(plan.build_options.target.as_deref(), Some("dev"));
        assert_eq!(
            plan.build_options.options,
            vec!["--platform=linux/amd64", "--network", "host"]
        );
        assert_eq!(
            plan.build_options.cache_from,
            vec!["type=registry,ref=example.test/cache:latest"]
        );
        assert!(!plan.build_options.no_cache);
        assert!(!plan.build_options.pull);
    }
    #[test]
    fn build_up_plan_hash_changes_when_dockerfile_content_changes() {
        let workspace = test_workspace("dockerfile-hash-plan");
        fs::create_dir_all(workspace.root().join(".devcontainer")).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\n",
        )
        .unwrap();
        write_devcontainer(
            &workspace,
            r#"
        {
          "build": {
            "dockerfile": "Dockerfile"
          }
        }
        "#,
        );

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::write(
            workspace.root().join(".devcontainer/Dockerfile"),
            "FROM alpine\nRUN true\n",
        )
        .unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.resources.config_hash, second.resources.config_hash);
        assert_ne!(first.image, second.image);
    }
    #[cfg(unix)]
    #[test]
    fn build_up_plan_hash_changes_when_resolved_mount_source_changes() {
        let workspace = test_workspace("mount-source-hash-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join("first-cache")).unwrap();
        fs::create_dir_all(workspace.root().join("second-cache")).unwrap();
        let link = workspace.root().join("host-cache");
        std::os::unix::fs::symlink(workspace.root().join("first-cache"), &link).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "host-cache"
target = "/cache"
type = "bind"
resolve_symlink = true
"#,
        )
        .unwrap();

        let first = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();
        fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink(workspace.root().join("second-cache"), &link).unwrap();
        let second = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_ne!(first.mounts[1].source, second.mounts[1].source);
        assert_ne!(first.resources.config_hash, second.resources.config_hash);
    }
    #[test]
    fn config_hash_changes_when_resolved_mount_options_change() {
        let mut cached = test_mount();
        cached.consistency = Some("cached".to_owned());
        let mut delegated = test_mount();
        delegated.consistency = Some("delegated".to_owned());
        assert_ne!(
            config_hash_for_mount(cached),
            config_hash_for_mount(delegated)
        );

        let mut rshared = test_mount();
        rshared.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindPropagation::RShared),
            ..MountBindOptions::default()
        });
        let mut rslave = test_mount();
        rslave.bind_options = Some(MountBindOptions {
            propagation: Some(MountBindPropagation::RSlave),
            ..MountBindOptions::default()
        });
        assert_ne!(
            config_hash_for_mount(rshared),
            config_hash_for_mount(rslave)
        );

        let mut deps = test_volume_mount();
        deps.volume_options = Some(MountVolumeOptions {
            subpath: Some("deps".to_owned()),
            ..MountVolumeOptions::default()
        });
        let mut cache = test_volume_mount();
        cache.volume_options = Some(MountVolumeOptions {
            subpath: Some("cache".to_owned()),
            ..MountVolumeOptions::default()
        });
        assert_ne!(config_hash_for_mount(deps), config_hash_for_mount(cache));
    }
    #[test]
    fn build_up_plan_uses_container_workspace_folder_basename_variable() {
        let workspace = test_workspace("container-basename-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20",
          "workspaceFolder": "/src"
        }
        "#,
        );
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "project-cache"
target = "/opt/${containerWorkspaceFolderBasename}"
type = "volume"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(plan.workspace_folder, "/src");
        assert_eq!(plan.mounts[0].target, default_workspace_folder(&workspace));
        assert_eq!(plan.mounts[1].target, "/opt/src");
    }
    #[cfg(unix)]
    #[test]
    fn build_up_plan_uses_current_uid_and_gid_variables() {
        let workspace = test_workspace("uid-gid-plan");
        write_devcontainer(
            &workspace,
            r#"
        {
          "image": "alpine:3.20"
        }
        "#,
        );
        let uid = unsafe { libc::getuid() };
        let gid = unsafe { libc::getgid() };
        let cache = workspace.root().join(format!("{uid}-{gid}"));
        fs::create_dir_all(&cache).unwrap();
        fs::create_dir_all(workspace.root().join(".decune")).unwrap();
        fs::write(
            workspace.root().join(".decune/config.toml"),
            r#"
version = 1

[[mounts]]
source = "${uid}-${gid}"
target = "/cache"
type = "bind"
"#,
        )
        .unwrap();

        let plan = build_up_plan(&workspace, None, ConfigLayer::default()).unwrap();

        assert_eq!(
            plan.mounts[1].source.as_deref(),
            Some(cache.canonicalize().unwrap().to_str().unwrap())
        );
    }
}

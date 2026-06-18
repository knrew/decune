use std::path::Path;

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
    static_mount_variable_context, static_uid_gid_sync_hash_input, workspace_mounts_from_resolved,
};

const FEATURE_ENTRYPOINT_SHIM_HASH_VERSION: &str = "2";

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
        UpPlanResolution::new(ForwardingResolution::Resolve, false),
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
        UpPlanResolution::new(ForwardingResolution::Resolve, update_features),
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
        UpPlanResolution::new(ForwardingResolution::Resolve, false),
    )
}

pub(super) fn build_preliminary_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::DeferConfigMounts,
        UpPlanResolution::new(forwarding_resolution, update_features),
    )
}

pub(crate) fn build_up_plan_with_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        Vec::new(),
        false,
        MountResolution::Resolve,
        UpPlanResolution::new(forwarding_resolution, update_features),
    )
}

pub(super) fn build_up_plan_with_image_metadata_and_forwarding_resolution(
    workspace: &Workspace,
    explicit_config_path: Option<&Path>,
    cli_layer: ConfigLayer,
    image_metadata: Vec<ConfigLayer>,
    ignored_image_metadata_forwarding: bool,
    forwarding_resolution: ForwardingResolution,
    update_features: bool,
) -> Result<UpPlan> {
    build_up_plan_inner(
        workspace,
        explicit_config_path,
        cli_layer,
        image_metadata,
        ignored_image_metadata_forwarding,
        MountResolution::Resolve,
        UpPlanResolution::new(forwarding_resolution, update_features),
    )
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
    let global_layer = ConfigLayer::from_raw_decune_with_origin(
        load_config_file(workspace.paths().global_config_path())?,
        crate::config::path::ConfigPathOrigin::Global,
    );
    let project_layer = ConfigLayer::from_raw_decune_with_origin(
        load_config_file(workspace.paths().project_config_path())?,
        crate::config::path::ConfigPathOrigin::Project,
    );
    let config_layers = ConfigMergeInput {
        image_metadata,
        global: Some(global_layer),
        devcontainer: Some(devcontainer_layer),
        project: Some(project_layer),
        cli: Some(cli_layer),
        ..ConfigMergeInput::default()
    };
    let workspace_validation = match mount_resolution {
        MountResolution::Resolve => WorkspaceLocationValidation::ConfigResolved,
        MountResolution::DeferConfigMounts => WorkspaceLocationValidation::Preliminary,
    };
    let mut config = resolve_config(config_layers.clone());
    let static_expansion = expand_static_plan_fields(
        workspace,
        devcontainer_json.path(),
        &mut config,
        workspace_validation,
    )?;
    let mount_variables = static_mount_variable_context(
        workspace,
        &static_expansion.workspace_location.workspace_folder,
        &config,
    );
    let compose_project = compose_project_plan(workspace, devcontainer_json.path(), &config)?;
    let mounts = workspace_mounts_from_resolved(
        static_expansion.workspace_location.workspace_mount.clone(),
        workspace.root(),
        &config,
        &mount_variables,
        mount_resolution,
        workspace.paths().state_dir(),
    )?;
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
    if mount_resolution == MountResolution::Resolve {
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
) -> Result<StaticPlanExpansion> {
    let preliminary_variables =
        static_mount_variable_context(workspace, &default_workspace_folder(workspace), config);
    expand_static_user_fields(config, &preliminary_variables)?;
    let workspace_location = resolve_workspace_location(
        workspace,
        config,
        workspace_validation,
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
            | LayerRunArg::DnsSearch(value) => {
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
            | LayerRunArg::DnsSearch(value) => {
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
    use std::fs;

    use crate::{
        config::{ConfigLayer, layer::LayerRunArg, types::MountType},
        workspace::Workspace,
    };

    use super::build_up_plan;

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
                "--dns-search=${localWorkspaceFolderBasename}.test"
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
                "--dns-search=${remoteUser}.test"
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
}

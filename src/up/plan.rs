use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, FeatureLockHashEntry, config_hash,
        load::load_config_file,
        resolve_config,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
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
    workspace::Workspace,
};

use super::{
    ForwardingResolution, MountResolution, UpPlan, UpPlanResolution, WorkspaceLocationValidation,
    mount_hash_inputs, resolve_workspace_location, static_mount_variable_context,
    static_uid_gid_sync_hash_input, workspace_mounts_from_resolved,
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
    let config = resolve_config(config_layers.clone());
    let (build_context, build_options) =
        dockerfile_build_input(workspace.root(), devcontainer_json.path(), &config)?;
    let workspace_validation = match mount_resolution {
        MountResolution::Resolve => WorkspaceLocationValidation::ConfigResolved,
        MountResolution::DeferConfigMounts => WorkspaceLocationValidation::Preliminary,
    };
    let workspace_location = resolve_workspace_location(
        workspace,
        &config,
        workspace_validation,
        |workspace_folder| static_mount_variable_context(workspace, workspace_folder, &config),
    )?;
    let mount_variables =
        static_mount_variable_context(workspace, &workspace_location.workspace_folder, &config);
    let mounts = workspace_mounts_from_resolved(
        workspace_location.workspace_mount,
        workspace.root(),
        &config,
        &mount_variables,
        mount_resolution,
    )?;
    let mut hash_input = ConfigHashInput::new(&config);
    if let Some(context) = &build_context {
        hash_input.build = Some(build_hash_input(context)?);
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
        ForwardingResolution::Resolve => resolve_forward_ports(&config.ports.entries)?,
        ForwardingResolution::IgnoreDetached => Vec::new(),
    };
    let ignored_detached_forwarding = resolution.forwarding == ForwardingResolution::IgnoreDetached
        && (ignored_image_metadata_forwarding
            || !metadata.forward_ports().is_empty()
            || !config.ports.entries.is_empty());

    Ok(UpPlan {
        image,
        base_image,
        build_context,
        build_options,
        feature_install: None,
        feature_build_context_dir: None,
        uid_gid_sync_build_context_dir: None,
        resources,
        pre_uid_gid_sync_resources: None,
        config_layers,
        config,
        effective_users: EffectiveUsers::root(),
        uid_gid_sync_plan: UidGidSyncPlan::default(),
        workspace_folder: workspace_location.workspace_folder,
        mounts,
        forward_ports,
        ignored_detached_forwarding,
    })
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

    use crate::{config::ConfigLayer, config::types::MountType, workspace::Workspace};

    use super::build_up_plan;

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
}

use super::*;

pub(super) async fn prepare_feature_metadata_for_plan(
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
    let feature_devcontainer_file = devcontainer_file.clone();
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
            &feature_devcontainer_file,
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
    let static_expansion = expand_static_plan_fields(
        workspace,
        &devcontainer_file,
        &mut plan.config,
        WorkspaceLocationValidation::Preliminary,
        MountResolution::DeferConfigMounts,
    )?;
    plan.build_context = static_expansion.build_context;
    plan.build_options = static_expansion.build_options;
    plan.sensitive_build_args = static_expansion.sensitive_build_args;
    plan.workspace_folder = static_expansion.workspace_location.workspace_folder;
    plan.feature_install = Some(feature_install);
    plan.feature_build_context_dir =
        Some(workspace.paths().cache_dir().join("feature-build-context"));

    Ok(plan)
}

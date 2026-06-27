use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};

use crate::{
    config::{
        ConfigHashInput, ConfigLayer, ConfigMergeInput, config_hash, load::load_config_file,
        resolve_config,
    },
    devcontainer::{json::DevcontainerJson, metadata::parse_metadata},
    docker::{
        build::build_hash_input,
        ports::resolve_forward_ports,
        resource::DockerResources,
        user::{EffectiveUsers, UidGidSyncPlan},
    },
    workspace::Workspace,
};

use super::{
    ForwardingResolution, MountResolution, UpPlan, UpPlanResolution, WorkspaceLocationValidation,
    mount_hash_inputs, static_mount_variable_context, static_uid_gid_sync_hash_input,
    workspace_mount_plan_from_resolved,
};

mod compose;
mod expand;
mod hash;
mod source;

use compose::{compose_project_plan, validate_service_qualified_forward_ports};
pub(super) use expand::{expand_runtime_devcontainer_fields, expand_static_plan_fields};
pub(super) use hash::{add_internal_hash_versions, feature_lock_hash_inputs};
pub(super) use source::{base_image_source, config_requires_workspace_layer, final_image_source};

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
        sensitive_container_env: crate::config::variables::SensitiveEnvMap::default(),
        sensitive_build_args: static_expansion.sensitive_build_args,
        compose_interpolation_env: BTreeMap::default(),
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
        sensitive_container_env: crate::config::variables::SensitiveEnvMap::default(),
        sensitive_build_args: static_expansion.sensitive_build_args,
        compose_interpolation_env: BTreeMap::default(),
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

#[cfg(test)]
mod tests {
    use std::{fs, net::TcpListener};

    use crate::{
        config::{ConfigLayer, resolved::ResolvedPublishPort, types::PortProtocol},
        docker::ports::ResolvedForwardPort,
        up::{ForwardingResolution, test_support::test_workspace},
        workspace::Workspace,
    };

    use super::super::mounts::default_workspace_folder;
    use super::{
        build_preliminary_up_plan_with_forwarding_resolution, build_up_plan,
        build_up_plan_with_forwarding_resolution,
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
                r"
version = 1

[[ports]]
container = 4321
host = {host_port}
require_local = true
"
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
}

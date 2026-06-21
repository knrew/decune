use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        MountBindOptionsHashInput, MountHashInput, MountVolumeDriverConfigHashInput,
        MountVolumeOptionsHashInput,
        layer::LayerDevcontainerMount,
        resolved::{ResolvedConfig, ResolvedDevcontainerSource},
        types::MountType,
        variables::expand_variables,
    },
    docker::{
        dotfiles::{DotfileSkeletonPlan, dotfile_mount_plan},
        mounts::{
            DockerMountSpec, HostPathCreateMode, MountBindOptions, MountVolumeOptions,
            config_mount_specs_with_host_path_create,
            devcontainer_mount_spec_with_host_path_create, normalize_container_path,
        },
    },
    workspace::Workspace,
};

use super::{MountResolution, WorkspaceLocation};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::up) enum WorkspaceLocationValidation {
    Preliminary,
    ConfigResolved,
    RuntimeResolved,
}

impl WorkspaceLocationValidation {
    fn require_explicit_workspace_folder(self) -> bool {
        matches!(
            self,
            WorkspaceLocationValidation::ConfigResolved
                | WorkspaceLocationValidation::RuntimeResolved
        )
    }

    fn validate_workspace_folder_under_mount(self) -> bool {
        matches!(self, WorkspaceLocationValidation::RuntimeResolved)
    }
}

pub(crate) fn default_workspace_folder(workspace: &Workspace) -> String {
    format!("/workspaces/{}", workspace.basename())
}

#[derive(Debug, Clone, Default, PartialEq)]
pub(in crate::up) struct WorkspaceMountPlan {
    pub(in crate::up) mounts: Vec<DockerMountSpec>,
    pub(in crate::up) dotfile_skeletons: Vec<DotfileSkeletonPlan>,
}

pub(in crate::up) fn workspace_mount_plan_from_resolved(
    workspace_mount: DockerMountSpec,
    workspace_root: &Path,
    config: &ResolvedConfig,
    variables: &crate::config::variables::VariableContext,
    mount_resolution: MountResolution,
    state_root: &Path,
) -> Result<WorkspaceMountPlan> {
    if matches!(
        config.devcontainer.source,
        Some(ResolvedDevcontainerSource::Compose(_))
    ) {
        if !mount_resolution.resolves_config_mounts() {
            return Ok(WorkspaceMountPlan::default());
        }
        let mut mounts = config_mount_specs_with_host_path_create(
            config,
            workspace_root,
            variables,
            host_path_create_mode(mount_resolution),
        )?;
        let dotfiles = dotfile_mount_plan(config, workspace_root, variables, state_root)?;
        mounts.extend(dotfiles.mounts);
        return Ok(WorkspaceMountPlan {
            mounts,
            dotfile_skeletons: dotfiles.skeletons,
        });
    }

    let workspace_target = workspace_mount.target.clone();
    let mut mounts = vec![workspace_mount];
    let mut dotfile_skeletons = Vec::new();
    if mount_resolution.resolves_config_mounts() {
        let config_mounts = config_mount_specs_with_host_path_create(
            config,
            workspace_root,
            variables,
            host_path_create_mode(mount_resolution),
        )?;
        reject_workspace_mount_target_conflicts(&workspace_target, &config_mounts)?;
        mounts.extend(config_mounts);

        let dotfiles = dotfile_mount_plan(config, workspace_root, variables, state_root)?;
        reject_workspace_mount_target_conflicts(&workspace_target, &dotfiles.mounts)?;
        mounts.extend(dotfiles.mounts);
        dotfile_skeletons = dotfiles.skeletons;
    }

    Ok(WorkspaceMountPlan {
        mounts,
        dotfile_skeletons,
    })
}

fn reject_workspace_mount_target_conflicts(
    workspace_target: &str,
    mounts: &[DockerMountSpec],
) -> Result<()> {
    let workspace_target = normalize_container_path(workspace_target);
    if mounts
        .iter()
        .any(|mount| normalize_container_path(&mount.target) == workspace_target)
    {
        bail!("Mount target conflicts with workspace mount target: {workspace_target}");
    }

    Ok(())
}

pub(super) fn resolve_workspace_location<F>(
    workspace: &Workspace,
    config: &ResolvedConfig,
    validation: WorkspaceLocationValidation,
    mount_resolution: MountResolution,
    variables_for_workspace_folder: F,
) -> Result<WorkspaceLocation>
where
    F: Fn(&str) -> crate::config::variables::VariableContext,
{
    let default_folder = default_workspace_folder(workspace);
    let explicit_workspace_folder = config.devcontainer.workspace_folder.as_deref();
    if config.devcontainer.workspace_mount.is_some()
        && explicit_workspace_folder.is_none()
        && validation.require_explicit_workspace_folder()
    {
        bail!("workspaceFolder is required when workspaceMount is specified");
    }

    let pre_variables = variables_for_workspace_folder(&default_folder);
    let workspace_folder = match explicit_workspace_folder {
        Some(workspace_folder) => {
            let expanded_workspace_folder = expand_variables(workspace_folder, &pre_variables)
                .context("Failed to expand workspaceFolder")?;
            validate_workspace_folder(&expanded_workspace_folder)?
        }
        None => validate_workspace_folder(&default_folder)?,
    };
    let variables = variables_for_workspace_folder(&workspace_folder);
    let workspace_mount = workspace_mount_spec(workspace, config, &variables, mount_resolution)?;
    let workspace_folder = if explicit_workspace_folder.is_some() {
        workspace_folder
    } else {
        workspace_mount.target.clone()
    };

    if config.devcontainer.workspace_mount.is_some()
        && validation.validate_workspace_folder_under_mount()
    {
        validate_workspace_folder_under_mount(&workspace_folder, &workspace_mount.target)?;
    }

    Ok(WorkspaceLocation {
        workspace_folder,
        workspace_mount,
    })
}

fn validate_workspace_folder(workspace_folder: &str) -> Result<String> {
    if !workspace_folder.starts_with('/') {
        bail!("workspaceFolder must be an absolute container path: {workspace_folder}");
    }

    Ok(normalize_container_path(workspace_folder))
}

fn validate_workspace_folder_under_mount(
    workspace_folder: &str,
    workspace_mount_target: &str,
) -> Result<()> {
    let workspace_folder = normalize_container_path(workspace_folder);
    let workspace_mount_target = normalize_container_path(workspace_mount_target);

    if workspace_mount_target == "/"
        || workspace_folder == workspace_mount_target
        || workspace_folder.starts_with(&format!("{workspace_mount_target}/"))
    {
        return Ok(());
    }

    bail!(
        "workspaceFolder must be under the workspaceMount target: workspaceFolder={workspace_folder}, workspaceMount target={workspace_mount_target}"
    );
}

fn workspace_mount_spec(
    workspace: &Workspace,
    config: &ResolvedConfig,
    variables: &crate::config::variables::VariableContext,
    mount_resolution: MountResolution,
) -> Result<DockerMountSpec> {
    if let Some(workspace_mount) = &config.devcontainer.workspace_mount {
        return devcontainer_mount_spec_with_host_path_create(
            &LayerDevcontainerMount::String(workspace_mount.clone()),
            workspace.root(),
            variables,
            host_path_create_mode(mount_resolution),
        )
        .context("Failed to resolve workspaceMount");
    }

    Ok(DockerMountSpec {
        source: Some(workspace.root().display().to_string()),
        target: default_workspace_folder(workspace),
        mount_type: MountType::Bind,
        read_only: false,
        consistency: None,
        bind_options: None,
        volume_options: None,
    })
}

fn host_path_create_mode(mount_resolution: MountResolution) -> HostPathCreateMode {
    match mount_resolution {
        MountResolution::ReadOnly => HostPathCreateMode::ReadOnly,
        MountResolution::Resolve | MountResolution::DeferConfigMounts => {
            HostPathCreateMode::Materialize
        }
    }
}

pub(crate) fn mount_hash_inputs(mounts: &[DockerMountSpec]) -> Vec<MountHashInput> {
    mounts
        .iter()
        .map(|mount| MountHashInput {
            source: mount.source.clone(),
            target: mount.target.clone(),
            mount_type: mount.mount_type,
            read_only: mount.read_only,
            consistency: mount.consistency.clone(),
            bind_options: mount.bind_options.as_ref().map(bind_options_hash_input),
            volume_options: mount.volume_options.as_ref().map(volume_options_hash_input),
        })
        .collect()
}

fn bind_options_hash_input(options: &MountBindOptions) -> MountBindOptionsHashInput {
    MountBindOptionsHashInput {
        propagation: options.propagation.map(|value| value.to_string()),
        non_recursive: options.non_recursive,
        create_mountpoint: options.create_mountpoint,
        read_only_non_recursive: options.read_only_non_recursive,
        read_only_force_recursive: options.read_only_force_recursive,
    }
}

fn volume_options_hash_input(options: &MountVolumeOptions) -> MountVolumeOptionsHashInput {
    MountVolumeOptionsHashInput {
        no_copy: options.no_copy,
        labels: options
            .labels
            .clone()
            .map(|labels| labels.into_iter().collect()),
        driver_config: options.driver_config.as_ref().map(|driver_config| {
            MountVolumeDriverConfigHashInput {
                name: driver_config.name.clone(),
                options: driver_config
                    .options
                    .clone()
                    .map(|options| options.into_iter().collect()),
            }
        }),
        subpath: options.subpath.clone(),
    }
}

pub(super) fn static_mount_variable_context(
    workspace: &Workspace,
    workspace_folder: &str,
    config: &ResolvedConfig,
) -> crate::config::variables::VariableContext {
    let remote_user = config
        .devcontainer
        .remote_user
        .clone()
        .or_else(|| config.devcontainer.container_user.clone())
        .unwrap_or_else(|| "root".to_owned());

    mount_variable_context(
        workspace,
        workspace_folder,
        remote_user,
        Some("/root".to_owned()),
    )
}

pub(super) fn mount_variable_context(
    workspace: &Workspace,
    workspace_folder: &str,
    remote_user: String,
    remote_user_home: Option<String>,
) -> crate::config::variables::VariableContext {
    crate::config::variables::VariableContext::new(crate::config::variables::VariableContextInput {
        local_workspace_folder: workspace.root().to_path_buf(),
        local_workspace_folder_basename: workspace.basename().to_owned(),
        container_workspace_folder: workspace_folder.to_owned(),
        container_workspace_folder_basename: container_workspace_folder_basename(
            workspace_folder,
            workspace,
        ),
        devcontainer_id: workspace.id().to_owned(),
        uid: current_uid(),
        gid: current_gid(),
        remote_user,
        remote_user_home,
    })
}

fn container_workspace_folder_basename(workspace_folder: &str, workspace: &Workspace) -> String {
    Path::new(workspace_folder)
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| workspace.basename())
        .to_owned()
}

#[cfg(unix)]
fn current_uid() -> u32 {
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

#[cfg(unix)]
fn current_gid() -> u32 {
    unsafe { libc::getgid() }
}

#[cfg(not(unix))]
fn current_gid() -> u32 {
    0
}

#[cfg(test)]
mod tests {
    use super::{default_workspace_folder, mount_hash_inputs};
    use crate::{config::types::MountType, docker::mounts::DockerMountSpec, workspace::Workspace};

    #[test]
    fn default_workspace_folder_uses_real_workspace_basename() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("Project Name!");
        std::fs::create_dir(&root).unwrap();
        let workspace = Workspace::resolve(&root).unwrap();

        assert_eq!(
            default_workspace_folder(&workspace),
            "/workspaces/Project Name!"
        );
    }

    #[test]
    fn mount_hash_inputs_preserve_resolved_mount_fields() {
        let mount = DockerMountSpec {
            source: Some("/host/project".to_owned()),
            target: "/workspaces/project".to_owned(),
            mount_type: MountType::Bind,
            read_only: true,
            consistency: Some("cached".to_owned()),
            bind_options: None,
            volume_options: None,
        };

        let input = mount_hash_inputs(&[mount]);

        assert_eq!(input.len(), 1);
        assert_eq!(input[0].source.as_deref(), Some("/host/project"));
        assert_eq!(input[0].target, "/workspaces/project");
        assert_eq!(input[0].mount_type, MountType::Bind);
        assert!(input[0].read_only);
        assert_eq!(input[0].consistency.as_deref(), Some("cached"));
    }
}

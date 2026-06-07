use std::path::Path;

use anyhow::{Context, Result, bail};
use bollard::models::{MountBindOptions, MountVolumeOptions};

use crate::{
    config::{
        MountBindOptionsHashInput, MountHashInput, MountVolumeDriverConfigHashInput,
        MountVolumeOptionsHashInput, layer::LayerDevcontainerMount, resolved::ResolvedConfig,
        types::MountType,
    },
    docker::{
        dotfiles::dotfile_mount_specs,
        mounts::{
            DockerMountSpec, config_mount_specs, devcontainer_mount_spec, normalize_container_path,
        },
    },
    workspace::Workspace,
};

use super::{MountResolution, WorkspaceLocation};

pub(crate) fn default_workspace_folder(workspace: &Workspace) -> String {
    format!("/workspaces/{}", workspace.basename())
}

pub(super) fn workspace_mounts_from_resolved(
    workspace_mount: DockerMountSpec,
    workspace_root: &Path,
    config: &ResolvedConfig,
    variables: &crate::config::variables::VariableContext,
    mount_resolution: MountResolution,
) -> Result<Vec<DockerMountSpec>> {
    let workspace_target = workspace_mount.target.clone();
    let mut mounts = vec![workspace_mount];
    if mount_resolution == MountResolution::Resolve {
        let config_mounts = config_mount_specs(config, workspace_root, variables)?;
        reject_workspace_mount_target_conflicts(&workspace_target, &config_mounts)?;
        mounts.extend(config_mounts);

        let dotfile_mounts = dotfile_mount_specs(config, workspace_root, variables)?;
        reject_workspace_mount_target_conflicts(&workspace_target, &dotfile_mounts)?;
        mounts.extend(dotfile_mounts);
    }

    Ok(mounts)
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
    variables_for_workspace_folder: F,
) -> Result<WorkspaceLocation>
where
    F: Fn(&str) -> crate::config::variables::VariableContext,
{
    let seed_workspace_folder = config
        .devcontainer
        .workspace_folder
        .clone()
        .unwrap_or_else(|| default_workspace_folder(workspace));
    let explicit_workspace_folder = config.devcontainer.workspace_folder.as_deref();
    if config.devcontainer.workspace_mount.is_some() && explicit_workspace_folder.is_none() {
        bail!("workspaceFolder is required when workspaceMount is specified");
    }

    let workspace_folder = validate_workspace_folder(&seed_workspace_folder)?;
    let variables = variables_for_workspace_folder(&workspace_folder);
    let workspace_mount = workspace_mount_spec(workspace, config, &variables)?;
    let workspace_folder = if explicit_workspace_folder.is_some() {
        validate_workspace_folder(&workspace_folder)?
    } else {
        workspace_mount.target.clone()
    };

    if config.devcontainer.workspace_mount.is_some() {
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
) -> Result<DockerMountSpec> {
    if let Some(workspace_mount) = &config.devcontainer.workspace_mount {
        return devcontainer_mount_spec(
            &LayerDevcontainerMount::String(workspace_mount.clone()),
            workspace.root(),
            variables,
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
    crate::config::variables::VariableContext::new(
        workspace.root().to_path_buf(),
        workspace.basename().to_owned(),
        workspace_folder.to_owned(),
        container_workspace_folder_basename(workspace_folder, workspace),
        workspace.id().to_owned(),
        current_uid(),
        current_gid(),
        remote_user,
        remote_user_home,
    )
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

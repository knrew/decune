use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        path::{HostPathOptions, PathCreate, SymlinkResolution, resolve_host_path},
        resolved::ResolvedConfig,
        types::MountType,
        variables::VariableContext,
    },
    docker::mounts::DockerMountSpec,
};

mod setup;
mod skeleton;
mod targets;
mod tree;

pub(crate) use setup::setup_dotfiles;
use skeleton::skeleton_dotfile_mount_plan;
pub(crate) use skeleton::{DotfileSkeletonPlan, materialize_dotfile_skeletons};
use targets::{ExpandedDotfile, dotfile_mount_target, expanded_dotfiles};
use tree::{backing_root_mount_source, collect_dotfile_tree, directory_contains_any_symlink};

pub(super) const DOTFILES_MOUNT_ROOT: &str = "/opt/decune/dotfiles";
pub(super) const DOTFILE_BACKINGS_MOUNT_ROOT: &str = "/opt/decune/dotfile-backings";
pub(super) const DOTFILE_MOUNT_SKELETON_DIR: &str = "dotfile-mount-skeleton";
pub(super) const MAX_DOTFILE_TREE_DEPTH: u32 = 32;
pub(super) const MAX_DOTFILE_MOUNTS: usize = 1024;

#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct DotfileMountPlan {
    pub(crate) mounts: Vec<DockerMountSpec>,
    pub(crate) skeletons: Vec<DotfileSkeletonPlan>,
}

#[cfg(test)]
pub(crate) fn dotfile_mount_specs(
    config: &ResolvedConfig,
    workspace_root: &Path,
    variables: &VariableContext,
    state_root: &Path,
) -> Result<Vec<DockerMountSpec>> {
    Ok(dotfile_mount_plan(config, workspace_root, variables, state_root)?.mounts)
}

pub(crate) fn dotfile_mount_plan(
    config: &ResolvedConfig,
    workspace_root: &Path,
    variables: &VariableContext,
    state_root: &Path,
) -> Result<DotfileMountPlan> {
    let mut plan = DotfileMountPlan::default();
    for dotfile in expanded_dotfiles(config, variables)? {
        let dotfile_plan = dotfile_mount_spec(&dotfile, workspace_root, variables, state_root)?;
        plan.mounts.extend(dotfile_plan.mounts);
        plan.skeletons.extend(dotfile_plan.skeletons);
    }

    Ok(plan)
}

fn dotfile_mount_spec(
    dotfile: &ExpandedDotfile<'_>,
    workspace_root: &Path,
    variables: &VariableContext,
    state_root: &Path,
) -> Result<DotfileMountPlan> {
    let target = dotfile_mount_target(&dotfile.target)?;
    let source = resolve_host_path(
        &dotfile.dotfile.source,
        &HostPathOptions::new(dotfile.dotfile.origin, workspace_root, variables)
            .with_create(PathCreate::None)
            .with_symlink_resolution(symlink_resolution(dotfile.dotfile.resolve_symlink)),
    )
    .with_context(|| {
        format!(
            "Failed to resolve dotfile source for target: {}",
            dotfile.dotfile.target
        )
    })?;

    if !dotfile.dotfile.resolve_symlink || source.is_file() {
        return Ok(DotfileMountPlan {
            mounts: vec![dotfile_bind_mount(
                &source,
                target,
                dotfile.dotfile.read_only,
            )],
            skeletons: Vec::new(),
        });
    }
    if !source.is_dir() {
        bail!(
            "Dotfile source must be a file or directory: {}",
            source.display()
        );
    }

    if !directory_contains_any_symlink(&source)? {
        return Ok(DotfileMountPlan {
            mounts: vec![dotfile_bind_mount(
                &source,
                target,
                dotfile.dotfile.read_only,
            )],
            skeletons: Vec::new(),
        });
    }

    let tree = collect_dotfile_tree(&source)?;
    if let Some(backing_root) = backing_root_mount_source(&tree)? {
        return Ok(DotfileMountPlan {
            mounts: vec![dotfile_bind_mount(
                &backing_root,
                target,
                dotfile.dotfile.read_only,
            )],
            skeletons: Vec::new(),
        });
    }

    let plan = skeleton_dotfile_mount_plan(
        &source,
        &dotfile.target,
        target,
        state_root,
        dotfile.dotfile.read_only,
    )?;
    if plan.mounts.len() > MAX_DOTFILE_MOUNTS {
        bail!(
            "Dotfile target generates too many bind mounts ({} > {}): {}",
            plan.mounts.len(),
            MAX_DOTFILE_MOUNTS,
            dotfile.target
        );
    }

    Ok(plan)
}

fn dotfile_bind_mount(source: &Path, target: String, read_only: bool) -> DockerMountSpec {
    DockerMountSpec {
        source: Some(source.display().to_string()),
        target,
        mount_type: MountType::Bind,
        read_only,
        consistency: None,
        bind_options: None,
        volume_options: None,
    }
}

const fn symlink_resolution(resolve_symlink: bool) -> SymlinkResolution {
    if resolve_symlink {
        SymlinkResolution::Resolve
    } else {
        SymlinkResolution::Preserve
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use anyhow::Result;

    use super::*;
    use crate::{
        config::{
            path::ConfigPathOrigin,
            resolved::{ResolvedConfig, ResolvedDotfile},
            types::{DotfileConflict, MountType},
            variables::{VariableContext, VariableContextInput},
        },
        docker::mounts::DockerMountSpec,
    };

    fn variables(workspace_root: &Path) -> VariableContext {
        VariableContext::new(VariableContextInput {
            local_workspace_folder: workspace_root.to_path_buf(),
            local_workspace_folder_basename: "project".to_owned(),
            container_workspace_folder: "/workspaces/project".to_owned(),
            container_workspace_folder_basename: "project".to_owned(),
            devcontainer_id: "abc123def456".to_owned(),
            uid: 1000,
            gid: 1000,
            remote_user: "vscode".to_owned(),
            remote_user_home: Some("/home/vscode".to_owned()),
        })
    }

    fn materialized_dotfile_mount_specs(
        config: &ResolvedConfig,
        workspace_root: &Path,
        variables: &VariableContext,
        state_root: &Path,
    ) -> Result<Vec<DockerMountSpec>> {
        let plan = dotfile_mount_plan(config, workspace_root, variables, state_root)?;
        materialize_dotfile_skeletons(&plan.skeletons)?;
        Ok(plan.mounts)
    }

    #[test]
    fn converts_directory_dotfile_to_read_only_direct_mount() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join(".decune/nvim");
        fs::create_dir_all(&source).unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/nvim".to_owned(),
                target: ".config/nvim".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(
            mounts,
            vec![DockerMountSpec {
                source: Some(source.canonicalize().unwrap().display().to_string()),
                target: "/opt/decune/dotfiles/.config/nvim".to_owned(),
                mount_type: MountType::Bind,
                read_only: true,
                consistency: None,
                bind_options: None,
                volume_options: None,
            }]
        );
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn preserves_dotfile_symlink_source_when_requested() {
        let workspace = tempfile::tempdir().unwrap();
        let real_source = workspace.path().join("real-gitconfig");
        let link_source = workspace.path().join("linked-gitconfig");
        fs::write(&real_source, "[user]\nname = decune\n").unwrap();
        unix_fs::symlink(&real_source, &link_source).unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "linked-gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: false,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(link_source.to_str().unwrap())
        );
    }

    #[test]
    fn does_not_create_skeleton_for_file_dotfile_source() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("gitconfig");
        fs::write(&source, "[user]\nname = test\n").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source.canonicalize().unwrap().to_str().unwrap())
        );
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
    }

    #[cfg(unix)]
    #[test]
    fn does_not_create_skeleton_when_resolve_symlink_is_false() {
        let workspace = tempfile::tempdir().unwrap();
        let source_dir = workspace.path().join("lazygit");
        fs::create_dir_all(&source_dir).unwrap();
        fs::write(source_dir.join("config.yml"), "content").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "lazygit".to_owned(),
                target: ".config/lazygit".to_owned(),
                read_only: true,
                resolve_symlink: false,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = materialized_dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(source_dir.to_str().unwrap())
        );
        assert!(!workspace.path().join(DOTFILE_MOUNT_SKELETON_DIR).exists());
    }
}

use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

use crate::config::{
    resolved::{ResolvedConfig, ResolvedDotfile, ResolvedDotfileEntry},
    variables::{VariableContext, expand_variables},
};

use super::DOTFILES_MOUNT_ROOT;

pub(super) struct ExpandedDotfile<'a> {
    pub(super) dotfile: &'a ResolvedDotfile,
    pub(super) target: String,
}

enum DotfileEntryRef<'a> {
    Enabled(&'a ResolvedDotfile),
    Disabled(&'a str),
}

pub(super) fn expanded_dotfiles<'a>(
    config: &'a ResolvedConfig,
    variables: &VariableContext,
) -> Result<Vec<ExpandedDotfile<'a>>> {
    let mut expanded = Vec::new();

    for entry in dotfile_entries(config) {
        match entry {
            DotfileEntryRef::Enabled(dotfile) => {
                let target = normalized_expanded_dotfile_target(&dotfile.target, variables)?;
                replace_dotfile_by_target(&mut expanded, ExpandedDotfile { dotfile, target });
            }
            DotfileEntryRef::Disabled(target) => {
                let target = normalized_expanded_dotfile_target(target, variables)?;
                remove_dotfile_by_target(&mut expanded, &target);
            }
        }
    }

    Ok(expanded)
}

fn dotfile_entries(config: &ResolvedConfig) -> Vec<DotfileEntryRef<'_>> {
    if config.dotfile_entries.is_empty() {
        return config
            .dotfiles
            .iter()
            .map(DotfileEntryRef::Enabled)
            .collect();
    }

    config
        .dotfile_entries
        .iter()
        .map(|entry| match entry {
            ResolvedDotfileEntry::Enabled(dotfile) => DotfileEntryRef::Enabled(dotfile),
            ResolvedDotfileEntry::Disabled(dotfile) => DotfileEntryRef::Disabled(&dotfile.target),
        })
        .collect()
}

fn replace_dotfile_by_target<'a>(
    dotfiles: &mut Vec<ExpandedDotfile<'a>>,
    dotfile: ExpandedDotfile<'a>,
) {
    match dotfiles
        .iter()
        .position(|existing| existing.target == dotfile.target)
    {
        Some(index) => dotfiles[index] = dotfile,
        None => dotfiles.push(dotfile),
    }
}

fn remove_dotfile_by_target(dotfiles: &mut Vec<ExpandedDotfile<'_>>, target: &str) {
    dotfiles.retain(|existing| existing.target != target);
}

fn normalized_expanded_dotfile_target(target: &str, variables: &VariableContext) -> Result<String> {
    let target = expanded_dotfile_target(target, variables)?;
    let components = relative_target_components(&target)?;

    Ok(components.join("/"))
}

fn expanded_dotfile_target(target: &str, variables: &VariableContext) -> Result<String> {
    expand_variables(target, variables)
        .with_context(|| format!("Failed to expand dotfile target: {target}"))
}

pub(super) fn dotfile_mount_target(target: &str) -> Result<String> {
    let components = relative_target_components(target)?;

    Ok(format!("{DOTFILES_MOUNT_ROOT}/{}", components.join("/")))
}

pub(super) fn remote_home_target(remote_home: &str, target: &str) -> Result<String> {
    let components = relative_target_components(target)?;
    let remote_home = normalized_remote_home(remote_home)?;
    if remote_home == "/" {
        return Ok(format!("/{}", components.join("/")));
    }

    Ok(format!("{remote_home}/{}", components.join("/")))
}

fn normalized_remote_home(remote_home: &str) -> Result<&str> {
    let trimmed = remote_home.trim_end_matches('/');
    if trimmed.is_empty() && remote_home.starts_with('/') {
        return Ok("/");
    }
    if trimmed.is_empty() {
        bail!("Remote user home must not be empty");
    }

    Ok(trimmed)
}

pub(super) fn container_parent(target: &str) -> Result<String> {
    let (parent, _) = target
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("Dotfile target must be absolute: {target}"))?;
    if parent.is_empty() {
        return Ok("/".to_owned());
    }

    Ok(parent.to_owned())
}

pub(super) fn relative_target_components(target: &str) -> Result<Vec<String>> {
    let path = Path::new(target);
    let mut components = Vec::new();

    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value.to_str().ok_or_else(|| {
                    anyhow::anyhow!("Dotfile target is not valid Unicode: {target}")
                })?;
                components.push(value.to_owned());
            }
            Component::CurDir => {}
            Component::ParentDir => bail!("Dotfile target must not contain '..': {target}"),
            Component::RootDir | Component::Prefix(_) => {
                bail!("Dotfile target must be relative: {target}");
            }
        }
    }

    if components.is_empty() {
        bail!("Dotfile target must not be empty");
    }

    Ok(components)
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path};

    use crate::{
        config::{
            path::ConfigPathOrigin,
            resolved::{
                ResolvedConfig, ResolvedDotfile, ResolvedDotfileDisable, ResolvedDotfileEntry,
            },
            types::DotfileConflict,
            variables::{VariableContext, VariableContextInput},
        },
        docker::dotfiles::dotfile_mount_specs,
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

    #[test]
    fn rejects_absolute_dotfile_target() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("dotfile");
        fs::write(&source, "dotfile").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotfile".to_owned(),
                target: "/root/.config".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Dotfile target must be relative")
        );
    }

    #[test]
    fn expands_dotfile_target_variables_for_dotfile_mount() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("dotfile");
        fs::write(&source, "dotfile").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotfile".to_owned(),
                target: ".config/${remoteUser}/nvim".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let mounts = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(mounts[0].target, "/opt/decune/dotfiles/.config/vscode/nvim");
    }

    #[test]
    fn dotfile_mount_specs_replaces_duplicate_expanded_target() {
        let workspace = tempfile::tempdir().unwrap();
        let global_source = workspace.path().join("global-gitconfig");
        let project_source = workspace.path().join("project-gitconfig");
        fs::write(&global_source, "global").unwrap();
        fs::write(&project_source, "project").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: global_source.display().to_string(),
                    target: ".config/${remoteUser}/gitconfig".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Global,
                },
                ResolvedDotfile {
                    source: project_source.display().to_string(),
                    target: ".config/vscode/gitconfig".to_owned(),
                    read_only: false,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Backup,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let mounts = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert_eq!(mounts.len(), 1);
        assert_eq!(
            mounts[0].source.as_deref(),
            Some(project_source.canonicalize().unwrap().to_str().unwrap())
        );
        assert_eq!(
            mounts[0].target,
            "/opt/decune/dotfiles/.config/vscode/gitconfig"
        );
        assert!(!mounts[0].read_only);
    }

    #[test]
    fn dotfile_mount_specs_disables_global_dotfile_by_expanded_target() {
        let workspace = tempfile::tempdir().unwrap();
        let global_source = workspace.path().join("global-gitconfig");
        fs::write(&global_source, "global").unwrap();
        let global_dotfile = ResolvedDotfile {
            source: global_source.display().to_string(),
            target: ".config/${remoteUser}/gitconfig".to_owned(),
            read_only: true,
            resolve_symlink: true,
            on_conflict: DotfileConflict::Fail,
            origin: ConfigPathOrigin::Global,
        };
        let config = ResolvedConfig {
            dotfile_entries: vec![
                ResolvedDotfileEntry::Enabled(global_dotfile.clone()),
                ResolvedDotfileEntry::Disabled(ResolvedDotfileDisable {
                    target: ".config/vscode/gitconfig".to_owned(),
                    origin: ConfigPathOrigin::Project,
                }),
            ],
            dotfiles: vec![global_dotfile],
            ..ResolvedConfig::default()
        };

        let mounts = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap();

        assert!(mounts.is_empty());
    }

    #[test]
    fn rejects_dotfile_target_variables_that_expand_to_absolute_path() {
        let workspace = tempfile::tempdir().unwrap();
        let source = workspace.path().join("dotfile");
        fs::write(&source, "dotfile").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: "dotfile".to_owned(),
                target: "${remoteUserHome}/.gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let error = dotfile_mount_specs(
            &config,
            workspace.path(),
            &variables(workspace.path()),
            workspace.path(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Dotfile target must be relative")
        );
    }
}

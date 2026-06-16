use std::path::{Component, Path};

use anyhow::{Context, Result, bail};

use crate::{
    config::{
        path::{HostPathOptions, PathCreate, SymlinkResolution, resolve_host_path},
        resolved::{ResolvedConfig, ResolvedDotfile, ResolvedDotfileEntry},
        types::{DotfileConflict, MountType},
        variables::{VariableContext, expand_variables},
    },
    docker::{
        client::DockerClient,
        exec::{ExecCommandSpec, exec_capture},
        mounts::DockerMountSpec,
        user::ResolvedRemoteUser,
    },
};

const DOTFILES_STAGING_ROOT: &str = "/opt/decune/dotfiles";

pub(crate) fn dotfile_mount_specs(
    config: &ResolvedConfig,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<Vec<DockerMountSpec>> {
    expanded_dotfiles(config, variables)?
        .iter()
        .map(|dotfile| dotfile_mount_spec(dotfile, workspace_root, variables))
        .collect()
}

pub(crate) async fn setup_dotfiles(
    client: &DockerClient,
    container: &str,
    config: &ResolvedConfig,
    remote_user: &ResolvedRemoteUser,
    variables: &VariableContext,
) -> Result<()> {
    if config.dotfiles.is_empty() {
        return Ok(());
    }

    let remote_home = remote_user.home()?;
    let script = dotfile_setup_script(config, remote_home, variables)?;
    if script.is_empty() {
        return Ok(());
    }

    exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-lc".to_owned(), script],
            user: Some(remote_user.user.clone()),
            working_dir: Some(remote_home.to_owned()),
            env: Default::default(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await
    .with_context(|| format!("Failed to setup dotfiles in container: {container}"))?;

    Ok(())
}

fn dotfile_setup_script(
    config: &ResolvedConfig,
    remote_home: &str,
    variables: &VariableContext,
) -> Result<String> {
    let dotfiles = expanded_dotfiles(config, variables)?;
    let mut script = String::new();
    if !dotfiles.is_empty() {
        script.push_str("set -e\n");
    }

    for dotfile in dotfiles {
        let target = remote_home_target(remote_home, &dotfile.target)?;
        let source = staging_target(&dotfile.target)?;
        let parent = container_parent(&target)?;
        script.push_str(&dotfile_setup_script_entry(
            &source,
            &target,
            &parent,
            dotfile.dotfile.on_conflict,
        ));
    }

    Ok(script)
}

fn dotfile_mount_spec(
    dotfile: &ExpandedDotfile<'_>,
    workspace_root: &Path,
    variables: &VariableContext,
) -> Result<DockerMountSpec> {
    let target = staging_target(&dotfile.target)?;
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

    Ok(DockerMountSpec {
        source: Some(source.display().to_string()),
        target,
        mount_type: MountType::Bind,
        read_only: dotfile.dotfile.read_only,
        consistency: None,
        bind_options: None,
        volume_options: None,
    })
}

struct ExpandedDotfile<'a> {
    dotfile: &'a ResolvedDotfile,
    target: String,
}

enum DotfileEntryRef<'a> {
    Enabled(&'a ResolvedDotfile),
    Disabled(&'a str),
}

fn expanded_dotfiles<'a>(
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

fn staging_target(target: &str) -> Result<String> {
    let components = relative_target_components(target)?;

    Ok(format!("{DOTFILES_STAGING_ROOT}/{}", components.join("/")))
}

fn remote_home_target(remote_home: &str, target: &str) -> Result<String> {
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

fn container_parent(target: &str) -> Result<String> {
    let (parent, _) = target
        .rsplit_once('/')
        .ok_or_else(|| anyhow::anyhow!("Dotfile target must be absolute: {target}"))?;
    if parent.is_empty() {
        return Ok("/".to_owned());
    }

    Ok(parent.to_owned())
}

fn dotfile_setup_script_entry(
    source: &str,
    target: &str,
    parent: &str,
    on_conflict: DotfileConflict,
) -> String {
    let source = shell_quote(source);
    let target = shell_quote(target);
    let parent = shell_quote(parent);
    let conflict_body = match on_conflict {
        DotfileConflict::Fail => format!(
            "printf '%s\\n' {message} >&2\nexit 1\n",
            message = shell_quote("Dotfile target already exists")
        ),
        DotfileConflict::ReplaceSymlink => format!(
            "if [ -L \"$dest\" ]; then\n  rm \"$dest\"\nelse\n  printf '%s\\n' {message} >&2\n  exit 1\nfi\n",
            message = shell_quote("Dotfile target already exists and is not a symlink")
        ),
        DotfileConflict::Backup => {
            "backup=\"$dest.decune-backup-$(date +%s)\"\nindex=0\nwhile [ -e \"$backup\" ] || [ -L \"$backup\" ]; do\n  index=$((index + 1))\n  backup=\"$dest.decune-backup-$(date +%s)-$index\"\ndone\nmv \"$dest\" \"$backup\"\n".to_owned()
        }
    };

    format!(
        "src={source}\ndest={target}\nparent={parent}\nmkdir -p \"$parent\"\nif [ -L \"$dest\" ] && [ \"$(readlink \"$dest\")\" = \"$src\" ]; then\n  :\nelif [ -e \"$dest\" ] || [ -L \"$dest\" ]; then\n{conflict_body}  ln -s \"$src\" \"$dest\"\nelse\n  ln -s \"$src\" \"$dest\"\nfi\n"
    )
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn relative_target_components(target: &str) -> Result<Vec<String>> {
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

fn symlink_resolution(resolve_symlink: bool) -> SymlinkResolution {
    if resolve_symlink {
        SymlinkResolution::Resolve
    } else {
        SymlinkResolution::Preserve
    }
}

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    #[cfg(unix)]
    use std::os::unix::fs as unix_fs;

    use super::*;
    use crate::config::{
        path::ConfigPathOrigin,
        resolved::{ResolvedConfig, ResolvedDotfile, ResolvedDotfileDisable, ResolvedDotfileEntry},
        types::{DotfileConflict, MountType},
        variables::VariableContext,
    };

    fn variables(workspace_root: &Path) -> VariableContext {
        VariableContext::new(
            workspace_root.to_path_buf(),
            "project".to_owned(),
            "/workspaces/project".to_owned(),
            "project".to_owned(),
            "abc123def456".to_owned(),
            1000,
            1000,
            "vscode".to_owned(),
            Some("/home/vscode".to_owned()),
        )
    }

    #[test]
    fn converts_dotfile_to_read_only_staging_mount() {
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

        let mounts =
            dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

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

        let mounts =
            dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

        assert_eq!(
            mounts[0].source.as_deref(),
            Some(link_source.to_str().unwrap())
        );
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

        let error = dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Dotfile target must be relative")
        );
    }

    #[test]
    fn expands_dotfile_target_variables_for_staging_mount() {
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

        let mounts =
            dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

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

        let mounts =
            dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

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

        let mounts =
            dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path())).unwrap();

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

        let error = dotfile_mount_specs(&config, workspace.path(), &variables(workspace.path()))
            .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("Dotfile target must be relative")
        );
    }

    #[test]
    fn expands_dotfile_target_variables_for_setup_script() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/nvim".to_owned(),
                target: ".config/${remoteUser}/nvim".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains("/opt/decune/dotfiles/.config/vscode/nvim"));
        assert!(script.contains("/home/vscode/.config/vscode/nvim"));
    }

    #[test]
    fn dotfile_setup_script_replaces_duplicate_expanded_target() {
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: ".decune/global-gitconfig".to_owned(),
                    target: ".config/${remoteUser}/gitconfig".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Global,
                },
                ResolvedDotfile {
                    source: ".decune/project-gitconfig".to_owned(),
                    target: ".config/vscode/gitconfig".to_owned(),
                    read_only: false,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Backup,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert_eq!(
            script
                .matches("dest='/home/vscode/.config/vscode/gitconfig'")
                .count(),
            1
        );
        assert!(script.contains(".decune-backup-$(date +%s)"));
        assert!(!script.contains("Dotfile target already exists\n"));
    }

    #[test]
    fn dotfile_setup_script_disables_global_dotfile_by_expanded_target() {
        let global_dotfile = ResolvedDotfile {
            source: ".decune/global-gitconfig".to_owned(),
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

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.is_empty());
    }

    #[test]
    fn setup_script_is_idempotent_for_existing_expected_symlink() {
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

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains("readlink \"$dest\""));
        assert!(script.contains("/opt/decune/dotfiles/.config/nvim"));
        assert!(script.contains("/home/vscode/.config/nvim"));
        assert!(script.contains("Dotfile target already exists"));
    }

    #[test]
    fn setup_script_fails_when_intermediate_dotfile_setup_fails() {
        let remote_home = tempfile::tempdir().unwrap();
        fs::write(remote_home.path().join(".config"), "not a directory").unwrap();
        let config = ResolvedConfig {
            dotfiles: vec![
                ResolvedDotfile {
                    source: ".decune/nvim".to_owned(),
                    target: ".config/nvim".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                },
                ResolvedDotfile {
                    source: ".decune/gitconfig".to_owned(),
                    target: ".gitconfig".to_owned(),
                    read_only: true,
                    resolve_symlink: true,
                    on_conflict: DotfileConflict::Fail,
                    origin: ConfigPathOrigin::Project,
                },
            ],
            ..ResolvedConfig::default()
        };

        let script = dotfile_setup_script(
            &config,
            remote_home.path().to_str().unwrap(),
            &variables(Path::new("/workspace")),
        )
        .unwrap();
        let output = Command::new("/bin/sh")
            .args(["-lc", &script])
            .output()
            .unwrap();

        assert!(!output.status.success());
    }

    #[test]
    fn setup_script_allows_root_remote_home() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Fail,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/", &variables(Path::new("/workspace"))).unwrap();

        assert!(script.contains("dest='/.gitconfig'"));
    }

    #[test]
    fn setup_script_replaces_only_existing_symlink_for_replace_symlink() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::ReplaceSymlink,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains("if [ -L \"$dest\" ]; then"));
        assert!(script.contains("rm \"$dest\""));
        assert!(script.contains("is not a symlink"));
        assert!(script.contains("rm \"$dest\"\nelse"));
        assert!(script.contains("fi\n  ln -s \"$src\" \"$dest\""));
    }

    #[test]
    fn setup_script_moves_existing_target_for_backup() {
        let config = ResolvedConfig {
            dotfiles: vec![ResolvedDotfile {
                source: ".decune/gitconfig".to_owned(),
                target: ".gitconfig".to_owned(),
                read_only: true,
                resolve_symlink: true,
                on_conflict: DotfileConflict::Backup,
                origin: ConfigPathOrigin::Project,
            }],
            ..ResolvedConfig::default()
        };

        let script =
            dotfile_setup_script(&config, "/home/vscode", &variables(Path::new("/workspace")))
                .unwrap();

        assert!(script.contains(".decune-backup-$(date +%s)"));
        assert!(script.contains("mv \"$dest\" \"$backup\""));
        assert!(script.contains("mv \"$dest\" \"$backup\"\n  ln -s \"$src\" \"$dest\""));
    }
}

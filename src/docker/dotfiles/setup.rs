use anyhow::{Context, Result};

use crate::{
    config::{resolved::ResolvedConfig, types::DotfileConflict, variables::VariableContext},
    docker::{
        client::DockerClient,
        exec::{ExecCommandSpec, exec_capture},
        user::ResolvedRemoteUser,
    },
};

use super::{
    DOTFILES_MOUNT_ROOT,
    targets::{container_parent, dotfile_mount_target, expanded_dotfiles, remote_home_target},
};

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

    fix_dotfiles_mount_root_ownership(client, container, remote_user).await;

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

async fn fix_dotfiles_mount_root_ownership(
    client: &DockerClient,
    container: &str,
    remote_user: &ResolvedRemoteUser,
) {
    let script = format!(
        "chown {}:{} '{}'",
        remote_user.uid, remote_user.gid, DOTFILES_MOUNT_ROOT,
    );
    let _ = exec_capture(
        client,
        container,
        &ExecCommandSpec {
            command: vec!["/bin/sh".to_owned(), "-c".to_owned(), script],
            user: Some("root".to_owned()),
            working_dir: None,
            env: Default::default(),
            redactions: Vec::new(),
            tty: false,
        },
    )
    .await;
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
        let source = dotfile_mount_target(&dotfile.target)?;
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

#[cfg(test)]
mod tests {
    use std::{fs, path::Path, process::Command};

    use super::*;
    use crate::config::{
        path::ConfigPathOrigin,
        resolved::{ResolvedConfig, ResolvedDotfile, ResolvedDotfileDisable, ResolvedDotfileEntry},
        types::DotfileConflict,
        variables::{VariableContext, VariableContextInput},
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
